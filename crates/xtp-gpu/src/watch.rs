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
//! write access back. The runtime tells us when title memory changes
//! ([`title_memory_remapped`]): after it maps a view, before and after it
//! unmaps one, and after the title reserves, commits or frees memory
//! (`XMemVirtualAlloc`, the memory API set's `VirtualAlloc` and
//! `VirtualFree`). That resets the blocks' protection and all we noted of
//! them. Blocks found unmapped are not asked about again until a report
//! there: view memory comes from placeholders, which only the runtime's
//! mapping fills. Other blocks that cannot be made read-only (free address
//! space, a reservation not committed yet) are reported written at each
//! asking, and asked about again only after a report there too: anyone may
//! allocate free address space, so memory that appears there unreported is
//! copied each time, never missed.
//!
//! shortcut: the title's memory calls that bypass xmem are not reported:
//! its own views of sections (`MapViewOfFile` and its kin) and the memory
//! API set's other calls (`VirtualAllocEx`, `VirtualAlloc2`), which go to
//! the host. Where a range reaches into such memory from a view, a
//! reservation the title placed itself (not write-watched, so it looks like
//! a placeholder) that such a call commits is not seen, and what we noted
//! of memory they unmap or free stays until a report there. Report them
//! from xmem if a title makes them on GPU memory.
//!
//! shortcut: writes into physical pages through another view (xmem's
//! `write_physical`, DirectStorage's page destinations) are not seen at
//! the guarded addresses; they go to the texture pool, whose textures are
//! hashed, not guarded. A system call writing into a guarded block fails
//! (none is known to).
//!
//! Costs (the review of the VIctory #464 and #490 ports; gpu-measure.md
//! §4, run1's world frames): the 4.6 to 9.9 GB a world frame checks is not
//! reduced yet. `guard` walks every block of an asking with one to four set
//! lookups each (one for a clean block), and `writes` and the copies'
//! `copy_stale` walk every page. The [`writing`] fast path never fires in
//! the world, where some view is always guarded. Cutting that volume is
//! G16 to G18's.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

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
        #[cfg(feature = "diagnostics")]
        let guarding = std::time::Instant::now();
        let written = guard(first * PAGE, last * PAGE);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::GuardUs,
            guarding.elapsed().as_micros() as u64,
        );
        written?
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
    /// runtime remaps them. Asking the system each time (`VirtualQuery`,
    /// which walks a view's page tables to the region's end) cost 100-230
    /// ms per world frame in VIctory #490's runs, and 300-400 ms for a
    /// frame's 1 050 askings of one 174 MB view here (`guard_costs`).
    views: BTreeSet<usize>,
    /// Blocks that could not be made read-only and are no placeholders
    /// (free address space, a reservation or a view not committed yet) when
    /// a copy took them, until the runtime reports a change there: reported
    /// written at every asking, without asking the system again (a failed
    /// protection change, a `VirtualQuery` and a `GetWriteWatch` each, since
    /// 4a873fb). Memory that appears there unreported stays reported
    /// written: copied each time, never missed.
    unguardable: BTreeSet<usize>,
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    clean: BTreeSet::new(),
    absent: BTreeSet::new(),
    views: BTreeSet::new(),
    unguardable: BTreeSet::new(),
});

/// How many blocks `Guards::protected` holds, set under the guards' lock,
/// so that [`writing`] (called before every kernel write into title memory:
/// file reads, DirectStorage) sees without the lock that nothing is
/// guarded. Raised before blocks are made read-only and lowered after
/// write access is back: when it reads 0, no block is read-only, and a
/// guard that starts after that is one that would have started after
/// `writing` took the lock.
static GUARDED: AtomicUsize = AtomicUsize::new(0);

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
    // Asked under the lock, which `title_memory_remapped` takes. The runtime
    // tells us before and after it unmaps a view, so a view noted between
    // the two may be gone, its blocks guarded again: the report after
    // forgets it all. An asking in between that finds the view gone notes
    // its blocks absent (the report before opened them, 5553e29), which the
    // report after forgets too.
    if !g.views.contains(&first) {
        if !view(start) {
            return None;
        }
        g.views.insert(first);
    }
    let mut written = Vec::new();
    let mut run: Option<usize> = None;
    for block in first..=last {
        #[cfg(feature = "diagnostics")]
        if block < last && g.absent.contains(&block) {
            crate::copystat::add(crate::copystat::C::Absent, 1);
        }
        let open = block < last && !g.clean.contains(&block) && !g.absent.contains(&block);
        if open {
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            if g.protected.contains(&block) {
                // Written through the runtime, still read-only.
                g.clean.insert(block);
            } else if !g.unguardable.contains(&block) {
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
/// block by block if they span views, leaving out any that fail, noting
/// placeholders absent and the others unguardable.
fn protect(g: &mut Guards, from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::PAGE_READONLY;
    // Counted before they are read-only (see `GUARDED`); none of them is
    // guarded yet.
    GUARDED.store(g.protected.len() + (to - from), SeqCst);
    let note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.extend(from..to);
        g.clean.extend(from..to);
    };
    if set_protection(from..to, PAGE_READONLY) {
        note(g, from, to);
    } else {
        for block in from..to {
            if set_protection(block..block + 1, PAGE_READONLY) {
                note(g, block, block + 1);
            } else if placeholder(block * BLOCK) {
                g.absent.insert(block);
            } else {
                g.unguardable.insert(block);
            }
        }
    }
    GUARDED.store(g.protected.len(), SeqCst);
}

/// Sets the protection of `blocks` (title memory); false if it failed: not
/// all mapped, or more than one view (whose protections change apart).
fn set_protection(blocks: Range<usize>, protection: u32) -> bool {
    use windows_sys::Win32::System::Memory::VirtualProtect;
    #[cfg(test)]
    tests::PROTECTED.with(|n| n.set(n.get() + 1));
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Protections, 1);
    let mut old = 0;
    // SAFETY: title view memory; only its protection changes.
    unsafe {
        VirtualProtect(
            (blocks.start * BLOCK) as *const _,
            blocks.len() * BLOCK,
            protection,
            &mut old,
        ) != 0
    }
}

/// Whether `at` is in a view (of a section).
fn view(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery};
    #[cfg(test)]
    tests::QUERIED.with(|n| n.set(n.get() + 1));
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::ViewQueries, 1);
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
    #[cfg(test)]
    tests::QUERIED.with(|n| n.set(n.get() + 1));
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::ViewQueries, 1);
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

