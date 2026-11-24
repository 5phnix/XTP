//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::watch`]: write
//! watching, or read-only guards on physical-page views), all of it where
//! neither works. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! A range is copied at most once until the title writes it (W1): a
//! refresh asks the write watch once for its whole range, not once per gap
//! between the ranges GPU work in flight writes, and a page is copied again
//! only if written since, also where a refresh took only part of it (the
//! part is kept, [`Taken`]; before, such a page was copied at every
//! refresh). Copies between title memory and the copies are plain loads and
//! stores, which a fault on a page the title unmapped ends ([`try_copy`]):
//! the kernel's copy (`ReadProcessMemory`, `WriteProcessMemory`) they
//! replace costs a system call per range and ran at 0.25 to 0.8 GB/s in the
//! world (run-world, world-frame session C). The copy-back reads a run of
//! pieces the GPU changed at once (Marvisak/VIctory#1124) and writes only
//! the bytes the GPU changed and the title left alone (a three-way merge
//! ported from simnJS/AnyPS5), piece by piece, each byte once per
//! submission however many of its ranges overlap there
//! ([`Shadow::copy_back`]). `XTP_SHADOW_ONCE=0` brings
//! back all of it as before: kernel copies, an asking per gap, partial
//! pages copied at every refresh, the copy-back read piece by piece.
//!
//! The list's buffer copies and memory writes that touch a copy run on
//! the GPU at their place in the list ([`copy`], [`write`]), so they see
//! what shaders before them wrote and later commands see their result.
//!
//! shortcut: only the ranges draws are known to read are brought up to
//! date (vertex and index buffer views, constant buffers in tables, root
//! descriptors, buffer views in tables); buffers read through directly
//! indexed heaps keep what their first use copied.
//!
//! shortcut: copies have no tracked resource state. Like every other use,
//! the GPU copies and writes take them as common (draws rely on implicit
//! promotion), with an unordered access barrier first so that earlier
//! shader writes are done; the debug layer may report the state.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::host::{HostList, host, transition, uav_barrier};
use crate::watch::PAGE;

/// A title buffer's host copy.
pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// Per page of title memory from `start`'s on: what the copy last took
    /// of it, and after which write-watch asking.
    copied: Mutex<Vec<Taken>>,
    /// Whether the memory is write-watched: unknown, yes, no.
    watched: AtomicU8,
    /// Ranges GPU work of submissions not finished yet writes (its shaders,
    /// or its buffer copies): title memory there is older than the copy,
    /// so no refresh copies it over them.
    busy: Mutex<Vec<Busy>>,
    /// Per submission not finished yet whose shaders write the copy: the
    /// copy's bytes before it ran, over the ranges they write. What the GPU
    /// wrote differs.
    before: Mutex<Vec<Before>>,
    /// For the first-touch census ([`census`]): whether the title memory
    /// copied back is a view of physical pages, asked once: unknown, a
    /// view, other memory.
    #[cfg(any(test, feature = "diagnostics"))]
    kind: AtomicU8,
}

/// What a copy holds of a page of title memory as the title wrote it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Taken {
    /// The write-watch asking after which the copy took it (0: never).
    asked: u64,
    /// The bytes taken then, as offsets in the page: the page's part in
    /// the buffer, or for a page a refresh took in part, that part.
    from: u32,
    to: u32,
}

impl Taken {
    /// Whether the copy holds the page's bytes `[from, to)` (offsets in the
    /// page) as title memory has them, the page last found written at the
    /// asking `write` (0: never).
    fn holds(&self, from: usize, to: usize, write: u64) -> bool {
        self.asked != 0
            && write <= self.asked
            && self.from as usize <= from
            && to <= self.to as usize
    }

    /// The page's bytes `[from, to)` (offsets in it), taken after the
    /// asking `now`.
    fn new(now: u64, from: usize, to: usize) -> Taken {
        Taken {
            asked: now,
            from: from as u32,
            to: to as u32,
        }
    }
}

/// A range of a copy that GPU work in flight writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Busy {
    /// The submission ([`submission`]).
    submission: u64,
    /// Title addresses `[from, to)`.
    from: usize,
    to: usize,
    /// Its shaders write it (else a buffer copy does).
    shaders: bool,
}

/// A submission's snapshot of a copy ([`Shadow::writing`]).
struct Before {
    submission: u64,
    /// Sorted pieces that neither overlap nor touch: offset in the copy,
    /// bytes. Each covers ranges the submission declared, not the gaps
    /// between them (a refresh may change a gap before a later declaration
    /// covers it). A copy-back makes what it put back equal to the copy
    /// ([`Shadow::put_back`]).
    parts: Vec<(usize, Vec<u8>)>,
    /// Title pages a copy-back of the submission put back only in part
    /// (its range did not cover them whole), for a later one that covers
    /// them whole to note current ([`Shadow::mark_current`]).
    partial: Vec<usize>,
}

impl Before {
    /// Covers the copy's `[from, to)` (offsets): bytes this submission took
    /// already keep their value (the copy is unchanged there since, nothing
    /// refreshes a busy range), the rest is taken from `host` now.
    fn cover<'a>(&mut self, from: usize, to: usize, host: impl Fn(usize, usize) -> &'a [u8]) {
        // The pieces that overlap or touch the range.
        let first = self.parts.partition_point(|(at, b)| at + b.len() < from);
        let last = first
            + self.parts[first..]
                .iter()
                .take_while(|(at, _)| *at <= to)
                .count();
        if last == first + 1 {
            let (at, bytes) = &self.parts[first];
            if *at <= from && to <= at + bytes.len() {
                return;
            }
        }
        let start = self
            .parts
            .get(first)
            .filter(|_| first < last)
            .map_or(from, |p| p.0.min(from));
        let end = self.parts[first..last]
            .last()
            .map_or(to, |(at, b)| (at + b.len()).max(to));
        let mut merged = pooled(end - start);
        let mut at = start;
        for (part, bytes) in self.parts.drain(first..last) {
            if at < part {
                merged.extend_from_slice(host(at, part));
            }
            merged.extend_from_slice(&bytes);
            at = part + bytes.len();
            give_back(bytes);
        }
        if at < end {
            merged.extend_from_slice(host(at, end));
        }
        self.parts.insert(first, (start, merged));
    }

    /// The snapshot's bytes of `[from, to)` (offsets), if it took them.
    fn bytes(&mut self, from: usize, to: usize) -> Option<&mut [u8]> {
        let i = self.parts.partition_point(|(at, b)| at + b.len() < to);
        let (at, bytes) = self.parts.get_mut(i)?;
        (*at <= from && to <= *at + bytes.len()).then(|| &mut bytes[from - *at..to - *at])
    }

    fn release(self) {
        self.parts.into_iter().for_each(|(_, b)| give_back(b));
    }
}

/// Numbers the host's submissions, for what they leave in the copies until
/// they finish (snapshots, busy ranges).
static SUBMISSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A new submission's number (higher than any before), taken as its
/// preparation starts.
pub(crate) fn submission() -> u64 {
    SUBMISSIONS.fetch_add(1, Ordering::Relaxed) + 1
}

/// Snapshot memory kept for reuse (already touched, so no page faults).
static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// An empty buffer with room for `n` bytes, from the pool if it has one.
fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => pool.swap_remove(i),
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

/// Returns a snapshot buffer to the pool (at most 16 kept, the largest:
/// each submission in flight has its own).
fn give_back(v: Vec<u8>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    if pool.len() > 16 {
        pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
        pool.truncate(16);
    }
}

