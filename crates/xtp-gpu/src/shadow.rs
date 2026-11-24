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
//! the bytes the GPU changed, piece by piece. `XTP_SHADOW_ONCE=0` brings
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
    /// covers it).
    parts: Vec<(usize, Vec<u8>)>,
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
    fn bytes(&self, from: usize, to: usize) -> Option<&[u8]> {
        let i = self.parts.partition_point(|(at, b)| at + b.len() < to);
        let (at, bytes) = self.parts.get(i)?;
        (*at <= from && to <= at + bytes.len()).then(|| &bytes[from - at..to - at])
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
        }
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
    /// Returns the bytes written.
    pub(crate) fn copy_back(&self, submission: u64, address: usize, len: usize) -> usize {
        let mut snapshots = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let Some(before) = snapshots
            .iter()
            .find(|b| b.submission == submission)
            .and_then(|b| b.bytes(from, to))
            && from < to
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
            let differs = |a: usize, b: usize| host(a, b) != &before[a - from..b - from];
            let mut synced = Vec::new();
            let mut at = from;
            while at < to {
                let mut end = end_of(at);
                if differs(at, end) {
                    // The pieces after it the GPU changed too, up to a run
                    // (one piece at a time as before W1).
                    while once() && end < to && end - at < RUN && differs(end, end_of(end)) {
                        end = end_of(end);
                    }
                    let old = &before[at - from..end - from];
                    written += self.put_back(at, end, old, &mut synced);
                }
                at = end;
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::BackCompared, (to - from) as u64);
            // Pages now equal to title memory are current in the copy, so
            // our own write is not taken for the title's; others keep what
            // they had (a title write there shows at the next refresh).
            let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
            synced.retain(|p| (first..last).contains(p));
            self.mark_current(&synced);
        }
        // Every step of the submission that declared the range is done with
        // it, and the submission's snapshot with its last range.
        let mut busy = self.busy();
        busy.retain(|b| {
            !(b.shaders && b.submission == submission && (b.from, b.to) == (address, address + len))
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
        let written = self.write_copied(address, len);
        let end = address + len;
        self.busy()
            .retain(|b| b.shaders || b.submission != submission || b.from < address || b.to > end);
        written
    }

    fn write_copied(&self, address: usize, len: usize) -> usize {
        let (from, to) = self.inside(address, len);
        if from >= to {
            return 0;
        }
        let start = self.start as usize;
        crate::watch::writing(start + from, to - from);
        // SAFETY: inside the mapping; the submitting thread leaves the
        // range alone until this submission has finished.
        let host = unsafe { self.data.add(from) };
        write_title(start + from, host, to - from);
        // Whole pages written are now as the copy has them, unless the
        // title wrote them meanwhile: our own write is not taken for the
        // title's.
        let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
        self.mark_current(&(first..last).collect::<Vec<_>>());
        to - from
    }

    /// [`Shadow::copy_back`] of `[from, to)` (offsets in the copy), a run of
    /// pieces the GPU changed (`old`: their bytes before the submission):
    /// title memory read with one copy, where it can all be read (else the
    /// pieces that can: [`read_pieces`]), then piece by piece the GPU's
    /// bytes written into title memory where it lacks them, the title's
    /// taken into the copy elsewhere. Pieces that cannot be read are left
    /// alone; those read go into `synced` (page numbers). Returns the bytes
    /// written.
    ///
    /// Unlike Marvisak/VIctory#1124, which writes back whole runs of pieces
    /// with one kernel call, only the bytes the GPU changed are written
    /// (with plain stores, which cost no call): the title writes these
    /// pages while this runs, and a piece written whole puts back the
    /// title's bytes as read before over its writes since.
    fn put_back(&self, from: usize, to: usize, old: &[u8], synced: &mut Vec<usize>) -> usize {
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
                    let (old, title) = (&old[piece..end], &title[piece..end]);
                    written += self.write_gpu_runs(at + piece, host, old, title);
                    synced.push((at + piece) / PAGE);
                    piece = end;
                }
            }
            written
        })
    }

    /// Writes into title memory at `at` the runs of bytes the GPU changed
    /// in a piece of the copy (`host`; `old`, its bytes before the
    /// submission; `title`, title memory as just read) where they differ
    /// from the title's; the copy takes the title's bytes everywhere else.
    /// Returns the bytes written.
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
            if host[run..i] != title[run..i] {
                if !opened {
                    // The piece's guard, once.
                    crate::watch::writing(at, host.len());
                    opened = true;
                }
                write_title(at + run, host[run..].as_ptr(), i - run);
                written += i - run;
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::BackRuns, 1);
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

