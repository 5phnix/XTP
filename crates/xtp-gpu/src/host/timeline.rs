//! Submissions the host GPU may still be running, oldest first, and what is
//! done once each has run: its work (writing what GPU copies and shaders
//! wrote back into title memory, then the bottom-of-pipe writes that tell
//! the title the GPU is done), then the title's fence signals queued behind
//! it. Finished in submission order, one thread at a time.
//!
//! Each submission also names the title memory its GPU work writes (written
//! back when it finishes) and reads (host copies and textures brought up to
//! date for it), so that a later submission's CPU-side steps can find the
//! earlier ones they must wait for ([`Timeline::conflict`]), and so does a
//! page mapping, for the pages it replaces ([`Host::settle_remap`]).
//!
//! Ported from Marvisak/VIctory#595 (wbkepler), as `timeline.rs` there.

use std::collections::{HashSet, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard};

use super::Host;

/// Title memory ranges `[from, to)`, sorted and merged.
#[derive(Default)]
pub(crate) struct Ranges(Vec<(usize, usize)>);

impl Ranges {
    pub(crate) fn new(mut ranges: Vec<(usize, usize)>) -> Self {
        ranges.retain(|&(a, b)| a < b);
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (a, b) in ranges {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        Ranges(merged)
    }

    /// Whether `[from, to)` overlaps one of the ranges.
    pub(crate) fn overlaps(&self, from: usize, to: usize) -> bool {
        let i = self.0.partition_point(|&(_, b)| b <= from);
        self.0.get(i).is_some_and(|&(a, _)| a < to)
    }

    /// Whether one of `other`'s ranges overlaps one of these (a search in
    /// these for each of `other`'s: `other` is the short one).
    fn meets(&self, other: &Ranges) -> bool {
        other.0.iter().any(|&(a, b)| self.overlaps(a, b))
    }
}

type Signal = Box<dyn FnOnce() + Send>;

/// The title memory a submission's GPU work touches.
#[derive(Default)]
pub(crate) struct Touches {
    /// What its shaders write through host copies: the host copy (by
    /// address) and the range.
    pub(crate) shaders: Vec<(usize, usize, usize)>,
    /// What its buffer copies write through host copies, likewise.
    pub(crate) copies: Vec<(usize, usize, usize)>,
    /// What its bottom-of-pipe writes write.
    pub(crate) labels: Vec<(usize, usize)>,
    /// What it reads (host copies and textures brought up to date for it).
    pub(crate) reads: Vec<(usize, usize)>,
    /// Texture staging buffers (by address) its GPU copies read.
    pub(crate) staging: HashSet<usize>,
}

struct Entry<W> {
    /// The host fence value the submission signals.
    value: u64,
    /// Everything it writes, through host copies and by labels.
    writes: Ranges,
    labels: Ranges,
    shaders: Vec<(usize, usize, usize)>,
    copies: Vec<(usize, usize, usize)>,
    reads: Ranges,
    staging: HashSet<usize>,
    /// Title fence signals the title queued after it.
    signals: Vec<Signal>,
    /// Taken while it is being finished.
    work: Option<W>,
}

pub(crate) struct Timeline<W> {
    entries: Mutex<VecDeque<Entry<W>>>,
    added: Condvar,
    /// Held while submissions are finished: in order, by one thread.
    finishing: Mutex<()>,
}

impl<W> Timeline<W> {
    pub(crate) const fn new() -> Self {
        Timeline {
            entries: Mutex::new(VecDeque::new()),
            added: Condvar::new(),
            finishing: Mutex::new(()),
        }
    }

    fn entries(&self) -> MutexGuard<'_, VecDeque<Entry<W>>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A submission that signals `value` (higher than any before it), the
    /// title memory its GPU work touches, and its work once it ran.
    pub(crate) fn push(&self, value: u64, touches: Touches, work: W) {
        let Touches {
            shaders,
            copies,
            labels,
            reads,
            staging,
        } = touches;
        let writes = shaders
            .iter()
            .chain(&copies)
            .map(|&(_, a, b)| (a, b))
            .chain(labels.iter().copied());
        let entry = Entry {
            value,
            writes: Ranges::new(writes.collect()),
            labels: Ranges::new(labels),
            shaders,
            copies,
            reads: Ranges::new(reads),
            staging,
            signals: Vec::new(),
            work: Some(work),
        };
        self.entries().push_back(entry);
        self.added.notify_all();
    }