const NOT_WATCHED: u8 = 2;

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written by the submitting thread, and by the thread finishing an earlier
// submission only in ranges that one's GPU work wrote (which the submitting
// thread leaves alone until it has finished: `host::Batch::settle`).
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    /// The host GPU address of a title address inside the buffer.
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    /// Bytes from a title address to the buffer's end.
    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    /// Offset of a title address from the buffer's start.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start
    }

    /// Brings `len` bytes at `address` up to date when `list` is submitted,
    /// and keeps the copy alive until the list is reset.
    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.start + self.len - address);
        list.defer_refresh(self, address as usize, len as usize);
    }

    fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Shadow {
        let pages = (start + len).div_ceil(PAGE as u64) - start / PAGE as u64;
        Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            copied: Mutex::new(vec![Taken::default(); pages as usize]),
            watched: AtomicU8::new(0),
            busy: Mutex::default(),
            before: Mutex::default(),
            #[cfg(any(test, feature = "diagnostics"))]
            kind: AtomicU8::new(0),
        }
    }

    /// Whether the title memory at `at` (in the buffer) is a view of
    /// physical pages, which the guard of view memory keeps read-only
    /// ([`census`]). Asked of the system once per copy: a query in a big
    /// view walks its page tables to the region's end.
    #[cfg(any(test, feature = "diagnostics"))]
    fn census_view(&self, at: usize) -> bool {
        use windows_sys::Win32::System::Memory::{
            MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
        };
        let kind = match self.kind.load(Ordering::Relaxed) {
            0 => {
                // SAFETY: plain data filled in by the query.
                let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                // SAFETY: a valid out pointer of the right size.
                let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
                let kind = if n != 0 && info.Type == MEM_MAPPED {
                    1
                } else {
                    2
                };
                self.kind.store(kind, Ordering::Relaxed);
                kind
            }
            kind => kind,
        };
        kind == 1
    }

    fn busy(&self) -> std::sync::MutexGuard<'_, Vec<Busy>> {
        self.busy.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `[address, address + len)` cut to the buffer, as offsets in the copy.
    fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (from - start, (address + len).clamp(from, end) - start)
    }

    /// Shaders of `list` may write `[address, address + len)` of the copy
    /// (unordered access): after the list's submission has run, the range
    /// is copied back into title memory, where the title reads it; until
    /// then no refresh copies title memory over it. Call after the
    /// [`Shadow::refresh`] that gives the shaders the title's data.
    ///
    /// shortcut: what shaders wrote is found by keeping the range's bytes
    /// before the submission (a copy per declared range and submission);
    /// a GPU-side dirty mask would save that if big ranges make it slow.
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    /// At the preparation of `submission`, after the range's refresh: its
    /// shaders write this range. Its bytes are kept to tell later what the
    /// GPU wrote: one snapshot per copy and submission (submissions in
    /// flight each need the bytes from before their own GPU work: one
    /// snapshot shared by them gave a later one bytes from before an
    /// earlier one, which lost GPU writes and undid title writes), grown to
    /// cover every range the submission declares (the world declares
    /// overlapping views of its 174 MB heap dozens of times), in reused
    /// memory (fresh snapshots cost ~2.3 million page faults per world
    /// frame).
    pub(crate) fn writing(&self, submission: u64, address: usize, len: usize) {
        self.busy().push(Busy {
            submission,
            from: address,
            to: address + len,
            shaders: true,
        });
        let (from, to) = self.inside(address, len);
        if from >= to {
            return;
        }
        // SAFETY: inside the mapping; GPU work in flight does not write it
        // (a step declaring a range first finishes the submissions whose
        // GPU work writes it).
        let host =
            |a: usize, b: usize| unsafe { std::slice::from_raw_parts(self.data.add(a), b - a) };
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let i = match before.iter().position(|b| b.submission == submission) {
            Some(i) => i,
            None => {
                before.push(Before {
                    submission,
                    parts: Vec::new(),
                    partial: Vec::new(),
                });
                before.len() - 1
            }
        };
        before[i].cover(from, to, host);
    }

    /// At the preparation of `submission`: a buffer copy it records on the
    /// GPU writes `[address, address + len)` of the copy, which refreshes
    /// leave alone until [`Shadow::write_back`] has put it in title memory.
    pub(crate) fn copying(&self, submission: u64, address: usize, len: usize) {
        self.busy().push(Busy {
            submission,
            from: address,
            to: address + len,
            shaders: false,
        });
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). The title keeps
    /// writing while this runs (on the completion thread), so only the runs
    /// of bytes the GPU changed are written, not the pieces around them.
    ///
    /// Each byte goes back once per submission, whatever ranges over it
    /// the submission declared and in whatever order they come back: what
    /// a copy-back put back is in the snapshot after it ([`Shadow::put_back`]),
    /// so another range over it finds nothing the GPU changed there. (Before,
    /// a second range over bytes the first had taken from the title took
    /// them for the GPU's, and wrote them over what the title wrote since.
    /// The host merges the ranges of a submission that overlap or touch.)
    /// `[address, address + len)` covers whole ranges the submission
    /// declared: they are done with once it is back. Returns the bytes
    /// written.
    pub(crate) fn copy_back(&self, submission: u64, address: usize, len: usize) -> usize {
        let mut snapshots = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let Some(snapshot) = snapshots.iter_mut().find(|b| b.submission == submission)
            && from < to
            && let Some(before) = snapshot.bytes(from, to)
        {
            // By piece: the copy compared with its bytes before the
            // submission, title memory read only where the GPU wrote, a run
            // of such pieces with one copy (Marvisak/VIctory#1124: through
            // the kernel a call cost tens to hundreds of microseconds however
            // small, and its world put back ~650 pieces a frame).
            let end_of = |at: usize| (at + PIECE - (start + at) % PIECE).min(to);
            // SAFETY: inside the mapping; the submitting thread leaves the
            // range alone until this submission has finished.
            let host =
                |a: usize, b: usize| unsafe { std::slice::from_raw_parts(self.data.add(a), b - a) };
            let differs =
                |before: &[u8], a: usize, b: usize| host(a, b) != &before[a - from..b - from];
            #[cfg(any(test, feature = "diagnostics"))]
            let census = census::before_back(submission, start + from, start + to, || {
                self.census_view(start + from)
            });
            let mut synced = Vec::new();
            let mut at = from;
            while at < to {
                let mut end = end_of(at);
                if differs(before, at, end) {
                    // The pieces after it the GPU changed too, up to a run
                    // (one piece at a time as before W1).
                    while once() && end < to && end - at < RUN && differs(before, end, end_of(end))
                    {
                        end = end_of(end);
                    }
                    let old = &mut before[at - from..end - from];
                    written += self.put_back(at, end, old, &mut synced);
                }
                at = end;
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::BackCompared, (to - from) as u64);
            // Pages now equal to title memory are current in the copy, so
            // our own write is not taken for the title's; others keep what
            // they had (a title write there shows at the next refresh). The
            // range's whole pages only, with those an earlier range of the
            // submission put back in part; the other part pages wait for a
            // range that covers them whole.
            let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
            let whole = |p: &usize| (first..last).contains(p);
            let mut current: Vec<usize> = synced
                .iter()
                .chain(&snapshot.partial)
                .copied()
                .filter(whole)
                .collect();
            current.sort_unstable();
            current.dedup();
            snapshot.partial.retain(|p| !whole(p));
            snapshot
                .partial
                .extend(synced.into_iter().filter(|p| !whole(p)));
            self.mark_current(&current);
            #[cfg(any(test, feature = "diagnostics"))]
            census::after_back(census, submission, start + from, start + to);
        }
        // Every step of the submission that declared a range in it is done
        // with it, and the submission's snapshot with its last range.
        let end = address + len;
        let mut busy = self.busy();
        busy.retain(|b| {
            !(b.shaders && b.submission == submission && address <= b.from && b.to <= end)
        });
        if !busy.iter().any(|b| b.shaders && b.submission == submission)
            && let Some(i) = snapshots.iter().position(|b| b.submission == submission)
        {
            snapshots.swap_remove(i).release();
        }
        written
    }

    /// A buffer copy `list` records on the GPU writes `[address, address +
    /// len)` of the copy: once the list has run, the range goes back into
    /// title memory, all of it ([`Shadow::write_back`]).
    pub(crate) fn gpu_copies(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.left(address));
        list.defer_gpu_copy(self, address as usize, len as usize);
    }

    /// After the submission ran: writes `[address, address + len)` of the
    /// copy into title memory (unmapped pages left alone), as the GPU
    /// copies that wrote it would have. Unlike [`Shadow::copy_back`], no
    /// snapshot tells the GPU's bytes apart: the range is exactly what a
    /// copy wrote. `[address, address + len)` covers ranges the
    /// submission's copies declared ([`Shadow::copying`]): refreshes may
    /// take them again. Returns the bytes written.
    pub(crate) fn write_back(&self, submission: u64, address: usize, len: usize) -> usize {
        let written = self.write_copied(submission, address, len);
        let end = address + len;
        self.busy()
            .retain(|b| b.shaders || b.submission != submission || b.from < address || b.to > end);
        written
    }

    fn write_copied(&self, submission: u64, address: usize, len: usize) -> usize {
        let (from, to) = self.inside(address, len);
        if from >= to {
            return 0;
        }
        let start = self.start as usize;
        // The submission, for the census only.
        #[cfg(not(any(test, feature = "diagnostics")))]
        let _ = submission;
        #[cfg(any(test, feature = "diagnostics"))]
        let census = census::before_back(submission, start + from, start + to, || {
            self.census_view(start + from)
        });
        crate::watch::writing(start + from, to - from);
        // SAFETY: inside the mapping; the submitting thread leaves the
        // range alone until this submission has finished.
        let host = unsafe { self.data.add(from) };
        write_title(start + from, host, to - from);
        #[cfg(any(test, feature = "diagnostics"))]
        census::wrote(start + from, to - from);
        // Whole pages written are now as the copy has them, unless the
        // title wrote them meanwhile: our own write is not taken for the
        // title's.
        let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
        self.mark_current(&(first..last).collect::<Vec<_>>());
        #[cfg(any(test, feature = "diagnostics"))]
        census::after_back(census, submission, start + from, start + to);
        to - from
    }

    /// [`Shadow::copy_back`] of `[from, to)` (offsets in the copy), a run of
    /// pieces the GPU changed (`old`: their bytes before the submission):
    /// title memory read with one copy, where it can all be read (else the
    /// pieces that can: [`read_pieces`]), then piece by piece the GPU's
    /// bytes written into title memory where it lacks them, the title's
    /// taken into the copy elsewhere. Pieces that cannot be read are left
    /// alone; those read go into `synced` (page numbers), and `old` takes
    /// what the copy holds of them then: another range of the submission
    /// over them compares with that, and puts back only what this left.
    /// Returns the bytes written.
    ///
    /// Unlike Marvisak/VIctory#1124, which writes back whole runs of pieces
    /// with one kernel call, only the bytes the GPU changed are written
    /// (with plain stores, which cost no call): the title writes these
    /// pages while this runs, and a piece written whole puts back the
    /// title's bytes as read before over its writes since.
    ///
    /// shortcut: what stays open (watch.md item 58, the review of
    /// Marvisak/VIctory#1124). A title write into bytes the GPU changed
    /// stays when it was made before this read title memory for the run
    /// (the three-way merge, [`Shadow::write_gpu_runs`]), but not when made
    /// after that read (up to 16 pieces) and before the piece's write: the
    /// merge compares with the bytes as read, and the GPU's go over it. Nor
    /// when it put back the very value the snapshot holds: the byte looks
    /// untouched. A title write into bytes the GPU left alone stays; the
    /// copy then holds the title's bytes as read, and the page is not
    /// current ([`Shadow::mark_current`] compares).
    fn put_back(&self, from: usize, to: usize, old: &mut [u8], synced: &mut Vec<usize>) -> usize {
        thread_local! {
            /// Title memory of a run, kept (already touched) for the next.
            static TITLE: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let at = self.start as usize + from;
        TITLE.with_borrow_mut(|title| {
            title.resize(to - from, 0);
            let mut written = 0;
            for (a, b) in read_pieces(at, title) {
                let mut piece = a;
                while piece < b {
                    let end = (piece + PIECE - (at + piece) % PIECE).min(b);
                    #[cfg(test)]
                    tests::after_reading(at + piece);
                    // SAFETY: inside the mapping; the submitting thread
                    // leaves the range alone until this submission has
                    // finished.
                    let host = unsafe {
                        std::slice::from_raw_parts_mut(self.data.add(from + piece), end - piece)
                    };
                    let (old, title) = (&mut old[piece..end], &title[piece..end]);
                    written += self.write_gpu_runs(at + piece, host, old, title);
                    // Put back: the copy's bytes are the snapshot's now.
                    old.copy_from_slice(host);
                    synced.push((at + piece) / PAGE);
                    piece = end;
                }
            }
            written
        })
    }

    /// Writes into title memory at `at` the runs of bytes the GPU changed
    /// in a piece of the copy (`host`; `old`, its bytes before the
    /// submission; `title`, title memory as just read) where the title
    /// left them alone; the copy takes the title's bytes everywhere else.
    /// Returns the bytes written.
    ///
    /// A three-way merge (ported from simnJS/AnyPS5 (65a1b01db4), its
    /// `CompareWriteBack`): a byte both changed, the GPU in the copy and the
    /// title in title memory since the snapshot, keeps the title's. Before,
    /// the GPU's went over it; title memory is the title's records first,
    /// and the copy-back comes late (once the submission has finished).
    fn write_gpu_runs(&self, at: usize, host: &mut [u8], old: &[u8], title: &[u8]) -> usize {
        let (mut written, mut opened) = (0, false);
        let mut i = 0;
        while i < host.len() {
            let run = first_change(host, old, i);
            host[i..run].copy_from_slice(&title[i..run]);
            if run == host.len() {
                break;
            }
            i = first_same(host, old, run);
            // In the run the GPU changed: the parts the title left alone go
            // back, those it wrote stay its own (and the copy takes them).
            let (title_run, old_run) = (&title[..i], &old[..i]);
            let mut j = run;
            while j < i {
                let theirs = first_change(title_run, old_run, j);
                if theirs > j {
                    if !opened {
                        // The piece's guard, once.
                        crate::watch::writing(at, host.len());
                        opened = true;
                    }
                    write_title(at + j, host[j..].as_ptr(), theirs - j);
                    written += theirs - j;
                    #[cfg(any(test, feature = "diagnostics"))]
                    census::wrote(at + j, theirs - j);
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::BackRuns, 1);
                }
                j = first_same(title_run, old_run, theirs);
                host[theirs..j].copy_from_slice(&title[theirs..j]);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::BackKept, (j - theirs) as u64);
            }
        }
        written
    }

    /// After our own write into title memory: notes `pages` (title page
    /// numbers, all in the copy) current where title memory still equals
    /// the copy. The write watch is asked first, so that it does not report
    /// our write later; a title write before that asking is in title memory
    /// when it is compared (the page is not current then), one after it
    /// shows at a later asking.
    fn mark_current(&self, pages: &[usize]) {
        let (Some(&first), Some(&last)) = (pages.iter().min(), pages.iter().max()) else {
            return;
        };
        if self.watched.load(Ordering::Relaxed) == NOT_WATCHED {
            return;
        }
        let Some((now, _)) = crate::watch::writes(first * PAGE, (last + 1) * PAGE, true) else {
            return;
        };
        let start = self.start as usize;
        let mut title = vec![0u8; PAGE];
        let current: Vec<usize> = pages
            .iter()
            .copied()
            .filter(|&page| {
                let from = page * PAGE - start;
                // SAFETY: a whole page inside the mapping.
                let host = unsafe { std::slice::from_raw_parts(self.data.add(from), PAGE) };
                read_whole(page * PAGE, &mut title) && *host == *title
            })
            .collect();
        let base = start / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        for page in current {
            copied[page - base] = Taken::new(now, 0, PAGE);
        }
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched), but where GPU work in flight writes the copy.
    /// Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = self
            .busy()
            .iter()
            .map(|b| (b.from, b.to))
            .filter(|&(a, e)| a < to && e > from)
            .collect();
        busy.sort_unstable();
        let copied = if once() {
            self.update_once(from, to, &busy)
        } else {
            self.update_gaps(from, to, busy)
        };
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::Updates, 1);
            add(C::Updated, (to - from) as u64);
            add(C::Refreshed, copied as u64);
            add(C::SkippedBytes, (to - from).saturating_sub(copied) as u64);
            if copied == 0 {
                add(C::Skipped, 1);
            }
        }
        copied
    }

    /// [`Shadow::update`] of `[from, to)` around `busy` (sorted): one asking
    /// of the write watch for the whole range (the journal keeps what it
    /// finds in the busy ranges for later refreshes), then the pages
    /// written since their last copy in each gap.
    fn update_once(&self, from: usize, to: usize, busy: &[(usize, usize)]) -> usize {
        let mut gaps = Vec::with_capacity(busy.len() + 1);
        let mut at = from;
        for &(a, e) in busy {
            if a > at {
                gaps.push((at, a));
            }
            at = at.max(e);
        }
        if at < to {
            gaps.push((at, to));
        }
        if self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
            #[cfg(test)]
            tests::ASKED.set(tests::ASKED.get() + 1);
            if let Some((now, written)) = crate::watch::writes(from, to, true) {
                self.watched.store(1, Ordering::Relaxed);
                let copied: usize = gaps
                    .iter()
                    .map(|&(a, b)| self.copy_stale(a, b, now, &written))
                    .sum();
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            self.watched.store(NOT_WATCHED, Ordering::Relaxed);
        }
        gaps.iter().map(|&(a, b)| self.copy(a, b)).sum()
    }

    /// [`Shadow::update`] as before W1 (`XTP_SHADOW_ONCE=0`): an asking per
    /// gap around `busy` (sorted).
    fn update_gaps(&self, from: usize, to: usize, busy: Vec<(usize, usize)>) -> usize {
        if !busy.is_empty() {
            // Around ranges GPU work is writing: the gaps, each as below
            // (in the world most submissions have such ranges; copying the
            // gaps whole cost ~1 s per frame).
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                if a > at {
                    bytes += self.update_free(at, a);
                }
                at = at.max(e);
            }
            if at < to {
                bytes += self.update_free(at, to);
            }
            return bytes;
        }
        self.update_free(from, to)
    }

    /// Whether the title may have written a page of `[from, to)` since the
    /// copy last took it, or the copy holds only another part of the page
    /// (always, for memory that is neither write-watched nor guarded).
    /// Asks the write journal, which keeps the answer for the
    /// refresh that follows. (Marvisak/VIctory#595, for refreshes that
    /// overlap what an earlier submission's GPU work writes.)
    pub(crate) fn stale(&self, from: usize, to: usize) -> bool {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        if from >= to {
            return false;
        }
        if self.watched.load(Ordering::Relaxed) == NOT_WATCHED {
            return true;
        }
        let Some((_, written)) = crate::watch::writes(from, to, true) else {
            return true;
        };
        let base = start / PAGE;
        let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        (from / PAGE..=(to - 1) / PAGE).any(|page| {
            let write = written
                .binary_search_by_key(&page, |&(p, _)| p)
                .map_or(0, |i| written[i].1);
            let at = page * PAGE;
            !copied[page - base].holds(from.max(at) - at, to.min(at + PAGE) - at, write)
        })
    }

    /// [`Shadow::update_gaps`] of `[from, to)` (inside the buffer) where no
    /// GPU work writes.
    fn update_free(&self, from: usize, to: usize) -> usize {
        if self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
            #[cfg(test)]
            tests::ASKED.set(tests::ASKED.get() + 1);
            if let Some((now, written)) = crate::watch::writes(from, to, true) {
                self.watched.store(1, Ordering::Relaxed);
                let copied = self.copy_stale(from, to, now, &written);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            self.watched.store(NOT_WATCHED, Ordering::Relaxed);
        }
        self.copy(from, to)
    }

    /// Copies the parts in `[from, to)` of the pages written after the copy
    /// took them (`written`: last writes by page, in page order, from
    /// `from`'s page or before; `now`: this asking), whole pages within the
    /// buffer. A page only partly in the range is copied in part, and that
    /// part is kept ([`Taken`]): a refresh of it (or of less) finds it
    /// current until the title writes the page. (Before W1, such a page
    /// stayed stale, and was copied at every refresh: the ends of every
    /// range that does not start and end on a page, each submission.)
    fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let base = start / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let first = written.partition_point(|&(p, _)| p < from / PAGE);
        let mut writes = written[first..].iter().peekable();
        let (mut bytes, mut run) = (0, None::<usize>);
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let at = page * PAGE;
            // The page's part in the range, and in the buffer (offsets in
            // the page).
            let (a, b) = (from.max(at) - at, to.min(at + PAGE) - at);
            let whole = (start.max(at) - at, end.min(at + PAGE) - at);
            let taken = &mut copied[page - base];
            if taken.holds(a, b, write) {
                if let Some(first) = run.take() {
                    bytes += self.copy((first * PAGE).max(from), at.min(to));
                }
                continue;
            }
            if (a, b) == whole {
                *taken = Taken::new(now, a, b);
            } else if once() {
                // What it holds still, unwritten since, if this part
                // overlaps or touches it: both.
                let near = taken.asked != 0
                    && write <= taken.asked
                    && a <= taken.to as usize
                    && taken.from as usize <= b;
                *taken = if near {
                    Taken::new(now, a.min(taken.from as usize), b.max(taken.to as usize))
                } else {
                    Taken::new(now, a, b)
                };
            }
            run.get_or_insert(page);
        }
        // Cut to the range: next to it shaders may be writing the copy.
        if let Some(first) = run {
            bytes += self.copy((first * PAGE).max(from), ((last + 1) * PAGE).min(to));
        }
        bytes
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy
    /// (zeros for what cannot be read). Returns the bytes copied.
    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        if from >= to {
            return 0;
        }
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let host = unsafe { self.data.add(from - start) };
        if !plain() {
            crate::host::copy_mapped(host, from as *const u8, to - from, false);
            return to - from;
        }
        #[cfg(feature = "diagnostics")]
        if crate::copystat::mode() == 2 {
            return to - from;
        }
        #[cfg(feature = "diagnostics")]
        let moved = std::time::Instant::now();
        let zeros = read_title(host, from, to - from);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::PlainNs, moved.elapsed().as_nanos() as u64);
            add(C::Plain, (to - from - zeros) as u64);
            add(C::Zeroed, zeros as u64);
        }
        let _ = zeros;
        to - from
    }
}

/// The first offset from `at` where `a` and `b` differ (their length if
/// none), 8 bytes at a time: the copy-back's byte loop cost about 1 ns a
/// byte, 8 ms for each 8 MiB range the GPU wrote here and there
/// (`world_frame_copies_cost`). A byte at a time as before W1
/// (`XTP_SHADOW_ONCE=0`).
fn first_change(a: &[u8], b: &[u8], mut at: usize) -> usize {
    if once() {
        while at + 8 <= a.len() {
            let x = word(a, at) ^ word(b, at);
            if x != 0 {
                return at + (x.trailing_zeros() / 8) as usize;
            }
            at += 8;
        }
    }
    while at < a.len() && a[at] == b[at] {
        at += 1;
    }
    at
}

/// The first offset from `at` where `a` and `b` are the same (their length
/// if none), 8 bytes at a time as [`first_change`].
fn first_same(a: &[u8], b: &[u8], mut at: usize) -> usize {
    const ONES: u64 = u64::from_le_bytes([1; 8]);
    const HIGHS: u64 = u64::from_le_bytes([0x80; 8]);
    if once() {
        while at + 8 <= a.len() {
            let x = word(a, at) ^ word(b, at);
            // The high bit of each zero byte of `x` (bytes above the first
            // may be marked wrongly, not the first).
            let zero = x.wrapping_sub(ONES) & !x & HIGHS;
            if zero != 0 {
                return at + (zero.trailing_zeros() / 8) as usize;
            }
            at += 8;
        }
    }
    while at < a.len() && a[at] != b[at] {
        at += 1;
    }
    at
}

