//! Names the faulting code when title code crashes.
//!
//! A vectored handler observes the first 16 title C++ throws and translates
//! debugger thread-name notifications to host descriptions. It also prints the
//! kinds that end a process (access violations and the like, and
//! breakpoints: titles assert with `int3`, fatal without a debugger) as
//! first-chance, since a handler may still take them (ntdll's loader does
//! with its own probes). What nobody handles, of any kind (an uncaught C++
//! exception too), is reported from the unhandled-exception filter
//! (`report_unhandled`, which the runtime calls ahead of the title's own)
//! and appended to the crash log, with the title's last debug output, which
//! often states the reason. Panics of ours go to the crash log too.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, EXCEPTION_ACCESS_VIOLATION, EXCEPTION_BREAKPOINT, EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_PRIV_INSTRUCTION, EXCEPTION_STACK_OVERFLOW, HMODULE,
};

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, CONTEXT, EXCEPTION_POINTERS, EXCEPTION_RECORD, M128A,
    ReadProcessMemory, RtlLookupFunctionEntry, RtlVirtualUnwind, SetUnhandledExceptionFilter,
    UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThread, GetCurrentThreadStackLimits,
    GetProcessIdOfThread, OpenThread, SetThreadDescription, THREAD_QUERY_LIMITED_INFORMATION,
    THREAD_SET_LIMITED_INFORMATION,
};

use crate::insn;

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
/// Frames listed per report.
pub(crate) const MAX_FRAMES: usize = 40;

/// Memory saved to files when a crash is reported: (address, length, file).
#[cfg(feature = "diagnostics")]
static SAVE: std::sync::OnceLock<Vec<(usize, usize, std::path::PathBuf)>> =
    std::sync::OnceLock::new();

/// Debugging aid: on a reported crash, also write each `[address, address
/// + len)` (readable title memory, e.g. an engine table) to its file.
#[cfg(feature = "diagnostics")]
pub fn save_on_crash(ranges: Vec<(usize, usize, std::path::PathBuf)>) {
    let _ = SAVE.set(ranges);
}

/// The file crash reports are appended to, besides being printed.
static LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Optional callback invoked once after an unhandled crash is reported
/// (used by `XLaunchRestartOnCrash`). Stored in an atomic pointer so crash
/// handling never blocks on a mutex.
static ON_CRASH: std::sync::atomic::AtomicPtr<()> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Registers a callback to run once after an unhandled crash has been
/// reported (before the process terminates).
pub fn on_crash(hook: fn()) {
    ON_CRASH.store(hook as *mut (), Ordering::Release);
}

fn run_on_crash() {
    static FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if FIRED.swap(true, Ordering::SeqCst) {
        return;
    }
    let ptr = ON_CRASH.load(Ordering::Acquire);
    if !ptr.is_null() {
        // SAFETY: `on_crash` only stores valid `fn()` pointers.
        let hook: fn() = unsafe { std::mem::transmute(ptr) };
        hook();
    }
}

/// Also appends every unhandled-exception report and panic to `file`, so a
/// crash can be read after the console is gone.
pub fn log_crashes_to(file: std::path::PathBuf) {
    if LOG.set(file).is_ok() {
        let print = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            print(info);
            append_to_log(&format!(
                "XTP: panic on thread {:?}: {info}\n{}",
                std::thread::current().name().unwrap_or("?"),
                title_output()
            ));
        }));
    }
}

/// Appends a failure that is not an exception (the host GPU's) to the crash
/// log, with the title's last debug output, and prints it.
pub fn log_crash(text: &str) {
    let text = format!(
        "{text}
{}",
        title_output()
    );
    eprint!("{text}");
    append_to_log(&text);
}

fn append_to_log(text: &str) {
    use std::io::Write;
    let Some(file) = LOG.get() else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file);
    if let Ok(mut f) = opened {
        let _ = write!(
            f,
            "unix time {now}, process {}\n{text}\n",
            std::process::id()
        );
    }
}

/// The title's last lines of debug output, kept for crash reports.
static SAID: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
const SAID_LINES: usize = 8;

/// Keeps a line of the title's debug output for the next crash report.
pub fn note_title_output(line: &str) {
    if line.is_empty() {
        return;
    }
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.len() == SAID_LINES {
        said.pop_front();
    }
    said.push_back(line.to_owned());
}

/// The kept lines, one per report line; nothing if another thread holds
/// them (it may be the one that faulted).
fn title_output() -> String {
    let Ok(said) = SAID.try_lock() else {
        return String::new();
    };
    said.iter()
        .map(|l| format!("  title said: {l}\n"))
        .collect()
}

/// Installs the handlers, and the reports of failures no handler sees
/// (`fastfail`: CRT invalid parameters, the process ending with a
/// failure status). Idempotent through `Once`; call before running title code.
pub fn report_crashes() {
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        unsafe { report_unhandled(info) };
        EXCEPTION_CONTINUE_SEARCH
    }
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        // SAFETY: registers handlers that only read the exception they are
        // given; the title's own filter, once it sets one, takes the place of
        // ours and calls `report_unhandled` first.
        unsafe {
            AddVectoredExceptionHandler(0, Some(handler));
            SetUnhandledExceptionFilter(Some(filter));
        }
        crate::fastfail::install();
    });
}

/// What the vectored handler reports as it happens.
const FATAL: [i32; 6] = [
    EXCEPTION_ACCESS_VIOLATION,
    EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_PRIV_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO,
    EXCEPTION_STACK_OVERFLOW,
    EXCEPTION_BREAKPOINT,
];

/// Console CPUs are Zen 2: titles use AMD-only instructions Intel CPUs
/// lack. Does their job instead: `clzero` (`0f 01 fc`) zeroes the 64-byte
/// line at `rax` (at `eax` with an address-size prefix, a whole loop of
/// them at once: `insn::clzero_loop`); `monitorx`/`mwaitx` (`0f 01 fa`/`fb`)
/// are wait hints, so a spin loop around them works with mwaitx as a yield;
/// `rdpru` (`0f 01 fd`) reads MPERF and APERF from the TSC; `rdpid` reads
/// the processor number; `clwb` has nothing to do; SSE4a works on the
/// saved registers.
unsafe fn emulate_amd_only(info: *mut EXCEPTION_POINTERS) -> bool {
    // SAFETY: the system passes valid exception pointers; the faulting
    // instruction's bytes are readable, as the CPU just fetched them.
    unsafe {
        let record = &*(*info).ExceptionRecord;
        let context = &mut *(*info).ContextRecord;
        if record.ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION {
            return false;
        }
        let code = context.Rip as *const u8;
        // Only bytes inside the faulting instruction's page are read: the
        // next page may not be mapped.
        let room = 0x1000 - (context.Rip as usize & 0xfff);
        let mut buffer = [0u8; 32];
        let n = room.min(buffer.len());
        std::ptr::copy_nonoverlapping(code, buffer.as_mut_ptr(), n);
        let Some(len) = emulate(&buffer[..n], code, context) else {
            return false;
        };
        context.Rip += len as u64;
    }
    true
}

/// Does the job of the instruction `bytes` (read from `code`) start with,
/// on the saved `context`; returns its length (that of a whole loop for
/// the loops), or `None` if it is not one we emulate.
///
/// # Safety
/// `code` is the faulting instruction, readable up to its end, and the
/// memory `context`'s registers make it write is the title's to write.
unsafe fn emulate(bytes: &[u8], code: *const u8, context: &mut CONTEXT) -> Option<usize> {
    // SAFETY: what the caller guarantees.
    unsafe {
        match bytes {
            [0x0f, 0x01, 0xfc, ..] => return Some(clzero(bytes, context)),
            [0x67, 0x0f, 0x01, 0xfc, ..] => {
                // The line at `eax`, zero-extended, as a 32-bit address.
                (((context.Rax as u32) & !63) as usize as *mut u8).write_bytes(0, 64);
                return Some(4);
            }
            [0x0f, 0x01, 0xfa, ..] => return Some(3),
            [0x0f, 0x01, 0xfb, ..] => {
                std::thread::yield_now();
                return Some(3);
            }
            [0x0f, 0x01, 0xfd, ..] => {
                let read = insn::rdpru(context.Rcx as u32, std::arch::x86_64::_rdtsc());
                let read = read.unwrap_or(0);
                (context.Rax, context.Rdx) = (read & 0xffff_ffff, read >> 32);
                return Some(3);
            }
            _ => {}
        }
        if let Some((register, len)) = insn::rdpid(bytes) {
            let number = windows_sys::Win32::System::Threading::GetCurrentProcessorNumber();
            *gpr(context, register) = u64::from(number);
            return Some(len);
        }
        if let Some(pass) = clwb_loop(bytes) {
            // A whole write-back loop in one exception (see `clwb_loop`).
            let limit = *gpr(context, pass.limit);
            let base = gpr(context, pass.base);
            *base = if *base <= limit {
                base.wrapping_add(((limit - *base) / 64 + 1) * 64)
            } else {
                base.wrapping_add(64)
            };
            let flags = cmp_flags(*base, limit);
            context.EFlags = context.EFlags & !CMP_FLAGS | flags;
            return Some(pass.len);
        }
        if bytes.len() >= 15
            && let Some(len) = clwb_len(bytes[..15].try_into().unwrap_or([0; 15]))
        {
            // A write-back hint: the PC's memory is coherent, so it has nothing to do.
            return Some(len);
        }
        sse4a(code, &mut context.Anonymous.FltSave.XmmRegisters)
    }
}

/// `clzero` at the start of `bytes`: a whole loop of them if it is one
/// (`insn::clzero_loop`) and all of its lines are writable, else the line
/// at `rax`. Returns the length done.
///
/// # Safety
/// The line at `rax` is the title's to write.
unsafe fn clzero(bytes: &[u8], context: &mut CONTEXT) -> usize {
    let line = context.Rax & !63;
    if let Some(pass) = insn::clzero_loop(bytes) {
        let limit = *gpr(context, pass.limit);
        if let Some(passes) = pass.passes(context.Rax, limit)
            && writable(line, passes * 64)
        {
            // SAFETY: committed and writable, checked just above; the
            // title's loop would write the same lines.
            unsafe { (line as *mut u8).write_bytes(0, (passes * 64) as usize) };
            context.Rax += passes * 64;
            let flags = cmp_flags(context.Rax, limit);
            context.EFlags = context.EFlags & !CMP_FLAGS | flags;
            return pass.len;
        }
    }
    // SAFETY: the caller's guarantee.
    unsafe { (line as *mut u8).write_bytes(0, 64) };
    3
}

/// Whether all of `[start, start + len)` is committed, writable memory (a
/// guard page is not).
///
/// Each page is asked about once, whatever the number of lines in it: the
/// pages in the working set by their entries there (`QueryWorkingSetEx`,
/// about a microsecond for a loop's few pages, however large the
/// allocation around them), the others with `VirtualQuery`, one call for
/// as many of them as share its answer. `VirtualQuery` alone is no use on
/// a title's heap: it measures the run of pages alike around the address,
/// page by page, so it costs with the size of the allocation, not of the
/// range asked (about 7 µs per MiB on the cloud VM's Xeon). BTP 6 zeroes lines
/// of a 1 GiB heap in loops, where that took 4.5 ms a loop and stalled the
/// world's loading on CPUs without `clzero` (docs/analysis/l4-world.md).
fn writable(start: u64, len: u64) -> bool {
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    /// Pages asked of the working set in one call (512 bytes of stack).
    const BATCH: u64 = 32;
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    let mut page = start & !(PAGE - 1);
    while page < end {
        let count = (end - page).div_ceil(PAGE).min(BATCH);
        let batch_end = page + count * PAGE;
        let mut entries = [PSAPI_WORKING_SET_EX_INFORMATION::default(); BATCH as usize];
        for (entry, at) in entries
            .iter_mut()
            .zip((page..batch_end).step_by(PAGE as usize))
        {
            entry.VirtualAddress = at as *mut _;
        }
        // SAFETY: `count` entries of ours, sized as the call expects; it
        // reads the page tables, not the memory.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                entries.as_mut_ptr().cast(),
                (count as usize * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        let mut at = page;
        while at < batch_end {
            if asked {
                let entry = &entries[((at - page) / PAGE) as usize];
                // SAFETY: every bit pattern of the union is a valid usize.
                match resident_writable(unsafe { entry.VirtualAttributes.Flags }) {
                    Some(true) => {
                        at += PAGE;
                        continue;
                    }
                    Some(false) => return false,
                    None => {}
                }
            }
            // Not in the working set (never touched, or trimmed), or the
            // call failed: the region's answer, for every page it covers.
            let Some(region_end) = region_writable(at) else {
                return false;
            };
            at = region_end;
        }
        page = at;
    }
    true
}

/// The host's page.
const PAGE: u64 = 0x1000;

/// What a working-set entry's flags say of its page: `Some(writable)` for
/// a page in the working set, `None` for one that is not (its entry has no
/// protection then).
fn resident_writable(flags: usize) -> Option<bool> {
    // PSAPI_WORKING_SET_EX_BLOCK: Valid (bit 0), ShareCount (1 to 3),
    // Win32Protection (4 to 14).
    if flags & 1 == 0 {
        return None;
    }
    Some(protection_writable((flags >> 4) as u32 & 0x7ff))
}

/// A page protection that lets data be written: read-write or copy-on-write,
/// executable or not, and no guard.
fn protection_writable(protection: u32) -> bool {
    use windows_sys::Win32::System::Memory::{
        PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY,
    };
    let write = matches!(
        protection & 0xff,
        PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    );
    write && protection & PAGE_GUARD == 0
}

/// If the memory at `at` is committed and writable, the end of the run of
/// pages `VirtualQuery` finds alike from there (page-aligned, past `at`).
fn region_writable(at: u64) -> Option<u64> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    #[cfg(test)]
    tests::REGION_QUERIES.with(|n| n.set(n.get() + 1));
    // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: reads the address space's bookkeeping, not the memory.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    if n == 0 || info.State != MEM_COMMIT || !protection_writable(info.Protect) {
        return None;
    }
    Some(info.BaseAddress as u64 + info.RegionSize as u64)
}

