//! Stubs for imports nobody implements yet.
//!
//! Each unresolved import gets 32 bytes of generated code:
//!
//! ```text
//! mov rcx, <&'static ImportSite>   ; 48 B9 imm64
//! mov rdx, [rsp]                   ; 48 8B 14 24: where the call returns to
//! mov rax, <unimplemented_import>  ; 48 B8 imm64
//! jmp rax                          ; FF E0
//! ```
//!
//! The jump keeps the title's return address on the stack, so the handler
//! returns straight to the caller. Bring-up then shows exactly which API to
//! implement next, and who called it, rather than failing at load time.
//! Every stub counts its calls and keeps its first caller ([`stub_calls`]).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anyhow::{Result, bail};
use windows_sys::Win32::System::Diagnostics::Debug::{
    DebugBreak, FlushInstructionCache, IsDebuggerPresent,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use xtp_pe::ImportSymbol;

use crate::StubDb;

/// What an unimplemented import does when called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnimplementedPolicy {
    /// Log, break into an attached debugger, then abort. The default:
    /// silently continuing usually crashes later and further from the cause.
    #[default]
    Trap,
    /// Log once per import and return 0. Useful to push through bring-up.
    ReturnZero,
    /// Log once per import and return the value the stub table
    /// ([`StubDb::builtin`], `data/stubs.toml`) gives it; trap, as `Trap`
    /// does, for an import the table does not list.
    Table,
}

impl UnimplementedPolicy {
    /// What the stub for `dll!symbol` does under this policy.
    pub(crate) fn stub_for(self, dll: &str, symbol: &ImportSymbol) -> Stub {
        match self {
            UnimplementedPolicy::Trap => Stub::Trap,
            UnimplementedPolicy::ReturnZero => Stub::Return(0),
            UnimplementedPolicy::Table => StubDb::builtin()
                .value(dll, symbol)
                .map_or(Stub::Trap, Stub::Return),
        }
    }
}

/// What one stub does, decided when its import is bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stub {
    Trap,
    /// Log the first call, then return this value.
    Return(u64),
}

/// Identifies one unresolved import. Leaked: lives as long as the process.
#[derive(Debug)]
pub(crate) struct ImportSite {
    pub(crate) dll: String,
    pub(crate) symbol: String,
    pub(crate) stub: Stub,
    pub(crate) calls: AtomicU64,
    /// Where the first call returns to; 0 until then.
    pub(crate) first_caller: AtomicUsize,
}

/// Every site with a stub, for [`stub_calls`].
static SITES: Mutex<Vec<&'static ImportSite>> = Mutex::new(Vec::new());

impl ImportSite {
    /// A site for `dll!symbol`, kept for the life of the process.
    pub(crate) fn leak(dll: &str, symbol: &str, stub: Stub) -> &'static ImportSite {
        let site: &'static ImportSite = Box::leak(Box::new(ImportSite {
            dll: dll.to_owned(),
            symbol: symbol.to_owned(),
            stub,
            calls: AtomicU64::new(0),
            first_caller: AtomicUsize::new(0),
        }));
        SITES.lock().unwrap_or_else(|e| e.into_inner()).push(site);
        site
    }
}

/// Calls that reached the stub of one unimplemented import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubCalls {
    pub dll: String,
    pub symbol: String,
    pub calls: u64,
    /// Where the first call returns to, in the caller ([`locate`](crate::locate)
    /// names it).
    pub first_caller: usize,
}

/// Every stub called so far, with its number of calls and first caller:
/// what the title asked for that nobody implements.
pub fn stub_calls() -> Vec<StubCalls> {
    let sites = SITES.lock().unwrap_or_else(|e| e.into_inner());
    sites
        .iter()
        .filter(|s| s.calls.load(Ordering::Relaxed) > 0)
        .map(|s| StubCalls {
            dll: s.dll.clone(),
            symbol: s.symbol.clone(),
            calls: s.calls.load(Ordering::Relaxed),
            first_caller: s.first_caller.load(Ordering::Relaxed),
        })
        .collect()
}