/// Gives write access back to the guarded blocks among `blocks` and notes
/// them written; true if any was guarded. One protection change per run of
/// neighbouring blocks (a kernel read into a big buffer opens hundreds),
/// block by block where a run spans views.
fn open(g: &mut Guards, blocks: Range<usize>) -> bool {
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;
    let blocks: Vec<usize> = g.protected.range(blocks).copied().collect();
    for block in &blocks {
        g.protected.remove(block);
        g.clean.remove(block);
    }
    for run in blocks.chunk_by(|a, b| a + 1 == *b) {
        let (first, last) = (run[0], run[run.len() - 1]);
        if !set_protection(first..last + 1, PAGE_READWRITE) && run.len() > 1 {
            for &block in run {
                set_protection(block..block + 1, PAGE_READWRITE);
            }
        }
    }
    // Lowered once write access is back (see `GUARDED`).
    GUARDED.store(g.protected.len(), SeqCst);
    !blocks.is_empty()
}

/// The blocks `[at, at + len)` touches; the length is the title's
/// (`fread`'s size times count, saturated), so the end is cut to the
/// address space.
fn blocks(at: usize, len: usize) -> Range<usize> {
    at / BLOCK..at.saturating_add(len).div_ceil(BLOCK)
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // Without the lock when nothing is guarded (no copy of view memory
    // taken yet, or all of it remapped since), which DirectStorage's
    // workers would contend for; with blocks guarded, the lock as before.
    if GUARDED.load(SeqCst) == 0 {
        return;
    }
    open(&mut guards(), blocks(at, len));
}