    /// Submissions not finished yet.
    pub(crate) fn len(&self) -> usize {
        self.entries().len()
    }

    /// The values of the oldest and newest submissions not finished yet.
    pub(crate) fn span(&self) -> Option<(u64, u64)> {
        let entries = self.entries();
        Some((entries.front()?.value, entries.back()?.value))
    }

    /// Runs `signal` once every submission pushed so far has finished: now
    /// if none is left.
    pub(crate) fn after(&self, signal: impl FnOnce() + Send + 'static) {
        let mut entries = self.entries();
        match entries.back_mut() {
            Some(last) => last.signals.push(Box::new(signal)),
            // Under the lock, so it stays behind signals still running.
            None => signal(),
        }
    }

    /// The newest submission not finished yet whose GPU work writes
    /// `[from, to)`, or (with `writing`, for a step that writes it) reads
    /// it.
    pub(crate) fn conflict(&self, from: usize, to: usize, writing: bool) -> Option<u64> {
        if from >= to {
            return None;
        }
        self.entries()
            .iter()
            .rev()
            .find(|e| e.writes.overlaps(from, to) || writing && e.reads.overlaps(from, to))
            .map(|e| e.value)
    }

    /// For title memory `pages` about to show other pages (a page
    /// mapping): the newest submission not finished yet whose GPU work
    /// touches them, writing or reading them, as for a step that writes
    /// them ([`Timeline::conflict`]); and the newest that writes them.
    pub(crate) fn remap_conflict(&self, pages: &Ranges) -> (Option<u64>, Option<u64>) {
        let mut touched = None;
        for e in self.entries().iter().rev() {
            if e.writes.meets(pages) {
                return (touched.or(Some(e.value)), Some(e.value));
            }
            if touched.is_none() && e.reads.meets(pages) {
                touched = Some(e.value);
            }
        }
        (touched, None)
    }

    /// The newest submission not finished yet whose shaders write
    /// `[from, to)` through the host copy `copy`: what it writes back is
    /// told from a snapshot of the copy, which a later GPU write to the
    /// range would confuse.
    pub(crate) fn shader_conflict(&self, copy: usize, from: usize, to: usize) -> Option<u64> {
        if from >= to {
            return None;
        }
        self.entries()
            .iter()
            .rev()
            .find(|e| {
                e.writes.overlaps(from, to)
                    && e.shaders
                        .iter()
                        .any(|&(c, a, b)| c == copy && a < to && b > from)
            })
            .map(|e| e.value)
    }

    /// The newest submission not finished yet whose GPU copies read the
    /// texture staging buffer `staging` (by address).
    pub(crate) fn staging_conflict(&self, staging: usize) -> Option<u64> {
        self.entries()
            .iter()
            .rev()
            .find(|e| e.staging.contains(&staging))
            .map(|e| e.value)
    }

    /// For a refresh of `[from, to)` through the host copy `copy`: the
    /// newest submission not finished yet it must wait for. One whose
    /// bottom-of-pipe writes overlap the range counts, and so does one whose
    /// shaders or copies write it through another host copy; one that
    /// writes it through this copy counts only if `stale` says the title
    /// wrote some of the overlap since the copy took it. Otherwise the copy
    /// already holds what the refresh would bring, and the GPU's writes
    /// reach it in queue order.
    pub(crate) fn refresh_conflict(
        &self,
        copy: usize,
        from: usize,
        to: usize,
        stale: impl Fn(usize, usize) -> bool,
    ) -> Option<u64> {
        if from >= to {
            return None;
        }
        let (mut must, mut parts) = (None, Vec::new());
        // Most refreshes touch nothing in flight: the merged ranges say so
        // in a binary search.
        for e in self
            .entries()
            .iter()
            .filter(|e| e.writes.overlaps(from, to))
        {
            if e.labels.overlaps(from, to) {
                must = Some(e.value);
            }
            let writes = e.shaders.iter().chain(&e.copies);
            for &(c, a, b) in writes.filter(|&&(_, a, b)| a < to && b > from) {
                if c == copy {
                    parts.push((e.value, a.max(from), b.min(to)));
                } else {
                    must = Some(e.value);
                }
            }
        }
        // Asked outside the lock, newest first.
        parts.sort_unstable_by_key(|p| std::cmp::Reverse(p.0));
        parts
            .into_iter()
            .take_while(|&(v, ..)| must.is_none_or(|m| v > m))
            .find(|&(_, a, b)| stale(a, b))
            .map(|(v, ..)| v)
            .or(must)
    }

