//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`xtp-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection. Blocks found
//! unmapped are not asked about again until it maps them: view memory
//! comes from placeholders, which only the runtime's mapping fills. A
//! write-watched reservation can be committed without the runtime telling
//! us (the title's own allocations), and free address space can be
//! allocated, so blocks there are still asked about each time.
//! shortcut: a reservation that is not write-watched (one the title placed
//! itself) looks like a placeholder, so memory committed into it later
//! without a mapping is not seen, where a range reaches into it from a
//! view.
//!
//! shortcut: writes into physical pages through another view (xmem's
//! `write_physical`, DirectStorage's page destinations) are not seen at
//! the guarded addresses; they go to the texture pool, whose textures are
//! hashed, not guarded. A system call writing into a guarded block fails
//! (none is known to).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

struct Journal {
    /// Numbers the askings; 0 is "never".
    asked: u64,
    /// Page number → the asking that last found it written.
    written: BTreeMap<usize, u64>,
    /// Room for the kernel's answer.
    found: Vec<usize>,
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    asked: 0,
    written: BTreeMap::new(),
    found: Vec::new(),
});

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    let guarded = if failed {
        count = 0;
        if !guarded {
            return None;
        }
        guard(first * PAGE, last * PAGE)?
    } else {
        Vec::new()
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
    }
    j.asked += 1;
    let now = j.asked;
    let pages = j.found[..count].iter().map(|&at| at / PAGE).chain(guarded);
    for page in pages.collect::<Vec<_>>() {
        j.written.insert(page, now);
    }
    Some((
        now,
        j.written
            .range(first..last)
            .map(|(&p, &w)| (p, w))
            .collect(),
    ))
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

struct Guards {
    /// Blocks we made read-only.
    protected: BTreeSet<usize>,
    /// Of those, the ones not written since a copy took them.
    clean: BTreeSet<usize>,
    /// Blocks found unmapped ([`placeholder`]) when a copy took them: the
    /// copy holds their zeros until the runtime maps them
    /// ([`title_memory_remapped`]). Without this, every asking found them
    /// "written" again (they cannot be made read-only) and the copy read
    /// them page by page through the kernel's failing reads, hundreds of
    /// MB per world frame.
    absent: BTreeSet<usize>,
    /// Blocks found to be in a view when an asking began there, until the
    /// runtime remaps them. Asking the system each time (`VirtualQuery`)
    /// cost 100-230 ms per world frame, more than the rest of the guard.
    views: BTreeSet<usize>,
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    clean: BTreeSet::new(),
    absent: BTreeSet::new(),
    views: BTreeSet::new(),
});

fn guards() -> std::sync::MutexGuard<'static, Guards> {
    GUARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    install();
    let mut g = guards();
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // Asked under the lock: a remap in between would leave a stale view.
    if !g.views.contains(&first) {
        if !view(start) {
            return None;
        }
        g.views.insert(first);
    }
    let mut written = Vec::new();
    let mut run: Option<usize> = None;
    for block in first..=last {
        let open = block < last && !g.clean.contains(&block) && !g.absent.contains(&block);
        if open {
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            if g.protected.contains(&block) {
                // Written through the runtime, still read-only.
                g.clean.insert(block);
            } else {
                run.get_or_insert(block);
                continue;
            }
        }
        if let Some(from) = run.take() {
            protect(&mut g, from, block);
        }
    }
    Some(written)
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail, and
/// noting placeholders absent.
fn protect(g: &mut Guards, from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.extend(from..to);
        g.clean.extend(from..to);
    };
    if set(from, to) {
        note(g, from, to);
        return;
    }
    for block in from..to {
        if set(block, block + 1) {
            note(g, block, block + 1);
        } else if placeholder(block * BLOCK) {
            g.absent.insert(block);
        }
    }
}

/// Whether `at` is in a view (of a section).
fn view(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery};
    #[cfg(test)]
    tests::QUERIED.with(|n| n.set(n.get() + 1));
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.Type == MEM_MAPPED
}