/// The 8 bytes of `bytes` at `at`, the first lowest.
fn word(bytes: &[u8], at: usize) -> u64 {
    let mut w = [0; 8];
    w.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(w)
}

/// The copy-back's unit: title memory is compared, read and written back
/// by page.
const PIECE: usize = PAGE;
/// At most this many bytes of pieces the GPU changed are read at once.
const RUN: usize = 16 * PIECE;

/// Whether refreshes and copy-backs work as W1 has them (see the module):
/// unless `XTP_SHADOW_ONCE=0`.
fn once() -> bool {
    static ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| {
        std::env::var("XTP_SHADOW_ONCE")
            .ok()
            .is_none_or(|v| v.trim() != "0")
    })
}

/// Whether title memory is copied with plain loads and stores: with
/// [`once`], unless `XTP_COPY_MODE=kernel` (diagnostics) asks for the
/// kernel's copy to time it.
fn plain() -> bool {
    #[cfg(feature = "diagnostics")]
    if crate::copystat::mode() == 3 {
        return false;
    }
    once()
}

// One `rep movsb` that a fault ends (see [`try_copy`]). Windows x64: the
// destination, source and length come in rcx, rdx and r8; rdi and rsi are
// the callee's to keep, hence the unwind data.
core::arch::global_asm!(
    ".text",
    ".p2align 4",
    ".globl xtp_gpu_try_copy",
    "xtp_gpu_try_copy:",
    ".seh_proc xtp_gpu_try_copy",
    "pushq %rdi",
    ".seh_pushreg %rdi",
    "pushq %rsi",
    ".seh_pushreg %rsi",
    ".seh_endprologue",
    "movq %rcx, %rdi",
    "movq %rdx, %rsi",
    "movq %r8, %rcx",
    ".globl xtp_gpu_try_copy_moving",
    "xtp_gpu_try_copy_moving:",
    "rep movsb",
    ".globl xtp_gpu_try_copy_stopped",
    "xtp_gpu_try_copy_stopped:",
    "movq %rcx, %rax",
    "popq %rsi",
    "popq %rdi",
    "retq",
    ".seh_endproc",
    options(att_syntax)
);

unsafe extern "C" {
    /// Copies `len` bytes from `src` to `dst`; returns how many were left
    /// when a fault stopped it (0: all of them copied).
    fn xtp_gpu_try_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    /// Its `rep movsb`, and the instruction after it.
    safe static xtp_gpu_try_copy_moving: u8;
    safe static xtp_gpu_try_copy_stopped: u8;
}

/// The kernel's page, the unit of protections and of what can be mapped.
const KERNEL_PAGE: usize = 4096;

thread_local! {
    /// The address the last fault at the copy's instruction on this thread
    /// could not access, as the system reported it (0: not reported).
    static FAULT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The stores into guarded blocks [`stopped`] let go on in this copy.
    static OPENED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The copy's vectored handler: a fault at its `rep movsb` ends the copy
/// (the instruction after it returns what was left), the address that
/// faulted noted in [`FAULT`]. It looks at nothing else.
///
/// A store into a block the guard of view memory made read-only goes to
/// the guard first ([`crate::watch::writing`] opens it, as the guard's own
/// handler does), and on once the page is writable: both handlers come
/// first ahead of the crash reporter's, so the one registered last is
/// called first, and that depends on what ran first. Ended there, a copy
/// into title memory fell back to the kernel's ([`write_title`]), which
/// fails on a block a refresh guarded again meanwhile: the GPU's bytes
/// were lost without a word.
unsafe extern "system" fn stopped(
    info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    use windows_sys::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, EXCEPTION_IN_PAGE_ERROR, STATUS_GUARD_PAGE_VIOLATION,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    };
    /// At most this many guarded blocks opened per copy (a refresh may
    /// guard a block again before the store is made again).
    const OPENINGS: u32 = 16;
    const WRITE: usize = 1;
    // SAFETY: the system passes the exception being dispatched.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
    // The census's guards, from any code: the system took the guard off,
    // and the access goes on (the copy's too). A write that faults while
    // the census changes a block's protection goes on once it is done.
    #[cfg(any(test, feature = "diagnostics"))]
    if record.NumberParameters >= 2 {
        let (kind, at) = (
            record.ExceptionInformation[0],
            record.ExceptionInformation[1],
        );
        let going_on = match record.ExceptionCode {
            STATUS_GUARD_PAGE_VIOLATION => census::fault(kind == WRITE, at, context.Rip as usize),
            EXCEPTION_ACCESS_VIOLATION if kind == WRITE => census::settle(at, context.Rip as usize),
            _ => false,
        };
        if going_on {
            return EXCEPTION_CONTINUE_EXECUTION;
        }
    }
    let faults = [
        EXCEPTION_ACCESS_VIOLATION,
        EXCEPTION_IN_PAGE_ERROR,
        STATUS_GUARD_PAGE_VIOLATION,
    ];
    if context.Rip != &raw const xtp_gpu_try_copy_moving as u64
        || !faults.contains(&record.ExceptionCode)
    {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // Access violations, in-page errors and guard pages give the address.
    let at = if record.NumberParameters >= 2 {
        record.ExceptionInformation[1]
    } else {
        0
    };
    if record.ExceptionCode == EXCEPTION_ACCESS_VIOLATION
        && record.NumberParameters >= 2
        && record.ExceptionInformation[0] == WRITE
        && OPENED.get() < OPENINGS
    {
        use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
        // The faulting thread's last error stays its own (it may be the
        // title's): opening the other views of a page fails where one is
        // unmapped meanwhile, as the guard's handler knows.
        // SAFETY: plain reads and writes of this thread's last error.
        let error = unsafe { GetLastError() };
        crate::watch::writing(at, 1);
        let opened = writable(at);
        // SAFETY: as above.
        unsafe { SetLastError(error) };
        if opened {
            OPENED.set(OPENED.get() + 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
    }
    FAULT.set(at);
    #[cfg(test)]
    {
        // A string instruction may stop short of the faulting byte (fast
        // strings move blocks): as if this one had stopped `lag` bytes
        // before it.
        let lag = tests::LAG.replace(0) as u64;
        context.Rcx += lag;
        context.Rsi -= lag;
        context.Rdi -= lag;
    }
    context.Rip = &raw const xtp_gpu_try_copy_stopped as u64;
    EXCEPTION_CONTINUE_EXECUTION
}

/// Registers [`stopped`], once, first, ahead of the crash reporter's:
/// before the first plain copy, and before the census guards a page.
fn install() {
    use windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler;
    static HANDLER: std::sync::Once = std::sync::Once::new();
    // SAFETY: a handler that only moves past the copy's instruction, opens
    // a guarded block as the guard's own handler does, or lets an access to
    // a page the census guarded go on.
    HANDLER.call_once(|| unsafe {
        #[cfg(any(test, feature = "diagnostics"))]
        census::find_our_image();
        AddVectoredExceptionHandler(1, Some(stopped));
    });
}

/// Whether `at` is committed memory that can be written now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && can_write(info.Protect)
}

/// Whether committed memory of protection `protect` can be written.
fn can_write(protect: u32) -> bool {
    use windows_sys::Win32::System::Memory::{
        PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY,
    };
    let writes = PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    protect & PAGE_GUARD == 0 && protect & writes != 0
}

/// Copies `len` bytes from `src` to `dst` with plain loads and stores;
/// returns how many were left from the first 4 KiB page of either that
/// could not be accessed (0: all of them copied), every byte before it
/// copied. The kernel's copy reports such a page instead of faulting, but
/// runs at 1 GB/s or less, and title memory can be unmapped or decommitted
/// under a copy at any time (by the title's other threads): here a
/// vectored handler turns a fault at the copy's `rep movsb` into its end
/// ([`stopped`]).
///
/// A string instruction's registers say how far it got, which may be short
/// of the faulting byte: the processor may move blocks at a time (fast
/// strings), and on a fault keeps only that the iterations before the
/// registers are done. So the copy is made again from there up to the 4
/// KiB page of the address the system reported, and only that page is out
/// of reach. (Before, the bytes between were taken for unreadable: zeros
/// in the copy, a readable piece left out of the copy-back.) The stores
/// past where it stopped (if any) are only what the copy would have
/// stored.
///
/// # Safety
///
/// `dst` and `src` are `len` bytes that do not overlap, of which every page
/// that is accessible may be read (`src`) and written (`dst`): title memory,
/// or memory of ours no reference points to meanwhile.
unsafe fn try_copy(dst: *mut u8, src: *const u8, len: usize) -> usize {
    install();
    // Every byte before `at` is copied; `[at, end)` is left to copy.
    let (mut at, mut end) = (0, len);
    while at < end {
        FAULT.set(0);
        OPENED.set(0);
        // SAFETY: as the caller vouches; a fault ends the copy.
        let left = unsafe { xtp_gpu_try_copy(dst.add(at), src.add(at), end - at) };
        if left == 0 {
            at = end;
            break;
        }
        let stop = end - left;
        // The faulting page, as an offset in the copy, if the system said
        // where in the part not copied yet; else where the copy stopped.
        let fault = FAULT.get();
        let page = [src as usize, dst as usize]
            .into_iter()
            .find(|&base| (base + stop..base + end).contains(&fault))
            .map_or(stop, |base| {
                (fault & !(KERNEL_PAGE - 1)).max(base + stop) - base
            });
        if page == stop {
            return len - stop;
        }
        (at, end) = (stop, page);
    }
    len - at
}

/// Copies title memory `[src, src + len)` into memory of ours at `dst`
/// with plain loads; what cannot be read is left as zeros (as the kernel's
/// copy did). Returns the bytes zeroed.
fn read_title(dst: *mut u8, src: usize, len: usize) -> usize {
    let (mut at, mut zeros) = (0, 0);
    while at < len {
        // SAFETY: our memory, `len` bytes; title memory a fault leaves alone.
        let left = unsafe { try_copy(dst.add(at), (src + at) as *const u8, len - at) };
        at = len - left;
        if left == 0 {
            break;
        }
        let n = out_of_reach(src + at, false).min(left);
        // SAFETY: inside our memory.
        unsafe { dst.add(at).write_bytes(0, n) };
        zeros += n;
        at += n;
    }
    zeros
}

/// Reads title memory at `src` into `dst`: with one copy where it can all
/// be read, else the pieces that can be (Marvisak/VIctory#1124: a range
/// shorter than a piece can still cross a piece boundary, and a read that
/// fails keeps the readable pieces). Returns the parts read, as offsets in
/// `dst`: whole pieces of title memory, but at `dst`'s ends.
fn read_pieces(src: usize, dst: &mut [u8]) -> Vec<(usize, usize)> {
    let len = dst.len();
    // The end of the piece holding `at` (an offset in `dst`).
    let piece_end = |at: usize| (at + PIECE - (src + at) % PIECE).min(len);
    let mut parts: Vec<(usize, usize)> = Vec::new();
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::BackRead, len as u64);
    if !plain() {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackReads, 1);
        if crate::host::read_into(src as *const u8, dst) {
            parts.push((0, len));
            return parts;
        }
        // Not all of it is mapped: piece by piece.
        let mut at = 0;
        while at < len {
            let end = piece_end(at);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::BackReads, 1);
            if crate::host::read_into((src + at) as *const u8, &mut dst[at..end]) {
                match parts.last_mut() {
                    Some(last) if last.1 == at => last.1 = end,
                    _ => parts.push((at, end)),
                }
            }
            at = end;
        }
        return parts;
    }
    let mut at = 0;
    while at < len {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackReads, 1);
        #[cfg(test)]
        tests::READS.set(tests::READS.get() + 1);
        // SAFETY: our buffer, from `at` to its end; title memory a fault
        // leaves alone.
        let left = unsafe { try_copy(dst[at..].as_mut_ptr(), (src + at) as *const u8, len - at) };
        if left == 0 {
            parts.push((at, len));
            break;
        }
        // Read up to the piece the fault is in, which is left out whole.
        let stop = len - left;
        let read = stop.saturating_sub((src + stop) % PIECE).max(at);
        if read > at {
            parts.push((at, read));
        }
        // Past what cannot be read, to the end of the piece it ends in.
        let past = stop + out_of_reach(src + stop, false).min(len - stop);
        at = if past < len && !(src + past).is_multiple_of(PIECE) {
            piece_end(past)
        } else {
            past
        };
    }
    parts
}

/// How many bytes cannot be read (with `write`, written) from `at`, where
/// a copy faulted: to the end of its region if that is not committed or
/// not readable (writable), else (a page that changed since, or failed
/// otherwise) to the end of its 4 KiB page. One query per region out of
/// reach: the kernel's copy tried each 4 KiB page of it.
fn out_of_reach(at: usize, write: bool) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    let page_end = KERNEL_PAGE - at % KERNEL_PAGE;
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    let reachable = info.State == MEM_COMMIT
        && if write {
            can_write(info.Protect)
        } else {
            info.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0
        };
    if n == 0 || reachable {
        return page_end;
    }
    (info.BaseAddress as usize + info.RegionSize)
        .saturating_sub(at)
        .max(page_end)
}

/// Reads title memory at `src` into `dst`; false if a page of it cannot be
/// read.
fn read_whole(src: usize, dst: &mut [u8]) -> bool {
    if !plain() {
        return crate::host::read_into(src as *const u8, dst);
    }
    // SAFETY: our buffer; title memory a fault leaves alone.
    unsafe { try_copy(dst.as_mut_ptr(), src as *const u8, dst.len()) == 0 }
}

/// Writes `len` bytes of ours at `src` into title memory at `dst`, whose
/// guarded blocks the caller opened ([`crate::watch::writing`]; one a
/// refresh guarded again meanwhile is opened at the store's fault,
/// [`stopped`]): plain stores where it is mapped and writable, the pages
/// that are not left alone, as the kernel's copy left them before W1.
/// (Before, the kernel's copy took over from the first such page, and it
/// fails on a block a refresh guarded again meanwhile: the GPU's bytes
/// there were lost without a word.)
fn write_title(dst: usize, src: *const u8, len: usize) {
    if !plain() {
        crate::host::write_mapped(dst as *mut u8, src, len);
        return;
    }
    let (mut at, mut again) = (0, usize::MAX);
    while at < len {
        // SAFETY: title memory, as the caller vouches; a fault ends it.
        let left = unsafe { try_copy((dst + at) as *mut u8, src.wrapping_add(at), len - at) };
        if left == 0 {
            break;
        }
        at = len - left;
        // Writable since the fault (opened, or mapped again): once more.
        if at != again && writable(dst + at) {
            again = at;
            continue;
        }
        at += out_of_reach(dst + at, true).min(left);
    }
}