    /// Blocks until a submission is left; its value.
    pub(crate) fn next(&self) -> u64 {
        let mut entries = self.entries();
        loop {
            if let Some(first) = entries.front() {
                return first.value;
            }
            entries = self.added.wait(entries).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Finishes, oldest first, the submissions whose values are at most
    /// `reached`: `finish` takes each one's work, then its signals run.
    /// Returns once those are all finished, by this thread or another.
    pub(crate) fn finish(&self, reached: u64, mut finish: impl FnMut(W)) {
        let _finishing = self.finishing.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let work = {
                let mut entries = self.entries();
                match entries.front_mut() {
                    Some(first) if first.value <= reached => first.work.take(),
                    _ => return,
                }
            };
            if let Some(work) = work {
                finish(work);
            }
            let mut entries = self.entries();
            if let Some(done) = entries.pop_front() {
                // Under the lock: a signal queued now goes behind these.
                done.signals.into_iter().for_each(|s| s());
            }
        }
    }

    /// Calls `f` with the work of the submissions not finished yet (not
    /// those being finished).
    pub(crate) fn each_work(&self, mut f: impl FnMut(&W)) {
        self.entries()
            .iter()
            .filter_map(|e| e.work.as_ref())
            .for_each(&mut f);
    }
}

impl Host {
    /// Before title memory `pages` (ranges `[from, to)`) shows other
    /// physical pages (a queue's page-mapping copy,
    /// `queue::copy_page_mappings`): finishes the submissions in flight
    /// whose GPU work touches them, as before a step that writes title
    /// memory ([`Timeline::remap_conflict`]). What they write then goes back
    /// into the pages they were submitted with, and no later refresh
    /// brings the new pages into host copies they still read; the others
    /// run on. Returns whether it waited, and whether that went past the
    /// newest submission writing the pages (for one that only reads them).
    #[track_caller]
    pub(crate) fn settle_remap(&self, pages: Vec<(usize, usize)>) -> (bool, bool) {
        let pages = Ranges::new(pages);
        match self.timeline.remap_conflict(&pages) {
            (Some(touched), written) => {
                self.settle(touched);
                (true, written != Some(touched))
            }
            (None, _) => (false, false),
        }
    }