/// Whether the block at `at` holds nothing until the runtime maps a view
/// there: reserved private memory, not committed and not write-watched, as
/// a placeholder between views is. The system tells a placeholder from a
/// reservation the title placed itself only by the write watch. Free
/// address space (anyone may allocate it), a write-watched reservation (the
/// title commits it without a mapping) and a view not committed yet are
/// not placeholders.
fn placeholder(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        GetWriteWatch, MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    if n == 0 || info.State != MEM_RESERVE || info.Type != MEM_PRIVATE {
        return false;
    }
    let (mut found, mut count, mut granularity) = (0usize, 1, 0);
    // SAFETY: room for the one address asked for; no reset. It fails
    // outside write-watched memory.
    let watched = unsafe {
        GetWriteWatch(
            0,
            at as *const _,
            BLOCK,
            (&raw mut found).cast(),
            &mut count,
            &mut granularity,
        )
    } == 0;
    !watched
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded.
fn open(g: &mut Guards, start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let blocks: Vec<usize> = g
        .protected
        .range(start / BLOCK..end.div_ceil(BLOCK))
        .copied()
        .collect();
    for &block in &blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
        let mut old = 0;
        // SAFETY: a block we made read-only.
        unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old) };
    }
    !blocks.is_empty()
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // The length is the title's (`fread`'s size times count, saturated).
    open(&mut guards(), at, at.saturating_add(len));
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded (nor known
/// unmapped).
pub fn title_memory_remapped(at: usize, len: usize) {
    let mut g = guards();
    let range = at / BLOCK..at.saturating_add(len).div_ceil(BLOCK);
    let absent: Vec<usize> = g.absent.range(range.clone()).copied().collect();
    for block in absent {
        g.absent.remove(&block);
    }
    let views: Vec<usize> = g.views.range(range.clone()).copied().collect();
    for block in views {
        g.views.remove(&block);
    }
    let blocks: Vec<usize> = g.protected.range(range).copied().collect();
    for block in blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
    }
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        // Held across both checks: another thread may have opened the block
        // since this one faulted, and a copy may have guarded it again since
        // (then it is ours again, not read-only for some other reason).
        let mut g = guards();
        if open(&mut g, at, at + 1) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if writable(at) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    thread_local! {
        /// The view queries `guard` made on this thread (each test runs on
        /// its own).
        pub(super) static QUERIED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn queried() -> usize {
        QUERIED.with(|n| n.get())
    }

    /// A view is asked about once per block an asking starts in, again
    /// only after the runtime remapped it; other memory every time.
    #[test]
    fn views_are_queried_once_until_remapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        let at = queried();
        assert!(guard(view, view + 2 * BLOCK).is_some());
        assert!(guard(view + 8, view + BLOCK).is_some());
        assert_eq!(queried(), at + 1);
        // Another start block is asked about once.
        assert!(guard(view + BLOCK, view + 2 * BLOCK).is_some());
        assert!(guard(view + BLOCK, view + 2 * BLOCK).is_some());
        assert_eq!(queried(), at + 2);
        // The runtime remapped the first block: asked again.
        title_memory_remapped(view, BLOCK);
        assert!(guard(view, view + 2 * BLOCK).is_some());
        assert_eq!(queried(), at + 3);
        // Private memory is not a view, however often asked.
        let plain = vec![0u8; 2 * BLOCK];
        let p = plain.as_ptr() as usize;
        assert!(guard(p, p + 16).is_none());
        assert!(guard(p, p + 16).is_none());
        assert_eq!(queried(), at + 5);
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: view as *mut _,
            })
        };
        title_memory_remapped(view, 2 * BLOCK);
    }

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (_, before) = writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        writing(view + 100, 13);
        assert!(read(&mut file));
        assert_eq!(
            // SAFETY: the bytes just read.
            unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) },
            b"from the disk"
        );
        assert_eq!(
            guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
        title_memory_remapped(view, 2 * BLOCK);
    }

    /// What refreshing a buffer the size of the world's 174 MB heap costs:
    /// asking the kernel for its written pages (a few written, or 200 MB of
    /// them as in a world frame), and copying 200 MB of pages through the
    /// kernel (`host::copy_mapped`, as refreshes do) or plainly. Run with
    /// `cargo test --release -p xtp-gpu -- --ignored --nocapture watch_and_copy_costs`.
    #[test]
    #[ignore = "benchmark"]
    fn watch_and_copy_costs() {
        const LEN: usize = 174 << 20;
        const WRITTEN: usize = 200 << 20;
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                LEN,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        // SAFETY: our committed pages.
        let title = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, LEN) };
        title.iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);
        let mut copy = vec![0u8; LEN];
        let ask = || {
            let started = std::time::Instant::now();
            let (_, pages) = writes(p, p + LEN, false).expect("watched");
            (started.elapsed(), pages.len())
        };
        ask();
        let few: Vec<_> = (0..20)
            .map(|k| {
                // A few scattered kernel pages written by the title.
                for i in 0..50 {
                    title[(k * 7919 + i * 104_729) * 4096 % LEN] ^= 1;
                }
                ask().0
            })
            .collect();
        let many: Vec<_> = (0..5)
            .map(|_| {
                for page in (0..LEN).step_by(4096) {
                    title[page] ^= 1;
                }
                ask().0
            })
            .collect();
        let per_call =
            |t: &[std::time::Duration]| t.iter().sum::<std::time::Duration>() / t.len() as u32;
        println!(
            "174 MB write-watched: asking the kernel takes {:?} with a few pages written, {:?} \
             with all written",
            per_call(&few),
            per_call(&many)
        );
        // 200 MB in runs of stale 64 KiB pages, through the kernel as
        // refreshes copy, or plainly.
        for run in [PAGE, 64 * PAGE] {
            let timed = |copy_run: &mut dyn FnMut(usize)| {
                let started = std::time::Instant::now();
                for r in 0..WRITTEN / run {
                    copy_run(r * run % (LEN - run + 1));
                }
                WRITTEN as f64 / started.elapsed().as_secs_f64() / 1e9
            };
            let kernel = timed(&mut |at| {
                let (to, from) = (copy[at..].as_mut_ptr(), title[at..].as_ptr());
                crate::host::copy_mapped(to, from, run, false)
            });
            let plain = timed(&mut |at| copy[at..at + run].copy_from_slice(&title[at..at + run]));
            println!(
                "200 MB in {} KiB runs: {kernel:.2} GB/s through the kernel, {plain:.2} GB/s plainly",
                run >> 10
            );
        }
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// What a world frame's small buffer copies into guarded view memory
    /// cost (run1: 30 to 37 submissions and about 6 000 copies of about
    /// 600 bytes per frame, into the 174 MB heap, a view): each copied on
    /// the CPU at submission (`copy_mapped` into title memory, which opens
    /// the guard per copy) with the mapping asked first as before or only
    /// when the copy fails, or written back after the GPU copied them,
    /// range by range or merged per submission; the refresh that guards the
    /// blocks again counted too. Run with
    /// `cargo test --release -p xtp-gpu -- --ignored --nocapture guarded_copy_costs`.
    #[test]
    #[ignore = "benchmark"]
    fn guarded_copy_costs() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            UnmapViewOfFile,
        };
        const LEN: usize = 174 << 20;
        const SUBMISSIONS: usize = 30;
        const COPIES: usize = 200;
        const BYTES: usize = 600;
        // SAFETY: a pagefile section mapped read-write, unmapped at the end.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                LEN as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, LEN).Value as usize
        };
        assert_ne!(view, 0);
        let source = vec![7u8; BYTES];
        // Where the copies of submission `s` go: a ring the title fills in
        // order, or scattered over the heap.
        let ring = |s: usize, c: usize| ((s * COPIES + c) * BYTES) % (LEN - BYTES);
        let scattered =
            |s: usize, c: usize| ((s * COPIES + c) * 7_919 * 4_096 + c * 64) % (LEN - BYTES);
        let frame = |place: &dyn Fn(usize, usize) -> usize, how: &str| {
            let started = std::time::Instant::now();
            for s in 0..SUBMISSIONS {
                // The submission's refresh guards the heap's blocks again.
                writes(view, view + LEN, true).expect("a view");
                let ranges: Vec<(usize, usize)> =
                    (0..COPIES).map(|c| (view + place(s, c), BYTES)).collect();
                match how {
                    "asked first" => {
                        for &(at, n) in &ranges {
                            if crate::texture::committed(at as *const u8, n) {
                                crate::host::copy_mapped(at as *mut u8, source.as_ptr(), n, true);
                            }
                        }
                    }
                    "cpu" => {
                        for &(at, n) in &ranges {
                            crate::host::copy_mapped(at as *mut u8, source.as_ptr(), n, true);
                        }
                    }
                    "back" => {
                        for &(at, n) in &ranges {
                            writing(at, n);
                            crate::host::write_mapped(at as *mut u8, source.as_ptr(), n);
                        }
                    }
                    _ => {
                        let ranges = ranges.into_iter().map(|(at, n)| ((), at, n)).collect();
                        let merged = crate::host::merge_ranges(ranges, |_| 0);
                        let block = vec![7u8; merged.iter().map(|r| r.2).max().unwrap_or(0)];
                        for ((), at, n) in merged {
                            writing(at, n);
                            crate::host::write_mapped(at as *mut u8, block.as_ptr(), n);
                        }
                    }
                }
            }
            started.elapsed()
        };
        for (layout, place) in [
            ("in a ring", &ring as &dyn Fn(usize, usize) -> usize),
            ("scattered", &scattered),
        ] {
            let [first, cpu, back, merged] =
                ["asked first", "cpu", "back", "merged"].map(|how| frame(place, how));
            println!(
                "a frame of {SUBMISSIONS} x {COPIES} copies of {BYTES} bytes {layout}: on the \
                 CPU at submission {first:?} with the mapping asked first, {cpu:?} asked on \
                 failure only; {back:?} written back range by range, {merged:?} merged"
            );
        }
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: view as *mut _,
            })
        };
        title_memory_remapped(view, LEN);
    }

    /// A write or mapping that runs past the end of the address space (a
    /// title's `fread` with a huge count) covers up to the end; the sum used
    /// to overflow. Up there no test has guarded blocks.
    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
    }

    /// Blocks of a placeholder between views are reported once, then not
    /// again (they cannot be guarded) until the runtime maps them.
    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of two; block 0 shows the section's first block, as the
        // runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// A range from a view into a write-watched reservation (the title's
    /// own allocation) that is not committed yet: the reservation is no
    /// placeholder, since the title commits it without a mapping. It stays
    /// reported until committed, and once committed its writes are seen.
    #[test]
    fn watched_reservations_next_to_views_are_not_absent() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a pagefile section of one block.
        let section = unsafe {
            CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                BLOCK as u32,
                std::ptr::null(),
            )
        };
        assert!(!section.is_null());
        // A view of the section in block 0, then a write-watched reservation
        // in block 1, right after it (retried if another thread takes the
        // address in between).
        let (p, view) = (0..8)
            .find_map(|_| {
                // SAFETY: a fresh placeholder of two blocks, split; block 0
                // shows the section, block 1 goes back to free address space
                // and is reserved again with write watching.
                unsafe {
                    let p = VirtualAlloc2(
                        GetCurrentProcess(),
                        std::ptr::null(),
                        2 * BLOCK,
                        MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                        PAGE_NOACCESS,
                        std::ptr::null_mut(),
                        0,
                    ) as usize;
                    assert_ne!(p, 0);
                    VirtualFree(p as *mut _, BLOCK, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
                    let view = MapViewOfFile3(
                        section,
                        GetCurrentProcess(),
                        p as *const _,
                        0,
                        BLOCK,
                        MEM_REPLACE_PLACEHOLDER,
                        PAGE_READWRITE,
                        std::ptr::null_mut(),
                        0,
                    );
                    assert_eq!(view.Value as usize, p);
                    VirtualFree((p + BLOCK) as *mut _, 0, MEM_RELEASE);
                    let at = VirtualAlloc(
                        (p + BLOCK) as *const _,
                        BLOCK,
                        MEM_RESERVE | MEM_WRITE_WATCH,
                        PAGE_READWRITE,
                    ) as usize;
                    if at == p + BLOCK {
                        return Some((p, view));
                    }
                    UnmapViewOfFile2(GetCurrentProcess(), view, 0);
                    None
                }
            })
            .expect("the address after the view stays free");
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let both: Vec<usize> = (0..2).flat_map(pages).collect();
        assert_eq!(guard(p, p + 2 * BLOCK), Some(both));
        assert_eq!(
            guard(p, p + 2 * BLOCK),
            Some(pages(1).collect()),
            "not committed, but no placeholder"
        );
        // The title commits it and writes there, with no mapping.
        // SAFETY: our reservation; one byte of the committed block.
        unsafe {
            let at = VirtualAlloc(
                (p + BLOCK) as *const _,
                BLOCK,
                MEM_COMMIT,
                PAGE_READWRITE,
            );
            assert_eq!(at as usize, p + BLOCK);
            ((p + BLOCK + 8) as *mut u8).write_volatile(1);
        }
        assert_eq!(guard(p, p + 2 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 2 * BLOCK), Some(Vec::new()));
        // SAFETY: the committed block, now guarded: into our handler.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(2) };
        assert_eq!(guard(p, p + 2 * BLOCK), Some(pages(1).collect()));
        // SAFETY: our view and reservation.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), view, 0);
            VirtualFree((p + BLOCK) as *mut _, 0, MEM_RELEASE);
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 2 * BLOCK);
    }
}