/// `clwb m8` (`66 [rex] 0f ae /6`, a memory operand), which Intel CPUs before
/// Ice Lake lack: its length, or `None` for any other instruction.
fn clwb_len(code: [u8; 15]) -> Option<usize> {
    let at = if code[1] & 0xf0 == 0x40 { 2 } else { 1 };
    if code[0] != 0x66 || code[at] != 0x0f || code[at + 1] != 0xae {
        return None;
    }
    let modrm = code[at + 2];
    let (mode, rm) = (modrm >> 6, modrm & 7);
    if mode == 3 || (modrm >> 3) & 7 != 6 {
        return None;
    }
    let mut len = at + 3;
    let mut base = rm;
    if rm == 4 {
        base = code[len] & 7;
        len += 1;
    }
    len += match mode {
        0 if rm == 5 || (rm == 4 && base == 5) => 4,
        1 => 1,
        2 => 4,
        _ => 0,
    };
    Some(len)
}

/// A `clwb` write-back loop as the title's compilers build it, one cache line a
/// pass (`base` and `limit` any registers):
///
/// ```text
/// top: clwb [base]
///      add  base, 0x40
///      cmp  base, limit
///      jbe  top
/// ```
///
/// An exception costs about a microsecond and the title writes back whole
/// buffers this way while it loads, so the loop is done in one: `clwb` has
/// nothing to do on the PC, `base` ends past `limit` as the last pass leaves it.
struct ClwbLoop {
    base: u8,
    limit: u8,
    /// Bytes from `clwb` to the instruction after `jbe`.
    len: usize,
}

fn clwb_loop(code: &[u8]) -> Option<ClwbLoop> {
    // clwb [base]: 66 [40|41] 0f ae, ModRM mod 00, reg 6, no SIB or displacement.
    let (rex, at) = match code.get(1)? {
        0x40 | 0x41 => (code[1], 2),
        _ => (0, 1),
    };
    if *code.first()? != 0x66 || code.get(at..at + 2)? != [0x0f, 0xae] {
        return None;
    }
    let modrm = *code.get(at + 2)?;
    let rm = modrm & 7;
    if modrm >> 6 != 0 || (modrm >> 3) & 7 != 6 || rm == 4 || rm == 5 {
        return None;
    }
    let base = rm | (rex & 1) << 3;
    // add base, 0x40: REX.W(+B) 83 /0 ib.
    let add = at + 3;
    if *code.get(add..add + 4)? != [0x48 | base >> 3, 0x83, 0xc0 | (base & 7), 0x40] {
        return None;
    }
    // cmp base, limit: REX.W 39 /r with base in r/m.
    let cmp = add + 4;
    let &[cmp_rex @ 0x48..=0x4f, 0x39, cmp_modrm] = code.get(cmp..cmp + 3)? else {
        return None;
    };
    if cmp_modrm >> 6 != 3 || (cmp_modrm & 7 | (cmp_rex & 1) << 3) != base {
        return None;
    }
    let limit = (cmp_modrm >> 3) & 7 | (cmp_rex & 4) << 1;
    // jbe top.
    let jump = cmp + 3;
    let len = jump + 2;
    if *code.get(jump)? != 0x76 || i64::from(*code.get(jump + 1)? as i8) != -(len as i64) {
        return None;
    }
    (limit != base).then_some(ClwbLoop { base, limit, len })
}

/// The general register numbered `n` as instructions encode it.
fn gpr(context: &mut CONTEXT, n: u8) -> &mut u64 {
    match n & 15 {
        0 => &mut context.Rax,
        1 => &mut context.Rcx,
        2 => &mut context.Rdx,
        3 => &mut context.Rbx,
        4 => &mut context.Rsp,
        5 => &mut context.Rbp,
        6 => &mut context.Rsi,
        7 => &mut context.Rdi,
        8 => &mut context.R8,
        9 => &mut context.R9,
        10 => &mut context.R10,
        11 => &mut context.R11,
        12 => &mut context.R12,
        13 => &mut context.R13,
        14 => &mut context.R14,
        _ => &mut context.R15,
    }
}

/// The status flags `cmp a, b` leaves (CF, PF, AF, ZF, SF, OF) as `EFLAGS` bits.
fn cmp_flags(a: u64, b: u64) -> u32 {
    let r = a.wrapping_sub(b);
    u32::from(a < b)
        | u32::from((r as u8).count_ones().is_multiple_of(2)) << 2
        | (((a ^ b ^ r) >> 4) as u32 & 1) << 4
        | u32::from(r == 0) << 6
        | ((r >> 63) as u32) << 7
        | ((((a ^ b) & (a ^ r)) >> 63) as u32) << 11
}
const CMP_FLAGS: u32 = 0x8d5;

/// SSE4a's `extrq`/`insertq` (register forms, the only ones there are) on
/// the saved XMM registers; returns the instruction's length.
unsafe fn sse4a(code: *const u8, xmm: &mut [M128A; 16]) -> Option<usize> {
    // SAFETY: the caller's code bytes are readable up to the instruction's end.
    let b = |i: usize| unsafe { code.add(i).read() };
    let prefix = b(0);
    let (rex, at) = if b(1) & 0xf0 == 0x40 {
        (b(1), 2)
    } else {
        (0, 1)
    };
    if !matches!(prefix, 0x66 | 0xf2) || b(at) != 0x0f || !matches!(b(at + 1), 0x78 | 0x79) {
        return None;
    }
    let modrm = b(at + 2);
    if modrm >> 6 != 3 {
        return None;
    }
    let dst = usize::from((modrm >> 3) & 7 | (rex & 4) << 1);
    let src = usize::from(modrm & 7 | (rex & 1) << 3);
    let immediate = b(at + 1) == 0x78;
    let (fields, len) = if immediate {
        ((b(at + 3), b(at + 4)), at + 5)
    } else {
        // extrq reads its fields from src's low half, insertq from its high.
        let f = if prefix == 0x66 {
            xmm[src].Low
        } else {
            xmm[src].High as u64
        };
        ((f as u8, (f >> 8) as u8), at + 3)
    };
    let (n, at_bit) = (u32::from(fields.0 & 63), u32::from(fields.1 & 63));
    let bits = if n == 0 { 64 } else { n };
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    if prefix == 0x66 {
        // extrq: register form takes its fields from src, works on dst.
        if immediate && (modrm >> 3) & 7 != 0 {
            return None;
        }
        let target = if immediate { src } else { dst };
        xmm[target].Low = (xmm[target].Low >> at_bit) & mask;
    } else {
        let value = (xmm[src].Low & mask) << at_bit;
        xmm[dst].Low = xmm[dst].Low & !(mask << at_bit) | value;
    }
    Some(len)
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes valid exception pointers.
    if unsafe { emulate_amd_only(info) } {
        return -1; // EXCEPTION_CONTINUE_EXECUTION
    }
    // SAFETY: as above.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, &*(*info).ContextRecord) };
    match record.ExceptionCode {
        CPP_EXCEPTION => observe_cpp(record, context),
        THREAD_NAME_EXCEPTION => {
            let count = (record.NumberParameters as usize).min(record.ExceptionInformation.len());
            name_thread(&record.ExceptionInformation[..count]);
        }
        _ => {}
    }
    if FATAL.contains(&record.ExceptionCode) {
        // SAFETY: as above.
        unsafe { report("first-chance exception", info, false) };
    }
    // Observing a throw or a debugger thread-name notification must not
    // consume it: the debugger and the title's own SEH handlers come next.
    EXCEPTION_CONTINUE_SEARCH
}

/// For an unhandled-exception filter: reports the exception that is about
/// to end the process, and appends it to the crash log.
///
/// # Safety
/// `info` comes from the system, for an exception being dispatched.
pub unsafe fn report_unhandled(info: *const EXCEPTION_POINTERS) {
    // SAFETY: forwarded from the caller.
    unsafe { report("unhandled exception", info, true) };
    run_on_crash();
}

/// The bytes at `at` as hex, as many as the longest x86 instruction (15),
/// but none past the end of `at`'s page: the next page may not be mapped.
/// For an instruction the CPU refused, they name it (`0f 01 fd` is
/// `rdpru`), which the address alone does not.
///
/// # Safety
/// `at`'s page is readable.
unsafe fn instruction_bytes(at: usize) -> String {
    let n = (0x1000 - (at & 0xfff)).min(15);
    // SAFETY: `n` bytes from `at` stay inside its page, which the caller
    // says is readable.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, n) };
    let hex: Vec<_> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hex.join(" ")
}

/// MSVC's code for a C++ `throw`, and the three EH_MAGIC values its x64
/// ThrowInfo layout accepts. The debugger's legacy thread-name notification
/// is deliberately different: its parameters are the packed struct's words.
const CPP_EXCEPTION: i32 = 0xE06D_7363_u32 as i32;
const CPP_MAGICS: [usize; 3] = [0x1993_0520, 0x1993_0521, 0x1993_0522];
const THREAD_NAME_EXCEPTION: i32 = 0x406D_1388;
const FIRST_CHANCE_LIMIT: usize = 16;
const MAX_CATCHABLE_TYPES: usize = 32;
const MAX_STRING: usize = 256;
static FIRST_CHANCE: AtomicUsize = AtomicUsize::new(0);

/// Copies readable memory in the current process through the kernel. Returns
/// false for unmapped, reserved, no-access or guard pages.
pub fn read_memory(address: usize, dst: &mut [u8]) -> bool {
    if dst.is_empty() {
        return true;
    }
    let Some(end) = address.checked_add(dst.len() - 1).filter(|_| address != 0) else {
        return false;
    };
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    let mut at = address;
    loop {
        // SAFETY: a zeroed query buffer is valid; VirtualQuery reads bookkeeping.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: writes our buffer without dereferencing `at`.
        if unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) } == 0
            || info.State != MEM_COMMIT
            || info.Protect & (PAGE_GUARD | PAGE_NOACCESS) != 0
        {
            return false;
        }
        let Some(next) = (info.BaseAddress as usize).checked_add(info.RegionSize) else {
            return false;
        };
        if next > end {
            break;
        }
        if next <= at {
            return false;
        }
        at = next;
    }
    let mut done = 0;
    // SAFETY: `dst` is writable for its length; the kernel validates the
    // complete source range in this process instead of faulting on it.
    unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            address as *const _,
            dst.as_mut_ptr().cast(),
            dst.len(),
            &mut done,
        ) != 0
            && done == dst.len()
    }
}

fn read_array<const N: usize>(address: usize) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    read_memory(address, &mut bytes).then_some(bytes)
}

fn read_u32(address: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(read_array(address)?))
}

fn read_usize(address: usize) -> Option<usize> {
    Some(usize::from_ne_bytes(read_array(address)?))
}

/// A bounded C string, including a useful prefix when the next page is
/// unreadable. Reading page by page permits strings crossing readable pages.
fn bounded_cstr(address: usize, limit: usize) -> Result<String, (String, &'static str)> {
    let mut bytes = Vec::new();
    while bytes.len() < limit {
        let Some(at) = address.checked_add(bytes.len()) else {
            break;
        };
        let mut chunk = [0u8; MAX_STRING];
        let n = (limit - bytes.len())
            .min(0x1000 - (at & 0xfff))
            .min(chunk.len());
        if at == 0 || !read_memory(at, &mut chunk[..n]) {
            let prefix = String::from_utf8_lossy(&bytes).into_owned();
            return Err((prefix, "unreadable"));
        }
        if let Some(end) = chunk[..n].iter().position(|&b| b == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    Err((String::from_utf8_lossy(&bytes).into_owned(), "unterminated"))
}

/// SizeOfImage from a loaded PE32+ header, all read fallibly. This also
/// handles host runtime metadata without acquiring the title registry again.
fn image_size(base: usize) -> Option<usize> {
    if read_array::<2>(base)? != *b"MZ" {
        return None;
    }
    let pe = base.checked_add(read_u32(base.checked_add(0x3c)?)? as usize)?;
    if read_array::<4>(pe)? != *b"PE\0\0" {
        return None;
    }
    let optional = pe.checked_add(24)?;
    if read_array::<2>(optional)? != 0x20bu16.to_ne_bytes() {
        return None;
    }
    Some(read_u32(optional.checked_add(56)?)? as usize)
}

fn image_address(base: usize, size: usize, rva: usize, len: usize) -> Option<usize> {
    if rva.checked_add(len).is_none_or(|end| end > size) {
        return None;
    }
    base.checked_add(rva.checked_add(len)?)?;
    base.checked_add(rva)
}

/// Only these CRT families may intervene before a title frame. Match a DLL
/// basename and suffix boundary, never an arbitrary path substring.
fn runtime_family(module: &str) -> bool {
    let module = module.to_ascii_lowercase();
    let Some(stem) = module.strip_suffix(".dll") else {
        return false;
    };
    ["vcruntime140", "msvcp140"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|tail| {
            tail.is_empty()
                || tail == "d"
                || tail.strip_prefix('_').is_some_and(|version| {
                    !version.is_empty()
                        && version
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                })
        })
    })
}

fn throw_machinery(address: usize) -> bool {
    crate::fastfail::in_module(address, &["ntdll.dll", "kernelbase.dll", "ucrtbase.dll"])
        || module_of(address).and_then(|module| {
            module
                .rsplit_once('+')
                .map(|(name, _)| runtime_family(name))
        }) == Some(true)
}