    /// Before the first submission in an isolated test, leave write-backs
    /// pending until a caller settles them, even if the GPU is already done.
    #[cfg(test)]
    pub(crate) fn finish_on_caller(&self) {
        assert!(!self.finisher.is_completed());
        self.finisher.call_once(|| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    type Log = Arc<Mutex<Vec<String>>>;

    fn note(log: &Log, what: &str) {
        log.lock().unwrap().push(what.to_owned());
    }

    #[test]
    fn submissions_finish_in_order_and_signals_follow_their_work() {
        let log: Log = Arc::default();
        let t: Timeline<&str> = Timeline::new();
        // Nothing in flight: a signal is at once.
        let l = log.clone();
        t.after(move || note(&l, "signal 0"));
        t.push(1, Touches::default(), "work 1");
        t.push(2, Touches::default(), "work 2");
        // Queued after both: behind the second's work.
        let l = log.clone();
        t.after(move || note(&l, "signal 2"));
        let finish = |w: &str| note(&log, w);
        t.finish(0, finish);
        assert_eq!(*log.lock().unwrap(), ["signal 0"]);
        t.finish(1, finish);
        assert_eq!(*log.lock().unwrap(), ["signal 0", "work 1"]);
        assert_eq!(t.span(), Some((2, 2)));
        t.finish(5, finish);
        assert_eq!(
            *log.lock().unwrap(),
            ["signal 0", "work 1", "work 2", "signal 2"]
        );
        assert_eq!(t.len(), 0);
        // Finished again: nothing more.
        t.finish(5, finish);
        assert_eq!(log.lock().unwrap().len(), 4);
    }

    #[test]
    fn steps_conflict_with_the_newest_submission_touching_their_memory() {
        let t: Timeline<()> = Timeline::new();
        // Writes 0x1000..0x2000 and a 4-byte word at 0x9000; reads 0x5000..0x6000.
        let touches = |shaders: Vec<(usize, usize, usize)>, labels, reads| Touches {
            shaders,
            labels,
            reads,
            ..Touches::default()
        };
        t.push(
            1,
            touches(
                vec![(1, 0x1000, 0x2000)],
                vec![(0x9000, 0x9004)],
                vec![(0x5000, 0x6000)],
            ),
            (),
        );
        t.push(2, touches(vec![(1, 0x1800, 0x1900)], vec![], vec![]), ());
        // A GPU buffer copy through copy 1 of 0xa000..0xa100.
        let copied = Touches {
            copies: vec![(1, 0xa000, 0xa100)],
            ..Touches::default()
        };
        t.push(3, copied, ());
        // Reading what either writes waits for the newest that does.
        assert_eq!(t.conflict(0x1880, 0x1890, false), Some(2));
        assert_eq!(t.conflict(0x1000, 0x1010, false), Some(1));
        assert_eq!(t.conflict(0x9002, 0x9003, false), Some(1));
        // Their GPU reads matter only to steps that write.
        assert_eq!(t.conflict(0x5000, 0x5001, false), None);
        assert_eq!(t.conflict(0x5fff, 0x7000, true), Some(1));
        // Next to, not in.
        assert_eq!(t.conflict(0x2000, 0x5000, true), None);
        assert_eq!(t.conflict(0x9004, 0x9008, true), None);
        // A refresh through copy 1 waits only where the title wrote the
        // overlap; labels and other copies always count.
        let clean = |_: usize, _: usize| false;
        let dirty_at = |at: usize| move |a: usize, b: usize| (a..b).contains(&at);
        assert_eq!(t.refresh_conflict(1, 0x1000, 0x2000, clean), None);
        assert_eq!(
            t.refresh_conflict(1, 0x1000, 0x2000, dirty_at(0x1100)),
            Some(1)
        );
        assert_eq!(
            t.refresh_conflict(1, 0x1000, 0x2000, dirty_at(0x1880)),
            Some(2)
        );
        assert_eq!(t.refresh_conflict(2, 0x1000, 0x1010, clean), Some(1));
        assert_eq!(t.refresh_conflict(1, 0x8000, 0x9001, clean), Some(1));
        // A copy destination counts as a shader range for refreshes and
        // steps.
        assert_eq!(t.conflict(0xa0f0, 0xa200, false), Some(3));
        assert_eq!(t.refresh_conflict(1, 0xa000, 0xa010, clean), None);
        assert_eq!(
            t.refresh_conflict(1, 0xa000, 0xa010, dirty_at(0xa004)),
            Some(3)
        );
        assert_eq!(t.refresh_conflict(2, 0xa000, 0xa010, clean), Some(3));
        // A later GPU write waits only for shaders writing through the
        // same copy (whose write-back is told from a snapshot).
        assert_eq!(t.shader_conflict(1, 0x1880, 0xa080), Some(2));
        assert_eq!(t.shader_conflict(1, 0x1000, 0x1010), Some(1));
        assert_eq!(t.shader_conflict(1, 0xa000, 0xa100), None);
        assert_eq!(t.shader_conflict(2, 0x1000, 0x2000), None);
        t.finish(3, |()| {});
        assert_eq!(t.conflict(0x1000, 0x2000, true), None);

        // Shared texture staging buffers track the newest in-flight reader.
        let (staging_a, staging_b) = (0xdead_0000usize, 0xdead_1000usize);
        let mut t1 = Touches::default();
        t1.staging.insert(staging_a);
        let mut t2 = Touches::default();
        t2.staging.insert(staging_a);
        t2.staging.insert(staging_b);
        t.push(4, t1, ());
        t.push(5, t2, ());
        assert_eq!(t.staging_conflict(staging_a), Some(5));
        assert_eq!(t.staging_conflict(staging_b), Some(5));
        assert_eq!(t.staging_conflict(0xdead_2000), None);
        t.finish(5, |()| {});
        assert_eq!(t.staging_conflict(staging_a), None);
    }

    /// A page mapping finds the newest submission whose GPU work touches
    /// any of its pages, one that only reads them included, and the newest
    /// that writes them.
    #[test]
    fn a_page_mapping_conflicts_with_the_newest_submission_touching_its_pages() {
        let t: Timeline<()> = Timeline::new();
        let page = 0x1_0000;
        // 1 writes page 1 by shaders and a label on page 4; 2 reads page 2;
        // 3 writes page 3 by a buffer copy.
        let shaders = Touches {
            shaders: vec![(7, page, page + 0x100)],
            labels: vec![(4 * page + 8, 4 * page + 16)],
            ..Touches::default()
        };
        t.push(1, shaders, ());
        let reads = Touches {
            reads: vec![(2 * page, 2 * page + 0x100)],
            ..Touches::default()
        };
        t.push(2, reads, ());
        let copies = Touches {
            copies: vec![(7, 3 * page + 0x40, 3 * page + 0x80)],
            ..Touches::default()
        };
        t.push(3, copies, ());
        let pages =
            |p: &[usize]| Ranges::new(p.iter().map(|&p| (p * page, (p + 1) * page)).collect());
        // Elsewhere, and next to what they touch: nothing.
        assert_eq!(t.remap_conflict(&pages(&[0, 5])), (None, None));
        let next_to = Ranges::new(vec![(page + 0x100, 2 * page)]);
        assert_eq!(t.remap_conflict(&next_to), (None, None));
        assert_eq!(t.remap_conflict(&Ranges::default()), (None, None));
        // The one writing the old pages, labels and buffer copies included.
        assert_eq!(t.remap_conflict(&pages(&[1])), (Some(1), Some(1)));
        assert_eq!(t.remap_conflict(&pages(&[4])), (Some(1), Some(1)));
        assert_eq!(t.remap_conflict(&pages(&[3])), (Some(3), Some(3)));
        // One reading them, past the newest writing them.
        assert_eq!(t.remap_conflict(&pages(&[2])), (Some(2), None));
        assert_eq!(t.remap_conflict(&pages(&[1, 2])), (Some(2), Some(1)));
        assert_eq!(t.remap_conflict(&pages(&[0, 1, 2, 3])), (Some(3), Some(3)));
        t.finish(3, |()| {});
        assert_eq!(t.remap_conflict(&pages(&[1, 2, 3, 4])), (None, None));
    }

    /// A thread that needs a submission finished waits for the one
    /// finishing it, and finds it done (not finished twice).
    #[test]
    fn a_submission_is_finished_once_whichever_thread_asks() {
        let t: Arc<Timeline<u32>> = Arc::new(Timeline::new());
        let done: Arc<Mutex<Vec<u32>>> = Arc::default();
        for v in 1..=50 {
            t.push(u64::from(v), Touches::default(), v);
        }
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (t, done) = (t.clone(), done.clone());
                std::thread::spawn(move || {
                    for reached in (5..=50).step_by(5) {
                        t.finish(reached, |w| done.lock().unwrap().push(w));
                        assert!(t.span().is_none_or(|(oldest, _)| oldest > reached));
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(*done.lock().unwrap(), (1..=50).collect::<Vec<u32>>());
        assert_eq!(t.len(), 0);
    }
}