/// Copies `len` bytes from `src` to `dst` with plain loads and stores;
/// returns how many were left when a page of either could not be accessed
/// (0: all of them copied). The kernel's copy reports such a page instead
/// of faulting, but runs at 1 GB/s or less, and title memory can be
/// unmapped or decommitted under a copy at any time (by the title's other
/// threads): here a vectored handler turns a fault at the copy's `rep
/// movsb` into its end. A string instruction stops at the faulting byte
/// with its registers saying where; the stores it made past it (if any)
/// are only what the copy would have stored.
///
/// # Safety
///
/// `dst` and `src` are `len` bytes that do not overlap, of which every page
/// that is accessible may be read (`src`) and written (`dst`): title memory,
/// or memory of ours no reference points to meanwhile.
unsafe fn try_copy(dst: *mut u8, src: *const u8, len: usize) -> usize {
    use windows_sys::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, EXCEPTION_IN_PAGE_ERROR, STATUS_GUARD_PAGE_VIOLATION,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn stopped(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let (record, context) = unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
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
        context.Rip = &raw const xtp_gpu_try_copy_stopped as u64;
        EXCEPTION_CONTINUE_EXECUTION
    }
    static HANDLER: std::sync::Once = std::sync::Once::new();
    // First, ahead of the crash reporter's: it looks at nothing but faults
    // at the copy's instruction. (The write guard's handler may come
    // first for a store into a guarded block: it opens the block and the
    // copy goes on.)
    // SAFETY: a handler that only moves past the copy's instruction.
    HANDLER.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(stopped));
    });
    if len == 0 {
        return 0;
    }
    // SAFETY: as the caller vouches; a fault ends the copy.
    unsafe { xtp_gpu_try_copy(dst, src, len) }
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
        let n = unreadable_at(src + at).min(left);
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
        let past = stop + unreadable_at(src + stop).min(len - stop);
        at = if past < len && !(src + past).is_multiple_of(PIECE) {
            piece_end(past)
        } else {
            past
        };
    }
    parts
}

/// How many bytes cannot be read from `at`, where a read faulted: to the
/// end of its region if that is not committed or not readable, else (a
/// page that changed since, or failed otherwise) to the end of its 4 KiB
/// page. One query per unreadable region: the kernel's copy tried each
/// 4 KiB page of it.
fn unreadable_at(at: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    const KERNEL_PAGE: usize = 4096;
    let page_end = KERNEL_PAGE - at % KERNEL_PAGE;
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    let readable = info.State == MEM_COMMIT && info.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0;
    if n == 0 || readable {
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
/// guarded blocks the caller opened ([`crate::watch::writing`]): plain
/// stores where it is mapped and writable, the kernel's copy from the
/// first page that is not, as before W1 (it leaves alone the pages it
/// cannot write; a block a refresh guarded again meanwhile is opened by
/// the guard's handler, or here before the kernel writes it).
fn write_title(dst: usize, src: *const u8, len: usize) {
    if !plain() {
        crate::host::write_mapped(dst as *mut u8, src, len);
        return;
    }
    // SAFETY: title memory, as the caller vouches; a fault ends it.
    let left = unsafe { try_copy(dst as *mut u8, src, len) };
    if left > 0 {
        let done = len - left;
        crate::watch::writing(dst + done, left);
        crate::host::write_mapped((dst + done) as *mut u8, src.wrapping_add(done), left);
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

/// Titles also use memory outside any buffer object as GPU memory (Grand
/// Theft Auto VI's interface vertices, from its own allocator). Such memory
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
        assert_eq!(before.bytes(14, 18), Some(&[1, 1, 2, 2][..]));
        assert_eq!(before.bytes(40, 41), None, "past what it took");
        assert_eq!(before.bytes(47, 49), None);
        assert_eq!(before.bytes(48, 50), Some(&[3, 3][..]));
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
    /// whole range, the pages written since copied but where the GPU work
    /// writes, which keeps the GPU's bytes; the copy-back puts them in
    /// title memory.
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
        set_title(window, 3 * PAGE, 0x33);
        assert_eq!(shadow.update(window, 3 * PAGE), 2 * PAGE);
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
        // Writes stop at the page they cannot write; past it, the kernel's
        // copy writes what is mapped.
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