fn title_frame<const N: usize>(frames: &[usize; N]) -> Option<usize> {
    let title = crate::threads::title_membership(frames)?
        .iter()
        .position(|&is_title| is_title)?;
    frames[..title]
        .iter()
        .all(|&pc| throw_machinery(pc))
        .then_some(frames[title])
}

/// Walk in dispatch order. A host/HLE/unknown caller stops classification;
/// finding a title farther down cannot attribute that host exception to it.
fn throw_site(record: &EXCEPTION_RECORD, context: &CONTEXT) -> Option<usize> {
    let mut frames = [0; MAX_FRAMES + 1];
    frames[0] = record.ExceptionAddress as usize;
    for (slot, pc) in frames[1..].iter_mut().zip(unwind(*context)) {
        *slot = pc as usize;
    }
    title_frame(&frames)
}

fn claim_first_chance() -> bool {
    FIRST_CHANCE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            (used < FIRST_CHANCE_LIMIT).then_some(used + 1)
        })
        .is_ok()
}

#[cfg(test)]
static FIRST_CHANCE_LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn observe_cpp(record: &EXCEPTION_RECORD, context: &CONTEXT) {
    if crate::threads::exiting() {
        return;
    }
    thread_local! { static OBSERVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    struct ObserveGuard;
    impl Drop for ObserveGuard {
        fn drop(&mut self) {
            OBSERVING.set(false);
        }
    }
    if OBSERVING.replace(true) {
        return;
    }
    let _guard = ObserveGuard;
    let p = &record.ExceptionInformation;
    if FIRST_CHANCE.load(Ordering::Relaxed) >= FIRST_CHANCE_LIMIT
        || record.NumberParameters != 4
        || !CPP_MAGICS.contains(&p[0])
    {
        return;
    }
    let Some(site) = throw_site(record, context) else {
        return;
    };
    // Each verified title event, including a rethrow, claims one slot.
    if !claim_first_chance() {
        return;
    }
    let line = format!(
        "XTP: first-chance C++ {} at {}",
        thrown(&p[..4]),
        locate(site)
    );
    // Match crash reports' stderr output, but never panic on an I/O error.
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{line}");
    #[cfg(test)]
    FIRST_CHANCE_LINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(line);
}

/// Applies a legacy Visual Studio THREADNAME_INFO notification. On x64 its
/// first three words contain { type, padding }, { name }, { id, flags }.
/// Legacy DWORD-count samples provide extra words; reserved flags are ignored.
fn name_thread(p: &[usize]) -> bool {
    if crate::threads::exiting() {
        return false;
    }
    if p.len() < 3 || p[0] as u32 != 0x1000 {
        return false;
    }
    let name = match bounded_cstr(p[1], MAX_STRING) {
        Ok(name) | Err((name, "unterminated")) => name,
        Err(_) => return false,
    };
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let id = p[2] as u32;
    // SAFETY: the sentinel names this thread; otherwise open only its ID.
    let thread = unsafe {
        if id == u32::MAX {
            GetCurrentThread()
        } else if id == 0 {
            return false;
        } else {
            OpenThread(
                THREAD_SET_LIMITED_INFORMATION | THREAD_QUERY_LIMITED_INFORMATION,
                0,
                id,
            )
        }
    };
    if thread.is_null() {
        return false;
    }
    // SAFETY: `thread` is current or open. Check ownership before naming,
    // then close every owned handle below, including rejected foreign ones.
    let ours = unsafe { GetProcessIdOfThread(thread) == GetCurrentProcessId() };
    // SAFETY: `wide` is NUL-terminated; only this process's threads are named.
    let result = ours && unsafe { SetThreadDescription(thread, wide.as_ptr()) } >= 0;
    if id != u32::MAX {
        // SAFETY: this branch opened `thread`; the current-thread pseudo
        // handle is never closed.
        unsafe { CloseHandle(thread) };
    }
    result
}

/// What a C++ throw threw. Every pointer and RVA comes from the exception,
/// so every access is bounded and fallible; no exception-object method runs.
fn thrown(p: &[usize]) -> String {
    if p.len() != 4 || !CPP_MAGICS.contains(&p[0]) {
        return "<invalid MSVC exception parameters>".into();
    }
    if p[1] == 0 && p[2] == 0 {
        return "<rethrow>".into();
    }
    let Some(size) = image_size(p[3]) else {
        return "<unreadable C++ image metadata>".into();
    };
    let Some(info_rva) = p[2].checked_sub(p[3]) else {
        return "<invalid ThrowInfo address>".into();
    };
    let Some(info) = image_address(p[3], size, info_rva, 16) else {
        return "<invalid ThrowInfo RVA>".into();
    };
    let Some(types_rva) = read_u32(info + 12).map(|rva| rva as usize) else {
        return "<unreadable ThrowInfo>".into();
    };
    let Some(types) = image_address(p[3], size, types_rva, 4) else {
        return "<invalid catchable-type array RVA>".into();
    };
    let Some(count) = read_u32(types).map(|n| n as usize) else {
        return "<unreadable catchable-type count>".into();
    };
    if count == 0 || count > MAX_CATCHABLE_TYPES {
        return format!("<invalid catchable-type count {count}>");
    }
    let Some(array_bytes) = count.checked_mul(4).and_then(|n| n.checked_add(4)) else {
        return "<overflowing catchable-type array>".into();
    };
    if image_address(p[3], size, types_rva, array_bytes).is_none() {
        return "<truncated catchable-type array>".into();
    }
    let mut names = Vec::with_capacity(count);
    let mut standard_base = None;
    for i in 0..count {
        let entry = types + 4 + i * 4;
        let Some(catchable_rva) = read_u32(entry).map(|rva| rva as usize) else {
            return "<unreadable catchable-type entry>".into();
        };
        let Some(catchable) = image_address(p[3], size, catchable_rva, 28) else {
            return "<invalid catchable-type RVA>".into();
        };
        let Some(descriptor_rva) = read_u32(catchable + 4).map(|rva| rva as usize) else {
            return "<unreadable type descriptor RVA>".into();
        };
        let Some(descriptor) = image_address(p[3], size, descriptor_rva, 17) else {
            return "<invalid type descriptor RVA>".into();
        };
        let room = size - descriptor_rva - 16;
        match bounded_cstr(descriptor + 16, MAX_STRING.min(room)) {
            Ok(name) => {
                if name == ".?AVexception@std@@" {
                    // PMD: a non-virtual base is at mdisp; do not guess a
                    // virtual base's layout or invoke its vtable.
                    standard_base = read_u32(catchable + 12)
                        .filter(|&pdisp| pdisp == u32::MAX)
                        .and_then(|_| read_u32(catchable + 8))
                        .map(|d| d as i32);
                }
                names.push(name);
            }
            Err((_, reason)) => return format!("<{reason} C++ type name>"),
        }
    }
    let first = &names[0];
    if matches!(first.as_str(), ".H" | ".I" | ".J" | ".K") {
        return match read_u32(p[1]) {
            Some(value) => format!("{first} = {value:#010x}"),
            None if p[1] == 0 => first.clone(),
            None => format!("{first} = <unreadable value>"),
        };
    }
    if !names.iter().any(|name| name == ".?AVexception@std@@") {
        return first.clone();
    }
    let Some(offset) = standard_base else {
        return format!("{first}: <unsupported std::exception base>");
    };
    // MSVC x64 std::exception stores its message after the base's vftable.
    let Some(message) = p[1]
        .checked_add_signed(offset as isize)
        .and_then(|base| base.checked_add(size_of::<usize>()))
        .and_then(read_usize)
    else {
        return format!("{first}: <unreadable exception object>");
    };
    if message == 0 {
        return format!("{first}: <null what()>");
    }
    match bounded_cstr(message, MAX_STRING) {
        Ok(message) => format!("{first}: {message}"),
        Err((prefix, reason)) if prefix.is_empty() => format!("{first}: <{reason} what()>"),
        Err((prefix, reason)) => {
            format!("{first}: <{reason} what() after {:?}>", prefix)
        }
    }
}

/// # Safety
/// See [`report_unhandled`].
unsafe fn report(what: &str, info: *const EXCEPTION_POINTERS, to_file: bool) {
    // SAFETY: the system passes valid exception pointers.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, &*(*info).ContextRecord) };
    let code = record.ExceptionCode;
    let at = record.ExceptionAddress as usize;
    let p = record.ExceptionInformation;
    let detail = if code == EXCEPTION_ACCESS_VIOLATION {
        let kind = match p[0] {
            0 => "read",
            1 => "write",
            _ => "execute",
        };
        format!(" ({kind} {:#x}: {})", p[1], memory_at(p[1]))
    } else if code == CPP_EXCEPTION {
        let count = (record.NumberParameters as usize).min(p.len());
        format!(" (C++ {} thrown)", thrown(&p[..count]))
    } else if code == EXCEPTION_ILLEGAL_INSTRUCTION || code == EXCEPTION_PRIV_INSTRUCTION {
        // SAFETY: the CPU refused the instruction at `at`, so it fetched it.
        format!(" (code {})", unsafe { instruction_bytes(at) })
    } else {
        String::new()
    };
    // Printed line by line (a fault while unwinding must not lose the
    // lines before it), and kept for the file.
    let mut text = String::new();
    let mut line = |l: String| {
        eprintln!("{l}");
        text.push_str(&l);
        text.push('\n');
    };
    line(format!(
        "XTP: {what} {:#010x} at {}{detail}",
        code as u32,
        locate(at)
    ));
    line(format!(
        "  rax {:#x} rbx {:#x} rcx {:#x} rdx {:#x} rsi {:#x} rdi {:#x} rsp {:#x} rbp {:#x}",
        context.Rax,
        context.Rbx,
        context.Rcx,
        context.Rdx,
        context.Rsi,
        context.Rdi,
        context.Rsp,
        context.Rbp
    ));
    line(format!(
        "  r8 {:#x} r9 {:#x} r10 {:#x} r11 {:#x} r12 {:#x} r13 {:#x} r14 {:#x} r15 {:#x}",
        context.R8,
        context.R9,
        context.R10,
        context.R11,
        context.R12,
        context.R13,
        context.R14,
        context.R15
    ));
    for (i, pc) in unwind(*context).into_iter().enumerate() {
        line(format!("  #{i:<2} {}", locate(pc as usize)));
    }
    if to_file {
        let said = title_output();
        eprint!("{said}");
        text.push_str(&said);
        append_to_log(&text);
    }
    #[cfg(feature = "diagnostics")]
    for (address, len, file) in SAVE.get().into_iter().flatten() {
        // SAFETY: the caller of `save_on_crash` named readable memory.
        let bytes = unsafe { std::slice::from_raw_parts(*address as *const u8, *len) };
        let saved = std::fs::write(file, bytes);
        eprintln!(
            "  saved {address:#x}+{len:#x} to {} ({saved:?})",
            file.display()
        );
    }
}

/// Return addresses of the faulting thread's frames, from the faulting
/// instruction up, by the unwind data of each image (the Windows loader
/// registered it for title images too). Code without unwind data (our
/// thunks, leaf functions) is taken to have its return address at `rsp`.
pub(crate) fn unwind(context: CONTEXT) -> Vec<u64> {
    let (mut low, mut high) = (0, 0);
    // SAFETY: writes the current thread's stack bounds.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    let mut frames = Vec::with_capacity(MAX_FRAMES);
    unwind_into(context, low, high, &mut frames);
    frames
}

/// A `CONTEXT` at the 16-byte alignment the unwind and capture functions need.
#[repr(C, align(16))]
pub(crate) struct Aligned16(pub(crate) CONTEXT);

/// [`unwind`] for any thread of the process, its stack being `low..high`;
/// pushes no more than `MAX_FRAMES`, so it does not allocate when `frames`
/// has that capacity (another thread may be suspended holding the heap).
pub(crate) fn unwind_into(context: CONTEXT, low: usize, high: usize, frames: &mut Vec<u64>) {
    // `RtlVirtualUnwind` writes the context with 16-byte aligned stores, and
    // windows-sys declares `CONTEXT` without that alignment: a by-value copy on
    // the stack can land 8 bytes off and fault (seen in a debug build).
    let mut aligned = Aligned16(context);
    let context = &mut aligned.0;
    frames.push(context.Rip);
    while frames.len() < MAX_FRAMES {
        // Stop where reading the stack would fault inside this handler.
        let rsp = context.Rsp as usize;
        if rsp < low || rsp + 8 > high {
            break;
        }
        let mut image = 0;
        // SAFETY: looks up the function table entry covering `Rip`, if any.
        let entry = unsafe { RtlLookupFunctionEntry(context.Rip, &mut image, null_mut()) };
        if entry.is_null() {
            // SAFETY: `rsp` is inside this thread's stack.
            context.Rip = unsafe { (rsp as *const u64).read() };
            context.Rsp += 8;
        } else {
            let (mut data, mut frame) = (null_mut(), 0);
            // SAFETY: `entry` belongs to `image`; the context is our copy.
            unsafe {
                RtlVirtualUnwind(
                    UNW_FLAG_NHANDLER,
                    image,
                    context.Rip,
                    entry,
                    context,
                    &mut data,
                    &mut frame,
                    null_mut(),
                )
            };
        }
        if context.Rip == 0 {
            break;
        }
        frames.push(context.Rip);
    }
}

/// `module+rva`, or the bare address.
pub fn locate(address: usize) -> String {
    module_of(address).unwrap_or_else(|| format!("{address:#x}"))
}

fn module_of(address: usize) -> Option<String> {
    let mut module: HMODULE = std::ptr::null_mut();
    // SAFETY: looks up the module containing `address`; no reference taken.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address as *const u16,
            &mut module,
        )
    };
    if found == 0 {
        return None;
    }
    let mut path = [0u16; 260];
    // SAFETY: `module` is a loaded module; the buffer length is passed.
    let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) };
    let path = String::from_utf16_lossy(&path[..len as usize]);
    let name = path.rsplit('\\').next().unwrap_or(&path);
    Some(format!("{name}+{:#x}", address - module as usize))
}