/// Title memory `[at, at + len)` changed: the runtime mapped or unmapped a
/// view there, or the title reserved, committed or freed memory there. What
/// it shows is new, so it is no longer guarded, nor known to be a view,
/// unmapped or unguardable.
///
/// The runtime tells us after it maps, and both before and after it
/// unmaps, so a copy may have guarded the view in between: blocks still
/// guarded get write access back, else the title's next write there would
/// fault with nothing to take it (the old view may still be written before
/// it goes). The report after an unmapping also forgets what a copy noted
/// of the view since the report before, its start block as a view and its
/// blocks as guarded: the address may be ordinary memory next.
pub fn title_memory_remapped(at: usize, len: usize) {
    let mut g = guards();
    let g = &mut *g;
    let range = blocks(at, len);
    for known in [&mut g.absent, &mut g.views, &mut g.unguardable] {
        let gone: Vec<usize> = known.range(range.clone()).copied().collect();
        for block in gone {
            known.remove(&block);
        }
    }
    open(g, range);
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
        if open(&mut g, at / BLOCK..at / BLOCK + 1) {
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
        /// The address-space queries `guard` made on this thread (`view`,
        /// `placeholder`; each test runs on its own).
        pub(super) static QUERIED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// The protection changes made on this thread.
        pub(super) static PROTECTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn queried() -> usize {
        QUERIED.with(|n| n.get())
    }

    fn protected() -> usize {
        PROTECTED.with(|n| n.get())
    }

    /// Held by every test that guards blocks: the count of guarded blocks
    /// is the process's, and some tests need it at 0. Each test gives its
    /// guards back before it ends; what one that failed left guarded is
    /// given back here, so that none depends on the others.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        title_memory_remapped(0, usize::MAX);
        serial
    }

    /// A pagefile section of `blocks` blocks.
    fn section(blocks: usize) -> windows_sys::Win32::Foundation::HANDLE {
        use windows_sys::Win32::System::Memory::CreateFileMappingW;
        // SAFETY: plain section creation.
        let section = unsafe {
            CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (blocks * BLOCK) as u32,
                std::ptr::null(),
            )
        };
        assert!(!section.is_null());
        section
    }

    /// A pagefile section of `blocks` blocks, mapped read-write: its view.
    fn mapped(blocks: usize) -> usize {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let section = section(blocks);
        // SAFETY: the fresh section, mapped whole; the view keeps it.
        let view = unsafe {
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, blocks * BLOCK).Value;
            windows_sys::Win32::Foundation::CloseHandle(section);
            view as usize
        };
        assert_ne!(view, 0);
        view
    }

    /// Unmaps a view of `blocks` blocks, as the runtime does: reported
    /// before and after.
    fn unmapped(view: usize, blocks: usize) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        title_memory_remapped(view, blocks * BLOCK);
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: view as *mut _,
            })
        };
        title_memory_remapped(view, blocks * BLOCK);
    }

    /// A fresh placeholder of `blocks` blocks: its address.
    fn placeholder_of(blocks: usize) -> usize {
        use windows_sys::Win32::System::Memory::{
            MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: plain reservation of a placeholder.
        let p = unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                blocks * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            )
        } as usize;
        assert_ne!(p, 0);
        p
    }

    /// Maps `blocks` blocks of `section`, from block `from` on, over
    /// `[at, at + blocks)` of a placeholder, as xmem maps physical pages:
    /// split out of the placeholder first (which fails harmlessly when it
    /// is the whole placeholder), then reported. The view.
    fn map_view(
        section: windows_sys::Win32::Foundation::HANDLE,
        at: usize,
        from: usize,
        blocks: usize,
    ) -> windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
        use windows_sys::Win32::System::Memory::{
            MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER, MapViewOfFile3,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: acts on our placeholder only, and maps our section there.
        let view = unsafe {
            VirtualFree(
                at as *mut _,
                blocks * BLOCK,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            );
            MapViewOfFile3(
                section,
                GetCurrentProcess(),
                at as *const _,
                (from * BLOCK) as u64,
                blocks * BLOCK,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(view.Value as usize, at);
        title_memory_remapped(at, blocks * BLOCK);
        view
    }

    /// Releases the allocation of `blocks` blocks at `at`, as the title
    /// frees memory through xmem: reported after.
    fn released(at: usize, blocks: usize) {
        // SAFETY: our own allocation.
        assert_ne!(unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) }, 0);
        title_memory_remapped(at, blocks * BLOCK);
    }

    /// Whether the guards note anything of blocks `[at, at + len)`.
    fn noted(at: usize, len: usize) -> bool {
        let g = guards();
        [&g.protected, &g.clean, &g.absent, &g.views, &g.unguardable]
            .into_iter()
            .any(|known| known.range(blocks(at, len)).next().is_some())
    }

    /// F5: 64 neighbouring blocks of one view are guarded with one
    /// protection change and opened by a kernel write over them with one.
    #[test]
    fn neighbouring_blocks_open_in_one_protection_change() {
        let _serial = serial();
        let view = mapped(64);
        let all: Vec<usize> = (view / PAGE..view / PAGE + 64).collect();
        let before = protected();
        assert_eq!(guard(view, view + 64 * BLOCK), Some(all.clone()));
        assert_eq!(protected() - before, 1, "guarded in one change");
        assert_eq!(GUARDED.load(SeqCst), 64);
        assert!((0..64).all(|b| !writable(view + b * BLOCK)));
        let before = protected();
        writing(view + 100, 64 * BLOCK - 200);
        assert_eq!(protected() - before, 1, "opened in one change");
        assert_eq!(GUARDED.load(SeqCst), 0);
        assert!((0..64).all(|b| writable(view + b * BLOCK)));
        // Opened means written: all reported again, and guarded again.
        assert_eq!(guard(view, view + 64 * BLOCK), Some(all));
        assert_eq!(guard(view, view + 64 * BLOCK), Some(Vec::new()));
        unmapped(view, 64);
        assert_eq!(GUARDED.load(SeqCst), 0);
    }

    /// The runtime reports a mapping after it is made: a copy that guarded
    /// the new view in between leaves it read-only, and the report gives
    /// write access back (the title's next write there used to fault with
    /// no guard to take it, and the process ended).
    #[test]
    fn remapped_memory_is_writable_again() {
        let _serial = serial();
        let view = mapped(2);
        let all: Vec<usize> = (view / PAGE..view / PAGE + 2).collect();
        assert_eq!(guard(view, view + 2 * BLOCK), Some(all.clone()));
        assert!(!writable(view) && !writable(view + BLOCK));
        title_memory_remapped(view, 2 * BLOCK);
        assert!(writable(view) && writable(view + BLOCK));
        assert_eq!(GUARDED.load(SeqCst), 0);
        // SAFETY: our view, read-write again.
        unsafe { ((view + BLOCK + 8) as *mut u8).write_volatile(1) };
        // New contents: reported again.
        assert_eq!(guard(view, view + 2 * BLOCK), Some(all));
        unmapped(view, 2);
    }

    /// F5: a run of guarded blocks over two views (which one protection
    /// change cannot span) opens block by block.
    #[test]
    fn runs_over_two_views_open_block_by_block() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let _serial = serial();
        // SAFETY: a placeholder of four blocks, split in two, each half
        // showing two blocks of one section.
        let (p, views) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                4 * BLOCK,
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
                (4 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            VirtualFree(
                p as *mut _,
                2 * BLOCK,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            );
            let views = [0, 2].map(|block| {
                let view = MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    2 * BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                );
                assert_eq!(view.Value as usize, p + block * BLOCK);
                view
            });
            windows_sys::Win32::Foundation::CloseHandle(section);
            (p, views)
        };
        assert!(guard(p, p + 4 * BLOCK).is_some());
        assert_eq!(GUARDED.load(SeqCst), 4);
        assert!((0..4).all(|b| !writable(p + b * BLOCK)));
        let before = protected();
        writing(p, 4 * BLOCK);
        assert_eq!(protected() - before, 5, "the run, then each block");
        assert!((0..4).all(|b| writable(p + b * BLOCK)));
        assert_eq!(GUARDED.load(SeqCst), 0);
        title_memory_remapped(p, 4 * BLOCK);
        // SAFETY: our views, back to free address space.
        unsafe {
            for view in views {
                UnmapViewOfFile2(GetCurrentProcess(), view, 0);
            }
        }
    }

    /// F5: with nothing guarded, a kernel write does not wait for the
    /// guards' lock (many DirectStorage workers call it); with a block
    /// guarded it does.
    #[test]
    fn writing_takes_the_lock_only_when_something_is_guarded() {
        use std::time::Duration;
        let _serial = serial();
        let plain = vec![0u8; PAGE];
        let at = plain.as_ptr() as usize;
        let returns = |lock_held_for: Duration| {
            let (tx, rx) = std::sync::mpsc::channel();
            let g = guards();
            std::thread::scope(|s| {
                s.spawn(move || {
                    writing(at, PAGE);
                    tx.send(()).unwrap();
                });
                let returned = rx.recv_timeout(lock_held_for).is_ok();
                drop(g);
                returned
            })
        };
        assert_eq!(GUARDED.load(SeqCst), 0, "`serial` gave every guard back");
        assert!(returns(Duration::from_secs(10)), "it waited for the lock");
        let view = mapped(1);
        assert!(guard(view, view + BLOCK).is_some());
        assert_eq!(GUARDED.load(SeqCst), 1);
        assert!(
            !returns(Duration::from_millis(200)),
            "it did not take the lock"
        );
        unmapped(view, 1);
    }

    /// A view is asked about once per block an asking starts in, again
    /// only after the runtime remapped it; other memory every time.
    #[test]
    fn views_are_queried_once_until_remapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
        };
        let _serial = serial();
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
        unmapped(view, 2);
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
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
        };
        let _serial = serial();
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
        unmapped(view, 2);
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
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
        };
        let _serial = serial();
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
        unmapped(view, LEN / BLOCK);
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
        let _serial = serial();
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
        // As xmem unmaps and the title frees: reported before and after.
        title_memory_remapped(p, 3 * BLOCK);
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

    /// A view of the first block of `section` in a block of its own, then a
    /// reservation of one block right after it, write-watched (the
    /// runtime's) or not (one the title placed itself): their address and
    /// the view. Retried if another thread takes the address in between.
    fn view_then_reservation(
        section: windows_sys::Win32::Foundation::HANDLE,
        watched: bool,
    ) -> (
        usize,
        windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    ) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile2;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        (0..8)
            .find_map(|_| {
                // A placeholder of two blocks: block 0 shows the section,
                // block 1 goes back to free address space and is reserved
                // again.
                let p = placeholder_of(2);
                let view = map_view(section, p, 0, 1);
                let watching = if watched { MEM_WRITE_WATCH } else { 0 };
                // SAFETY: the placeholder's second block, ours, then a plain
                // reservation there.
                let at = unsafe {
                    VirtualFree((p + BLOCK) as *mut _, 0, MEM_RELEASE);
                    VirtualAlloc(
                        (p + BLOCK) as *const _,
                        BLOCK,
                        MEM_RESERVE | watching,
                        PAGE_READWRITE,
                    )
                } as usize;
                if at == p + BLOCK {
                    return Some((p, view));
                }
                title_memory_remapped(p, BLOCK);
                // SAFETY: our view, back to free address space.
                unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, 0) };
                None
            })
            .expect("the address after the view stays free")
    }

    /// Takes down what [`view_then_reservation`] made, as xmem unmaps and
    /// the title frees: reported before and after.
    fn view_and_reservation_gone(
        p: usize,
        view: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
        section: windows_sys::Win32::Foundation::HANDLE,
    ) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile2;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        title_memory_remapped(p, 2 * BLOCK);
        // SAFETY: our view, reservation and section.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), view, 0);
            VirtualFree((p + BLOCK) as *mut _, 0, MEM_RELEASE);
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 2 * BLOCK);
    }

    /// A range from a view into a write-watched reservation (the title's
    /// own allocation) that is not committed yet: the reservation is no
    /// placeholder, since the title commits it without a mapping. It stays
    /// reported, asked about once (since 4a873fb, every asking asked the
    /// system again), until committed: xmem reports the commit, and from
    /// then on it is guarded and its writes are seen.
    #[test]
    fn watched_reservations_next_to_views_are_not_absent() {
        let _serial = serial();
        let section = section(1);
        let (p, view) = view_then_reservation(section, true);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let both: Vec<usize> = (0..2).flat_map(pages).collect();
        assert_eq!(guard(p, p + 2 * BLOCK), Some(both));
        let asked = (queried(), protected());
        for _ in 0..3 {
            assert_eq!(
                guard(p, p + 2 * BLOCK),
                Some(pages(1).collect()),
                "not committed, but no placeholder"
            );
        }
        assert_eq!((queried(), protected()), asked, "asked about once");
        // The title commits it and writes there, with no mapping.
        // SAFETY: our reservation; one byte of the committed block.
        unsafe {
            let at = VirtualAlloc((p + BLOCK) as *const _, BLOCK, MEM_COMMIT, PAGE_READWRITE);
            assert_eq!(at as usize, p + BLOCK);
            ((p + BLOCK + 8) as *mut u8).write_volatile(1);
        }
        // A commit xmem did not see is still reported, not guarded.
        assert_eq!(guard(p, p + 2 * BLOCK), Some(pages(1).collect()));
        assert!(writable(p + BLOCK));
        // xmem reports the commit.
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 2 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 2 * BLOCK), Some(Vec::new()));
        // SAFETY: the committed block, now guarded: into our handler.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(2) };
        assert_eq!(guard(p, p + 2 * BLOCK), Some(pages(1).collect()));
        view_and_reservation_gone(p, view, section);
    }

    /// A range from a view into a reservation the title placed itself (not
    /// write-watched): it looks like a placeholder, so it is noted absent.
    /// The title commits it through `XMemVirtualAlloc`, which reports it
    /// (before, nothing did, and its writes there were never seen).
    #[test]
    fn reservations_the_title_placed_are_seen_once_committed() {
        let _serial = serial();
        let section = section(1);
        let (p, view) = view_then_reservation(section, false);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let both: Vec<usize> = (0..2).flat_map(pages).collect();
        assert_eq!(guard(p, p + 2 * BLOCK), Some(both));
        assert_eq!(guard(p, p + 2 * BLOCK), Some(Vec::new()), "absent");
        // SAFETY: our reservation; one byte of the committed block.
        unsafe {
            let at = VirtualAlloc((p + BLOCK) as *const _, BLOCK, MEM_COMMIT, PAGE_READWRITE);
            assert_eq!(at as usize, p + BLOCK);
            ((p + BLOCK + 8) as *mut u8).write_volatile(1);
        }
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 2 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 2 * BLOCK), Some(Vec::new()));
        // SAFETY: the committed block, now guarded: into our handler.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(2) };
        assert_eq!(guard(p, p + 2 * BLOCK), Some(pages(1).collect()));
        view_and_reservation_gone(p, view, section);
    }

    /// xmem unmaps a view as it does now: reported, unmapped, reported
    /// again, with a copy guarding the view between the two reports (which
    /// notes its start block a view, and its blocks guarded and clean).
    /// Afterwards nothing of it is noted: the next asking finds no view
    /// there. Then the title frees the placeholder (reported) and the
    /// address is reused as ordinary memory: still no view, and writable.
    /// Without the report after the unmapping, the asking hit the stale
    /// view, reported nothing of the blocks gone (the copy kept the view's
    /// bytes), and once the address was reused made ordinary memory
    /// read-only, where system calls writing fail.
    #[test]
    fn unmapping_forgets_a_view_guarded_meanwhile_before_reuse() {
        use windows_sys::Win32::System::Memory::{MEM_PRESERVE_PLACEHOLDER, UnmapViewOfFile2};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let _serial = serial();
        let section = section(2);
        let p = (0..8)
            .find_map(|_| {
                let p = placeholder_of(2);
                let all: Vec<usize> = (p / PAGE..p / PAGE + 2).collect();
                let view = map_view(section, p, 0, 2);
                assert_eq!(guard(p, p + 2 * BLOCK), Some(all.clone()));
                // xmem's unmapping, a copy guarding the view in between.
                title_memory_remapped(p, 2 * BLOCK);
                assert_eq!(guard(p, p + 2 * BLOCK), Some(all), "opened: reported");
                assert!(!writable(p) && !writable(p + BLOCK), "guarded again");
                // SAFETY: our view, back to a placeholder.
                unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
                title_memory_remapped(p, 2 * BLOCK);
                assert!(!noted(p, 2 * BLOCK), "the view forgotten");
                assert_eq!(guard(p, p + 2 * BLOCK), None, "no view any more");
                // The title frees the placeholder; ordinary memory takes the
                // address, unless another thread took it in between.
                released(p, 2);
                // SAFETY: a plain allocation.
                let at = unsafe {
                    VirtualAlloc(
                        p as *const _,
                        2 * BLOCK,
                        MEM_RESERVE | MEM_COMMIT,
                        PAGE_READWRITE,
                    )
                } as usize;
                (at == p).then_some(p)
            })
            .expect("the freed address stays free");
        assert_eq!(guard(p, p + 2 * BLOCK), None, "ordinary memory");
        assert!(writable(p) && writable(p + BLOCK));
        // SAFETY: plain writes into our ordinary memory.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
        released(p, 2);
        // SAFETY: our section.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(section) };
    }

    /// A range from a view into a placeholder notes the placeholder absent.
    /// The title frees the placeholder (xmem reports it) and ordinary
    /// memory takes the address, unreported (anyone may allocate it): the
    /// next asking reports it. Without the report of the free, it stayed
    /// absent and was never reported, whatever was written there.
    #[test]
    fn freed_placeholders_are_not_absent_once_reused() {
        let _serial = serial();
        let section = section(1);
        let (p, view) = (0..8)
            .find_map(|_| {
                let p = placeholder_of(2);
                let view = map_view(section, p, 0, 1);
                let pages: Vec<usize> = (p / PAGE..p / PAGE + 2).collect();
                assert_eq!(guard(p, p + 2 * BLOCK), Some(pages));
                assert_eq!(guard(p, p + 2 * BLOCK), Some(Vec::new()), "absent");
                released(p + BLOCK, 1);
                // SAFETY: a plain allocation, written.
                let at = unsafe {
                    let at = VirtualAlloc(
                        (p + BLOCK) as *const _,
                        BLOCK,
                        MEM_RESERVE | MEM_COMMIT,
                        PAGE_READWRITE,
                    ) as usize;
                    if at == p + BLOCK {
                        ((p + BLOCK + 8) as *mut u8).write_volatile(1);
                    }
                    at
                };
                if at == p + BLOCK {
                    return Some((p, view));
                }
                // Another thread took the address: try again.
                title_memory_remapped(p, BLOCK);
                // SAFETY: our view, back to free address space.
                unsafe {
                    windows_sys::Win32::System::Memory::UnmapViewOfFile2(
                        windows_sys::Win32::System::Threading::GetCurrentProcess(),
                        view,
                        0,
                    )
                };
                None
            })
            .expect("the freed address stays free");
        let reused: Vec<usize> = ((p + BLOCK) / PAGE..(p + 2 * BLOCK) / PAGE).collect();
        assert_eq!(guard(p, p + 2 * BLOCK), Some(reused));
        assert_eq!(guard(p, p + 2 * BLOCK), Some(Vec::new()));
        view_and_reservation_gone(p, view, section);
    }

    /// xmem unmaps the middle block of a guarded three-block view: the view
    /// reported, a copy guarding it again in between, unmapped, reported
    /// again, and its outer blocks mapped back (each reported). The next
    /// asking's run of the three blocks then spans two views and a
    /// placeholder: guarded block by block, the middle one noted absent,
    /// and all three reported (the middle one shows nothing now). Without
    /// the report after the unmapping, the middle block stayed guarded and
    /// clean, and was never reported again. A kernel write over the three
    /// then opens the outer two.
    #[test]
    fn a_run_over_a_block_unmapped_since_it_was_guarded() {
        use windows_sys::Win32::System::Memory::{MEM_PRESERVE_PLACEHOLDER, UnmapViewOfFile2};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let _serial = serial();
        let section = section(3);
        let p = placeholder_of(3);
        let pages = |blocks: Range<usize>| -> Vec<usize> {
            (p / PAGE + blocks.start * BLOCK / PAGE..p / PAGE + blocks.end * BLOCK / PAGE).collect()
        };
        let whole = map_view(section, p, 0, 3);
        let before = protected();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0..3)));
        assert_eq!(protected() - before, 1, "one run, one change");
        // xmem's unmapping of block 1, a copy guarding the view in between.
        title_memory_remapped(p, 3 * BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0..3)));
        // SAFETY: our view, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), whole, MEM_PRESERVE_PLACEHOLDER) };
        title_memory_remapped(p, 3 * BLOCK);
        let outer = [
            map_view(section, p, 0, 1),
            map_view(section, p + 2 * BLOCK, 2, 1),
        ];
        let before = protected();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0..3)));
        assert_eq!(protected() - before, 4, "the run, then each block");
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        assert!(!writable(p) && !writable(p + 2 * BLOCK));
        {
            let g = guards();
            let middle = p / BLOCK + 1;
            assert!(g.absent.contains(&middle) && !g.protected.contains(&middle));
        }
        writing(p, 3 * BLOCK);
        assert!(writable(p) && writable(p + 2 * BLOCK));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some([pages(0..1), pages(2..3)].concat())
        );
        // As xmem unmaps and the title frees: reported before and after.
        title_memory_remapped(p, 3 * BLOCK);
        for view in outer {
            // SAFETY: our views, back to placeholders.
            unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        }
        title_memory_remapped(p, 3 * BLOCK);
        for block in 0..3 {
            released(p + block * BLOCK, 1);
        }
        // SAFETY: our section.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(section) };
    }

    /// Writers fault into the handler on guarded blocks while a copier
    /// guards them again and again and copies what is reported, and the
    /// runtime reports some of them remapped meanwhile. Every fault is
    /// taken (else the process ends) and no write is lost: once all is
    /// quiet, the copy is the view.
    #[test]
    fn the_handler_races_guards_and_remap_reports() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
        const BLOCKS: usize = 16;
        const WRITERS: usize = 4;
        const REFRESHES: usize = 200;
        /// A word of the view, atomic: other threads write and read it.
        fn word(at: usize) -> &'static AtomicU64 {
            // SAFETY: an aligned word of the test's view, used only while
            // it is mapped.
            unsafe { AtomicU64::from_ptr(at as *mut u64) }
        }
        let _serial = serial();
        let view = mapped(BLOCKS);
        let len = BLOCKS * BLOCK;
        // What a copy does: asks, then copies the pages reported.
        let refresh = |copy: &mut [u64]| {
            for page in guard(view, view + len).expect("a view") {
                let from = (page * PAGE - view) / 8;
                for (i, w) in copy[from..from + PAGE / 8].iter_mut().enumerate() {
                    *w = word(view + (from + i) * 8).load(Relaxed);
                }
            }
        };
        let mut copy = vec![0u64; len / 8];
        // Guarded from the start: the writers' first writes fault too.
        refresh(&mut copy);
        let refreshed = AtomicUsize::new(0);
        let quiet = AtomicBool::new(false);
        std::thread::scope(|s| {
            for w in 0..WRITERS {
                let refreshed = &refreshed;
                s.spawn(move || {
                    let mut k = 0;
                    // As long as the copier copies, and 1 000 writes at least.
                    while k < 1_000 || refreshed.load(Relaxed) < REFRESHES {
                        // Over every block, each writer its own words.
                        let block = (k * 7 + w) % BLOCKS;
                        let at = view + block * BLOCK + (k % 512 * WRITERS + w) * 8;
                        word(at).store(((w as u64) << 32) + k as u64 + 1, Relaxed);
                        k += 1;
                    }
                });
            }
            s.spawn(|| {
                let mut k = 0;
                while !quiet.load(Relaxed) {
                    // One to four blocks remapped.
                    let from = k % BLOCKS;
                    let n = (k % 4 + 1).min(BLOCKS - from);
                    title_memory_remapped(view + from * BLOCK, n * BLOCK);
                    k += 1;
                    std::thread::yield_now();
                }
            });
            for _ in 0..REFRESHES {
                refresh(&mut copy);
                refreshed.fetch_add(1, Relaxed);
            }
            quiet.store(true, Relaxed);
        });
        refresh(&mut copy);
        let seen: Vec<u64> = (0..len / 8)
            .map(|i| word(view + i * 8).load(Relaxed))
            .collect();
        assert!(seen.iter().any(|&w| w != 0), "written");
        assert!(seen == copy, "a write was lost");
        unmapped(view, BLOCKS);
    }

    /// What guarding view memory costs in a world frame: 30 submissions
    /// asking the guards about 35 times each (gpu-measure.md §7: 750 to
    /// 1 500 askings a frame), over a 174 MB heap of physical-page views,
    /// with what they report copied as refreshes copy it (through the
    /// kernel, unmapped pages by the fallback). The heap is one view, then
    /// one view per block with every 16th block an unmapped placeholder
    /// (11 MB; run1 read 55 to 240 MB a frame through the fallback). Each
    /// submission asks about the whole heap and 34 ranges of 64 KiB to 2 MiB
    /// in it, after the title wrote 8 of its blocks. Then a kernel write
    /// over the whole guarded view (`writing`), and `writing` elsewhere from
    /// 16 threads, with the heap guarded and with nothing guarded. Run with
    /// `cargo test --release -p xtp-gpu -- --ignored --nocapture guard_costs`.
    #[test]
    #[ignore = "benchmark"]
    fn guard_costs() {
        use std::time::{Duration, Instant};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, MapViewOfFile3,
            PAGE_NOACCESS, UnmapViewOfFile, UnmapViewOfFile2, VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let _serial = serial();
        const LEN: usize = 174 << 20;
        const BLOCKS: usize = LEN / BLOCK;
        const SUBMISSIONS: usize = 30;
        const RANGES: usize = 34;
        const FRAMES: usize = 4;
        // SAFETY: a pagefile section of the heap's size.
        let section = unsafe {
            CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                LEN as u32,
                std::ptr::null(),
            )
        };
        assert!(!section.is_null());
        // SAFETY: the section, mapped whole.
        let whole = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, LEN) };
        assert!(!whole.Value.is_null());
        let gap = |block: usize| block % 16 == 15;
        // SAFETY: a fresh placeholder of the heap's size.
        let paged = unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                LEN,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            )
        } as usize;
        assert_ne!(paged, 0);
        let views: Vec<MEMORY_MAPPED_VIEW_ADDRESS> = (0..BLOCKS)
            .filter(|&b| !gap(b))
            .map(|block| {
                let at = paged + block * BLOCK;
                // SAFETY: splits the placeholder at the block (failing
                // harmlessly for its last block) and maps the section's
                // block there.
                let view = unsafe {
                    VirtualFree(at as *mut _, BLOCK, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
                    MapViewOfFile3(
                        section,
                        GetCurrentProcess(),
                        at as *const _,
                        (block * BLOCK) as u64,
                        BLOCK,
                        MEM_REPLACE_PLACEHOLDER,
                        PAGE_READWRITE,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                assert_eq!(view.Value as usize, at);
                view
            })
            .collect();
        /// Copies pages `pages` of the heap at `heap` into `copy` through
        /// the kernel, as a refresh does.
        fn copy_run(heap: usize, pages: std::ops::Range<usize>, copy: &mut [u8]) -> usize {
            let (at, n) = ((pages.start - heap / PAGE) * PAGE, pages.len() * PAGE);
            let from = (pages.start * PAGE) as *const u8;
            crate::host::copy_mapped(copy[at..].as_mut_ptr(), from, n, false);
            n
        }
        /// The pages of `pages` written since their copy, copied in runs,
        /// as `Shadow::copy_stale` does.
        fn copy_stale(
            heap: usize,
            pages: std::ops::Range<usize>,
            (now, written): (u64, &[(usize, u64)]),
            copied: &mut [u64],
            copy: &mut [u8],
        ) -> usize {
            let base = heap / PAGE;
            let mut writes = written.iter().peekable();
            let (mut bytes, mut run) = (0, None);
            for page in pages.clone() {
                let write = match writes.peek() {
                    Some(&&(p, w)) if p == page => {
                        writes.next();
                        w
                    }
                    _ => 0,
                };
                let at = &mut copied[page - base];
                if *at == 0 || write > *at {
                    *at = now;
                    run.get_or_insert(page);
                } else if let Some(first) = run.take() {
                    bytes += copy_run(heap, first..page, copy);
                }
            }
            if let Some(first) = run {
                bytes += copy_run(heap, first..pages.end, copy);
            }
            bytes
        }
        let mut copy = vec![0u8; LEN];
        // One frame: time asking, time copying, bytes copied.
        let frame = |heap: usize, f: usize, copied: &mut [u64], copy: &mut [u8]| {
            let (mut asking, mut copying, mut bytes) = (Duration::ZERO, Duration::ZERO, 0);
            for s in 0..SUBMISSIONS {
                for k in 0..8 {
                    let block = ((f * SUBMISSIONS + s) * 8 + k) * 7_919 % BLOCKS;
                    let block = if gap(block) { block - 1 } else { block };
                    // SAFETY: a mapped block of the heap; guarded, the write
                    // goes through our handler.
                    unsafe { ((heap + block * BLOCK + 64 * k) as *mut u8).write_volatile(s as u8) };
                }
                let ranges = std::iter::once((0, BLOCKS)).chain((0..RANGES).map(|r| {
                    let n = 1 << (r % 6);
                    let block = (s * RANGES + r) * 104_729 % (BLOCKS - n);
                    (if gap(block) { block - 1 } else { block }, n)
                }));
                for (block, n) in ranges {
                    let (from, to) = (heap + block * BLOCK, heap + (block + n) * BLOCK);
                    let started = Instant::now();
                    let (now, written) = writes(from, to, true).expect("a view");
                    asking += started.elapsed();
                    let started = Instant::now();
                    let pages = from / PAGE..to / PAGE;
                    bytes += copy_stale(heap, pages, (now, &written), copied, copy);
                    copying += started.elapsed();
                }
            }
            (asking, copying, bytes)
        };
        for (name, heap) in [
            ("one view", whole.Value as usize),
            ("a view per block, every 16th unmapped", paged),
        ] {
            let mut copied = vec![0u64; LEN / PAGE];
            // The first copies, which guard it all.
            frame(heap, 0, &mut copied, &mut copy);
            let times: Vec<_> = (1..=FRAMES)
                .map(|f| frame(heap, f, &mut copied, &mut copy))
                .collect();
            let asking = times.iter().map(|t| t.0).sum::<Duration>() / FRAMES as u32;
            let copying = times.iter().map(|t| t.1).sum::<Duration>() / FRAMES as u32;
            let mb = times.iter().map(|t| t.2).sum::<usize>() as f64 / FRAMES as f64 / 1e6;
            println!(
                "174 MB heap, {name}: a frame of {SUBMISSIONS} x {} askings: {asking:?} asking, \
                 {copying:?} copying {mb:.1} MB",
                RANGES + 1
            );
        }
        let heap = whole.Value as usize;
        let opened: Vec<Duration> = (0..10)
            .map(|_| {
                // Guarded whole again.
                writes(heap, heap + LEN, true).expect("a view");
                let started = Instant::now();
                writing(heap, LEN);
                started.elapsed()
            })
            .collect();
        println!(
            "a kernel write over the guarded 174 MB view: {:?}",
            opened.iter().sum::<Duration>() / 10
        );
        const THREADS: usize = 16;
        const CALLS: usize = 100_000;
        let plain = vec![0u8; PAGE];
        let elsewhere = |what: &str| {
            let at = plain.as_ptr() as usize;
            let started = Instant::now();
            std::thread::scope(|s| {
                for _ in 0..THREADS {
                    s.spawn(move || {
                        for _ in 0..CALLS {
                            writing(std::hint::black_box(at), PAGE);
                        }
                    });
                }
            });
            println!(
                "a kernel write elsewhere, {THREADS} threads, {what}: {:.1} ns per call",
                started.elapsed().as_secs_f64() * 1e9 / (THREADS * CALLS) as f64
            );
        };
        writes(heap, heap + LEN, true).expect("a view");
        elsewhere("the heap guarded");
        title_memory_remapped(heap, LEN);
        title_memory_remapped(paged, LEN);
        elsewhere("nothing guarded");
        // SAFETY: our views, the placeholder's blocks and the section.
        unsafe {
            UnmapViewOfFile(whole);
            for view in views {
                UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER);
            }
            for block in 0..BLOCKS {
                VirtualFree((paged + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
    }

    /// Whether one protection change over a run of blocks costs less than
    /// one per block (F5 merges `open`'s), for a 4 MiB buffer and the
    /// 174 MB heap, its pages present, read-only to read-write as `open`
    /// does: medians of 15 rounds, the two ways alternated. Run with
    /// `cargo test --release -p xtp-gpu -- --ignored --nocapture protection_change_costs`.
    #[test]
    #[ignore = "benchmark"]
    fn protection_change_costs() {
        use std::time::{Duration, Instant};
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
        let set = |at: usize, n: usize, protection: u32| {
            let mut old = 0;
            // SAFETY: our view; only its protection changes.
            let set = unsafe { VirtualProtect(at as *const _, n * BLOCK, protection, &mut old) };
            assert_ne!(set, 0);
        };
        for blocks in [64, (174 << 20) / BLOCK] {
            let view = mapped(blocks);
            // SAFETY: our view: every page made present.
            unsafe { std::ptr::write_bytes(view as *mut u8, 1, blocks * BLOCK) };
            let (mut merged, mut each) = (Vec::new(), Vec::new());
            for round in 0..15 {
                for way in [round % 2, 1 - round % 2] {
                    set(view, blocks, PAGE_READONLY);
                    let started = Instant::now();
                    if way == 0 {
                        set(view, blocks, PAGE_READWRITE);
                    } else {
                        for b in 0..blocks {
                            set(view + b * BLOCK, 1, PAGE_READWRITE);
                        }
                    }
                    [&mut merged, &mut each][way].push(started.elapsed());
                }
            }
            let median = |t: &mut Vec<Duration>| {
                t.sort();
                t[t.len() / 2]
            };
            println!(
                "{} MiB read-only to read-write: {:?} in one change, {:?} block by block",
                (blocks * BLOCK) >> 20,
                median(&mut merged),
                median(&mut each)
            );
            unmapped(view, blocks);
        }
    }
}