/// First-touch census of the copy-back (diagnostics; `XTP_COPYBACK_CENSUS`,
/// docs/analysis/lazy-copyback.md): of the title memory copy-backs cover,
/// how much the CPU touches before the GPU writes it again. The idea is xcl
/// issue #1640's (its census found about 96 % of the GPU's stores into
/// title memory overwritten before any CPU read); none of its code is
/// taken.
///
/// After each copy-back ([`Shadow::copy_back`], [`Shadow::write_back`]),
/// every 4 KiB page of its range gets a guard (`PAGE_GUARD`); the first
/// access to a page faults once into [`stopped`], which counts it (a read
/// or a write, by XTP's code or another's) and lets it go on: the system
/// takes the guard off. The next copy-back over the page counts what no
/// access reached as written again by the GPU first. Only what the copy-back
/// covers is counted: the GPU's bytes it writes back, and the whole range it
/// compares, which a copy-back on first touch would leave alone.
///
/// Which memory gets guards: with `XTP_COPYBACK_CENSUS=1`, only blocks the
/// guard of view memory keeps read-only ([`crate::watch`]). The runtime opens
/// those before every kernel write into title memory (file reads,
/// DirectStorage), which takes the census's guard off with the guard's
/// protection, so no system call fails on it. Read-write memory (write-
/// watched, or view blocks written since they were guarded) gets guards
/// with `XTP_COPYBACK_CENSUS=all` only: there a kernel write fails once on
/// the guard, which BTP 6's synchronous reads retry, but a DirectStorage or
/// `fread` read into it would fail.
///
/// shortcut: what the census does not see. A kernel read of a guarded page
/// (`ReadProcessMemory`: the textures' and the title copies' reads, which
/// read again after a failure) takes its guard off without a fault; the
/// page then counts as not touched. A kernel or runtime write the journal
/// sees counts as written unseen. A write to a read-only view block opens
/// the whole block ([`crate::watch`]), its other guards with it: the rest of
/// its pages count as uncounted after a write. A guard the guard of view
/// memory takes off otherwise (the other views of a page it guards, a
/// remapping) goes unnoticed.
#[cfg(any(test, feature = "diagnostics"))]
pub(crate) mod census {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::sync::{Mutex, OnceLock};

    use super::{KERNEL_PAGE, PAGE};

    /// The 4 KiB pages of a 64 KiB block (the census's unit, and that of
    /// protections).
    const SMALL: usize = PAGE / KERNEL_PAGE;
    /// Instruction addresses kept of the touches.
    const RIPS: usize = 32;

    /// Counters since the last line: bytes (of 4 KiB pages), but where said.
    #[derive(Clone, Copy)]
    pub(crate) enum K {
        /// Bytes of copied-back ranges guarded, and those left without a
        /// guard (read-write memory without `all`, memory neither watched
        /// nor a view, a protection change that failed).
        Armed,
        Unarmed,
        /// Guarded bytes a later copy-back covered before any touch: the
        /// GPU wrote them again first.
        Again,
        /// Guarded bytes touched first by a read, or a write, of code other
        /// than XTP's (the title's, the system's it calls), and by XTP's
        /// own (its copies, its texture hashing).
        Read,
        Write,
        Ours,
        /// Guarded bytes written without a fault (the journal saw it: a
        /// kernel or runtime write, another view of the page).
        Unseen,
        /// Guarded bytes uncounted after a write to their block: to a
        /// read-only block, which opens the whole block; to another, when
        /// the journal then finds the block written (it knows blocks, not
        /// pages).
        AfterWrite,
        /// Bytes of the 64 KiB blocks with a touch (a copy-back on first
        /// touch by block would compare those).
        Blocks,
        /// Bytes the GPU changed that a copy-back wrote into guarded pages;
        /// of them, in pages touched (or written unseen, or uncounted after
        /// a write) before a later copy-back, and in pages a later
        /// copy-back covered first.
        Gpu,
        GpuTouched,
        GpuAgain,
        /// Faults on guarded pages (a count: guard faults, and stores that
        /// fault on the read-only protection under a guard first),
        /// protection changes (a count), and the census's own time in its
        /// copy-back steps (microseconds).
        Faults,
        Protections,
        Us,
    }
    const COUNTERS: usize = 15;
    static COUNT: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];

    fn add(k: K, n: u64) {
        COUNT[k as usize].fetch_add(n, Relaxed);
    }

    /// The counters since the last call, cleared.
    pub(crate) fn take() -> [u64; COUNTERS] {
        std::array::from_fn(|i| COUNT[i].swap(0, Relaxed))
    }

    /// What a census guards: 0 nothing (no census), 1 read-only view
    /// blocks, 2 all.
    pub(crate) fn mode() -> u8 {
        #[cfg(test)]
        {
            let forced = super::tests::CENSUS.load(Relaxed);
            if forced != 0 {
                return forced - 1;
            }
        }
        static MODE: OnceLock<u8> = OnceLock::new();
        *MODE.get_or_init(|| {
            match std::env::var("XTP_COPYBACK_CENSUS")
                .as_deref()
                .map(str::trim)
            {
                Ok("1" | "on") => 1,
                Ok("all") => 2,
                _ => 0,
            }
        })
    }

    /// A 64 KiB block of title memory a census guarded.
    struct Block {
        /// The submission whose copy-back guarded it last.
        submission: u64,
        /// The journal's asking after that copy-back's own writes.
        asked: u64,
        /// Its 4 KiB pages counted (bit i: the i-th), and those guarded as
        /// far as the census knows.
        armed: u16,
        guarded: u16,
        /// The bytes the GPU wrote back into each page then.
        gpu: [u16; SMALL],
        /// Its protection under the guard is read-only (the guard of view
        /// memory's): a write opens the whole block.
        read_only: bool,
        /// A copy-back over it is under way: its faults are the copy-back's.
        busy: bool,
        /// A page of it was touched since it was guarded, and written (a
        /// guard fault on a store).
        touched: bool,
        wrote: bool,
    }

    struct Census {
        blocks: BTreeMap<usize, Block>,
        /// Every block a census guarded: a guard fault there is the
        /// census's, whatever it knows of the block since.
        ever: BTreeSet<usize>,
        /// Where touches came from: instruction address, touches.
        rips: Vec<(usize, u64)>,
    }

    static CENSUS: Mutex<Census> = Mutex::new(Census {
        blocks: BTreeMap::new(),
        ever: BTreeSet::new(),
        rips: Vec::new(),
    });

    /// Taken by the handler on the faulting thread: nothing holds it while
    /// it touches title memory.
    fn census() -> std::sync::MutexGuard<'static, Census> {
        CENSUS.lock().unwrap_or_else(|e| e.into_inner())
    }

    thread_local! {
        /// The GPU's bytes the copy-back under way on this thread wrote, as
        /// (4 KiB page number, bytes), while it is counted.
        static WROTE: std::cell::RefCell<Option<Vec<(usize, u32)>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// XTP's executable image (its code faults as XTP's own), and the copy's
    /// instruction.
    static IMAGE: OnceLock<(usize, usize)> = OnceLock::new();

    /// Notes where XTP's image lies (with the handler's registration). In
    /// tests none: the tests' accesses stand in for the title's.
    pub(crate) fn find_our_image() {
        #[cfg(not(test))]
        {
            use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows_sys::Win32::System::ProcessStatus::{K32GetModuleInformation, MODULEINFO};
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            // SAFETY: the process's own executable; a sized structure.
            let image = unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                let mut info: MODULEINFO = std::mem::zeroed();
                let ok = K32GetModuleInformation(
                    GetCurrentProcess(),
                    module,
                    &mut info,
                    size_of::<MODULEINFO>() as u32,
                ) != 0;
                let base = info.lpBaseOfDll as usize;
                if ok {
                    (base, base + info.SizeOfImage as usize)
                } else {
                    (0, 0)
                }
            };
            let _ = IMAGE.set(image);
        }
    }

    fn ours(rip: usize) -> bool {
        let (from, to) = IMAGE.get().copied().unwrap_or_default();
        rip == &raw const super::xtp_gpu_try_copy_moving as usize || (from..to).contains(&rip)
    }

    /// The bits of the 4 KiB pages `[from, to)` of a block.
    fn mask(from: usize, to: usize) -> u16 {
        ((((1u32 << to) - 1) & !((1u32 << from) - 1)) & 0xffff) as u16
    }

    fn bits(mask: u16) -> impl Iterator<Item = usize> {
        (0..SMALL).filter(move |i| mask & (1 << i) != 0)
    }

    /// The journal's last writing of `block`'s page (0: none known).
    fn last_write(written: &[(usize, u64)], block: usize) -> u64 {
        written
            .binary_search_by_key(&block, |&(p, _)| p)
            .map_or(0, |i| written[i].1)
    }

    /// How a copy-back takes part: whether the census counts it, and
    /// guards its range after.
    #[derive(Clone, Copy)]
    pub(crate) struct Back {
        counted: bool,
        view: bool,
    }

    /// Before a copy-back of `submission` writes title memory `[from, to)`
    /// (`view`: whether it is a view of physical pages, asked only with a
    /// census): what a census guarded there and no access reached, the GPU
    /// wrote again first. Faults there until [`after_back`] are the
    /// copy-back's own.
    pub(crate) fn before_back(
        submission: u64,
        from: usize,
        to: usize,
        view: impl FnOnce() -> bool,
    ) -> Back {
        let mode = mode();
        if mode == 0 || from >= to {
            return Back {
                counted: false,
                view: false,
            };
        }
        let view = view();
        if mode == 1 && !view {
            // Read-write memory, which a census leaves alone without `all`.
            add(
                K::Unarmed,
                (to - from).div_ceil(KERNEL_PAGE) as u64 * KERNEL_PAGE as u64,
            );
            return Back {
                counted: false,
                view,
            };
        }
        let started = std::time::Instant::now();
        // The journal as before the copy-back's own writes.
        let asked = crate::watch::writes(from, to, true);
        {
            let mut census = census();
            for (&block, b) in census.blocks.range_mut(from / PAGE..=(to - 1) / PAGE) {
                b.busy = true;
                if b.submission == submission {
                    // Another copy of the same title memory, back from the
                    // same submission: the same GPU work.
                    continue;
                }
                let written = asked.as_ref().map_or(0, |(_, w)| last_write(w, block));
                // Written since it was guarded: a write the guard did not see
                // (which may have taken the guards off), unless a store to
                // one of its pages faulted, which the journal cannot tell
                // apart (it knows blocks).
                let unseen = written > b.asked;
                for i in bits(b.armed & b.guarded) {
                    let (bytes, gpu) = match (unseen, b.wrote) {
                        (false, _) => (K::Again, K::GpuAgain),
                        (true, false) => (K::Unseen, K::GpuTouched),
                        (true, true) => (K::AfterWrite, K::GpuTouched),
                    };
                    add(bytes, KERNEL_PAGE as u64);
                    add(gpu, u64::from(b.gpu[i]));
                }
                b.armed = 0;
                if unseen {
                    b.guarded = 0;
                }
            }
        }
        WROTE.set(Some(Vec::new()));
        add(K::Us, started.elapsed().as_micros() as u64);
        Back {
            counted: true,
            view,
        }
    }

    /// The copy-back under way on this thread wrote the GPU's bytes into
    /// title memory `[at, at + len)`.
    pub(crate) fn wrote(at: usize, len: usize) {
        WROTE.with_borrow_mut(|wrote| {
            let Some(wrote) = wrote else {
                return;
            };
            let mut a = at;
            while a < at + len {
                let end = (a / KERNEL_PAGE + 1) * KERNEL_PAGE;
                let n = end.min(at + len) - a;
                match wrote.last_mut() {
                    Some((page, bytes)) if *page == a / KERNEL_PAGE => *bytes += n as u32,
                    _ => wrote.push((a / KERNEL_PAGE, n as u32)),
                }
                a += n;
            }
        });
    }

    /// After the copy-back of `[from, to)` by `submission` ([`before_back`]
    /// told how it takes part): guards each 4 KiB page of the range, for
    /// the first access to tell.
    pub(crate) fn after_back(back: Back, submission: u64, from: usize, to: usize) {
        if !back.counted {
            return;
        }
        super::install();
        let started = std::time::Instant::now();
        let mut wrote = WROTE.take().unwrap_or_default();
        wrote.sort_unstable();
        // The journal after the copy-back's own writes (its asking): a write
        // it finds now is another's, made while the copy-back ran.
        let asked = crate::watch::writes(from, to, true);
        let mut census = census();
        let census = &mut *census;
        if census.rips.capacity() < RIPS {
            // Room for the handler, which does not allocate.
            census.rips.reserve(RIPS);
        }
        for block in from / PAGE..=(to - 1) / PAGE {
            let at = block * PAGE;
            let (a, b) = (
                (from.max(at) - at) / KERNEL_PAGE,
                (to.min(at + PAGE) - at).div_ceil(KERNEL_PAGE),
            );
            let pages = mask(a, b);
            let bytes = ((b - a) * KERNEL_PAGE) as u64;
            let Some((now, written)) = &asked else {
                add(K::Unarmed, bytes);
                continue;
            };
            let mut gpu = [0u16; SMALL];
            let first = wrote.partition_point(|&(p, _)| p < at / KERNEL_PAGE);
            let end = (at + PAGE) / KERNEL_PAGE;
            for &(page, n) in wrote[first..].iter().take_while(|&&(p, _)| p < end) {
                let n = n.min(u32::from(u16::MAX)) as u16;
                gpu[page % SMALL] = gpu[page % SMALL].saturating_add(n);
            }
            let ours = gpu.iter().any(|&n| n != 0);
            // Where the copy-back wrote, its write and another's look alike.
            let theirs = !ours && last_write(written, block) == *now;
            let Census { blocks, ever, .. } = census;
            let entry = blocks.entry(block).or_insert(Block {
                submission,
                asked: 0,
                armed: 0,
                guarded: 0,
                gpu,
                read_only: back.view,
                busy: false,
                touched: false,
                wrote: false,
            });
            // Still guarded where it counts, and nothing took the guards
            // off: our write (the guard of view memory opens the block
            // first), another's while the copy-back ran.
            let intact = entry.guarded & pages == pages && !ours && !theirs;
            *entry = Block {
                submission,
                asked: *now,
                armed: pages,
                guarded: entry.guarded,
                gpu,
                read_only: entry.read_only,
                busy: false,
                touched: false,
                wrote: false,
            };
            if theirs {
                // Touched (written) as it came back: a touch before the
                // next GPU write.
                add(K::Unseen, bytes);
                add(K::GpuTouched, gpu.iter().map(|&n| u64::from(n)).sum());
                entry.armed = 0;
                continue;
            }
            if !intact {
                // Known as the census's before it is guarded: a fault there
                // waits for the lock, then finds it.
                ever.insert(block);
                match guard(at + a * KERNEL_PAGE, b - a, back.view) {
                    Some(read_only) => {
                        entry.guarded |= pages;
                        entry.read_only = read_only;
                    }
                    None => {
                        entry.armed = 0;
                        entry.guarded &= !pages;
                        add(K::Unarmed, bytes);
                        continue;
                    }
                }
            }
            add(K::Armed, bytes);
            add(K::Gpu, gpu.iter().map(|&n| u64::from(n)).sum());
        }
        add(K::Us, started.elapsed().as_micros() as u64);
    }

    /// Guards `pages` 4 KiB pages from `at`, keeping the protection under
    /// the guard: read-only for view memory, which the guard of view memory
    /// keeps so (read-write too with `all`), read-write for other memory
    /// (`all` only). Returns whether the protection under it is read-only,
    /// or `None` (the protection as it was) for any other protection.
    fn guard(at: usize, pages: usize, view: bool) -> Option<bool> {
        use windows_sys::Win32::System::Memory::{
            PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, VirtualProtect,
        };
        let len = pages * KERNEL_PAGE;
        let set = |protection: u32| {
            add(K::Protections, 1);
            let mut old = 0;
            // SAFETY: title memory; only its protection changes, and a
            // fault on the guard is the census's (the caller noted the
            // block first).
            let ok = unsafe { VirtualProtect(at as *const _, len, protection, &mut old) } != 0;
            ok.then_some(old)
        };
        let expected = if view { PAGE_READONLY } else { PAGE_READWRITE };
        let old = set(expected | PAGE_GUARD)?;
        let under = old & !PAGE_GUARD;
        if under == expected {
            return Some(view);
        }
        if view && under == PAGE_READWRITE && mode() == 2 {
            // A view block written since it was guarded: read-write.
            return set(PAGE_READWRITE | PAGE_GUARD).map(|_| false);
        }
        // Not what the guard or the title leaves there (another modifier,
        // no access): as it was.
        set(old);
        None
    }

    /// The handler, at a guard fault on `at` (`write`: a store) by the
    /// instruction at `rip`: whether the census guarded it, then counted
    /// and let go on (the system took the guard off).
    pub(crate) fn fault(write: bool, at: usize, rip: usize) -> bool {
        if mode() == 0 {
            return false;
        }
        let mut census = census();
        if !census.ever.contains(&(at / PAGE)) {
            return false;
        }
        touch(&mut census, write, at, rip);
        true
    }

    /// The handler, at a write fault on `at` by the instruction at `rip`:
    /// whether it is in a block the census guarded, and writable now, then
    /// let go on. A store to a read-only page faults on its protection
    /// before its guard (which stays): the census counts it here, and the
    /// guard of view memory then opens the block, its guards with it. A
    /// store also faults on a block the census made read-only for a moment
    /// (a protection it gives back): writable once that is done.
    pub(crate) fn settle(at: usize, rip: usize) -> bool {
        if mode() == 0 {
            return false;
        }
        {
            let mut census = census();
            if !census.ever.contains(&(at / PAGE)) {
                return false;
            }
            touch(&mut census, true, at, rip);
        }
        super::writable(at)
    }

    /// Counts the first access to the 4 KiB page of `at`, if guarded and
    /// counted, by the instruction at `rip`.
    fn touch(census: &mut Census, write: bool, at: usize, rip: usize) {
        add(K::Faults, 1);
        let Some(b) = census.blocks.get_mut(&(at / PAGE)) else {
            return;
        };
        let i = at % PAGE / KERNEL_PAGE;
        let bit = 1u16 << i;
        if b.armed & b.guarded & bit != 0 && !b.busy {
            let kind = if ours(rip) {
                K::Ours
            } else if write {
                K::Write
            } else {
                K::Read
            };
            add(kind, KERNEL_PAGE as u64);
            add(K::GpuTouched, u64::from(b.gpu[i]));
            b.armed &= !bit;
            b.wrote |= write;
            if !b.touched {
                b.touched = true;
                add(K::Blocks, PAGE as u64);
            }
        }
        b.guarded &= !bit;
        if write && b.read_only {
            // The guard of view memory opens the whole block for the store:
            // the other guards go with its read-only protection.
            if !b.busy {
                for j in bits(b.armed & b.guarded) {
                    add(K::AfterWrite, KERNEL_PAGE as u64);
                    add(K::GpuTouched, u64::from(b.gpu[j]));
                }
            }
            (b.armed, b.guarded) = (0, 0);
        }
        let rips = &mut census.rips;
        if let Some(r) = rips.iter_mut().find(|r| r.0 == rip) {
            r.1 += 1;
        } else if rips.len() < rips.capacity() {
            rips.push((rip, 1));
        } else if let Some(r) = rips.iter_mut().min_by_key(|r| r.1) {
            *r = (rip, 1);
        }
    }

    /// Logs the counters per frame over `frames` frames, and where the
    /// touches came from.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn log(frames: f64) {
        if mode() == 0 {
            return;
        }
        let c = take().map(|v| v as f64 / frames);
        let (armed, mut rips) = {
            let mut census = census();
            let armed: u64 = census
                .blocks
                .values()
                .map(|b| u64::from((b.armed & b.guarded).count_ones()))
                .sum();
            let rips = std::mem::take(&mut census.rips);
            (armed * KERNEL_PAGE as u64, rips)
        };
        let mb = |k: K| c[k as usize] / 1e6;
        tracing::info!(
            "copy-back census per frame (4 KiB pages): guarded {:.1} MB (not guarded {:.1} MB); \
             the GPU wrote {:.1} MB again before any touch; touched first {:.2} MB (reads \
             {:.2}, writes {:.2}, XTP's own {:.2}; {:.1} MB of 64 KiB blocks), written unseen \
             {:.2} MB, uncounted after a write {:.2} MB; the GPU's bytes written back {:.3} MB: \
             {:.3} MB touched, {:.3} MB written again first; {:.0} guard faults, {:.0} \
             protection changes, census {:.2} ms; {:.1} MB guarded now",
            mb(K::Armed),
            mb(K::Unarmed),
            mb(K::Again),
            mb(K::Read) + mb(K::Write) + mb(K::Ours),
            mb(K::Read),
            mb(K::Write),
            mb(K::Ours),
            mb(K::Blocks),
            mb(K::Unseen),
            mb(K::AfterWrite),
            mb(K::Gpu),
            mb(K::GpuTouched),
            mb(K::GpuAgain),
            c[K::Faults as usize],
            c[K::Protections as usize],
            c[K::Us as usize] / 1e3,
            armed as f64 / 1e6,
        );
        rips.sort_unstable_by_key(|r| std::cmp::Reverse(r.1));
        for (rip, n) in rips.into_iter().take(8) {
            tracing::info!(
                "  census touches at {}: {:.1}/frame",
                place(rip),
                n as f64 / frames
            );
        }
    }

    /// `rip` as module+offset, when a module the system knows holds it.
    #[cfg(feature = "diagnostics")]
    fn place(rip: usize) -> String {
        use windows_sys::Win32::System::LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            GetModuleFileNameW, GetModuleHandleExW,
        };
        let mut module = std::ptr::null_mut();
        // SAFETY: an address as the module name, with the flag that says so;
        // the reference count stays as it is.
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                rip as *const u16,
                &mut module,
            )
        } != 0;
        if !found {
            return format!("{rip:#x} (no module the system knows: the title's image?)");
        }
        let mut name = [0u16; 260];
        // SAFETY: a buffer of its stated length.
        let n =
            unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) } as usize;
        let path = String::from_utf16_lossy(&name[..n]);
        let file = path.rsplit('\\').next().unwrap_or(&path);
        format!("{file}+{:#x}", rip - module as usize)
    }
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