/// What is at `address`, for an access violation there: nothing, memory
/// reserved but never committed, a guard page, or the protection the access
/// broke (with the module, for an image's pages). Each points at a different
/// cause: a wild pointer, an allocation whose commit failed, a stack
/// overrun, a write to code or constants.
fn memory_at(address: usize) -> String {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_IMAGE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE,
        PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD,
        PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY, VirtualQuery,
    };
    // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: reads the address space's bookkeeping, not `address` itself.
    let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
    if n == 0 {
        // Above the process's address space: Windows reports a
        // non-canonical address as all ones.
        return "outside the address space".into();
    }
    let protect = match info.State {
        MEM_FREE => return "nothing mapped there".into(),
        MEM_RESERVE => return "reserved, never committed".into(),
        _ if info.Protect & PAGE_GUARD != 0 => return "a guard page".into(),
        _ => match info.Protect & 0xff {
            PAGE_NOACCESS => "no access".into(),
            PAGE_READONLY => "read-only".into(),
            PAGE_READWRITE => "read-write".into(),
            PAGE_WRITECOPY => "copy-on-write".into(),
            PAGE_EXECUTE => "execute-only".into(),
            PAGE_EXECUTE_READ => "read-execute".into(),
            PAGE_EXECUTE_READWRITE => "read-write-execute".into(),
            PAGE_EXECUTE_WRITECOPY => "copy-on-write, executable".into(),
            _ => format!("protection {:#x}", info.Protect),
        },
    };
    match module_of(address) {
        Some(module) if info.Type == MEM_IMAGE => format!("{protect}, in {module}"),
        _ => protect,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize as TestAtomicUsize};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_RECORD, RtlCaptureContext, RtlCaptureStackBackTrace,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        RaiseException, RemoveVectoredExceptionHandler,
    };
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    #[test]
    #[inline(never)]
    fn unwinds_like_the_system_walk() {
        // SAFETY: zeroed CONTEXT is valid; both calls write our buffers.
        let (frames, walked) = unsafe {
            // `RtlCaptureContext` wants it 16-byte aligned too.
            let mut context = Aligned16(std::mem::zeroed());
            RtlCaptureContext(&mut context.0);
            let mut walked = [null_mut(); 8];
            let n = RtlCaptureStackBackTrace(0, 8, walked.as_mut_ptr(), null_mut());
            (unwind(context.0), walked[..usize::from(n)].to_vec())
        };
        // Frame 0 differs (two call sites in this function); callers agree.
        assert!(walked.len() >= 4, "{walked:?}");
        let ours: Vec<_> = frames[1..walked.len()]
            .iter()
            .map(|&f| f as usize)
            .collect();
        let system: Vec<_> = walked[1..].iter().map(|&f| f as usize).collect();
        assert_eq!(ours, system);
    }

    #[test]
    fn memory_at_names_what_an_access_hit() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY,
            PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        // SAFETY: reserves four pages of ours and commits three of them;
        // nothing reads or writes them.
        let base = unsafe {
            let base = VirtualAlloc(std::ptr::null(), 0x4000, MEM_RESERVE, PAGE_NOACCESS);
            assert!(!base.is_null());
            let page = |i: usize, protect| {
                let at = base.cast::<u8>().add(i * 0x1000).cast();
                assert!(!VirtualAlloc(at, 0x1000, MEM_COMMIT, protect).is_null());
            };
            page(1, PAGE_READONLY);
            page(2, PAGE_READWRITE | PAGE_GUARD);
            page(3, PAGE_NOACCESS);
            base as usize
        };
        assert_eq!(memory_at(base + 8), "reserved, never committed");
        assert_eq!(memory_at(base + 0x1008), "read-only");
        assert_eq!(memory_at(base + 0x2008), "a guard page");
        assert_eq!(memory_at(base + 0x3008), "no access");
        // SAFETY: releases the reservation made above.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
        assert_eq!(memory_at(base + 8), "nothing mapped there");
        assert_eq!(memory_at(0), "nothing mapped there");
        assert_eq!(memory_at(usize::MAX), "outside the address space");
        // This test's own code: an image page, named by its module.
        let code = memory_at(memory_at_names_what_an_access_hit as *const () as usize);
        let named = code.starts_with("read-execute, in xtp_loader-");
        assert!(named, "{code}");
    }

    /// On an Intel CPU, the real instructions go through the vectored
    /// handler. On an AMD CPU they run natively, and the tests below test
    /// the emulation.
    #[test]
    fn clwb_loops() {
        // clwb [rcx]; add rcx, 0x40; cmp rcx, rax; jbe top
        let a = clwb_loop(&[
            0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf3,
        ])
        .expect("rcx loop");
        assert_eq!((a.base, a.limit, a.len), (1, 0, 13));
        // clwb [r9]; add r9, 0x40; cmp r9, rax; jbe top
        let b = clwb_loop(&[
            0x66, 0x41, 0x0f, 0xae, 0x31, 0x49, 0x83, 0xc1, 0x40, 0x49, 0x39, 0xc1, 0x76, 0xf2,
        ])
        .expect("r9 loop");
        assert_eq!((b.base, b.limit, b.len), (9, 0, 14));
        // A different stride, a jump elsewhere, or a cut-off loop is not one.
        assert!(
            clwb_loop(&[
                0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x20, 0x48, 0x39, 0xc1, 0x76, 0xf3
            ])
            .is_none()
        );
        assert!(
            clwb_loop(&[
                0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf0
            ])
            .is_none()
        );
        assert!(clwb_loop(&[0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40]).is_none());
        // The flags the last cmp leaves: base just past limit.
        assert_eq!(cmp_flags(0x1040, 0x1000) & 0x41, 0); // CF and ZF clear: the loop ends
        assert_eq!(cmp_flags(0x1000, 0x1000) & 0x40, 0x40);
    }

    #[test]
    fn clwb_lengths() {
        let pad = |b: &[u8]| {
            let mut c = [0u8; 15];
            c[..b.len()].copy_from_slice(b);
            c
        };
        assert_eq!(clwb_len(pad(&[0x66, 0x41, 0x0f, 0xae, 0x31])), Some(5)); // clwb [r9]
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0x30])), Some(4)); // clwb [rax]
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0x70, 0x40])), Some(5)); // clwb [rax+0x40]
        assert_eq!(
            clwb_len(pad(&[0x66, 0x0f, 0xae, 0xb0, 0, 1, 0, 0])),
            Some(8)
        ); // [rax+disp32]
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0x34, 0x24])), Some(5)); // clwb [rsp]
        assert_eq!(
            clwb_len(pad(&[0x66, 0x0f, 0xae, 0x35, 0, 0, 0, 0])),
            Some(8)
        ); // [rip+disp32]
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0x38])), None); // clflushopt, /7
        assert_eq!(clwb_len(pad(&[0x0f, 0xae, 0x30])), None); // xsaveopt, no 66
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0xf0])), None); // register form
    }

    #[test]
    fn clzero_zeroes_its_line_on_any_cpu() {
        report_crashes();
        #[repr(align(64))]
        struct Line([u8; 128]);
        let mut line = Line([0xff; 128]);
        let at = line.0.as_mut_ptr().wrapping_add(5); // inside the first line
        // SAFETY: clzero writes only the 64-byte line holding `at`, ours.
        unsafe { std::arch::asm!(".byte 0x0f, 0x01, 0xfc", in("rax") at) };
        assert!(line.0[..64].iter().all(|&b| b == 0));
        assert!(line.0[64..].iter().all(|&b| b == 0xff));
        // insertq xmm0, xmm1, len 16, at 16; then extrq xmm0, len 8, at 20.
        let out: u64;
        // SAFETY: touches only the named registers.
        unsafe {
            std::arch::asm!(
                "movq xmm0, {a}", "movq xmm1, {b}",
                ".byte 0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10",
                "movq {a}, xmm0",
                ".byte 0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14",
                "movq {b}, xmm0",
                a = inout(reg) 0x1111_2222_3333_4444u64 => _, b = inout(reg) 0xabcdu64 => out,
                out("xmm0") _, out("xmm1") _)
        };
        assert_eq!(out, 0xbc); // 0x1111_2222_abcd_4444 >> 20, 8 bits
        // SAFETY: monitorx/mwaitx are wait hints that write no memory.
        unsafe {
            std::arch::asm!(".byte 0x0f, 0x01, 0xfa", ".byte 0x0f, 0x01, 0xfb",
                in("rax") at, in("ecx") 0, in("edx") 0)
        };
    }

    fn reg(low: u64, high: u64) -> M128A {
        M128A {
            Low: low,
            High: high as i64,
        }
    }

    /// Gives `code` to the emulation as if it faulted with `exception` at
    /// its first byte, with `rax` and `xmm` as the saved registers. The
    /// emulation does not execute `code`, so this works on any CPU. Returns
    /// the number of bytes that the emulation skipped, or `None` if it did
    /// not emulate the instruction. `xmm` gets the saved registers after.
    fn emulate_as(exception: i32, code: &[u8], rax: u64, xmm: &mut [M128A; 16]) -> Option<u64> {
        // `int3` after the instruction, so that a decoder that reads too
        // far sees no valid bytes.
        let mut bytes = [0xcc_u8; 16];
        bytes[..code.len()].copy_from_slice(code);
        let start = bytes.as_ptr() as u64;
        // SAFETY: a zeroed record and context are valid; the emulation
        // reads only `bytes` and writes only `context` (and memory at
        // `rax` for clzero, which the caller owns).
        unsafe {
            let mut record: EXCEPTION_RECORD = std::mem::zeroed();
            record.ExceptionCode = exception;
            let mut context: CONTEXT = std::mem::zeroed();
            context.Rip = start;
            context.Rax = rax;
            context.Anonymous.FltSave.XmmRegisters = *xmm;
            let mut pointers = EXCEPTION_POINTERS {
                ExceptionRecord: &mut record,
                ContextRecord: &mut context,
            };
            let emulated = emulate_amd_only(&mut pointers);
            *xmm = context.Anonymous.FltSave.XmmRegisters;
            emulated.then(|| context.Rip - start)
        }
    }

    fn emulate(code: &[u8], rax: u64, xmm: &mut [M128A; 16]) -> Option<u64> {
        emulate_as(EXCEPTION_ILLEGAL_INSTRUCTION, code, rax, xmm)
    }

    fn low(xmm: &[M128A; 16], i: usize) -> u64 {
        xmm[i].Low
    }

    #[test]
    fn emulated_clzero_zeroes_its_line() {
        #[repr(align(64))]
        struct Line([u8; 128]);
        let mut line = Line([0xff; 128]);
        let at = line.0.as_mut_ptr().wrapping_add(5) as u64; // inside the first line
        let mut xmm = [reg(0, 0); 16];
        assert_eq!(emulate(&[0x0f, 0x01, 0xfc], at, &mut xmm), Some(3));
        assert!(line.0[..64].iter().all(|&b| b == 0));
        assert!(line.0[64..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn emulated_monitorx_and_mwaitx_are_skipped() {
        let mut xmm = [reg(1, 2); 16];
        assert_eq!(emulate(&[0x0f, 0x01, 0xfa], 0, &mut xmm), Some(3));
        assert_eq!(emulate(&[0x0f, 0x01, 0xfb], 0, &mut xmm), Some(3));
        assert!(xmm.iter().all(|r| r.Low == 1 && r.High == 2));
    }

    #[test]
    fn emulated_insertq_and_extrq_with_immediates() {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1111_2222_3333_4444, 0x5555);
        xmm[1] = reg(0xabcd, 0x6666);
        // insertq xmm0, xmm1, length 16, index 16
        let insertq = [0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10];
        assert_eq!(emulate(&insertq, 0, &mut xmm), Some(6));
        assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
        assert_eq!((low(&xmm, 1), xmm[1].High), (0xabcd, 0x6666));
        // extrq xmm0, length 8, index 20
        let extrq = [0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14];
        assert_eq!(emulate(&extrq, 0, &mut xmm), Some(6));
        assert_eq!(low(&xmm, 0), 0xbc);
    }

    #[test]
    fn emulated_insertq_and_extrq_with_fields_in_a_register() {
        let mut xmm = [reg(0, 0); 16];
        // insertq xmm0, xmm1: length and index in xmm1's high half.
        xmm[0] = reg(0x1111_2222_3333_4444, 0);
        xmm[1] = reg(0xabcd, 0x10_10);
        assert_eq!(emulate(&[0xf2, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
        assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
        // extrq xmm0, xmm1: length and index in xmm1's low half.
        xmm[1] = reg(0x14_08, 0);
        assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
        assert_eq!(low(&xmm, 0), 0xbc);
    }

    #[test]
    fn emulated_sse4a_reaches_xmm8_to_xmm15() {
        let mut xmm = [reg(0, 0); 16];
        xmm[8] = reg(0x1111_2222_3333_4444, 0);
        xmm[9] = reg(0xabcd, 0);
        // insertq xmm8, xmm9, length 16, index 16 (REX.R and REX.B)
        let insertq = [0xf2, 0x45, 0x0f, 0x78, 0xc1, 0x10, 0x10];
        assert_eq!(emulate(&insertq, 0, &mut xmm), Some(7));
        assert_eq!(low(&xmm, 8), 0x1111_2222_abcd_4444);
        assert_eq!((low(&xmm, 0), low(&xmm, 1)), (0, 0));
        // extrq xmm9, length 8, index 4 (REX.B)
        let extrq = [0x66, 0x41, 0x0f, 0x78, 0xc1, 0x08, 0x04];
        assert_eq!(emulate(&extrq, 0, &mut xmm), Some(7));
        assert_eq!(low(&xmm, 9), 0xbc);
        assert_eq!(low(&xmm, 1), 0);
    }

    #[test]
    fn emulated_sse4a_length_0_is_64_bits() {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1122_3344_5566_7788, 0);
        // extrq xmm0, length 0 (64), index 0. A non-zero index with
        // length 64 goes past bit 63, and AMD leaves that result undefined.
        assert_eq!(
            emulate(&[0x66, 0x0f, 0x78, 0xc0, 0x00, 0x00], 0, &mut xmm),
            Some(6)
        );
        assert_eq!(low(&xmm, 0), 0x1122_3344_5566_7788);
        // insertq xmm0, xmm1, length 0 (64), index 0
        xmm[1] = reg(0xdead_beef_cafe_f00d, 0);
        assert_eq!(
            emulate(&[0xf2, 0x0f, 0x78, 0xc1, 0x00, 0x00], 0, &mut xmm),
            Some(6)
        );
        assert_eq!(low(&xmm, 0), 0xdead_beef_cafe_f00d);
    }

    #[test]
    fn other_instructions_and_exceptions_are_not_emulated() {
        let mut xmm = [reg(0, 0); 16];
        assert_eq!(emulate(&[0x0f, 0x0b], 0, &mut xmm), None); // ud2
        // extrq with a memory operand, which does not exist
        assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0x00], 0, &mut xmm), None);
        // extrq with immediates needs ModRM.reg 0
        let bad_extrq = [0x66, 0x0f, 0x78, 0xc8, 0x08, 0x14];
        assert_eq!(emulate(&bad_extrq, 0, &mut xmm), None);
        // a clzero that did not cause an illegal instruction
        let clzero = [0x0f, 0x01, 0xfc];
        assert_eq!(
            emulate_as(EXCEPTION_ACCESS_VIOLATION, &clzero, 0, &mut xmm),
            None
        );
    }

    #[test]
    fn instruction_bytes_stop_at_the_end_of_their_page() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualProtect,
        };
        // rdpru, then the next instruction (mov rcx, rax) and padding.
        let mut code = [0x90_u8; 16];
        code[..6].copy_from_slice(&[0x0f, 0x01, 0xfd, 0x48, 0x89, 0xc1]);
        // SAFETY: two fresh pages of ours, the second made unreadable, so a
        // read past the end of the first faults.
        unsafe {
            let page = VirtualAlloc(
                std::ptr::null(),
                0x2000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!page.is_null());
            let mut old = 0;
            let guard = page.add(0x1000).cast();
            assert_ne!(VirtualProtect(guard, 0x1000, PAGE_NOACCESS, &mut old), 0);
            page.copy_from_nonoverlapping(code.as_ptr(), code.len());
            let whole = "0f 01 fd 48 89 c1 90 90 90 90 90 90 90 90 90";
            assert_eq!(instruction_bytes(page as usize), whole);
            // The page's last three bytes: rdpru, and nothing read after it.
            let end = page.add(0x1000 - 3);
            end.copy_from_nonoverlapping(code.as_ptr(), 3);
            assert_eq!(instruction_bytes(end as usize), "0f 01 fd");
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
    }

    /// Gives `code` to the emulation as if it faulted at its first byte with
    /// `context`'s registers, like `emulate_as`; the registers it changes
    /// are left in `context`.
    fn emulate_in(code: &[u8], context: &mut CONTEXT) -> Option<u64> {
        let mut bytes = [0xcc_u8; 16];
        bytes[..code.len()].copy_from_slice(code);
        let start = bytes.as_ptr() as u64;
        context.Rip = start;
        // SAFETY: a zeroed record is valid; the emulation reads only `bytes`
        // and writes `context` and the memory its registers name, which the
        // tests own.
        unsafe {
            let mut record: EXCEPTION_RECORD = std::mem::zeroed();
            record.ExceptionCode = EXCEPTION_ILLEGAL_INSTRUCTION;
            let mut pointers = EXCEPTION_POINTERS {
                ExceptionRecord: &mut record,
                ContextRecord: context,
            };
            let emulated = emulate_amd_only(&mut pointers);
            emulated.then(|| context.Rip - start)
        }
    }

    fn context() -> CONTEXT {
        // SAFETY: a zeroed CONTEXT is valid.
        unsafe { std::mem::zeroed() }
    }

    #[test]
    fn emulated_rdpru_reads_mperf_and_aperf_from_the_tsc() {
        let rdpru = [0x0f, 0x01, 0xfd];
        let mut c = context();
        c.Rcx = 1; // APERF
        c.EFlags = 0x8d5; // every status flag, which rdpru leaves alone
        // SAFETY: reads the time-stamp counter.
        let before = unsafe { std::arch::x86_64::_rdtsc() };
        assert_eq!(emulate_in(&rdpru, &mut c), Some(3));
        assert_eq!((c.Rax >> 32, c.Rdx >> 32), (0, 0), "32-bit halves");
        assert!(c.Rdx << 32 | c.Rax >= before);
        assert_eq!(c.EFlags, 0x8d5);
        // No such register: zero.
        c.Rcx = 2;
        assert_eq!(emulate_in(&rdpru, &mut c), Some(3));
        assert_eq!((c.Rax, c.Rdx), (0, 0));
    }

    #[test]
    fn emulated_rdpid_writes_the_processor_number() {
        let mut c = context();
        c.R9 = u64::MAX;
        // rdpid r9
        assert_eq!(emulate_in(&[0xf3, 0x41, 0x0f, 0xc7, 0xf9], &mut c), Some(5));
        assert!(c.R9 < 64, "a processor of the group: {:#x}", c.R9);
        assert_eq!(c.Rax, 0, "no other register");
    }

    /// A fresh page of ours below 4 GiB, where a 32-bit address reaches.
    fn low_page() -> *mut u8 {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
        };
        for hint in (0x1000_0000_usize..0xf000_0000).step_by(0x10_0000) {
            // SAFETY: a fresh allocation at a free address, or null.
            let page = unsafe {
                VirtualAlloc(
                    hint as *const _,
                    0x1000,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            if !page.is_null() {
                return page.cast();
            }
        }
        panic!("no free page below 4 GiB");
    }

    #[test]
    fn emulated_clzero_with_an_address_size_prefix_zeroes_the_line_at_eax() {
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
        let page = low_page();
        // SAFETY: our page.
        let bytes = unsafe {
            page.write_bytes(0xff, 0x1000);
            std::slice::from_raw_parts(page, 0x1000)
        };
        let mut c = context();
        // The upper half of rax is not part of a 32-bit address.
        c.Rax = 0xdead_0000_0000_0000 | (page as u64 + 0x45);
        assert_eq!(emulate_in(&[0x67, 0x0f, 0x01, 0xfc], &mut c), Some(4));
        assert!(bytes[0x40..0x80].iter().all(|&b| b == 0));
        assert!(
            bytes[..0x40]
                .iter()
                .chain(&bytes[0x80..])
                .all(|&b| b == 0xff)
        );
        // SAFETY: releases the page, which nothing uses any more.
        unsafe { VirtualFree(page.cast(), 0, MEM_RELEASE) };
    }

    /// clzero; add rax, 0x40; cmp rax, rcx; jbe top
    const CLZERO_JBE: [u8; 12] = [
        0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x39, 0xc8, 0x76, 0xf4,
    ];

    #[test]
    fn emulated_clzero_loop_zeroes_every_line_in_one_exception() {
        #[repr(align(64))]
        struct Lines([u8; 6 * 64]);
        let mut lines = Lines([0xff; 6 * 64]);
        let start = lines.0.as_mut_ptr() as u64;
        let mut c = context();
        c.Rax = start + 0x45; // in line 1
        c.Rcx = start + 0x105; // in line 4: lines 1 to 4, then past it
        assert_eq!(emulate_in(&CLZERO_JBE, &mut c), Some(12));
        assert_eq!(c.Rax, start + 0x145, "as the last pass leaves it");
        assert_eq!(c.EFlags & 0x41, 0, "CF and ZF clear: the loop ends");
        assert!(lines.0[0x40..0x140].iter().all(|&b| b == 0));
        assert!(lines.0[..0x40].iter().all(|&b| b == 0xff));
        assert!(lines.0[0x140..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn a_clzero_loop_into_memory_it_cannot_write_goes_a_line_at_a_time() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualProtect,
        };
        // SAFETY: two fresh pages of ours, the second made read-only.
        let page = unsafe {
            let page = VirtualAlloc(
                std::ptr::null(),
                0x2000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!page.is_null());
            page.write_bytes(0xff, 0x1000);
            let mut old = 0;
            assert_ne!(
                VirtualProtect(page.add(0x1000).cast(), 0x1000, PAGE_READONLY, &mut old),
                0
            );
            page
        };
        let mut c = context();
        c.Rax = page as u64 + 0xfc0; // the first page's last line
        c.Rcx = page as u64 + 0x1100; // into the read-only page
        assert_eq!(emulate_in(&CLZERO_JBE, &mut c), Some(3), "clzero alone");
        assert_eq!(c.Rax, page as u64 + 0xfc0, "the loop's add is still to run");
        // SAFETY: our first page, still readable.
        let first = unsafe { std::slice::from_raw_parts(page, 0x1000) };
        assert!(first[0xfc0..].iter().all(|&b| b == 0));
        assert!(first[..0xfc0].iter().all(|&b| b == 0xff));
        // SAFETY: releases the pages, which nothing uses any more.
        unsafe { VirtualFree(page.cast(), 0, MEM_RELEASE) };
    }

    thread_local! {
        /// `VirtualQuery` calls `writable` made on this thread (each test
        /// runs on its own).
        pub(super) static REGION_QUERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn region_queries() -> usize {
        REGION_QUERIES.with(|n| n.get())
    }

    /// `pages` fresh committed read-write pages of ours.
    fn fresh_pages(pages: usize) -> *mut u8 {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
        };
        // SAFETY: a fresh allocation, or null.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                pages * 0x1000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        assert!(!at.is_null());
        at.cast()
    }

    fn release(at: *mut u8) {
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
        // SAFETY: an allocation of ours that nothing uses any more.
        unsafe { VirtualFree(at.cast(), 0, MEM_RELEASE) };
    }

    #[test]
    fn writable_answers_for_touched_untouched_and_protected_pages() {
        use windows_sys::Win32::System::Memory::{
            MEM_RESERVE, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
        };
        // 47 pages, more than one batch: the first 20 touched, the rest not.
        // The 48th, in the allocation granule's rest, is free.
        let heap = fresh_pages(47);
        // SAFETY: our pages.
        unsafe { heap.write_bytes(0xff, 20 * 0x1000) };
        let at = heap as u64;
        assert!(writable(at + 0x40, 0x3980), "touched pages");
        assert!(
            writable(at + 30 * 0x1000, 0x3000),
            "untouched, committed pages"
        );
        assert!(writable(at, 47 * 0x1000), "both, over two batches");
        assert!(!writable(at, 48 * 0x1000), "one page past the allocation");
        let mut old = 0;
        // SAFETY: our pages; a touched one made read-only, an untouched
        // one a guard page.
        unsafe {
            assert_ne!(
                VirtualProtect(heap.add(5 * 0x1000).cast(), 0x1000, PAGE_READONLY, &mut old),
                0
            );
            assert_ne!(
                VirtualProtect(
                    heap.add(40 * 0x1000).cast(),
                    0x1000,
                    PAGE_READWRITE | PAGE_GUARD,
                    &mut old
                ),
                0
            );
        }
        assert!(
            !writable(at + 4 * 0x1000, 0x2000),
            "into the read-only page"
        );
        assert!(writable(at + 6 * 0x1000, 0x2000), "past it");
        assert!(!writable(at + 39 * 0x1000, 0x2000), "into the guard page");
        assert!(writable(at + 41 * 0x1000, 0x2000), "past it");
        release(heap);
        // Reserved, not committed.
        // SAFETY: a fresh reservation, or null.
        let reserved =
            unsafe { VirtualAlloc(std::ptr::null(), 0x10000, MEM_RESERVE, PAGE_READWRITE) };
        assert!(!reserved.is_null());
        assert!(!writable(reserved as u64, 0x40), "reserved only");
        release(reserved.cast());
        assert!(
            !writable(u64::MAX - 0x3f, 0x80),
            "past the end of the address space"
        );
    }

    #[test]
    fn a_loop_in_a_large_touched_heap_asks_no_region_query() {
        use windows_sys::Win32::System::Memory::{VirtualLock, VirtualUnlock};
        // A loop's few pages in a large allocation, touched and held in the
        // working set: their entries answer, and VirtualQuery, which would
        // walk the whole allocation, is not called.
        let pages = 4096; // 16 MiB
        let heap = fresh_pages(pages);
        // SAFETY: our pages; the five the loop reaches are locked in.
        unsafe {
            heap.write_bytes(0, pages * 0x1000);
            assert_ne!(VirtualLock(heap.add(0x80_0000).cast(), 5 * 0x1000), 0);
        }
        let before = region_queries();
        assert!(writable(heap as u64 + 0x80_0040, 0x3980));
        assert_eq!(
            region_queries(),
            before,
            "no VirtualQuery for resident pages"
        );
        // Untouched pages are asked of VirtualQuery once per run of pages
        // alike, not per page or per line.
        let fresh = fresh_pages(64);
        let before = region_queries();
        assert!(writable(fresh as u64, 64 * 0x1000));
        assert_eq!(region_queries(), before + 1, "one query for the run");
        // SAFETY: unlocks what was locked above.
        unsafe { VirtualUnlock(heap.add(0x80_0000).cast(), 5 * 0x1000) };
        release(heap);
        release(fresh);
    }

    #[test]
    fn working_set_flags_say_whether_a_resident_page_is_writable() {
        use windows_sys::Win32::System::Memory::{
            PAGE_EXECUTE_READWRITE, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
            PAGE_WRITECOPY,
        };
        let resident = |protection: u32| (protection as usize) << 4 | 1 | 0b1110;
        assert_eq!(resident_writable(resident(PAGE_READWRITE)), Some(true));
        assert_eq!(resident_writable(resident(PAGE_WRITECOPY)), Some(true));
        assert_eq!(
            resident_writable(resident(PAGE_EXECUTE_READWRITE)),
            Some(true)
        );
        assert_eq!(resident_writable(resident(PAGE_READONLY)), Some(false));
        assert_eq!(resident_writable(resident(PAGE_NOACCESS)), Some(false));
        assert_eq!(
            resident_writable(resident(PAGE_READWRITE | PAGE_GUARD)),
            Some(false)
        );
        // Not in the working set: its entry has no protection.
        assert_eq!(resident_writable((PAGE_READWRITE as usize) << 4), None);
        // Bits past Win32Protection (Shared, Node, ...) do not count.
        assert_eq!(
            resident_writable(resident(PAGE_READWRITE) | 1 << 15 | 0x3f << 16),
            Some(true)
        );
    }

    /// On a CPU that has them they run natively, and the tests above test
    /// the emulation; on one that lacks them, the vectored handler runs them.
    #[test]
    fn rdpru_rdpid_and_clzero_loops_run_on_any_cpu() {
        report_crashes();
        let (low, high): (u32, u32);
        // SAFETY: rdpru (APERF) reads a counter into eax and edx.
        unsafe {
            std::arch::asm!(".byte 0x0f, 0x01, 0xfd",
                in("ecx") 1, out("eax") low, out("edx") high)
        };
        assert_ne!(u64::from(high) << 32 | u64::from(low), 0, "APERF counts");
        let id: u64;
        // SAFETY: rdpid rax writes rax only.
        unsafe { std::arch::asm!(".byte 0xf3, 0x0f, 0xc7, 0xf8", out("rax") id) };
        assert_eq!(id >> 32, 0, "{id:#x}");
        #[repr(align(64))]
        struct Lines([u8; 4 * 64]);
        let mut lines = Lines([0xff; 4 * 64]);
        let start = lines.0.as_mut_ptr() as u64;
        let end: u64;
        // SAFETY: clzero; add rax, 0x40; cmp rax, rcx; jb top: zeroes the
        // three lines from `start`, ours.
        unsafe {
            std::arch::asm!(
                ".byte 0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x39, 0xc8, 0x72, 0xf4",
                inout("rax") start => end, in("rcx") start + 0xc0)
        };
        assert_eq!(end, start + 0xc0);
        assert!(lines.0[..0xc0].iter().all(|&b| b == 0));
        assert!(lines.0[0xc0..].iter().all(|&b| b == 0xff));
    }

    /// The title's handlers (registered after ours) still get what we do
    /// not emulate.
    #[test]
    fn an_unknown_instruction_reaches_the_handlers_after_ours() {
        use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
        use windows_sys::Win32::System::Diagnostics::Debug::RemoveVectoredExceptionHandler;
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree,
        };
        static PAGE: AtomicUsize = AtomicUsize::new(0);
        static SEEN: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn title(info: *mut EXCEPTION_POINTERS) -> i32 {
            // SAFETY: the system passes valid exception pointers.
            let (record, context) =
                unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
            if record.ExceptionAddress as usize != PAGE.load(Ordering::SeqCst)
                || record.ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION
            {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            SEEN.fetch_add(1, Ordering::SeqCst);
            context.Rip += 2; // past ud2
            -1 // EXCEPTION_CONTINUE_EXECUTION
        }
        report_crashes();
        // SAFETY: a fresh page of ours holding `ud2; ret`, run once with
        // the title's handler registered after ours.
        unsafe {
            let page = VirtualAlloc(
                std::ptr::null(),
                0x1000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
            .cast::<u8>();
            assert!(!page.is_null());
            page.copy_from_nonoverlapping([0x0f, 0x0b, 0xc3].as_ptr(), 3);
            PAGE.store(page as usize, Ordering::SeqCst);
            let handler = AddVectoredExceptionHandler(0, Some(title));
            assert!(!handler.is_null());
            let f: extern "system" fn() = std::mem::transmute(page);
            f();
            RemoveVectoredExceptionHandler(handler);
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
        assert_eq!(SEEN.load(Ordering::SeqCst), 1);
    }
    static SWALLOW_THREAD: AtomicU32 = AtomicU32::new(0);
    static SWALLOW_CODE: TestAtomicUsize = TestAtomicUsize::new(0);
    static SWALLOW_SEEN: AtomicUsize = AtomicUsize::new(0);
    static SWALLOW_COUNT: AtomicUsize = AtomicUsize::new(0);
    static SWALLOW_PARAMS: [AtomicUsize; 15] = [const { AtomicUsize::new(0) }; 15];

    fn child_ran(out: &std::process::Output, name: &str) -> bool {
        let stdout = String::from_utf8_lossy(&out.stdout);
        out.status.success()
            && stdout
                .lines()
                .any(|line| line == format!("test crash::tests::{name} ... ok"))
            && stdout.contains("test result: ok. 1 passed; 0 failed;")
    }

    fn isolated(name: &str) -> bool {
        if std::env::var("XTP_EXCEPTION_CHILD").as_deref() == Ok(name) {
            return false;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("crash::tests::{name}"), "--nocapture"])
            .env("XTP_EXCEPTION_CHILD", name)
            .output()
            .unwrap();
        assert!(
            child_ran(&out, name),
            "{name}: status {:?}\n{}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if name.starts_with("unreadable_what") {
            assert!(String::from_utf8_lossy(&out.stderr).contains("<unreadable what()>"));
        }
        true
    }

    fn lines() -> Vec<String> {
        FIRST_CHANCE_LINES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    unsafe extern "system" fn swallow_test_exception(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: Windows passes a live exception record; the thread-ID query
        // has no preconditions.
        let (record, current) = unsafe { (&*(*info).ExceptionRecord, GetCurrentThreadId()) };
        let mine = current == SWALLOW_THREAD.load(Ordering::Relaxed);
        let count = SWALLOW_COUNT.load(Ordering::Relaxed);
        let expected = record.NumberParameters as usize == count
            && count <= SWALLOW_PARAMS.len()
            && record.ExceptionInformation[..count]
                .iter()
                .zip(&SWALLOW_PARAMS)
                .all(|(&p, expected)| p == expected.load(Ordering::Relaxed));
        if mine && expected && record.ExceptionCode as usize == SWALLOW_CODE.load(Ordering::Relaxed)
        {
            SWALLOW_SEEN.fetch_add(1, Ordering::Relaxed);
            -1
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }

    /// Raises through Windows and handles only this test's exception after
    /// the production first-chance observer has declined to handle it.
    fn raise(code: i32, p: &[usize]) {
        raise_from(None, code, p);
    }

    fn raise_from(image: Option<&SyntheticThrow>, code: i32, p: &[usize]) {
        report_crashes();
        // SAFETY: a plain query of the calling thread.
        SWALLOW_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
        SWALLOW_CODE.store(code as usize, Ordering::Relaxed);
        assert!(p.len() <= SWALLOW_PARAMS.len());
        SWALLOW_COUNT.store(p.len(), Ordering::Relaxed);
        for (expected, &value) in SWALLOW_PARAMS.iter().zip(p) {
            expected.store(value, Ordering::Relaxed);
        }
        // SAFETY: installs a handler for the continuable exception below.
        let swallow = unsafe { AddVectoredExceptionHandler(0, Some(swallow_test_exception)) };
        assert!(!swallow.is_null());
        let before = SWALLOW_SEEN.load(Ordering::Relaxed);
        // SAFETY: `p` remains live throughout dispatch; the test handler
        // consumes this continuable exception after observation.
        unsafe {
            if let Some(image) = image {
                let call: unsafe extern "system" fn(u32, u32, u32, *const usize) =
                    std::mem::transmute(image.base + SyntheticThrow::CODE);
                call(code as u32, 0, p.len() as u32, p.as_ptr());
            } else {
                RaiseException(code as u32, 0, p.len() as u32, p.as_ptr());
            }
        };
        // SAFETY: `swallow` is the registration returned above.
        unsafe { RemoveVectoredExceptionHandler(swallow) };
        SWALLOW_THREAD.store(0, Ordering::Relaxed);
        assert_eq!(
            SWALLOW_SEEN.load(Ordering::Relaxed),
            before + 1,
            "observer must continue to the next VEH"
        );
    }

    fn forwarding_code(target: usize) -> Vec<u8> {
        let mut code = vec![0x48, 0x83, 0xec, 0x28, 0x48, 0xb8]; // sub rsp,40; mov rax,imm64
        code.extend_from_slice(&target.to_le_bytes());
        code.extend_from_slice(&[0xff, 0xd0, 0x48, 0x83, 0xc4, 0x28, 0xc3]);
        code
    }

    /// Our PE mapped by Windows directly, deliberately absent from the title registry.
    struct HostCaller {
        module: HMODULE,
        _dir: tempfile::TempDir,
    }
    impl HostCaller {
        fn new(name: &str) -> Self {
            use std::os::windows::ffi::OsStrExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(name);
            let prepared = crate::prepare(&SyntheticThrow::fixture()).unwrap();
            std::fs::write(&path, prepared.bytes).unwrap();
            let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: the synthetic DLL has no entry point, callbacks or imports.
            let module = unsafe {
                windows_sys::Win32::System::LibraryLoader::LoadLibraryExW(
                    wide.as_ptr(),
                    null_mut(),
                    0,
                )
            };
            assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
            let code = forwarding_code(RaiseException as *const () as usize);
            // SAFETY: this fixture's .test section is writable, and owns the code range.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    code.as_ptr(),
                    (module as usize + SyntheticThrow::CODE) as *mut u8,
                    code.len(),
                )
            };
            Self { module, _dir: dir }
        }
        fn code(&self) -> usize {
            self.module as usize + SyntheticThrow::CODE
        }
    }
    impl Drop for HostCaller {
        fn drop(&mut self) {
            // SAFETY: owns the load reference; no calls remain in flight.
            unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.module) };
        }
    }

    struct SyntheticThrow {
        image: crate::LoadedImage,
        _dir: tempfile::TempDir,
        base: usize,
        size: usize,
    }

    impl SyntheticThrow {
        const INFO: usize = 0x1200;
        const TYPES: usize = 0x1300;
        const CATCHABLE: usize = 0x1400;
        const DESCRIPTOR: usize = 0x1600;
        const CODE: usize = 0x2200;
        const BACK: usize = Self::CODE + 16;

        fn fixture() -> Vec<u8> {
            // Original minimal PE32+ DLL: one writable/executable section,
            // no imports, and unwind data for a tiny RaiseException caller.
            let mut pe = vec![0u8; 0x2200];
            let put = |pe: &mut [u8], at: usize, bytes: &[u8]| {
                pe[at..at + bytes.len()].copy_from_slice(bytes)
            };
            put(&mut pe, 0, b"MZ");
            put(&mut pe, 0x3c, &0x80u32.to_le_bytes());
            put(&mut pe, 0x80, b"PE\0\0");
            put(&mut pe, 0x84, &0x8664u16.to_le_bytes());
            put(&mut pe, 0x86, &1u16.to_le_bytes());
            put(&mut pe, 0x94, &240u16.to_le_bytes());
            put(&mut pe, 0x96, &0x2022u16.to_le_bytes());
            let o = 0x98;
            for (at, value) in [
                (0, 0x20bu32),
                (24, 0x80000000),
                (28, 1),
                (32, 0x1000),
                (36, 0x200),
                (40, 6),
                (48, 6),
                (56, 0x3000),
                (60, 0x200),
                (68, 3),
                (108, 16),
                (112 + 24, 0x2420),
                (116 + 24, 12),
            ] {
                put(&mut pe, o + at, &value.to_le_bytes());
            }
            let section = o + 240;
            put(&mut pe, section, b".test");
            for (at, value) in [
                (8, 0x2000u32),
                (12, 0x1000),
                (16, 0x2000),
                (20, 0x200),
                (36, 0xe0000060),
            ] {
                put(&mut pe, section + at, &value.to_le_bytes());
            }
            // UWOP_ALLOC_SMALL(40), prologue length 4, one unwind slot.
            put(&mut pe, 0x1600, &[1, 4, 1, 0, 4, 0x42, 0, 0]); // RVA 0x2400
            for (i, v) in [Self::CODE as u32, Self::CODE as u32 + 21, 0x2400]
                .iter()
                .enumerate()
            {
                put(&mut pe, 0x1620 + i * 4, &v.to_le_bytes());
            }
            pe
        }

        fn new(names: &[&[u8]]) -> Self {
            let prepared = crate::prepare(&Self::fixture()).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let image = crate::load(&prepared, dir.path()).unwrap();
            let this = Self {
                base: image.base(),
                size: image.info().size_of_image as usize,
                image,
                _dir: dir,
            };
            this.put(Self::INFO + 12, Self::TYPES as u32);
            this.put(Self::TYPES, names.len() as u32);
            for (i, name) in names.iter().enumerate() {
                let catchable = Self::CATCHABLE + i * 0x20;
                let descriptor = Self::DESCRIPTOR + i * 0x100;
                this.put(Self::TYPES + 4 + i * 4, catchable as u32);
                this.put(catchable + 4, descriptor as u32);
                this.put(catchable + 12, u32::MAX); // non-virtual base
                this.bytes(descriptor + 16, name);
                this.bytes(descriptor + 16 + name.len(), &[0]);
            }
            this.bytes(
                Self::CODE,
                &forwarding_code(RaiseException as *const () as usize),
            );
            assert_eq!(
                crate::threads::image_at(this.base + Self::BACK),
                Some((this.base, this.size))
            );
            this
        }

        fn put<T: Copy>(&self, offset: usize, value: T) {
            assert!(
                offset
                    .checked_add(size_of::<T>())
                    .is_some_and(|end| end <= self.size)
            );
            // SAFETY: all callers use offsets and sizes inside our image.
            unsafe { ((self.base + offset) as *mut T).write_unaligned(value) };
        }

        fn bytes(&self, offset: usize, bytes: &[u8]) {
            assert!(
                offset
                    .checked_add(bytes.len())
                    .is_some_and(|end| end <= self.size)
            );
            // SAFETY: all callers keep the copy inside our allocated image.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    (self.base + offset) as *mut u8,
                    bytes.len(),
                )
            };
        }

        fn raise(&self, object: usize) {
            raise_from(Some(self), CPP_EXCEPTION, &self.params(object));
        }

        fn params(&self, object: usize) -> [usize; 4] {
            [CPP_MAGICS[0], object, self.base + Self::INFO, self.base]
        }
    }

    impl Drop for SyntheticThrow {
        fn drop(&mut self) {
            // SAFETY: no calls are in flight and the synthetic image was never attached.
            unsafe { self.image.clone().unload() };
        }
    }

    fn description(thread: HANDLE) -> String {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::System::Threading::GetThreadDescription;
        let mut name = std::ptr::null_mut();
        // SAFETY: asks Windows to allocate this thread's description.
        assert!(unsafe { GetThreadDescription(thread, &mut name) } >= 0);
        assert!(!name.is_null());
        // SAFETY: Windows returns a NUL-terminated string owned by LocalAlloc.
        let text = unsafe {
            let len = (0..1024).take_while(|&i| *name.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(name, len))
        };
        // SAFETY: frees the string returned by GetThreadDescription.
        unsafe { LocalFree(name.cast()) };
        text
    }

    #[test]
    fn debugger_exception_names_the_host_thread() {
        if isolated("debugger_exception_names_the_host_thread") {
            return;
        }
        // SAFETY: a current-thread pseudo handle needs no open or close.
        let current = unsafe { GetCurrentThread() };
        let name = b"XTP acceptance thread\0";
        #[repr(C)]
        struct ThreadName {
            kind: u32,
            padding: u32,
            name: *const u8,
            id: u32,
            flags: u32,
        }
        let params = |id| {
            let info = ThreadName {
                kind: 0x1000,
                padding: 0,
                name: name.as_ptr(),
                id,
                flags: 0,
            };
            assert_eq!(size_of::<ThreadName>(), 3 * size_of::<usize>());
            assert_eq!(std::mem::offset_of!(ThreadName, id), 16);
            // SAFETY: every byte is initialized, including explicit padding.
            unsafe {
                std::ptr::from_ref(&info)
                    .cast::<[usize; 3]>()
                    .read_unaligned()
            }
        };
        let p = params(u32::MAX);
        raise(THREAD_NAME_EXCEPTION, &p);
        assert_eq!(description(current), "XTP acceptance thread");
        let (send, recv) = std::sync::mpsc::channel();
        let (go, wait) = std::sync::mpsc::channel();
        let other = std::thread::spawn(move || {
            // SAFETY: both APIs query the current thread.
            send.send(unsafe { GetCurrentThreadId() }).unwrap();
            wait.recv().unwrap();
            // SAFETY: a current-thread pseudo handle is always valid.
            description(unsafe { GetCurrentThread() })
        });
        raise(THREAD_NAME_EXCEPTION, &params(recv.recv().unwrap()));
        go.send(()).unwrap();
        assert_eq!(other.join().unwrap(), "XTP acceptance thread");
    }

    #[test]
    fn unreadable_what_is_logged_when_windows_raises_the_exception() {
        if isolated("unreadable_what_is_logged_when_windows_raises_the_exception") {
            return;
        }
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc, VirtualFree,
        };
        let image = SyntheticThrow::new(&[b".?AVexception@std@@"]);
        // SAFETY: allocates one deliberately inaccessible test page.
        let page = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_NOACCESS,
            )
        };
        assert!(!page.is_null());
        let object = [0, page as usize];
        image.raise(object.as_ptr() as usize);
        let logged = lines().join("\n");
        assert!(logged.contains(".?AVexception@std@@"), "{logged}");
        assert!(logged.contains("<unreadable what()>"), "{logged}");
        let expected = format!(" at {}", locate(image.base + SyntheticThrow::BACK));
        assert!(logged.contains(&expected), "{logged}");
        assert!(logged.contains("+0x2210"), "{logged}");
        // SAFETY: releases the page allocated above.
        unsafe { VirtualFree(page, 0, MEM_RELEASE) };
    }

    #[test]
    fn readable_messages_invalid_utf8_and_scalar_values_survive() {
        if isolated("readable_messages_invalid_utf8_and_scalar_values_survive") {
            return;
        }
        let image = SyntheticThrow::new(&[b".?AVderived@@", b".?AVexception@std@@"]);
        image.put(SyntheticThrow::CATCHABLE + 0x20 + 8, 16u32);
        let message = b"readable message\0";
        let object = [0, 1, 0, message.as_ptr() as usize];
        image.raise(object.as_ptr() as usize);
        assert!(lines()[0].contains(": readable message"), "{:?}", lines());
        let invalid = [0xff, 0];
        let object = [0, 1, 0, invalid.as_ptr() as usize];
        image.raise(object.as_ptr() as usize);
        assert!(lines()[1].contains('\u{fffd}'), "{:?}", lines());
        image.put(SyntheticThrow::CATCHABLE + 0x20 + 12, 0u32);
        assert!(thrown(&image.params(0)).contains("unsupported std::exception base"));
        drop(image);
        let scalar = SyntheticThrow::new(&[b".J"]);
        let value = 0x8000_4001u32;
        scalar.raise((&raw const value) as usize);
        assert!(lines()[2].contains(".J = 0x80004001"), "{:?}", lines());
    }

    #[test]
    fn invalid_payloads_are_logged_but_bad_parameter_headers_do_not_consume_budget() {
        if isolated("invalid_payloads_are_logged_but_bad_parameter_headers_do_not_consume_budget") {
            return;
        }
        let image = SyntheticThrow::new(&[b".?AVexception@std@@"]);
        let null_what = [0usize, 0];
        image.raise(null_what.as_ptr() as usize);
        image.raise(0);
        let mut bad_info = image.params(0);
        bad_info[1] = 1; // object non-null: not a rethrow
        bad_info[2] = 0;
        raise_from(Some(&image), CPP_EXCEPTION, &bad_info);
        let logged = lines();
        assert!(logged[0].contains("<null what()>"), "{logged:?}");
        assert!(
            logged[1].contains("<unreadable exception object>"),
            "{logged:?}"
        );
        assert!(logged[2].contains("<invalid ThrowInfo"), "{logged:?}");
        let used = FIRST_CHANCE.load(Ordering::Relaxed);
        let p = image.params(0);
        raise_from(Some(&image), CPP_EXCEPTION, &p[..3]);
        let mut wrong_magic = p;
        wrong_magic[0] = 0x1993_0000;
        raise_from(Some(&image), CPP_EXCEPTION, &wrong_magic);
        assert_eq!(FIRST_CHANCE.load(Ordering::Relaxed), used);
        assert_eq!(lines().len(), 3);
    }

    #[test]
    fn invalid_rvas_and_excessive_type_counts_have_diagnostics() {
        if isolated("invalid_rvas_and_excessive_type_counts_have_diagnostics") {
            return;
        }
        let image = SyntheticThrow::new(&[b".?AVexception@std@@"]);
        image.put(SyntheticThrow::INFO + 12, 0x2fffu32);
        image.raise(0);
        image.put(SyntheticThrow::INFO + 12, SyntheticThrow::TYPES as u32);
        image.put(SyntheticThrow::TYPES, (MAX_CATCHABLE_TYPES + 1) as u32);
        image.raise(0);
        let logged = lines();
        assert!(logged[0].contains("catchable-type array RVA"), "{logged:?}");
        assert!(
            logged[1].contains("invalid catchable-type count 33"),
            "{logged:?}"
        );
        assert_eq!(image_address(usize::MAX - 3, 8, 2, 4), None);
        assert_eq!(image_address(1, usize::MAX, usize::MAX, 8), None);
        image.put(SyntheticThrow::TYPES, 1u32);
        image.put(SyntheticThrow::CATCHABLE + 4, 0x2fefu32);
        image.bytes(0x2fff, b"x");
        assert!(thrown(&image.params(0)).contains("unterminated C++ type name"));
        // Range lookup is half-open; contention is tested separately.
        assert!(crate::threads::image_at(image.base - 1).is_none());
        assert!(crate::threads::image_at(image.base + image.size).is_none());
    }

    #[test]
    fn cross_page_and_unterminated_strings_are_bounded() {
        if isolated("cross_page_and_unterminated_strings_are_bounded") {
            return;
        }
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualProtect,
        };
        let image = SyntheticThrow::new(&[b".?AVexception@std@@"]);
        // SAFETY: owns two pages; only the second is made inaccessible.
        unsafe {
            let pages = VirtualAlloc(
                std::ptr::null(),
                0x2000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!pages.is_null());
            pages
                .add(0xff8)
                .copy_from_nonoverlapping(c"crossing-page".as_ptr().cast(), 14);
            assert_eq!(
                bounded_cstr(pages.add(0xff8) as usize, MAX_STRING).unwrap(),
                "crossing-page"
            );
            let mut old = 0;
            assert_ne!(
                VirtualProtect(pages.add(0x1000).cast(), 0x1000, PAGE_NOACCESS, &mut old),
                0
            );
            pages
                .add(0xff8)
                .copy_from_nonoverlapping(b"abcdefgh".as_ptr(), 8);
            let object = [0, pages.add(0xff8) as usize];
            image.raise(object.as_ptr() as usize);
            assert!(
                lines()[0].contains(r#"unreadable what() after "abcdefgh""#),
                "{:?}",
                lines()
            );
            pages.write_bytes(b'x', 0x1000);
            let error = bounded_cstr(pages as usize, 32).unwrap_err();
            assert_eq!(error, ("x".repeat(32), "unterminated"));
            VirtualFree(pages.cast(), 0, MEM_RELEASE);
        }
    }

    #[test]
    fn memory_reads_reject_guard_pages_and_overflow() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_GUARD,
            PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualQuery,
        };
        assert_eq!(read_u32(usize::MAX), None);
        assert_eq!(read_usize(1), None);
        // SAFETY: fresh guarded allocation; query its protection without touching it.
        unsafe {
            let page = VirtualAlloc(
                std::ptr::null(),
                0x1000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE | PAGE_GUARD,
            );
            assert!(!page.is_null());
            assert_eq!(read_u32(page as usize), None);
            assert_eq!(read_u32(page as usize), None);
            let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            assert_ne!(VirtualQuery(page, &mut info, size_of_val(&info)), 0);
            assert_ne!(
                info.Protect & PAGE_GUARD,
                0,
                "observer must preserve the guard"
            );
            VirtualFree(page, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn invalid_thread_name_parameters_are_ignored() {
        let name = b"ignored\0";
        for params in [
            vec![],
            vec![0, name.as_ptr() as usize, u32::MAX as usize],
            vec![0x1000, name.as_ptr() as usize, 1usize << 32],
            vec![0x1000, name.as_ptr() as usize, 0],
            vec![0x1000, 0, u32::MAX as usize],
        ] {
            assert!(!name_thread(&params));
        }
    }

    #[test]
    fn first_chance_budget_is_exact_and_concurrent() {
        if isolated("first_chance_budget_is_exact_and_concurrent") {
            return;
        }
        let image = SyntheticThrow::new(&[b".?AVexception@std@@"]);
        let message = b"budget\0";
        let object = [0, message.as_ptr() as usize];
        for _ in 0..18 {
            image.raise(object.as_ptr() as usize);
        }
        assert_eq!(lines().len(), FIRST_CHANCE_LIMIT);
        FIRST_CHANCE.store(0, Ordering::Relaxed);
        FIRST_CHANCE_LINES.lock().unwrap().clear();
        let params = image.params(object.as_ptr() as usize);
        let site = image.base + SyntheticThrow::BACK;
        let workers: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(move || {
                    // SAFETY: valid empty record; the fixture and object outlive all workers.
                    let mut record: EXCEPTION_RECORD = unsafe { std::mem::zeroed() };
                    record.ExceptionCode = CPP_EXCEPTION;
                    record.ExceptionAddress = site as *mut _;
                    record.NumberParameters = 4;
                    record.ExceptionInformation[..4].copy_from_slice(&params);
                    for _ in 0..32 {
                        observe_cpp(&record, &context());
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(lines().len(), FIRST_CHANCE_LIMIT);
        assert_eq!(FIRST_CHANCE.load(Ordering::Relaxed), FIRST_CHANCE_LIMIT);
    }

    #[test]
    fn unrelated_and_unhandled_exceptions_keep_their_existing_behaviour() {
        if isolated("unrelated_and_unhandled_exceptions_keep_their_existing_behaviour") {
            return;
        }
        let image = SyntheticThrow::new(&[b".?AVexception@std@@"]);
        let message = b"still reported\0";
        let object = [0, message.as_ptr() as usize];
        let mut unrelated = image.params(object.as_ptr() as usize);
        unrelated[3] = 1;
        raise(CPP_EXCEPTION, &unrelated);
        raise(0xe0420001u32 as i32, &[]);
        // A registered metadata base also cannot turn a host throw into a title throw.
        raise(CPP_EXCEPTION, &image.params(object.as_ptr() as usize));
        assert!(lines().is_empty());
        assert_eq!(FIRST_CHANCE.load(Ordering::Relaxed), 0);
        FIRST_CHANCE.store(FIRST_CHANCE_LIMIT, Ordering::Relaxed);
        // SAFETY: zero is a valid initial record; only its fields are set.
        let mut record: EXCEPTION_RECORD = unsafe { std::mem::zeroed() };
        record.ExceptionCode = CPP_EXCEPTION;
        record.NumberParameters = 4;
        record.ExceptionInformation[..4].copy_from_slice(&image.params(object.as_ptr() as usize));
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("crashes.log");
        log_crashes_to(log.clone());
        let mut context = context();
        let mut pointers = EXCEPTION_POINTERS {
            ExceptionRecord: &mut record,
            ContextRecord: &mut context,
        };
        // SAFETY: live synthetic record/context; the exhausted observer must continue dispatch.
        assert_eq!(unsafe { handler(&mut pointers) }, EXCEPTION_CONTINUE_SEARCH);
        // SAFETY: both records remain live while the unhandled report reads them.
        unsafe { report_unhandled(&pointers) };
        assert!(
            std::fs::read_to_string(log)
                .unwrap()
                .contains("still reported")
        );
    }
    #[test]
    fn review_rethrows_use_throw_site_and_event_budget() {
        if isolated("review_rethrows_use_throw_site_and_event_budget") {
            return;
        }
        let image = SyntheticThrow::new(&[]);
        let rethrow = [CPP_MAGICS[0], 0, 0, 0];
        raise(CPP_EXCEPTION, &rethrow); // host caller
        assert_eq!(FIRST_CHANCE.load(Ordering::Relaxed), 0);
        raise_from(Some(&image), CPP_EXCEPTION, &rethrow);
        let expected = format!(
            "XTP: first-chance C++ <rethrow> at {}",
            locate(image.base + SyntheticThrow::BACK)
        );
        assert_eq!(lines().as_slice(), std::slice::from_ref(&expected));
        raise_from(Some(&image), CPP_EXCEPTION, &rethrow);
        assert_eq!(
            FIRST_CHANCE.load(Ordering::Relaxed),
            2,
            "rethrows are separate events"
        );
        for bad in [[0, 0, 0, 0], [CPP_MAGICS[0], 0, 0, 0]] {
            let params = if bad[0] == 0 { &bad[..] } else { &bad[..3] };
            raise_from(Some(&image), CPP_EXCEPTION, params);
            assert!(thrown(params).contains("invalid MSVC exception parameters"));
        }
        assert_eq!(lines(), [expected.clone(), expected]);
        assert!(!thrown(&[CPP_MAGICS[0], 1, 0, 0]).contains("rethrow"));
        assert!(!thrown(&[CPP_MAGICS[0], 0, 1, 0]).contains("rethrow"));
        assert_eq!(FIRST_CHANCE.load(Ordering::Relaxed), 2);
    }

    unsafe extern "system" fn hle_raise(code: u32, flags: u32, count: u32, params: *const usize) {
        // SAFETY: forwards the live parameters supplied by the fixture caller.
        unsafe { RaiseException(code, flags, count, params) };
    }
    unsafe extern "system" fn hle_panics(_: u32, _: u32, _: u32, _: *const usize) {
        assert!(std::panic::catch_unwind(|| panic!("synthetic HLE panic")).is_err());
    }

    #[test]
    fn review_only_throw_machinery_may_precede_a_title_frame() {
        if isolated("review_only_throw_machinery_may_precede_a_title_frame") {
            return;
        }
        let title = SyntheticThrow::new(&[b".H"]);
        let value = 7u32;
        title.raise((&raw const value) as usize); // real KernelBase -> title
        let expected = format!(" at {}", locate(title.base + SyntheticThrow::BACK));
        assert!(lines()[0].ends_with(&expected));
        for name in ["msvcp140_xtp_review.dll", "VCRUNTIME140_123_XTP.dll"] {
            let runtime = HostCaller::new(name);
            assert!(throw_machinery(runtime.code()));
            title.bytes(SyntheticThrow::CODE, &forwarding_code(runtime.code()));
            title.raise((&raw const value) as usize);
            assert!(lines().last().unwrap().ends_with(&expected));
        }
        assert_eq!(
            lines().len(),
            3,
            "both runtime DLL callers must emit a new diagnostic"
        );
        let used = FIRST_CHANCE.load(Ordering::Relaxed);
        assert_eq!(used, 3);
        let host = HostCaller::new("xtp-unrelated-host.dll");
        assert!(!throw_machinery(host.code()));
        title.bytes(SyntheticThrow::CODE, &forwarding_code(host.code()));
        title.raise((&raw const value) as usize);
        title.bytes(
            SyntheticThrow::CODE,
            &forwarding_code(hle_raise as *const () as usize),
        );
        title.raise((&raw const value) as usize);
        title.bytes(
            SyntheticThrow::CODE,
            &forwarding_code(hle_panics as *const () as usize),
        );
        // SAFETY: our forwarding stub calls hle_panics, which catches its own panic.
        // No swallowing VEH: Rust's real catch_unwind must receive this exception.
        unsafe {
            let call: unsafe extern "system" fn(u32, u32, u32, *const usize) =
                std::mem::transmute(title.base + SyntheticThrow::CODE);
            call(0, 0, 0, std::ptr::null());
        }
        assert_eq!(FIRST_CHANCE.load(Ordering::Relaxed), used);
        use windows_sys::Win32::System::Diagnostics::Debug::{
            IMAGE_RUNTIME_FUNCTION_ENTRY, IMAGE_RUNTIME_FUNCTION_ENTRY_0, RtlAddFunctionTable,
            RtlDeleteFunctionTable,
        };
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree,
        };
        // SAFETY: fresh executable fixture and matching unwind table, removed before freeing it.
        unsafe {
            let page = VirtualAlloc(
                std::ptr::null(),
                0x1000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
            .cast::<u8>();
            assert!(!page.is_null());
            let code = forwarding_code(RaiseException as *const () as usize);
            page.copy_from_nonoverlapping(code.as_ptr(), code.len());
            page.add(0x100)
                .copy_from_nonoverlapping([1, 4, 1, 0, 4, 0x42, 0, 0].as_ptr(), 8);
            let entry = IMAGE_RUNTIME_FUNCTION_ENTRY {
                BeginAddress: 0,
                EndAddress: 21,
                Anonymous: IMAGE_RUNTIME_FUNCTION_ENTRY_0 { UnwindData: 0x100 },
            };
            assert!(RtlAddFunctionTable(&entry, 1, page as u64));
            assert!(module_of(page as usize).is_none());
            title.bytes(SyntheticThrow::CODE, &forwarding_code(page as usize));
            title.raise((&raw const value) as usize);
            assert_eq!(FIRST_CHANCE.load(Ordering::Relaxed), used);
            assert!(RtlDeleteFunctionTable(&entry));
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
        // An unknown frame must stop before a later title PC, even if readable.
        assert_eq!(title_frame(&[1, title.base + SyntheticThrow::BACK]), None);
        assert_eq!(
            title_frame(&[host.code(), title.base + SyntheticThrow::BACK]),
            None
        );
        assert_eq!(
            title_frame(&[title.base + SyntheticThrow::BACK]),
            Some(title.base + SyntheticThrow::BACK)
        );
        for name in [
            "vcruntime140.dll",
            "VCRUNTIME140_1D.DLL",
            "msvcp140_atomic_wait.dll",
        ] {
            assert!(runtime_family(name));
        }
        for name in [
            "other-msvcp140.dll",
            "vcruntime140fake.dll",
            "msvcp140.exe",
            "ucrtbase-extra.dll",
            "msvcp140_.dll",
        ] {
            assert!(!runtime_family(name));
        }
    }

    #[test]
    fn review_invalid_child_selector_is_rejected() {
        let name = "this_child_test_does_not_exist";
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("crash::tests::{name}"), "--nocapture"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "libtest returns success for zero matches"
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("0 passed"));
        assert!(!child_ran(&out, name));
    }

    #[test]
    fn review_legacy_names_accept_flags_extra_words_and_truncation() {
        if isolated("review_legacy_names_accept_flags_extra_words_and_truncation") {
            return;
        }
        // SAFETY: current-thread pseudo handle requires no ownership.
        let current = unsafe { GetCurrentThread() };
        let name = c"legacy name";
        let p = [
            0x1000,
            name.as_ptr() as usize,
            u32::MAX as usize | (42usize << 32),
            0,
            0,
            0,
        ];
        raise(THREAD_NAME_EXCEPTION, &p); // sizeof(THREADNAME_INFO)/sizeof(DWORD) = 6
        assert_eq!(description(current), "legacy name");
        let long = vec![b'x'; MAX_STRING + 64];
        raise(
            THREAD_NAME_EXCEPTION,
            &[0x1000, long.as_ptr() as usize, u32::MAX as usize],
        );
        assert_eq!(description(current), "x".repeat(MAX_STRING));
        assert!(!name_thread(&[0x1000, 1, u32::MAX as usize]));
    }

    #[test]
    fn review_foreign_thread_child() {
        if std::env::var_os("XTP_FOREIGN_THREAD_CHILD").is_none() {
            return;
        }
        use std::io::{BufRead, Write};
        // SAFETY: queries this child's calling thread.
        println!("XTP FOREIGN THREAD {}", unsafe { GetCurrentThreadId() });
        std::io::stdout().flush().unwrap();
        let mut input = String::new();
        std::io::stdin().lock().read_line(&mut input).unwrap();
        assert_eq!(input.trim(), "finish");
        // SAFETY: a live current-thread pseudo handle.
        let current = unsafe { GetCurrentThread() };
        assert_ne!(description(current), "foreign rename forbidden");
        println!("XTP FOREIGN NAME UNCHANGED");
    }

    #[test]
    fn review_explicit_thread_id_must_belong_to_this_process() {
        if isolated("review_explicit_thread_id_must_belong_to_this_process") {
            return;
        }
        use std::io::{BufRead, Write};
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash::tests::review_foreign_thread_child",
                "--nocapture",
            ])
            .env("XTP_FOREIGN_THREAD_CHILD", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let id = loop {
            let mut line = String::new();
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "foreign child ended before readiness"
            );
            if let Some((_, id)) = line.trim().split_once("XTP FOREIGN THREAD ") {
                break id.parse::<u32>().unwrap();
            }
        };
        // Prove Windows granted this foreign handle, rather than testing only OpenThread failure.
        // SAFETY: requesting limited access to the controlled child's live thread.
        let handle = unsafe {
            OpenThread(
                THREAD_SET_LIMITED_INFORMATION | THREAD_QUERY_LIMITED_INFORMATION,
                0,
                id,
            )
        };
        assert!(!handle.is_null());
        // SAFETY: live opened handle; this query precedes its close.
        assert_eq!(unsafe { GetProcessIdOfThread(handle) }, child.id());
        // SAFETY: owns this real handle.
        unsafe { CloseHandle(handle) };
        assert!(!name_thread(&[
            0x1000,
            c"foreign rename forbidden".as_ptr() as usize,
            id as usize
        ]));
        writeln!(child.stdin.take().unwrap(), "finish").unwrap();
        use std::io::Read;
        let mut rest = String::new();
        output.read_to_string(&mut rest).unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{rest}\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(rest.contains("XTP FOREIGN NAME UNCHANGED"), "{rest}");
    }

    #[test]
    fn review_exit_observers_child() {
        if std::env::var_os("XTP_OBSERVER_EXIT_CHILD").is_none() {
            return;
        }
        // Prepare everything before actual shutdown ends the harness's other threads.
        let image = SyntheticThrow::new(&[]);
        report_crashes();
        let mut record = EXCEPTION_RECORD {
            ExceptionCode: CPP_EXCEPTION,
            NumberParameters: 4,
            ExceptionAddress: (image.base + SyntheticThrow::BACK) as *mut _,
            ..Default::default()
        };
        record.ExceptionInformation[..4].copy_from_slice(&[CPP_MAGICS[0], 0, 0, 0]);
        let mut context = context();
        let mut info = EXCEPTION_POINTERS {
            ExceptionRecord: &mut record,
            ContextRecord: &mut context,
        };
        let name = [
            0x1000,
            c"must not name during exit".as_ptr() as usize,
            u32::MAX as usize,
        ];
        crate::threads::detach_all();
        assert!(crate::threads::exiting());
        observe_cpp(&record, &context);
        assert!(!name_thread(&name));
        assert_eq!(FIRST_CHANCE.load(Ordering::Relaxed), 0);
        // SAFETY: pointers are live; shutdown observers must continue dispatch.
        assert_eq!(unsafe { handler(&mut info) }, EXCEPTION_CONTINUE_SEARCH);
        println!("XTP EXIT OBSERVERS SKIPPED");
        std::process::exit(0);
    }

    #[test]
    fn review_exit_skips_both_observers() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash::tests::review_exit_observers_child",
                "--nocapture",
            ])
            .env("XTP_OBSERVER_EXIT_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success()
                && String::from_utf8_lossy(&out.stdout).contains("XTP EXIT OBSERVERS SKIPPED"),
            "status {:?}\n{}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
