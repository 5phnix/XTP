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
    /// Per page of title memory from `start`'s on: the write-watch asking
    /// after which it was last copied (0: never).
    copied: Mutex<Vec<u64>>,
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
            copied: Mutex::new(vec![0; pages as usize]),
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
            // By 64 KiB piece: the copy compared with its bytes before the
            // submission, title memory read only where the GPU wrote.
            const PIECE: usize = 64 << 10;
            let mut title = vec![0u8; PIECE];
            let mut synced = Vec::new();
            let mut at = from;
            while at < to {
                let n = (PIECE - (start + at) % PIECE).min(to - at);
                // SAFETY: inside the mapping; the submitting thread leaves
                // the range alone until this submission has finished.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                let old = &before[at - from..at - from + n];
                let title = &mut title[..n];
                if host != old && crate::host::read_into((start + at) as *const u8, title) {
                    #[cfg(test)]
                    tests::after_reading(start + at);
                    written += self.write_gpu_runs(start + at, host, old, title);
                    synced.push((start + at) / PAGE);
                }
                at += n;
            }
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
        crate::host::write_mapped((start + from) as *mut u8, host, to - from);
        // Whole pages written are now as the copy has them, unless the
        // title wrote them meanwhile: our own write is not taken for the
        // title's.
        let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
        self.mark_current(&(first..last).collect::<Vec<_>>());
        to - from
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
            if host[i] == old[i] {
                host[i] = title[i];
                i += 1;
                continue;
            }
            let run = i;
            while i < host.len() && host[i] != old[i] {
                i += 1;
            }
            if host[run..i] != title[run..i] {
                if !opened {
                    // The piece's guard, once.
                    crate::watch::writing(at, host.len());
                    opened = true;
                }
                crate::host::write_mapped((at + run) as *mut u8, host[run..].as_ptr(), i - run);
                written += i - run;
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
                crate::host::read_into((page * PAGE) as *const u8, &mut title) && *host == *title
            })
            .collect();
        let base = start / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        for page in current {
            copied[page - base] = now;
        }
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Returns the bytes copied.
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
        if !busy.is_empty() {
            // Around ranges GPU work is writing: the gaps, each as below
            // (in the world most submissions have such ranges; copying the
            // gaps whole cost ~1 s per frame).
            busy.sort_unstable();
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
    /// copy last took it (always, for memory that is neither write-watched
    /// nor guarded). Asks the write journal, which keeps the answer for the
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
            let at = copied[page - base];
            let write = written
                .binary_search_by_key(&page, |&(p, _)| p)
                .map_or(0, |i| written[i].1);
            at == 0 || write > at
        })
    }

    /// [`Shadow::update`] of `[from, to)` (inside the buffer) where no
    /// shader writes.
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
        self.copy(from, to);
        to - from
    }

    /// Copies the pages of `[from, to)` written after their last copy
    /// (`written`: last writes by page, in page order; `now`: this
    /// asking), whole pages within the buffer.
    fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let base = self.start as usize / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let mut writes = written.iter().peekable();
        let (mut bytes, mut run) = (0, None);
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let at = &mut copied[page - base];
            if *at == 0 || write > *at {
                // A page only partly in the range is copied in part: it
                // stays stale for the rest.
                let (start, end) = (self.start as usize, (self.start + self.len) as usize);
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    *at = now;
                }
                run.get_or_insert(page);
            } else if let Some(first) = run.take() {
                bytes += self.copy((first * PAGE).max(from), (page * PAGE).min(to));
            }
        }
        // Cut to the range: next to it shaders may be writing the copy.
        if let Some(first) = run {
            bytes += self.copy((first * PAGE).max(from), ((last + 1) * PAGE).min(to));
        }
        bytes
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy.
    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        crate::host::copy_mapped(at, from as *const u8, to - from, false);
        to - from
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
    let mut map = buffers();
    let key = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k);
    let Some(key) = key else {
        drop(map);
        return loose(address, end);
    };
    let entry = map.get_mut(&key)?;
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        // Rounded up so that views ending in the buffer's last 256 bytes fit.
        // Shaders write some buffers (unordered access), so not upload memory.
        let (buffer, data) = host()?.shared_buffer(len.next_multiple_of(1 << 16))?;
        crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
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
        entry.shadow = Some(Arc::new(Shadow::new(buffer, data, gpu, key.0, len)));
    }
    entry.shadow.clone()
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
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(shadow) = map.get(&(start, len)) {
        return Some(shadow.clone());
    }
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let shadow = Arc::new(Shadow::new(buffer, data, gpu, start, len));
    map.insert((start, len), shadow.clone());
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

    thread_local! {
        /// What [`after_reading`] does on this thread: a title write while
        /// a copy-back runs.
        static AFTER_READING: std::cell::RefCell<Option<Box<dyn FnMut(usize)>>> =
            const { std::cell::RefCell::new(None) };
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
}