/// Title buffers by (start address, resource), with their copy once made.
static BUFFERS: Mutex<BTreeMap<(u64, usize), Entry>> = Mutex::new(BTreeMap::new());

fn buffers() -> std::sync::MutexGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A title buffer exists at `start` (`len` bytes).
pub(crate) fn register(resource: usize, start: u64, len: u64) {
    buffers().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    );
}

/// The title released it; lists that still hold its copy keep it alive.
pub(crate) fn unregister(resource: usize, start: u64) {
    buffers().remove(&(start, resource));
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let (key, entry_end) = {
        let map = buffers();
        let found = map
            .range(..=(address, usize::MAX))
            .rev()
            .take(64)
            .find(|(_, e)| e.end >= end);
        match found {
            Some((
                _,
                Entry {
                    shadow: Some(s), ..
                },
            )) => return Some(s.clone()),
            Some((&key, e)) => (key, e.end),
            None => {
                drop(map);
                return loose(address, end);
            }
        }
    };
    // Made without the map's lock held: making a buffer may wait (for a
    // heap, or VidMm), and every recording thread looks copies up here.
    let len = entry_end - key.0;
    // Rounded up so that views ending in the buffer's last 256 bytes fit.
    // Shaders write some buffers (unordered access), so not upload memory.
    let (buffer, data) = host()?.shared_buffer(len.next_multiple_of(1 << 16))?;
    crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let made = Arc::new(Shadow::new(buffer, data, gpu, key.0, len));
    let mut map = buffers();
    match map.get_mut(&key) {
        // Another thread made one meanwhile: that one (ours goes).
        Some(Entry {
            shadow: Some(s), ..
        }) => return Some(s.clone()),
        Some(entry) if entry.end == entry_end => entry.shadow = Some(made.clone()),
        // Released (or made again) meanwhile: ours serves this use only.
        _ => return Some(made),
    }
    drop(map);
    if len >= 16 << 20 {
        tracing::info!(
            start = format_args!("{:#x}", key.0),
            len,
            "large buffer copied to the host"
        );
    } else {
        tracing::debug!(
            start = format_args!("{:#x}", key.0),
            len,
            "buffer copied to the host"
        );
    }
    Some(made)
}

/// The title buffers holding `address`, innermost first (the one [`find`]
/// copies), as (start, length); none for memory outside every buffer,
/// which [`find`] copies in windows. For logs: it makes no copy.
pub(crate) fn holders(address: u64) -> Vec<(u64, u64)> {
    buffers()
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .filter(|(_, e)| e.end > address)
        .map(|(k, e)| (k.0, e.end - k.0))
        .collect()
}

/// The copy [`find`] gives `[address, address + len)`, if it is made
/// already.
pub(crate) fn existing(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let map = buffers();
    match map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
    {
        Some((_, entry)) => entry.shadow.clone(),
        None => {
            drop(map);
            let start = address & !(WINDOW - 1);
            let len = end.next_multiple_of(WINDOW) - start;
            let map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
            map.get(&(start, len)).cloned()
        }
    }
}

/// A buffer copy of `len` bytes from title address `src` to `dst` in
/// `list`. When either side has a host copy, shaders may have written the
/// source earlier in the submission or read the destination later, so the
/// copy runs on the GPU between host copies (made for the other side if
/// need be) at its place in the list, and the destination goes back to
/// title memory once the list has run. Returns false, recording nothing,
/// when neither side has a copy: title memory is then all there is, and
/// the caller copies it on the CPU at submission.
pub(crate) fn copy(list: &HostList, dst: u64, src: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let (from, to) = (existing(src, len), existing(dst, len));
    if from.is_none() && to.is_none() {
        return false;
    }
    let (Some(from), Some(to), Some(host)) = (
        from.or_else(|| find(src, len)),
        to.or_else(|| find(dst, len)),
        host(),
    ) else {
        return false;
    };
    let (at, from_at) = (to.offset(dst), from.offset(src));
    let same = from.buffer.as_raw() == to.buffer.as_raw();
    // Overlapping ranges of one buffer go through a buffer of the list's
    // own, as if the source were read whole first.
    let through = if same && from_at < at + len && at < from_at + len {
        let Some((through, _)) = host.shared_buffer(len) else {
            return false;
        };
        list.keep(&through);
        Some(through)
    } else {
        None
    };
    // The source as the title or earlier work of the submission left it.
    from.refresh(list, src, len);
    to.gpu_copies(list, dst, len);
    #[cfg(feature = "diagnostics")]
    list.note_read(
        || format!("copy {len:#x} bytes from {src:#x} to {dst:#x} on the GPU"),
        (from.gpu(src), len),
    );
    let (common, source, dest) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: an open list and live buffers (the steps above keep both
    // copies until the list is reset); ranges inside both.
    unsafe {
        if let Some(through) = &through {
            let b = &from.buffer;
            list.list.ResourceBarrier(&[
                uav_barrier(),
                transition(b, common, source),
                transition(through, common, dest),
            ]);
            list.list.CopyBufferRegion(through, 0, b, from_at, len);
            list.list.ResourceBarrier(&[
                transition(b, source, dest),
                transition(through, dest, source),
            ]);
            list.list.CopyBufferRegion(b, at, through, 0, len);
            list.list.ResourceBarrier(&[transition(b, dest, common)]);
        } else if same {
            // One buffer is the source and the destination: copied within
            // it as a destination.
            let b = &to.buffer;
            list.list
                .ResourceBarrier(&[uav_barrier(), transition(b, common, dest)]);
            list.list.CopyBufferRegion(b, at, b, from_at, len);
            list.list.ResourceBarrier(&[transition(b, dest, common)]);
        } else {
            let (a, b) = (&from.buffer, &to.buffer);
            list.list.ResourceBarrier(&[
                uav_barrier(),
                transition(a, common, source),
                transition(b, common, dest),
            ]);
            list.list.CopyBufferRegion(b, at, a, from_at, len);
            list.list
                .ResourceBarrier(&[transition(a, source, common), transition(b, dest, common)]);
        }
    }
    true
}

/// A memory write in `list` of the low `bytes` (4 or 8) of `value` at
/// title address `address`: if the address has a host copy, also written
/// there on the GPU at this place in the list, so that later commands of
/// the list read it (title memory gets it once the list has run, as
/// before). Returns whether it was.
///
/// shortcut: only where a copy exists already (a word nothing on the GPU
/// read yet does not get one), and only values: timestamps are known when
/// the list has run.
pub(crate) fn write(list: &HostList, address: u64, value: u64, bytes: usize) -> bool {
    if !matches!(bytes, 4 | 8) || !address.is_multiple_of(4) {
        return false;
    }
    let Some(shadow) = existing(address, bytes as u64) else {
        return false;
    };
    let Ok(list2) = list.list.cast::<ID3D12GraphicsCommandList2>() else {
        return false;
    };
    let gpu = shadow.gpu(address);
    let words = [value as u32, (value >> 32) as u32];
    let params: Vec<D3D12_WRITEBUFFERIMMEDIATE_PARAMETER> = words[..bytes / 4]
        .iter()
        .zip(0..)
        .map(|(&word, i)| D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
            Dest: gpu + 4 * i,
            Value: word,
        })
        .collect();
    #[cfg(feature = "diagnostics")]
    list.note_op(|| format!("write {value:#x} ({bytes} bytes) at {address:#x} on the GPU"));
    list.keep(&shadow.buffer);
    list.count_gpu_write(bytes);
    let b = &shadow.buffer;
    // SAFETY: an open list, a live buffer (kept until the list is reset)
    // and destinations inside it.
    unsafe {
        list2.ResourceBarrier(&[
            uav_barrier(),
            transition(
                b,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            ),
        ]);
        list2.WriteBufferImmediate(params.len() as u32, params.as_ptr(), None);
        list2.ResourceBarrier(&[transition(
            b,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COMMON,
        )]);
    }
    true
}

/// Titles also use memory outside any buffer object as GPU memory (BTP 6's
/// interface vertices, from its own allocator). Such memory
/// gets copies of the aligned windows it touches.
/// shortcut: copies of windows are never freed; key them to the title's
/// allocations if memory use grows.
const WINDOW: u64 = 4 << 20;

/// Copies of title memory outside buffers, by (start, length).
static LOOSE: Mutex<BTreeMap<(u64, u64), Arc<Shadow>>> = Mutex::new(BTreeMap::new());

fn loose(address: u64, end: u64) -> Option<Arc<Shadow>> {
    let start = address & !(WINDOW - 1);
    let len = end.next_multiple_of(WINDOW) - start;
    if let Some(shadow) = LOOSE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(start, len))
    {
        return Some(shadow.clone());
    }
    // Made without the lock held, as in `find`.
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let made = Arc::new(Shadow::new(buffer, data, gpu, start, len));
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    // The first one made stays.
    let shadow = map.entry((start, len)).or_insert(made).clone();
    drop(map);
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    Some(shadow)
}