static UNIMPLEMENTED_CALLS: AtomicU64 = AtomicU64::new(0);

/// Total number of calls that reached an unimplemented-import stub.
pub fn unimplemented_call_count() -> u64 {
    UNIMPLEMENTED_CALLS.load(Ordering::Relaxed)
}

/// Entered from a stub (see the module comment), with the caller's return
/// address as its second argument.
extern "system" fn unimplemented_import(site: &'static ImportSite, caller: usize) -> u64 {
    UNIMPLEMENTED_CALLS.fetch_add(1, Ordering::Relaxed);
    let previous = site.calls.fetch_add(1, Ordering::Relaxed);
    let _ = site
        .first_caller
        .compare_exchange(0, caller, Ordering::Relaxed, Ordering::Relaxed);
    let (dll, symbol) = (&site.dll, &site.symbol);
    match site.stub {
        Stub::Return(value) => {
            if previous == 0 {
                tracing::warn!(
                    %dll,
                    %symbol,
                    caller = %crate::locate(caller),
                    value = format_args!("{value:#x}"),
                    "unimplemented import called, returning a stub value"
                );
            }
            value
        }
        Stub::Trap => {
            let caller = crate::locate(caller);
            tracing::error!(%dll, %symbol, %caller, "unimplemented import called");
            // Into the crash log too: `abort` ends the process without
            // running the handlers that write it.
            let text = format!("XTP: unimplemented import {dll}!{symbol} called from {caller}");
            crate::log_crash(&text);
            // SAFETY: plain Win32 calls without preconditions.
            unsafe {
                if IsDebuggerPresent() != 0 {
                    DebugBreak();
                }
            }
            std::process::abort();
        }
    }
}

const THUNK_SIZE: usize = 32;

/// Executable memory holding one stub per unresolved import.
pub(crate) struct ThunkBlock {
    base: *mut u8,
    len: usize,
    used: usize,
}