/// For device-removed reports: the copy of title memory whose host range
/// starts closest below host GPU address `va`, and where `va` is in it.
pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Title memory as the runtime reserves it, write-watched and touched:
    /// `len` bytes at the start of a 4 MiB window of their own (one loose
    /// host copy, [`find`]). Never freed: the copy outlives the test.
    pub(super) fn watched(len: usize) -> usize {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
        };
        const MEM_WRITE_WATCH: u32 = 0x0020_0000;
        let window = WINDOW as usize;
        assert!(len <= window);
        // SAFETY: a fresh reservation of our own.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * window,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(base, 0, "write-watched memory");
        let at = base.next_multiple_of(window);
        // SAFETY: inside the reservation.
        unsafe { (at as *mut u8).write_bytes(0, window) };
        at
    }

    /// Title memory `[at, at + len)`.
    pub(super) fn title(at: usize, len: usize) -> Vec<u8> {
        // SAFETY: the test's memory.
        (0..len)
            .map(|i| unsafe { ((at + i) as *const u8).read_volatile() })
            .collect()
    }

    /// Writes title memory as the title's CPU does.
    pub(super) fn set_title(at: usize, len: usize, byte: u8) {
        for i in 0..len {
            // SAFETY: the test's memory.
            unsafe { ((at + i) as *mut u8).write_volatile(byte) };
        }
    }

    /// A title write while a copy-back runs, given the piece's address.
    type TitleWrite = Box<dyn FnMut(usize)>;

    thread_local! {
        /// What [`after_reading`] does on this thread.
        static AFTER_READING: std::cell::RefCell<Option<TitleWrite>> =
            const { std::cell::RefCell::new(None) };
        /// The plain reads [`read_pieces`] made on this thread.
        pub(super) static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// The write-watch askings refreshes made on this thread.
        pub(super) static ASKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// How far short of the faulting byte the next fault at the copy's
        /// instruction on this thread stops it ([`stopped`]).
        pub(super) static LAG: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// The census's mode for this process ([`census::mode`]) plus one, 0
    /// to leave it to `XTP_COPYBACK_CENSUS`: set by the census tests, each
    /// in a process of its own ([`alone`]).
    pub(super) static CENSUS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

    /// The census's counters since the last call.
    fn counted() -> [u64; 15] {
        census::take()
    }

    fn at(c: &[u64; 15], k: census::K) -> u64 {
        c[k as usize]
    }

    /// Runs `body` in a process of its own: the test binary again, with
    /// this test alone. Guarded blocks and the order of the vectored
    /// handlers are the process's, and the guard's own tests count the
    /// guarded blocks (they hold a lock of theirs for it).
    fn alone(test: &str, body: impl FnOnce()) {
        const CHILD: &str = "XTP_SHADOW_TEST_ALONE";
        if std::env::var_os(CHILD).is_some() {
            body();
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{test} in a process of its own: {stdout}\n{stderr}"
        );
    }

    /// A view of a pagefile section of `len` bytes, mapped read-write, as
    /// the runtime maps physical pages: memory the guard watches. Never
    /// unmapped: copies made of it outlive the test.
    fn view(len: usize) -> usize {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        };
        // SAFETY: a fresh section, mapped whole; the view keeps it.
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                len as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null(), "a section");
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            windows_sys::Win32::Foundation::CloseHandle(section);
            assert_ne!(view, 0, "a view");
            view
        }
    }

    /// Whether the kernel's write watch has a page of `[at, at + PAGE)`
    /// written (asked without clearing).
    fn kernel_marked(at: usize) -> bool {
        use windows_sys::Win32::System::Memory::GetWriteWatch;
        let mut found = [0usize; PAGE / KERNEL_PAGE];
        let (mut count, mut granularity) = (found.len(), 0);
        // SAFETY: room for `count` addresses; no reset.
        let failed = unsafe {
            GetWriteWatch(
                0,
                at as *const _,
                PAGE,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        } != 0;
        assert!(!failed, "write-watched memory");
        count > 0
    }

    /// Makes `[at, at + len)` of title memory unmapped (decommitted).
    fn unmap(at: usize, len: usize) {
        use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
        // SAFETY: the test's own memory, which nothing else uses.
        assert_ne!(unsafe { VirtualFree(at as *mut _, len, MEM_DECOMMIT) }, 0);
    }

    /// Called by [`Shadow::copy_back`] between its read of a piece of title
    /// memory at `at` and its write.
    pub(super) fn after_reading(at: usize) {
        AFTER_READING.with_borrow_mut(|f| {
            if let Some(f) = f {
                f(at);
            }
        });
    }

    impl Shadow {
        /// The copy's bytes at title addresses `[at, at + len)`.
        pub(super) fn host_bytes(&self, at: usize, len: usize) -> Vec<u8> {
            let from = at - self.start as usize;
            assert!(from + len <= self.len as usize);
            // SAFETY: inside the mapping.
            unsafe { std::slice::from_raw_parts(self.data.add(from), len) }.to_vec()
        }

        /// Writes the copy at title addresses `[at, at + len)`, as the GPU
        /// does.
        pub(super) fn set_host(&self, at: usize, len: usize, byte: u8) {
            let from = at - self.start as usize;
            assert!(from + len <= self.len as usize);
            // SAFETY: inside the mapping; tests stand in for the GPU.
            unsafe { self.data.add(from).write_bytes(byte, len) };
        }
    }

    /// A submission's snapshot takes the bytes of each range it declares
    /// once, when first declared, and not the gaps between ranges.
    #[test]
    fn a_snapshot_keeps_what_it_took_first_and_skips_the_gaps() {
        let mut host = vec![1u8; 64];
        let mut before = Before {
            submission: 1,
            parts: Vec::new(),
            partial: Vec::new(),
        };
        let mut cover = |host: &Vec<u8>, from: usize, to: usize| {
            before.cover(from, to, |a, b| &host[a..b]);
        };
        cover(&host, 8, 16);
        cover(&host, 32, 40);
        host.fill(2);
        // Overlapping the first and touching the second: merged; what was
        // taken stays 1, the gap between them is taken now.
        cover(&host, 12, 32);
        host.fill(3);
        cover(&host, 48, 50);
        cover(&host, 10, 14);
        let parts: Vec<(usize, Vec<u8>)> = before.parts.clone();
        let mut want = vec![1u8; 8];
        want.extend([2u8; 16]);
        want.extend([1u8; 8]);
        assert_eq!(parts, [(8, want), (48, vec![3, 3])]);
        assert_eq!(before.bytes(14, 18).as_deref(), Some(&[1, 1, 2, 2][..]));
        assert_eq!(before.bytes(40, 41), None, "past what it took");
        assert_eq!(before.bytes(47, 49), None);
        assert_eq!(before.bytes(48, 50).as_deref(), Some(&[3, 3][..]));
        before.release();
    }

    /// A refresh leaves alone what a GPU buffer copy in flight writes into
    /// the host copy, even where the title wrote since: the copy's bytes go
    /// to title memory when its submission finishes, and only then may a
    /// refresh take them back.
    #[test]
    fn a_refresh_leaves_what_a_gpu_copy_in_flight_writes() {
        let test = "shadow::tests::a_refresh_leaves_what_a_gpu_copy_in_flight_writes";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(PAGE);
        let shadow = find(window as u64, PAGE as u64).expect("a host copy");
        shadow.update(window, PAGE);
        let (dst, len) = (window + 0x100, 0x100);
        let copy = submission();
        shadow.copying(copy, dst, len);
        // The GPU copy ran; the title writes the page, the destination too.
        shadow.set_host(dst, len, 0xcc);
        set_title(window, PAGE, 0x22);
        shadow.update(window, PAGE);
        assert!(shadow.host_bytes(dst, len).iter().all(|&b| b == 0xcc));
        assert!(shadow.host_bytes(window, 0x100).iter().all(|&b| b == 0x22));
        assert!(
            shadow
                .host_bytes(dst + len, 0x100)
                .iter()
                .all(|&b| b == 0x22)
        );
        assert_eq!(shadow.write_back(copy, window, PAGE), PAGE);
        assert!(title(dst, len).iter().all(|&b| b == 0xcc));
        // Written back: refreshes take the range again.
        set_title(dst, len, 0x33);
        shadow.update(window, PAGE);
        assert!(shadow.host_bytes(dst, len).iter().all(|&b| b == 0x33));
    }

    /// The title writes a piece of title memory while it is copied back,
    /// between the copy-back's read and its write: its bytes stay (only
    /// the runs the GPU changed are written), and the page is not taken
    /// for current, so the next refresh brings the title's write.
    #[test]
    fn a_copy_back_keeps_title_writes_made_while_it_runs() {
        let test = "shadow::tests::a_copy_back_keeps_title_writes_made_while_it_runs";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(2 * PAGE);
        let shadow = find(window as u64, PAGE as u64).expect("a host copy");
        let page = window + PAGE;
        shadow.update(window, 2 * PAGE);
        assert!(!shadow.stale(page, page + PAGE));
        let gpu = submission();
        shadow.writing(gpu, page, PAGE);
        // The GPU writes 16 bytes; the title 16 others meanwhile.
        shadow.set_host(page, 16, 0xab);
        AFTER_READING.set(Some(Box::new(move |at| {
            if at == page {
                set_title(page + 0x100, 16, 0x5c);
            }
        })));
        let written = shadow.copy_back(gpu, page, PAGE);
        AFTER_READING.set(None);
        assert_eq!(written, 16, "only the GPU's bytes");
        assert!(title(page, 16).iter().all(|&b| b == 0xab));
        assert!(
            title(page + 0x100, 16).iter().all(|&b| b == 0x5c),
            "the title's write is kept"
        );
        assert!(
            shadow.stale(page, page + PAGE),
            "not current: the title wrote it"
        );
        shadow.update(page, PAGE);
        assert!(
            shadow
                .host_bytes(page + 0x100, 16)
                .iter()
                .all(|&b| b == 0x5c)
        );
        assert!(shadow.host_bytes(page, 16).iter().all(|&b| b == 0xab));
        // Without a title write meanwhile, the page is current after it.
        let gpu = submission();
        shadow.writing(gpu, page, PAGE);
        shadow.set_host(page + 0x200, 8, 0xcd);
        assert_eq!(shadow.copy_back(gpu, page, PAGE), 8);
        assert!(title(page + 0x200, 8).iter().all(|&b| b == 0xcd));
        assert!(
            !shadow.stale(page, page + PAGE),
            "current after our own write"
        );
    }

    /// W1: a range is copied once, then not again by the submissions after
    /// it (the rest of the frame) until the title writes it: neither its
    /// whole pages nor the pages it takes only part of (before W1 those
    /// were copied at every refresh). A write brings back the page written,
    /// and only it; a part of a page next to what the copy holds of it is
    /// copied, then held with it.
    #[test]
    fn a_range_is_copied_once_until_the_title_writes_it() {
        let test = "shadow::tests::a_range_is_copied_once_until_the_title_writes_it";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(3 * PAGE);
        let shadow = find(window as u64, (3 * PAGE) as u64).expect("a host copy");
        // From inside the first page to inside the third, as views are.
        let (from, len) = (window + 0x100, 2 * PAGE);
        set_title(window, 3 * PAGE, 0x11);
        assert_eq!(shadow.update(from, len), len, "copied first");
        assert_eq!(shadow.host_bytes(from, len), title(from, len));
        for submission in 0..3 {
            assert_eq!(
                shadow.update(from, len),
                0,
                "submission {submission} after it"
            );
        }
        assert_eq!(shadow.update(from + 0x200, 0x100), 0, "a part of it");
        assert!(!shadow.stale(from, from + len));
        // The title writes the third page: its part in the range is copied
        // again, the rest is not.
        set_title(window + 2 * PAGE + 0x10, 4, 0x22);
        assert!(shadow.stale(from, from + len));
        assert!(
            !shadow.stale(from, window + 2 * PAGE),
            "the pages not written"
        );
        assert_eq!(shadow.update(from, len), 0x100);
        assert_eq!(
            shadow.host_bytes(window + 2 * PAGE, 0x100),
            title(window + 2 * PAGE, 0x100)
        );
        assert_eq!(shadow.update(from, len), 0);
        // The rest of the first page, which touches what it holds.
        assert_eq!(shadow.update(window, 0x100), 0x100);
        assert_eq!(shadow.update(window, PAGE), 0, "the first page whole");
        assert_eq!(shadow.host_bytes(window, PAGE), title(window, PAGE));
        // A part of the third page apart from what it holds: copied, and
        // held instead.
        assert_eq!(shadow.update(window + 2 * PAGE + 0x400, 0x100), 0x100);
        assert_eq!(shadow.update(window + 2 * PAGE + 0x400, 0x100), 0);
        assert_eq!(shadow.update(window + 2 * PAGE, 0x100), 0x100);
    }

    /// A refresh around what shaders in flight write: one asking for the
    /// whole range (W1; before, one per gap around it), the pages written
    /// since copied but where the GPU work writes, which keeps the GPU's
    /// bytes; the copy-back puts them in title memory.
    #[test]
    fn a_refresh_leaves_what_shaders_in_flight_write() {
        let test = "shadow::tests::a_refresh_leaves_what_shaders_in_flight_write";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(3 * PAGE);
        let shadow = find(window as u64, (3 * PAGE) as u64).expect("a host copy");
        shadow.update(window, 3 * PAGE);
        let gpu = submission();
        shadow.writing(gpu, window + PAGE, PAGE);
        shadow.set_host(window + PAGE, PAGE, 0xc1);
        // The title writes the pages around it (where both write, the title's
        // bytes stay: `where_the_gpu_and_the_title_both_wrote_the_titles_stay`).
        set_title(window, PAGE, 0x33);
        set_title(window + 2 * PAGE, PAGE, 0x33);
        ASKED.set(0);
        assert_eq!(shadow.update(window, 3 * PAGE), 2 * PAGE);
        assert_eq!(
            ASKED.get(),
            if once() { 1 } else { 2 },
            "write-watch askings"
        );
        assert!(shadow.host_bytes(window, PAGE).iter().all(|&b| b == 0x33));
        assert!(
            shadow
                .host_bytes(window + PAGE, PAGE)
                .iter()
                .all(|&b| b == 0xc1)
        );
        assert!(
            shadow
                .host_bytes(window + 2 * PAGE, PAGE)
                .iter()
                .all(|&b| b == 0x33)
        );
        assert_eq!(shadow.copy_back(gpu, window + PAGE, PAGE), PAGE);
        assert!(title(window + PAGE, PAGE).iter().all(|&b| b == 0xc1));
        assert!(!shadow.stale(window, window + 3 * PAGE), "all current");
        assert_eq!(shadow.update(window, 3 * PAGE), 0);
    }

    /// Plain copies of title memory read what the kernel's copy read, with
    /// zeros where a page is decommitted or not readable, from any offset;
    /// a copy stops at the first byte it cannot read.
    #[test]
    fn plain_copies_read_as_the_kernel_did() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
        };
        let len = 4 * PAGE;
        // SAFETY: a fresh reservation of our own, never freed.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(base, 0);
        for i in 0..len {
            // SAFETY: the test's memory.
            unsafe { ((base + i) as *mut u8).write((i * 7 + 3) as u8) };
        }
        // The second page decommitted, the first 4 KiB of the third not
        // readable.
        unmap(base + PAGE, PAGE);
        let mut old = 0;
        // SAFETY: the test's memory.
        let protected =
            unsafe { VirtualProtect((base + 2 * PAGE) as *const _, 4096, PAGE_NOACCESS, &mut old) };
        assert_ne!(protected, 0);
        let (src, n) = (base + 0x40, len - 0x80);
        let (mut plain, mut kernel) = (vec![0xeeu8; n], vec![0xeeu8; n]);
        let zeros = read_title(plain.as_mut_ptr(), src, n);
        crate::host::copy_mapped(kernel.as_mut_ptr(), src as *const u8, n, false);
        assert!(plain == kernel, "as the kernel's copy");
        assert_eq!(zeros, PAGE + 4096);
        assert_eq!(&plain[..0x40], &title(src, 0x40)[..]);
        let mut buffer = vec![0u8; 2 * PAGE];
        // SAFETY: our buffer; title memory a fault leaves alone.
        let left = unsafe { try_copy(buffer.as_mut_ptr(), (base + 0x10) as *const u8, 2 * PAGE) };
        assert_eq!(left, PAGE + 0x10, "stopped at the decommitted page");
        assert!(!read_whole(base + PAGE - 0x10, &mut buffer[..0x20]));
        assert!(read_whole(base + 3 * PAGE, &mut buffer[..PAGE]));
        // Writes leave alone the pages they cannot write, and write what is
        // mapped past them.
        let ours = vec![0x5au8; 3 * PAGE];
        write_title(base + 0x20, ours.as_ptr(), 3 * PAGE);
        assert!(title(base + 0x20, PAGE - 0x20).iter().all(|&b| b == 0x5a));
        assert!(
            title(base + 2 * PAGE + 4096, PAGE - 4096)
                .iter()
                .all(|&b| b == 0x5a)
        );
    }

    /// The copy-back's scans by 8 bytes find what the byte loop found: the
    /// first byte that differs, and the first that does not.
    #[test]
    fn the_copy_backs_scans_find_what_the_byte_loop_found() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let len = (next() % 100) as usize;
            let a: Vec<u8> = (0..len).map(|_| (next() % 3) as u8).collect();
            let b: Vec<u8> = (0..len).map(|_| (next() % 3) as u8).collect();
            for at in 0..=len {
                let change = (at..len).find(|&i| a[i] != b[i]).unwrap_or(len);
                let same = (at..len).find(|&i| a[i] == b[i]).unwrap_or(len);
                assert_eq!(first_change(&a, &b, at), change, "{a:?} {b:?} from {at}");
                assert_eq!(first_same(&a, &b, at), same, "{a:?} {b:?} from {at}");
            }
        }
    }

    /// A copy-back reads a run of pieces the GPU changed with one copy and
    /// keeps the readable pieces of a run that crosses an unmapped page; a
    /// range shorter than a piece across a piece boundary puts back both
    /// sides (the fallback fix of Marvisak/VIctory#1124).
    #[test]
    fn a_copy_back_keeps_the_readable_pieces_of_a_run() {
        let test = "shadow::tests::a_copy_back_keeps_the_readable_pieces_of_a_run";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(4 * PAGE);
        let shadow = find(window as u64, (4 * PAGE) as u64).expect("a host copy");
        shadow.update(window, 4 * PAGE);
        let (short, long) = ((window + PAGE - 0x80, 0x100), (window + PAGE, 3 * PAGE));
        let gpu = submission();
        shadow.writing(gpu, short.0, short.1);
        shadow.writing(gpu, long.0, long.1);
        // The GPU writes across the first boundary, in the third page and
        // in the fourth; the title unmaps the third.
        shadow.set_host(window + PAGE - 0x40, 0x80, 0xa1);
        shadow.set_host(window + 2 * PAGE + 0x10, 0x10, 0xa2);
        shadow.set_host(window + 3 * PAGE + 0x20, 0x10, 0xa3);
        unmap(window + 2 * PAGE, PAGE);
        READS.set(0);
        assert_eq!(shadow.copy_back(gpu, short.0, short.1), 0x80);
        assert_eq!(READS.get(), 1, "both pieces with one copy");
        assert!(title(window + PAGE - 0x40, 0x80).iter().all(|&b| b == 0xa1));
        READS.set(0);
        // The second page has the GPU's bytes already: only the fourth's go.
        assert_eq!(shadow.copy_back(gpu, long.0, long.1), 0x10);
        assert_eq!(READS.get(), 2, "the run, then past the unmapped page");
        assert!(
            title(window + 3 * PAGE + 0x20, 0x10)
                .iter()
                .all(|&b| b == 0xa3)
        );
        assert!(!shadow.stale(window + PAGE, window + 2 * PAGE));
        assert!(!shadow.stale(window + 3 * PAGE, window + 4 * PAGE));
    }

    /// A run of pieces is read at once, then put back piece by piece: a
    /// title write into a later piece after the read keeps its bytes (only
    /// the GPU's are written; Marvisak/VIctory#1124 wrote whole pieces as
    /// read), and that page is not taken for current.
    #[test]
    fn a_copy_back_run_keeps_title_writes_made_after_its_read() {
        let test = "shadow::tests::a_copy_back_run_keeps_title_writes_made_after_its_read";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(2 * PAGE);
        let shadow = find(window as u64, (2 * PAGE) as u64).expect("a host copy");
        let second = window + PAGE;
        shadow.update(window, 2 * PAGE);
        let gpu = submission();
        shadow.writing(gpu, window, 2 * PAGE);
        shadow.set_host(window + 0x10, 8, 0xb1);
        shadow.set_host(second + 0x10, 8, 0xb2);
        READS.set(0);
        AFTER_READING.set(Some(Box::new(move |at| {
            if at == second {
                set_title(second + 0x100, 8, 0x5d);
            }
        })));
        let written = shadow.copy_back(gpu, window, 2 * PAGE);
        AFTER_READING.set(None);
        assert_eq!(written, 16, "only the GPU's bytes");
        assert_eq!(READS.get(), 1, "the run read at once");
        assert!(title(window + 0x10, 8).iter().all(|&b| b == 0xb1));
        assert!(title(second + 0x10, 8).iter().all(|&b| b == 0xb2));
        assert!(
            title(second + 0x100, 8).iter().all(|&b| b == 0x5d),
            "the title's write is kept"
        );
        assert!(!shadow.stale(window, second), "current after our write");
        assert!(
            shadow.stale(second, second + PAGE),
            "not current: the title wrote it"
        );
        shadow.update(window, 2 * PAGE);
        assert!(
            shadow
                .host_bytes(second + 0x100, 8)
                .iter()
                .all(|&b| b == 0x5d)
        );
    }

    /// The copy-back is a three-way merge (ported from simnJS/AnyPS5
    /// (65a1b01db4)): where the GPU and, since the snapshot, the title both
    /// changed bytes, the title's stay and the copy takes them; the GPU's
    /// bytes around them go back. Before, the GPU's went over the title's.
    #[test]
    fn where_the_gpu_and_the_title_both_wrote_the_titles_stay() {
        let test = "shadow::tests::where_the_gpu_and_the_title_both_wrote_the_titles_stay";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(PAGE);
        let shadow = find(window as u64, PAGE as u64).expect("a host copy");
        shadow.update(window, PAGE);
        let gpu = submission();
        shadow.writing(gpu, window, PAGE);
        // The GPU writes 32 bytes; the title 8 of them meanwhile.
        shadow.set_host(window + 0x100, 32, 0xa7);
        set_title(window + 0x108, 8, 0x5e);
        assert_eq!(
            shadow.copy_back(gpu, window, PAGE),
            24,
            "the GPU's bytes but the title's 8"
        );
        assert!(title(window + 0x100, 8).iter().all(|&b| b == 0xa7));
        assert!(
            title(window + 0x108, 8).iter().all(|&b| b == 0x5e),
            "the title's write stays"
        );
        assert!(title(window + 0x110, 16).iter().all(|&b| b == 0xa7));
        assert!(
            shadow
                .host_bytes(window + 0x108, 8)
                .iter()
                .all(|&b| b == 0x5e),
            "the copy takes it"
        );
        assert!(title(window, 0x100).iter().all(|&b| b == 0), "nothing else");
        assert!(!shadow.stale(window, window + PAGE), "current");
    }

    /// Two ranges of one submission that overlap (the world declares
    /// overlapping views of its 174 MB heap dozens of times) go back once
    /// per byte. The first copy-back takes the title's bytes into the copy
    /// where the GPU left it alone; the second must not take them for the
    /// GPU's. Before, the snapshot kept the bytes from before the
    /// submission, and the second copy-back wrote the title's older bytes
    /// over what it wrote between the two, and took the page for current.
    #[test]
    fn overlapping_ranges_of_a_submission_go_back_once() {
        let test = "shadow::tests::overlapping_ranges_of_a_submission_go_back_once";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(2 * PAGE);
        let shadow = find(window as u64, (2 * PAGE) as u64).expect("a host copy");
        shadow.update(window, 2 * PAGE);
        let (small, big) = ((window, 0x1000), (window, 2 * PAGE));
        let gpu = submission();
        shadow.writing(gpu, small.0, small.1);
        shadow.writing(gpu, big.0, big.1);
        // The GPU writes in both ranges, then in the second only, in both
        // pages; the title writes A meanwhile, in the first range.
        shadow.set_host(window + 0x10, 16, 0xab);
        shadow.set_host(window + 0x2000, 16, 0xac);
        shadow.set_host(window + PAGE + 0x10, 16, 0xad);
        let x = window + 0x100;
        set_title(x, 8, 0xa0);
        assert_eq!(shadow.copy_back(gpu, small.0, small.1), 16);
        assert!(title(x, 8).iter().all(|&b| b == 0xa0));
        // Between the two copy-backs, the title writes B there.
        set_title(x, 8, 0xb0);
        assert_eq!(
            shadow.copy_back(gpu, big.0, big.1),
            32,
            "the GPU's bytes the first left, and only them"
        );
        assert!(
            title(x, 8).iter().all(|&b| b == 0xb0),
            "the title's newer write stays"
        );
        assert!(title(window + 0x10, 16).iter().all(|&b| b == 0xab));
        assert!(title(window + 0x2000, 16).iter().all(|&b| b == 0xac));
        assert!(title(window + PAGE + 0x10, 16).iter().all(|&b| b == 0xad));
        // The second copy-back took B into the copy: both pages current.
        assert!(shadow.host_bytes(x, 8).iter().all(|&b| b == 0xb0));
        assert!(!shadow.stale(window, window + 2 * PAGE));
        assert_eq!(shadow.update(window, 2 * PAGE), 0);
    }

    /// A title write into a page GPU work in flight writes, which the GPU
    /// leaves alone. The refresh around the busy range asks the write watch
    /// for it too (W1), which clears the kernel's mark, and does not copy
    /// it; the copy-back finds nothing the GPU changed there and leaves it
    /// too. Only the journal keeps the write, and the next refresh copies
    /// the page. (Marvisak/VIctory#1322 found such writes never copied in
    /// its tree.)
    #[test]
    fn a_title_write_under_a_busy_range_reaches_the_copy_later() {
        let test = "shadow::tests::a_title_write_under_a_busy_range_reaches_the_copy_later";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(3 * PAGE);
        let shadow = find(window as u64, (3 * PAGE) as u64).expect("a host copy");
        let page = window + PAGE;
        shadow.update(window, 3 * PAGE);
        let gpu = submission();
        // Shaders write the first two pages; the GPU changes the first.
        shadow.writing(gpu, window, 2 * PAGE);
        shadow.set_host(window + 0x10, 16, 0xc2);
        set_title(page + 0x40, 8, 0x44);
        set_title(window + 2 * PAGE, 8, 0x45);
        // A refresh meanwhile copies the third page only.
        assert_eq!(shadow.update(window, 3 * PAGE), PAGE);
        assert!(shadow.host_bytes(page + 0x40, 8).iter().all(|&b| b == 0));
        if once() {
            assert!(!kernel_marked(page), "asked for the busy page too");
        }
        assert_eq!(shadow.copy_back(gpu, window, 2 * PAGE), 16);
        assert!(title(window + 0x10, 16).iter().all(|&b| b == 0xc2));
        assert!(
            title(page + 0x40, 8).iter().all(|&b| b == 0x44),
            "the title's write stays"
        );
        assert!(!shadow.stale(window, page), "current after our write");
        assert!(shadow.stale(page, page + PAGE), "the journal keeps it");
        assert_eq!(shadow.update(window, 3 * PAGE), PAGE);
        assert!(shadow.host_bytes(page + 0x40, 8).iter().all(|&b| b == 0x44));
        assert_eq!(shadow.update(window, 3 * PAGE), 0);
    }

    /// A plain copy that stops short of the byte that faulted (fast strings
    /// may: the registers then say less was copied than was) is made again
    /// up to the faulting page. What comes before that page is read, not
    /// zeroed, and a copy-back's read keeps the piece before an unmapped
    /// one. Before, the bytes from where the copy stopped to the faulting
    /// page read as zeros, and that piece was left out of the copy-back.
    #[test]
    fn a_copy_that_stops_short_of_its_fault_is_made_again_up_to_it() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
        };
        const LAG_BYTES: usize = 0x40;
        let len = 4 * PAGE;
        // SAFETY: a fresh reservation of our own, never freed.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(base, 0);
        for i in 0..len {
            // SAFETY: the test's memory.
            unsafe { ((base + i) as *mut u8).write((i * 13 + 5) as u8) };
        }
        unmap(base + 2 * PAGE, PAGE);
        let (mut ours, mut kernel) = (vec![0xeeu8; len], vec![0xeeu8; len]);
        LAG.set(LAG_BYTES);
        let zeros = read_title(ours.as_mut_ptr(), base, len);
        assert_eq!(LAG.get(), 0, "the copy faulted");
        crate::host::copy_mapped(kernel.as_mut_ptr(), base as *const u8, len, false);
        assert!(ours == kernel, "as the kernel's copy");
        assert_eq!(zeros, PAGE);
        // A copy-back's read (with plain copies).
        if plain() {
            LAG.set(LAG_BYTES);
            let mut read = vec![0u8; len];
            assert_eq!(
                read_pieces(base, &mut read),
                [(0, 2 * PAGE), (3 * PAGE, len)]
            );
            assert_eq!(LAG.get(), 0, "the copy faulted");
            assert_eq!(&read[..2 * PAGE], &title(base, 2 * PAGE)[..]);
        }
        LAG.set(LAG_BYTES);
        let (src, n) = (base + PAGE + 0x10, 2 * PAGE);
        let mut buffer = vec![0u8; n];
        // SAFETY: our buffer; title memory a fault leaves alone.
        let left = unsafe { try_copy(buffer.as_mut_ptr(), src as *const u8, n) };
        assert_eq!(left, PAGE + 0x10, "from the faulting page");
        assert_eq!(&buffer[..n - left], &title(src, n - left)[..]);
        LAG.set(0);
    }

    /// A store of the copy into a block the guard made read-only goes on,
    /// whichever vectored handler the system calls first. With the guard's
    /// registered first (the first asking for view memory before the first
    /// plain copy), the copy's is called first, and gives the store to the
    /// guard. Before, it ended the copy there, and the copy-back fell back
    /// to the kernel's write, which fails on a block guarded again
    /// meanwhile.
    #[test]
    fn a_store_into_a_guarded_block_goes_on_whichever_handler_is_first() {
        let test = "shadow::tests::a_store_into_a_guarded_block_goes_on_whichever_handler_is_first";
        alone(test, || {
            let view = view(2 * PAGE);
            // An asking guards the blocks (and registers the guard's
            // handler), then a plain copy registers the copy's.
            crate::watch::writes(view, view + 2 * PAGE, true).expect("view memory");
            assert!(!writable(view) && !writable(view + PAGE), "guarded");
            let mut read = [0u8; 16];
            // SAFETY: our buffer; the test's view.
            let left = unsafe { try_copy(read.as_mut_ptr(), view as *const u8, 16) };
            assert_eq!(left, 0);
            let ours = [0x5au8; 0x100];
            // SAFETY: the test's view; a fault ends the copy.
            let left = unsafe { try_copy((view + PAGE + 0x10) as *mut u8, ours.as_ptr(), 0x100) };
            assert_eq!(left, 0, "the store went on");
            assert!(title(view + PAGE + 0x10, 0x100).iter().all(|&b| b == 0x5a));
            assert!(writable(view + PAGE), "the guard opened the block");
            assert!(!writable(view), "and only it");
            // Noted written, as the guard's handler notes a write.
            let (now, written) = crate::watch::writes(view, view + 2 * PAGE, true).unwrap();
            assert!(written.contains(&(view / PAGE + 1, now)));
            assert!(!written.contains(&(view / PAGE, now)));
        });
    }

    /// W1 on view memory, which the guard watches (its blocks read-only
    /// until written) instead of the kernel: a range is copied once, then
    /// not again until the title writes it, neither its whole pages nor the
    /// pages it takes part of, and a write brings back the page written. A
    /// refresh around what shaders in flight write leaves that alone with
    /// one asking; the copy-back puts the GPU's bytes in title memory, keeps
    /// the title's write next to them and makes the page current.
    #[test]
    fn w1_on_guarded_memory() {
        let test = "shadow::tests::w1_on_guarded_memory";
        alone(test, || {
            if crate::testgpu::device(test).is_none() {
                return;
            }
            // A window of a view's own: one loose host copy.
            let mapped = view(2 * WINDOW as usize);
            let window = mapped.next_multiple_of(WINDOW as usize);
            let shadow = find(window as u64, (3 * PAGE) as u64).expect("a host copy");
            let (from, len) = (window + 0x100, 2 * PAGE);
            set_title(window, 3 * PAGE, 0x11);
            assert_eq!(shadow.update(from, len), len, "copied first");
            assert_eq!(shadow.host_bytes(from, len), title(from, len));
            assert!(!writable(window), "guarded");
            for submission in 0..3 {
                assert_eq!(
                    shadow.update(from, len),
                    0,
                    "submission {submission} after it"
                );
            }
            assert!(!shadow.stale(from, from + len));
            // The title writes the third page: the guard takes the fault.
            set_title(window + 2 * PAGE + 0x10, 4, 0x22);
            assert!(shadow.stale(from, from + len));
            assert!(
                !shadow.stale(from, window + 2 * PAGE),
                "the pages not written"
            );
            assert_eq!(shadow.update(from, len), 0x100);
            assert_eq!(
                shadow.host_bytes(window + 2 * PAGE, 0x100),
                title(window + 2 * PAGE, 0x100)
            );
            assert_eq!(shadow.update(from, len), 0);
            // Shaders in flight write the second page; the title writes the
            // first and, next to what the GPU changes, the second.
            let gpu = submission();
            shadow.writing(gpu, window + PAGE, PAGE);
            shadow.set_host(window + PAGE + 0x40, 16, 0xc3);
            set_title(window + 0x200, 8, 0x33);
            set_title(window + PAGE + 0x80, 8, 0x34);
            ASKED.set(0);
            assert_eq!(
                shadow.update(from, len),
                PAGE - 0x100,
                "the first page's part, not the busy page"
            );
            assert_eq!(ASKED.get(), 1, "one asking");
            assert!(
                shadow
                    .host_bytes(window + 0x200, 8)
                    .iter()
                    .all(|&b| b == 0x33)
            );
            assert_eq!(shadow.copy_back(gpu, window + PAGE, PAGE), 16);
            assert!(title(window + PAGE + 0x40, 16).iter().all(|&b| b == 0xc3));
            assert!(
                title(window + PAGE + 0x80, 8).iter().all(|&b| b == 0x34),
                "the title's write stays"
            );
            assert_eq!(
                shadow.host_bytes(window + PAGE, PAGE),
                title(window + PAGE, PAGE)
            );
            assert!(!shadow.stale(window + PAGE, window + 2 * PAGE), "current");
            assert_eq!(shadow.update(from, len), 0);
        });
    }

    /// The census on view memory (`XTP_COPYBACK_CENSUS=1`): a copy-back
    /// guards every 4 KiB page of its range; the first access to a page is
    /// counted once (a read, a write, XTP's own copy) and sees the GPU's
    /// bytes; a write to a read-only block leaves the rest of it uncounted;
    /// the next copy-back over the range counts what no access reached as
    /// written again by the GPU first. XTP's own copy goes on through the
    /// guards (before, a guard fault ended it: zeros in the copy). Without
    /// a census nothing is guarded.
    #[test]
    fn the_census_counts_the_first_touch_of_what_came_back() {
        use census::K;
        let test = "shadow::tests::the_census_counts_the_first_touch_of_what_came_back";
        alone(test, || {
            if crate::testgpu::device(test).is_none() {
                return;
            }
            let mapped = view(2 * WINDOW as usize);
            let window = mapped.next_multiple_of(WINDOW as usize);
            let shadow = find(window as u64, (3 * PAGE) as u64).expect("a host copy");
            let len = 3 * PAGE;
            shadow.update(window, len);
            // No census: nothing counted, nothing guarded.
            let gpu = submission();
            shadow.writing(gpu, window, len);
            shadow.set_host(window + 0x40, 4, 0x99);
            assert_eq!(shadow.copy_back(gpu, window, len), 4);
            assert_eq!(counted(), [0; 15]);
            assert!(title(window + 0x40, 4).iter().all(|&b| b == 0x99));
            assert_eq!(counted(), [0; 15], "no guard fault");
            CENSUS.store(2, Ordering::SeqCst);
            // The GPU writes the first page of the first block and the third
            // of the third.
            let gpu = submission();
            shadow.writing(gpu, window, len);
            shadow.set_host(window + 0x10, 16, 0xab);
            shadow.set_host(window + 2 * PAGE + 0x2010, 8, 0xcd);
            assert_eq!(shadow.copy_back(gpu, window, len), 24);
            let c = counted();
            assert_eq!(at(&c, K::Armed), len as u64, "every page guarded");
            assert_eq!(at(&c, K::Unarmed), 0);
            assert_eq!(at(&c, K::Gpu), 24);
            assert_eq!(
                at(&c, K::Faults),
                0,
                "the copy-back's own access is not a touch"
            );
            // Reads see the GPU's bytes; a page faults once.
            assert!(title(window + 0x10, 16).iter().all(|&b| b == 0xab));
            assert!(title(window + 0x10, 16).iter().all(|&b| b == 0xab));
            let _ = title(window + KERNEL_PAGE, 4);
            assert!(
                title(window + 2 * PAGE + 0x2010, 8)
                    .iter()
                    .all(|&b| b == 0xcd)
            );
            // A write to the third block: the guard of view memory opens it
            // whole.
            set_title(window + 2 * PAGE + 0x5000, 4, 0x11);
            let c = counted();
            assert_eq!(at(&c, K::Read), 3 * KERNEL_PAGE as u64);
            assert_eq!(at(&c, K::Write), KERNEL_PAGE as u64);
            assert_eq!(at(&c, K::AfterWrite), 14 * KERNEL_PAGE as u64);
            assert_eq!(at(&c, K::Blocks), 2 * PAGE as u64);
            assert_eq!(at(&c, K::GpuTouched), 24);
            assert_eq!(at(&c, K::Faults), 4);
            // The next submission writes the range again, the GPU changing
            // nothing: the first block's 14 untouched pages and the second
            // block were written again first.
            let next = submission();
            shadow.writing(next, window, len);
            assert_eq!(shadow.copy_back(next, window, len), 0);
            let c = counted();
            assert_eq!(at(&c, K::Again), 30 * KERNEL_PAGE as u64);
            assert_eq!(at(&c, K::GpuAgain), 0);
            assert_eq!(at(&c, K::Unseen), 0);
            assert_eq!(at(&c, K::Armed), len as u64);
            // XTP's own copy through the guarded second block reads it all.
            let mut read = vec![0xeeu8; PAGE];
            assert_eq!(
                read_title(read.as_mut_ptr(), window + PAGE, PAGE),
                0,
                "no zeros"
            );
            assert_eq!(read, title(window + PAGE, PAGE));
            let c = counted();
            assert_eq!(at(&c, K::Ours), PAGE as u64);
            assert_eq!(at(&c, K::Read), 0, "the test's own reads came after");
            CENSUS.store(0, Ordering::SeqCst);
        });
    }

    /// The census on write-watched memory: left alone with
    /// `XTP_COPYBACK_CENSUS=1` (a kernel write into it would fail on the
    /// guard), guarded with `all`. A write there opens one page only, and a
    /// write the guard does not see (the kernel's) counts as written unseen
    /// at the next copy-back.
    #[test]
    fn the_census_guards_write_watched_memory_with_all_only() {
        use census::K;
        let test = "shadow::tests::the_census_guards_write_watched_memory_with_all_only";
        alone(test, || {
            if crate::testgpu::device(test).is_none() {
                return;
            }
            let len = 2 * PAGE;
            let window = watched(len);
            let shadow = find(window as u64, len as u64).expect("a host copy");
            shadow.update(window, len);
            CENSUS.store(2, Ordering::SeqCst);
            let gpu = submission();
            shadow.writing(gpu, window, len);
            shadow.set_host(window + 0x10, 8, 0xa1);
            assert_eq!(shadow.copy_back(gpu, window, len), 8);
            let c = counted();
            assert_eq!(at(&c, K::Armed), 0);
            assert_eq!(at(&c, K::Unarmed), len as u64);
            assert!(title(window + 0x10, 8).iter().all(|&b| b == 0xa1));
            assert_eq!(at(&counted(), K::Faults), 0, "not guarded");
            CENSUS.store(3, Ordering::SeqCst);
            let gpu = submission();
            shadow.writing(gpu, window, len);
            shadow.set_host(window + PAGE + 0x3000, 8, 0xa2);
            assert_eq!(shadow.copy_back(gpu, window, len), 8);
            assert_eq!(at(&counted(), K::Armed), len as u64);
            // A title write opens its page only; a read of another page of
            // the block still faults.
            set_title(window + PAGE + 0x3004, 2, 0x5a);
            let _ = title(window + PAGE, 4);
            let c = counted();
            assert_eq!(at(&c, K::Write), KERNEL_PAGE as u64);
            assert_eq!(at(&c, K::Read), KERNEL_PAGE as u64);
            assert_eq!(at(&c, K::AfterWrite), 0);
            assert_eq!(at(&c, K::GpuTouched), 8);
            // A kernel write into the first block (its guard fails it once,
            // then the system has taken the guard off).
            use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            let ours = [0x77u8; 4];
            let mut done = 0;
            for _ in 0..2 {
                // SAFETY: the test's memory, our own buffer.
                unsafe {
                    WriteProcessMemory(
                        GetCurrentProcess(),
                        (window + 0x100) as *const _,
                        ours.as_ptr().cast(),
                        ours.len(),
                        &mut done,
                    )
                };
            }
            assert_eq!(title(window + 0x100, 4), ours, "written the second time");
            let c = counted();
            assert_eq!(at(&c, K::Faults), 0, "the kernel's access does not fault");
            let next = submission();
            shadow.writing(next, window, len);
            assert_eq!(shadow.copy_back(next, window, len), 0);
            let c = counted();
            assert_eq!(
                at(&c, K::Unseen),
                16 * KERNEL_PAGE as u64,
                "the first block"
            );
            assert_eq!(
                at(&c, K::AfterWrite),
                14 * KERNEL_PAGE as u64,
                "the second's others: its write shows in the journal too"
            );
            assert_eq!(at(&c, K::Again), 0);
            CENSUS.store(0, Ordering::SeqCst);
        });
    }

    /// Benchmark (`--ignored --nocapture`; `XTP_SHADOW_ONCE=0` times the
    /// path before W1, `XTP_COPY_MODE=kernel` W1 with the kernel's
    /// copies): a world-like frame of 39 submissions on one 64 MiB host
    /// copy, as in world-frame session C (copies 17.9 MB, copy-back 1.1 MB
    /// a frame). Each submission refreshes the range as its views merge
    /// (from and to inside pages) and 30 constant views of 600 bytes on
    /// their own (as after steps that write title memory), while the
    /// previous one's shaders write 8 MiB of it, after the title wrote 8
    /// scattered pages and a ring of 200 constant blocks of 600 bytes; then
    /// the previous one is copied back, its shaders having written 200 runs
    /// of 16 bytes over the 8 MiB and a 16 KiB block.
    #[test]
    #[ignore = "benchmark"]
    fn world_frame_copies_cost() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
        };
        let test = "shadow::tests::world_frame_copies_cost";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        const LEN: usize = 64 << 20;
        const SUBMISSIONS: usize = 39;
        const MEM_WRITE_WATCH: u32 = 0x0020_0000;
        let window = WINDOW as usize;
        // SAFETY: a fresh reservation of our own, never freed.
        let reserved = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                LEN + window,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(reserved, 0);
        let base = reserved.next_multiple_of(window);
        set_title(base, LEN, 0);
        let shadow = find(base as u64, LEN as u64).expect("a host copy");
        shadow.update(base, LEN);
        let busy = (base + (16 << 20), 8 << 20);
        let pages = LEN / PAGE;
        let mut ring = 0;
        let (mut refresh, mut back) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        let (mut copied, mut written) = (0, 0);
        let mut previous = None;
        for s in 0..SUBMISSIONS {
            let byte = (s % 250) as u8 + 1;
            if s == 0 {
                // Once, the pages the merged range takes only part of.
                set_title(base + 0x200, 16, byte);
                set_title(base + LEN - 0x400, 16, byte);
            }
            for p in 0..8 {
                set_title(
                    base + (s * 8 + p) * 7_919 % pages * PAGE + 0x40,
                    0x100,
                    byte,
                );
            }
            for _ in 0..200 {
                set_title(base + (48 << 20) + ring, 600, byte);
                ring = (ring + 600) % (8 << 20);
            }
            let started = std::time::Instant::now();
            copied += shadow.update(base + 0x100, LEN - 0x200);
            for v in 0..30 {
                copied += shadow.update(base + (48 << 20) + v * 262_144 + 0x40, 600);
            }
            refresh += started.elapsed();
            if let Some(gpu) = previous.take() {
                let started = std::time::Instant::now();
                written += shadow.copy_back(gpu, busy.0, busy.1);
                back += started.elapsed();
            }
            let gpu = submission();
            shadow.writing(gpu, busy.0, busy.1);
            for r in 0..200 {
                shadow.set_host(busy.0 + r * 40_960 + s % 64, 16, byte);
            }
            shadow.set_host(busy.0 + (4 << 20), 16 << 10, byte);
            previous = Some(gpu);
        }
        if let Some(gpu) = previous {
            let started = std::time::Instant::now();
            written += shadow.copy_back(gpu, busy.0, busy.1);
            back += started.elapsed();
        }
        println!(
            "BENCH once={} plain={}: per frame of {SUBMISSIONS} submissions, refreshes {:.2} ms \
             ({:.1} MB copied), copy-back {:.2} ms ({:.2} MB written)",
            once(),
            plain(),
            refresh.as_secs_f64() * 1e3,
            copied as f64 / 1e6,
            back.as_secs_f64() * 1e3,
            written as f64 / 1e6
        );
    }

    /// A copy is made without the buffer map's lock held: while one thread
    /// waits to make a buffer's copy (a heap, VidMm), another finds the
    /// copies of other buffers. Before, every recording thread queued on
    /// that lock behind the one waiting (frame-prof's session A).
    #[test]
    fn a_copy_is_made_without_the_buffer_lock_held() {
        let test = "shadow::tests::a_copy_is_made_without_the_buffer_lock_held";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let window = watched(2 << 20);
        // Two title buffers (the resources are only keys here), the first
        // of a size no other test asks for.
        let (slow, quick) = ((window, 0x7_0123u64), (window + (1 << 20), 0x100u64));
        register(0x5eed_0001, slow.0 as u64, slow.1);
        register(0x5eed_0002, quick.0 as u64, quick.1);
        let (release, held) = std::sync::mpsc::channel();
        *crate::host::buffers::HOLD.lock().unwrap() =
            Some((slow.1.next_multiple_of(1 << 16), held));
        let waiting = std::thread::spawn(move || find(slow.0 as u64, 16).is_some());
        // The first is being made; the second is found meanwhile.
        while crate::host::buffers::HOLD.lock().unwrap().is_some() {
            std::thread::yield_now();
        }
        let (found, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = found.send(find(quick.0 as u64, 16).is_some());
        });
        let quick_found = done.recv_timeout(std::time::Duration::from_secs(10));
        release.send(()).unwrap();
        assert_eq!(quick_found, Ok(true), "not held up by the other's making");
        assert!(waiting.join().unwrap());
        let first = find(slow.0 as u64, 16).unwrap();
        assert!(
            Arc::ptr_eq(&first, &find(slow.0 as u64 + 8, 8).unwrap()),
            "made once"
        );
        unregister(0x5eed_0001, slow.0 as u64);
        unregister(0x5eed_0002, quick.0 as u64);
    }
}