impl ThunkBlock {
    /// Reserves room for `count` stubs (writable until [`Self::seal`]).
    pub(crate) fn new(count: usize) -> Result<Self> {
        let len = (count.max(1) * THUNK_SIZE).next_multiple_of(4096);
        // SAFETY: fresh anonymous allocation; no existing memory is touched.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as *mut u8;
        if base.is_null() {
            bail!(
                "VirtualAlloc for import stubs failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(ThunkBlock { base, len, used: 0 })
    }

    /// Emits a stub for `site` and returns its address.
    pub(crate) fn emit(&mut self, site: &'static ImportSite) -> Result<usize> {
        if self.used + THUNK_SIZE > self.len {
            bail!("import stub block is full");
        }
        let mut code = [0xCC_u8; THUNK_SIZE]; // int3 padding
        code[0..2].copy_from_slice(&[0x48, 0xB9]);
        code[2..10].copy_from_slice(&(site as *const ImportSite as u64).to_le_bytes());
        code[10..14].copy_from_slice(&[0x48, 0x8B, 0x14, 0x24]);
        code[14..16].copy_from_slice(&[0x48, 0xB8]);
        let handler = unimplemented_import as extern "system" fn(&'static ImportSite, usize) -> u64;
        code[16..24].copy_from_slice(&(handler as *const () as u64).to_le_bytes());
        code[24..26].copy_from_slice(&[0xFF, 0xE0]);
        // SAFETY: `used + THUNK_SIZE <= len`, and the block is still writable.
        let at = unsafe {
            let at = self.base.add(self.used);
            std::ptr::copy_nonoverlapping(code.as_ptr(), at, THUNK_SIZE);
            at
        };
        self.used += THUNK_SIZE;
        Ok(at as usize)
    }

    /// Makes the block read+execute. The block is intentionally leaked:
    /// title code may hold these addresses for the life of the process.
    pub(crate) fn seal(self) -> Result<()> {
        let mut old = 0;
        // SAFETY: `base..base+len` is the allocation made in `new`.
        let ok = unsafe { VirtualProtect(self.base as _, self.len, PAGE_EXECUTE_READ, &mut old) };
        if ok == 0 {
            bail!(
                "VirtualProtect for import stubs failed: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: flushing our own process's instruction cache for our own block.
        unsafe { FlushInstructionCache(GetCurrentProcess(), self.base as _, self.len) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In a child process of the next test: calls an unimplemented import
    /// that traps, with the crash log where `XTP_TRAP_LOG` says.
    #[test]
    fn trap_child() {
        let Some(log) = std::env::var_os("XTP_TRAP_LOG") else {
            return;
        };
        crate::log_crashes_to(log.into());
        let site = ImportSite::leak("probe.dll", "Missing", Stub::Trap);
        unimplemented_import(site, 0x1234);
    }

    #[test]
    fn a_trapped_import_is_in_the_crash_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("crashes.log");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "thunks::tests::trap_child", "--nocapture"])
            .env("XTP_TRAP_LOG", &log)
            .output()
            .unwrap();
        assert!(!out.status.success(), "the trap ends the process");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let named = text.contains("XTP: unimplemented import probe.dll!Missing called from 0x1234");
        assert!(named, "crash log: {text:?}");
    }

    fn name(n: &str) -> ImportSymbol {
        ImportSymbol::Name {
            hint: 0,
            name: n.into(),
        }
    }

    #[test]
    fn the_table_policy_stubs_what_the_table_lists_and_traps_the_rest() {
        let table = UnimplementedPolicy::Table;
        // A console DLL whose functions return HRESULT, and one of its
        // functions that returns a bool.
        let new = name("XSomethingNotImplementedYet");
        assert_eq!(
            table.stub_for("XGameRuntime.dll", &new),
            Stub::Return(0x8000_4001)
        );
        let feature = name("XGameRuntimeIsFeatureAvailable");
        assert_eq!(
            table.stub_for("xgameruntime.dll", &feature),
            Stub::Return(0)
        );
        assert_eq!(table.stub_for("unlisted.dll", &new), Stub::Trap);
        // The other policies ignore the table.
        let trap = UnimplementedPolicy::Trap.stub_for("xgameruntime.dll", &new);
        assert_eq!(trap, Stub::Trap);
        let zero = UnimplementedPolicy::ReturnZero.stub_for("unlisted.dll", &new);
        assert_eq!(zero, Stub::Return(0));
    }

    #[test]
    fn a_stub_returns_its_value_and_keeps_its_first_caller() {
        let stub = UnimplementedPolicy::Table.stub_for("xgameruntime.dll", &name("XNew"));
        let site = ImportSite::leak("xgameruntime.dll", "XNew", stub);
        let mut block = ThunkBlock::new(1).unwrap();
        let thunk = block.emit(site).unwrap();
        block.seal().unwrap();
        let before = unimplemented_call_count();
        // SAFETY: the stub takes any arguments and returns a value.
        let (result, back) = unsafe { crate::test_call::call(thunk, [1, 2, 3], true) };
        assert_eq!(result, 0x8000_4001, "E_NOTIMPL, for an HRESULT module");
        assert_eq!(site.first_caller.load(Ordering::Relaxed), back);
        // A second call, from elsewhere: counted, the first caller kept.
        // SAFETY: as above.
        let f: extern "system" fn() -> u64 = unsafe { std::mem::transmute(thunk) };
        assert_eq!(f(), 0x8000_4001);
        assert_eq!(site.calls.load(Ordering::Relaxed), 2);
        assert_eq!(site.first_caller.load(Ordering::Relaxed), back);
        assert!(unimplemented_call_count() >= before + 2);
        let calls = stub_calls();
        let ours = calls.iter().find(|c| c.symbol == "XNew").unwrap();
        assert_eq!((ours.calls, ours.first_caller), (2, back));
    }
}
