//! Descriptor heaps: zeroed memory, with handles that are plain addresses
//! (CPU and GPU alike, as on the console).
//!
//! Descriptor sizes: views 32 bytes and samplers 16, the size of RDNA 2's
//! image and sampler descriptors, so a title that copies descriptors itself
//! moves whole ones. Render target and depth views are ours alone; 128 bytes
//! leaves room for whatever stride a title assumes.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{self, DESCRIPTOR_HEAP, E_INVALIDARG, HRESULT, Iid, Object, Table, This, guid};
use crate::resource::{Memory, PAGEABLE_IIDS};

#[cfg(windows)]
pub(crate) mod host;

const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x8efb471d,
        0x616c,
        0x4f49,
        [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapDesc {
    kind: u32,
    count: u32,
    flags: u32,
    nodes: u32,
}

/// Newer titles (BTP 6) pass the description followed by
/// memory of their own for the descriptors: an address and its size in
/// bytes, or a null address to have the heap allocate (inferred from the
/// title's call sites; older titles pass the plain description).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Placement {
    address: u64,
    size: u64,
}

#[repr(C)]
pub(crate) struct Heap {
    object: Object,
    desc: HeapDesc,
    /// The first descriptor: in `memory`, or in the title's own memory.
    pub(crate) start: u64,
    memory: Option<Memory>,
    /// Shader-visible view and sampler heaps get a host heap with the same
    /// descriptor order when a draw first uses them (decision 0007, step
    /// 3); slots written before wait in `pending`.
    #[cfg(windows)]
    host: std::sync::OnceLock<Option<host::HostHeap>>,
    #[cfg(windows)]
    pending: std::sync::Mutex<Vec<u32>>,
    /// The heap's life, for work that runs after the recordings that use
    /// it ([`HeapHold`]).
    #[cfg(windows)]
    life: std::sync::Arc<Life>,
    /// Shader-visible heaps placed in the title's memory: what the look for
    /// records the title copies into them as bytes keeps ([`sync_heaps`]).
    #[cfg(windows)]
    watched: Option<std::sync::Mutex<Watched>>,
}

/// Whether a title heap is still there, for work that outlives the
/// recordings that use it: the sweeps a list runs as it closes and at its
/// submissions (`draw.rs` `picked_buffers`).
#[cfg(windows)]
struct Life {
    /// The heap's address while the title holds it; 0 once its release has
    /// begun. Work on the heap reads it locked, and the heap's drop locks it
    /// for writing first: a release waits for that work to end, and work
    /// after it finds 0.
    heap: std::sync::RwLock<usize>,
}

/// A hold on one title heap, this one and no other: a heap made later at
/// the same address has a life of its own (Codex review of a090f22, P1:
/// the work a list runs as it closes kept the heap's address, checked it
/// against the heaps registered, then used it without a lock, so a
/// concurrent last release could free it between the two, and a heap made
/// at the same address since passed the check with the old one's start).
#[cfg(windows)]
#[derive(Clone)]
pub(crate) struct HeapHold(std::sync::Arc<Life>);

/// Holds on the same heap are equal (whatever its address holds now).
#[cfg(windows)]
impl PartialEq for HeapHold {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(windows)]
impl Eq for HeapHold {}

#[cfg(windows)]
impl std::fmt::Debug for HeapHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HeapHold({:p})", std::sync::Arc::as_ptr(&self.0))
    }
}

#[cfg(windows)]
impl HeapHold {
    /// Runs `f` with the heap if the title has not released it, which keeps
    /// it until `f` returns: its release waits. `f` must not release a
    /// title heap.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&Heap) -> R) -> Option<R> {
        let at = self.0.heap.read().unwrap_or_else(|e| e.into_inner());
        if *at == 0 {
            return None;
        }
        // SAFETY: the heap's drop sets 0 under the write lock before any of
        // it goes, and the read lock held until `f` returns keeps it from
        // doing so meanwhile.
        Some(f(unsafe { &*(*at as *const Heap) }))
    }
}

pub(crate) fn heap<'a>(this: This) -> &'a Heap {
    // SAFETY: called from descriptor heap methods only.
    unsafe { &*this.cast::<Heap>() }
}

impl Heap {
    /// Whether this is a sampler heap.
    pub(crate) fn samplers(&self) -> bool {
        self.desc.kind == SAMPLERS
    }

    /// The host heap, made on first use from what the title wrote so far.
    /// Made late because a host view heap the size of BTP 6's
    /// costs over 100 MB of commit, which its boot needs for its page pool.
    #[cfg(windows)]
    pub(crate) fn host(&self) -> Option<&host::HostHeap> {
        if let Some(h) = self.host.get() {
            return h.as_ref();
        }
        if self.desc.flags & SHADER_VISIBLE == 0 || !matches!(self.desc.kind, VIEWS_KIND | SAMPLERS)
        {
            return None;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // A recorder can take the fast path as soon as the OnceLock is
        // initialized. Publish only after replaying the queued writes.
        self.host
            .get_or_init(|| {
                let step = u64::from(increment(self.desc.kind)?);
                let h = host::HostHeap::new(self.samplers(), self.desc.count)?;
                for i in pending.drain(..) {
                    h.mirror(i, self.start + u64::from(i) * step, false);
                }
                Some(h)
            })
            .as_ref()
    }

    /// A hold on this heap ([`HeapHold`]).
    #[cfg(windows)]
    pub(crate) fn hold(&self) -> HeapHold {
        HeapHold(self.life.clone())
    }

    /// The descriptor index of `handle` in this heap, if it lies in it.
    pub(crate) fn index(&self, handle: u64) -> Option<u32> {
        let step = u64::from(increment(self.desc.kind)?);
        let at = handle.checked_sub(self.start)?;
        let index = at / step;
        (index < u64::from(self.desc.count)).then_some(index as u32)
    }
}

const VIEWS_KIND: u32 = 0;
const SAMPLERS: u32 = 1;
const SHADER_VISIBLE: u32 = 1;

/// Shader-visible heaps by address range, for mirroring descriptor writes:
/// start, end, heap.
#[cfg(windows)]
static SHADER_HEAPS: std::sync::Mutex<Vec<(u64, u64, usize)>> = std::sync::Mutex::new(Vec::new());

/// The shader-visible heap holding `handle`, and the descriptor's index.
#[cfg(windows)]
fn shader_slot<'a>(handle: u64) -> Option<(&'a Heap, u32)> {
    let heaps = SHADER_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    let &(_, _, at) = heaps
        .iter()
        .find(|&&(start, end, _)| (start..end).contains(&handle))?;
    // SAFETY: registered heaps are live (removed when dropped).
    let heap = unsafe { &*(at as *const Heap) };
    Some((heap, heap.index(handle)?))
}

/// Makes the host descriptor for what the title wrote at `handle`, if it
/// is in a shader-visible heap.
#[cfg(windows)]
fn mirror(handle: u64) {
    let Some((heap, index)) = shader_slot(handle) else {
        return;
    };
    let mut pending = heap.pending.lock().unwrap_or_else(|e| e.into_inner());
    match heap.host.get() {
        None => pending.push(index),
        Some(host) => {
            drop(pending);
            if let Some(host) = host {
                host.mirror(index, handle, false);
            }
        }
    }
}

/// The key of a record header (tag, view index, generation): the view index
/// and generation of a record of ours (never 0: generations start at 1), 0
/// for anything else. Two slots hold the same view when their keys match.
#[cfg(windows)]
pub(crate) fn record_key([tag, i, generation]: [u32; 3]) -> u64 {
    if tag == VIEW_TAG {
        u64::from(generation) << 32 | u64::from(i)
    } else {
        0
    }
}

/// Whether [`sync_heaps`] looks for records copied in as bytes:
/// `XTP_HEAP_SYNC=1` does, `ab` in turns within the run ([`xtp_core::ab`]:
/// phase A not, B does). Off by default: in the world it took 6.7 to 9.9 ms
/// a frame (session faces-1, Series S, 24 to 34 submissions a frame).
#[cfg(windows)]
fn heap_sync() -> &'static xtp_core::ab::Switch<bool> {
    static SWITCH: std::sync::OnceLock<xtp_core::ab::Switch<bool>> = std::sync::OnceLock::new();
    SWITCH.get_or_init(|| xtp_core::ab::switch("XTP_HEAP_SYNC", false))
}

/// What the look for records copied in as bytes ([`sync_heaps`]) keeps of a
/// placed heap from one look to the next.
#[cfg(windows)]
#[derive(Default)]
struct Watched {
    /// The lowest write-watch asking of the last look (0: none yet): a page
    /// stamped after it may have changed since
    /// ([`crate::watch::last_write`]).
    since: u64,
    /// The heap's memory as runs of blocks ([`crate::watch::PAGE`]) the
    /// write watch answers for, each asked as a whole: the whole heap at
    /// first. The watch decides for a range by its first block (`watch.rs`
    /// `with_writes` and `guard` look at it alone), so over memory it
    /// watches only in part it answers nothing at all; such a run is split
    /// in two until each part gets an answer or is one block.
    runs: Vec<(usize, usize)>,
    /// Blocks no answer comes for: neither write-watched nor a view the
    /// guards take. Counted, not looked at; asked again every
    /// [`UNWATCHED_AGAIN`] looks (the title may map memory there since).
    unwatched: std::collections::BTreeSet<usize>,
    /// Looks so far.
    looks: u64,
    /// Room for the records of a changed page.
    bytes: Vec<u8>,
}

/// Looks after which the blocks of a placed heap that no write watch
/// answered for are asked about again ([`Watched::unwatched`]).
#[cfg(windows)]
const UNWATCHED_AGAIN: u64 = 256;

/// What looks for records copied in as bytes found ([`sync_heaps`]), summed.
#[cfg(windows)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Copies {
    /// Changed pages read, and the slots on them looked at.
    pub(crate) pages: u64,
    pub(crate) slots: u64,
    /// Slots holding a record the title copied in as bytes, with no view
    /// call nor descriptor copy, each counted once.
    pub(crate) copied: u64,
    /// Of them, views of textures the title fills, and of those the ones a
    /// draw read through a table lately (draws make them: the others are
    /// read only by shaders that pick textures by index).
    pub(crate) textures: u64,
    pub(crate) tabled: u64,
    /// Buffer views, and of those the ones in heaps whose shaders pick
    /// buffers at run time.
    pub(crate) buffers: u64,
    pub(crate) picked_buffers: u64,
    /// Samplers, other views of ours, and no view of ours.
    pub(crate) samplers: u64,
    pub(crate) other: u64,
    pub(crate) none: u64,
    /// Of the copied, those whose host slot was not made from their record
    /// when the look found them (null, or another view).
    pub(crate) unmade: u64,
    /// Blocks of placed heaps no write watch answers for (the most at one
    /// look, not summed).
    pub(crate) unwatched: u64,
}

#[cfg(windows)]
impl Copies {
    /// One slot the title copied a record into as bytes: what it holds,
    /// and whether its host slot was made from that record.
    fn add_slot(&mut self, kind: host::CopiedKind, made: bool) {
        use host::CopiedKind as K;
        self.copied += 1;
        self.unmade += u64::from(!made);
        match kind {
            K::Texture { tabled } => {
                self.textures += 1;
                self.tabled += u64::from(tabled);
            }
            K::Buffer { picked } => {
                self.buffers += 1;
                self.picked_buffers += u64::from(picked);
            }
            K::Sampler => self.samplers += 1,
            K::Other => self.other += 1,
            K::None => self.none += 1,
        }
    }

    /// The slots shaders pick by index: views of textures the title fills
    /// that no table read lately, and buffer views in heaps whose shaders
    /// pick buffers at run time.
    pub(crate) fn picked(&self) -> u64 {
        self.textures - self.tabled + self.picked_buffers
    }

    /// `other`'s counts added (blocks not watched: the most).
    fn add(&mut self, other: &Copies) {
        self.pages += other.pages;
        self.slots += other.slots;
        self.copied += other.copied;
        self.textures += other.textures;
        self.tabled += other.tabled;
        self.buffers += other.buffers;
        self.picked_buffers += other.picked_buffers;
        self.samplers += other.samplers;
        self.other += other.other;
        self.none += other.none;
        self.unmade += other.unmade;
        self.unwatched = self.unwatched.max(other.unwatched);
    }
}

/// Since the last frame line's count ([`take_synced`]): what
/// [`sync_heaps`] found, and the microseconds it took.
#[cfg(windows)]
static SYNCED: std::sync::Mutex<(Copies, u64)> = std::sync::Mutex::new((
    Copies {
        pages: 0,
        slots: 0,
        copied: 0,
        textures: 0,
        tabled: 0,
        buffers: 0,
        picked_buffers: 0,
        samplers: 0,
        other: 0,
        none: 0,
        unmade: 0,
        unwatched: 0,
    },
    0,
));

/// Heaps the tests look at themselves ([`Heap::look_for_copies`]), which
/// [`sync_heaps`] on other tests' threads leaves alone.
#[cfg(all(test, windows))]
static LOOKED_AT_BY_TESTS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Looks for descriptor records the title copied into its placed
/// shader-visible heaps as bytes (stores or memcpy, which no `Create*View`
/// nor `CopyDescriptors` shows), before each title submission: the pages the
/// write watch (the guards, for view memory) reports changed since the last
/// look are read through the kernel (a page the title unmapped meanwhile
/// fails the read instead of faulting), and each slot whose record is
/// neither what the title's calls wrote there nor what the last look found
/// is counted once, with what it holds ([`Copies`], on the frame line:
/// [`take_synced`]).
///
/// Counted only, for now: such a slot stays on the host as it was (null for
/// a placed texture, the view it showed before otherwise) unless a draw
/// reads it through a table, which makes it. The mirror of these slots
/// (VIctory 86a5ed6 by Marvis; the fork's a26859f and 9376fd6) is to be
/// ported once this count shows shaders picking them.
///
/// Ported from fxceless/victory-fork (a26859f) by Flyander (its PR #305)
/// and fxceless: the look before each submission at the pages the write
/// watch reports changed, read through the kernel, and its count on the
/// frame line. What differs: a slot's record is compared by its key (the
/// header XTP writes, [`record_key`]) with the one the title's calls last
/// wrote there, not by a fingerprint of its bytes against the last look's,
/// so a slot the title rewrites through a call is never taken for a copy,
/// and a slot draws read through tables lately is counted all the same
/// (9376fd6 left those out of its pass whatever they held); a record across
/// two pages is looked at from either page (a26859f left it to the range
/// holding its first byte, which fork #802 reports); and the watch is asked
/// per run of blocks it answers
/// for (the fork asked once for the heap, which over memory the watch
/// answers for only in part gave nothing at all).
///
/// shortcut: a record a call publishes while a look reads its page may be
/// counted as copied once (the call notes it just after its write).
/// With `XTP_HEAP_SYNC=1`, or `ab` in turns ([`heap_sync`]); off, nothing is
/// looked at.
#[cfg(windows)]
pub(crate) fn sync_heaps() {
    if !heap_sync().get() {
        return;
    }
    let started = std::time::Instant::now();
    // Held, not the registry's lock: a heap's release waits for its look,
    // and a heap made since at the same address is another.
    let holds: Vec<HeapHold> = {
        let heaps = SHADER_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        let tests = LOOKED_AT_BY_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        heaps
            .iter()
            .filter(|&&(_, _, at)| {
                #[cfg(test)]
                if tests.contains(&at) {
                    return false;
                }
                // SAFETY: registered heaps are live (removed when dropped).
                unsafe { &*(at as *const Heap) }.watched.is_some()
            })
            // SAFETY: as above.
            .map(|&(_, _, at)| unsafe { &*(at as *const Heap) }.hold())
            .collect()
    };
    if holds.is_empty() {
        return;
    }
    let mut found = Copies::default();
    for hold in holds {
        hold.with(|heap| heap.look_for_copies(&mut found));
    }
    let mut synced = SYNCED.lock().unwrap_or_else(|e| e.into_inner());
    synced.0.add(&found);
    synced.1 += started.elapsed().as_micros() as u64;
}

/// The slots of a heap of `count` slots of `step` bytes from `start` that
/// have a byte in page `page` ([`crate::watch::PAGE`]): a record across the
/// page's start or end is looked at from either page.
#[cfg(windows)]
fn page_slots(page: usize, start: usize, step: usize, count: usize) -> std::ops::Range<usize> {
    use crate::watch::PAGE;
    let end = start + count * step;
    let (from, to) = ((page * PAGE).max(start), ((page + 1) * PAGE).min(end));
    if from >= to {
        return 0..0;
    }
    (from - start) / step..(to - start).div_ceil(step)
}

/// The pages of `[start, end)` that changed since `w`'s last look, asking
/// the write watch per run of blocks it answers for ([`Watched::runs`]).
#[cfg(windows)]
fn changed_pages(w: &mut Watched, start: usize, end: usize) -> Vec<usize> {
    use crate::watch::{PAGE, last_write, writes};
    w.looks += 1;
    if w.runs.is_empty() && w.unwatched.is_empty() {
        w.runs.push((start / PAGE, end.div_ceil(PAGE)));
    } else if w.looks.is_multiple_of(UNWATCHED_AGAIN) {
        let again = std::mem::take(&mut w.unwatched);
        w.runs
            .extend(again.into_iter().map(|block| (block, block + 1)));
    }
    let mut todo = std::mem::take(&mut w.runs);
    let mut lowest: Option<u64> = None;
    while let Some((first, last)) = todo.pop() {
        match writes((first * PAGE).max(start), (last * PAGE).min(end), true) {
            Some(asked) => {
                lowest = Some(lowest.map_or(asked, |l| l.min(asked)));
                w.runs.push((first, last));
            }
            None if last - first > 1 => {
                let middle = first + (last - first) / 2;
                todo.push((middle, last));
                todo.push((first, middle));
            }
            None => {
                w.unwatched.insert(first);
            }
        }
    }
    w.runs.sort_unstable();
    let Some(lowest) = lowest else {
        return Vec::new();
    };
    // The lowest asking: a page another asking stamped between this look's
    // own (which cleared its kernel bit) is not taken for seen.
    let since = std::mem::replace(&mut w.since, lowest);
    w.runs
        .iter()
        .flat_map(|&(first, last)| first..last)
        .filter(|&page| last_write(page) > since)
        .collect()
}

#[cfg(windows)]
impl Heap {
    /// [`sync_heaps`]' look at this heap, if it is placed in the title's
    /// memory and has its host heap: what it found added to `found`.
    fn look_for_copies(&self, found: &mut Copies) {
        let (Some(watched), Some(Some(host)), Some(step)) =
            (&self.watched, self.host.get(), increment(self.desc.kind))
        else {
            return;
        };
        let step = step as usize;
        let start = self.start as usize;
        let mut w = watched.lock().unwrap_or_else(|e| e.into_inner());
        let w = &mut *w;
        let pages = changed_pages(w, start, start + self.desc.count as usize * step);
        found.unwatched += w.unwatched.len() as u64;
        // Slots past the host heap (a capped one) have no host slot.
        let count = self.desc.count.min(host.count()) as usize;
        // The first slot not looked at yet: one across two changed pages
        // is looked at once.
        let mut next = 0;
        for page in pages {
            let slots = page_slots(page, start, step, count);
            let (from, to) = (slots.start.max(next), slots.end);
            if from >= to {
                continue;
            }
            next = to;
            w.bytes.resize((to - from) * step, 0);
            let read = {
                let _records = records_read();
                crate::host::read_into((start + from * step) as *const u8, &mut w.bytes)
            };
            if !read {
                continue;
            }
            found.pages += 1;
            found.slots += (to - from) as u64;
            let headers: Vec<[u32; 3]> = w
                .bytes
                .chunks_exact(step)
                .map(|r| {
                    std::array::from_fn(|k| u32::from_le_bytes([0, 1, 2, 3].map(|b| r[4 * k + b])))
                })
                .collect();
            let keys: Vec<u64> = headers.iter().map(|&h| record_key(h)).collect();
            for (index, made) in host.copied_records(from as u32, &keys) {
                let at = index as usize - from;
                let [_, i, generation] = headers[at];
                let view = (keys[at] != 0).then(|| view_at(i, generation)).flatten();
                found.add_slot(host.copied_kind(index, view.as_ref()), made);
            }
        }
    }
}

/// What [`sync_heaps`] found since the last call, for the frame line over
/// `frames` frames ([`HeapSyncLine::log`]).
#[cfg(windows)]
pub(crate) fn take_synced(frames: f64) -> HeapSyncLine {
    let (copies, micros) = std::mem::take(&mut *SYNCED.lock().unwrap_or_else(|e| e.into_inner()));
    // Slots of shader-visible heaps in XTP's own memory, whose host heap is
    // made: the title may copy records into them as well, and no write
    // watch shows it.
    let unplaced = SHADER_HEAPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|&(_, _, at)| {
            // SAFETY: registered heaps are live (removed when dropped).
            let heap = unsafe { &*(at as *const Heap) };
            let made = heap.host.get().is_some_and(Option::is_some);
            if heap.watched.is_none() && made {
                u64::from(heap.desc.count)
            } else {
                0
            }
        })
        .sum();
    HeapSyncLine {
        copies,
        micros,
        unplaced,
        frames: frames.max(1.0),
    }
}

/// [`sync_heaps`]' part of the frame line ([`take_synced`]).
#[cfg(windows)]
pub(crate) struct HeapSyncLine {
    copies: Copies,
    micros: u64,
    unplaced: u64,
    frames: f64,
}

#[cfg(windows)]
impl HeapSyncLine {
    /// Logs it, per frame, unless the look is off for the whole run.
    pub(crate) fn log(&self) {
        use xtp_core::ab::Arm;
        if !heap_sync().at(Arm::A) && !heap_sync().at(Arm::B) {
            return;
        }
        let c = &self.copies;
        let per = |n: u64| n as f64 / self.frames;
        tracing::info!(
            "descriptor records the title copied into its placed heaps as bytes, with no view call \
             nor descriptor copy (counted only, with XTP_HEAP_SYNC=1 or ab), per frame: \
             {:.1} changed heap pages, {:.0} slots looked at, {:.1} copied slots; picked by index \
             {:.1} ({:.1} views of textures the title fills that no table read lately, {:.1} \
             buffer views in heaps whose shaders pick buffers at run time); besides {:.1} textures \
             read through tables, {:.1} buffer views, {:.1} samplers, {:.1} other views, {:.1} no \
             view of ours; {:.1} not what their host slot was made from; {:.3} ms; {} blocks no \
             write watch answers for, {} slots of heaps in XTP's memory (not looked at)",
            per(c.pages),
            per(c.slots),
            per(c.copied),
            per(c.picked()),
            per(c.textures - c.tabled),
            per(c.picked_buffers),
            per(c.tabled),
            per(c.buffers - c.picked_buffers),
            per(c.samplers),
            per(c.other),
            per(c.none),
            per(c.unmade),
            self.micros as f64 / 1000.0 / self.frames,
            c.unwatched,
            self.unplaced,
        );
    }
}

/// Gives back dynamic slots of the host heap behind the title heap at
/// `heap`, if that heap still exists: what a recording leases with
/// [`crate::host::HostList::keep_dynamic`], once it has gone. A
/// [`Recording`]'s lease (`heap` [`BATCH`]) gives back all it holds.
#[cfg(windows)]
pub(crate) fn release_dynamic(heap: usize, first: u32, n: u32) {
    if heap == BATCH {
        return batch_gone(u64::from(first) | u64::from(n) << 32);
    }
    // shortcut: a lease by address alone (`keep_dynamic` with a title
    // heap's, tests only now) is given back to the heap at that address,
    // whichever it is; recordings keep their heaps' own ([`Held`]).
    give_back(&mut Held {
        ranges: vec![(heap, first, n)],
        ..Held::default()
    });
}

/// The lease key of [`Recording`]s (no title heap is at this address).
#[cfg(windows)]
const BATCH: usize = usize::MAX - 1;

/// What one recording of a list holds: dynamic slots, holds on shared
/// views, and memory of the list's own its commands use (dropped with it).
/// It all goes back in one batch, per heap one lock, once both the list's
/// state is done with the recording (reset or released) and the recording
/// has gone: at the host list's reset, or, for a list reset while it runs,
/// once that submission has finished. A recording leases one entry of
/// [`crate::host::HostList::keep_dynamic`] for it all.
#[cfg(windows)]
#[derive(Default)]
pub(crate) struct Recording {
    /// Its batch, from the first thing it holds on.
    id: Option<u64>,
    held: Held,
}

/// A recording's batch: what its state handed over, and whether the state
/// is done with it and whether the recording has gone.
#[cfg(windows)]
#[derive(Default)]
struct Batch {
    held: Held,
    memory: Vec<Box<dyn Send>>,
    finished: bool,
    gone: bool,
}

#[cfg(windows)]
static BATCHES: std::sync::Mutex<Option<std::collections::HashMap<u64, Batch>>> =
    std::sync::Mutex::new(None);

#[cfg(windows)]
fn batches() -> std::sync::MutexGuard<'static, Option<std::collections::HashMap<u64, Batch>>> {
    BATCHES.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(windows)]
impl Recording {
    /// Leases the recording's batch in `list`, once.
    pub(crate) fn lease(&mut self, list: &crate::host::HostList) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        if self.id.is_some() {
            return;
        }
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        batches()
            .get_or_insert_with(Default::default)
            .insert(id, Batch::default());
        list.keep_dynamic(BATCH, id as u32, (id >> 32) as u32);
        self.id = Some(id);
    }

    /// `n` dynamic slots from `first` of `heap`'s host heap, held until the
    /// recording has gone.
    pub(crate) fn range(&mut self, list: &crate::host::HostList, heap: usize, first: u32, n: u32) {
        self.lease(list);
        self.held.add_range(heap, first, n);
    }

    /// A hold on shared view `slot` of `heap`'s host heap (see
    /// [`host::HostHeap::shared_raw`]), given back once the recording has
    /// gone.
    pub(crate) fn shared(&mut self, list: &crate::host::HostList, heap: usize, slot: u32) {
        self.lease(list);
        self.held.add_shared(heap, slot);
    }

    /// Descriptor leases already held before attempting a bind.
    pub(crate) fn checkpoint(&self) -> (usize, usize) {
        (self.held.ranges.len(), self.held.shared.len())
    }

    /// Only descriptors acquired by a bind that issued no draw or dispatch.
    /// Copies and other commands already recorded retain their resources.
    pub(crate) fn reject(&mut self, (ranges, shared): (usize, usize)) {
        let held = &mut self.held;
        give_back(&mut Held {
            ranges: held.ranges.split_off(ranges),
            shared: held.shared.split_off(shared),
            range_heaps: held
                .range_heaps
                .split_off(ranges.min(held.range_heaps.len())),
            shared_heaps: held
                .shared_heaps
                .split_off(shared.min(held.shared_heaps.len())),
        });
    }

    /// The state is done with the recording: what it holds goes, now or
    /// once the recording has gone, with `memory`.
    pub(crate) fn finish(&mut self, memory: Box<dyn Send>) {
        let Some(id) = self.id.take() else {
            // Nothing held, nothing the recording's commands use.
            return;
        };
        let mut all = batches();
        let Some(batch) = all.as_mut().and_then(|b| b.get_mut(&id)) else {
            return;
        };
        batch.held.append(&mut self.held);
        batch.memory.push(memory);
        batch.finished = true;
        if batch.gone {
            let batch = all.as_mut().and_then(|b| b.remove(&id));
            drop(all);
            if let Some(mut batch) = batch {
                give_back(&mut batch.held);
            }
        }
    }

    /// What the recording holds (tests).
    #[cfg(test)]
    pub(crate) fn held(&self) -> &Held {
        &self.held
    }
}

/// Recording `id` has gone: its batch goes back once its state is done
/// with it.
#[cfg(windows)]
fn batch_gone(id: u64) {
    let mut all = batches();
    let Some(batch) = all.as_mut().and_then(|b| b.get_mut(&id)) else {
        return;
    };
    batch.gone = true;
    if batch.finished {
        let batch = all.as_mut().and_then(|b| b.remove(&id));
        drop(all);
        if let Some(mut batch) = batch {
            give_back(&mut batch.held);
        }
    }
}

/// What a list holds in host view heaps, by heap: the title heap's
/// address, or [`OWN_VIEW_HEAP`].
#[cfg(windows)]
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    /// Dynamic slot ranges of its own: heap, first, count.
    pub(crate) ranges: Vec<(usize, u32, u32)>,
    /// Shared views it holds (see [`host::HostHeap::shared_raw`]): heap,
    /// slot.
    pub(crate) shared: Vec<(usize, u32)>,
    /// The title heap each of `ranges` and of `shared` was taken from (none
    /// for XTP's own view heap, or a lease by address alone): the address
    /// says which heap only until the title releases it, and a heap it
    /// makes at the same address since has slots of its own. Codex review
    /// of the port #1331 (task 69adac42, point 1): a list A held slot S of
    /// heap H1, the title made H2 at H1's address, a list B took S of H2,
    /// and A's give-back found H2 by the address and freed S, which a third
    /// list could then take while B read it.
    range_heaps: Vec<Option<HeapHold>>,
    shared_heaps: Vec<Option<HeapHold>>,
}

#[cfg(windows)]
impl Held {
    /// `n` dynamic slots from `first` of the host heap behind `heap`, which
    /// a recording uses (so live).
    fn add_range(&mut self, heap: usize, first: u32, n: u32) {
        self.ranges.push((heap, first, n));
        self.range_heaps.push(hold_of(heap));
    }

    /// Shared view `slot` of the host heap behind `heap`, live as above.
    fn add_shared(&mut self, heap: usize, slot: u32) {
        self.shared.push((heap, slot));
        self.shared_heaps.push(hold_of(heap));
    }

    /// All `other` holds, moved here.
    fn append(&mut self, other: &mut Held) {
        self.ranges.append(&mut other.ranges);
        self.shared.append(&mut other.shared);
        self.range_heaps.append(&mut other.range_heaps);
        self.shared_heaps.append(&mut other.shared_heaps);
    }
}

/// A hold on the title heap at `key`, live (a recording uses it); none for
/// XTP's own view heap and the keys that name no title heap.
#[cfg(windows)]
fn hold_of(key: usize) -> Option<HeapHold> {
    // SAFETY: the caller's recording uses the heap, which the title keeps
    // while its lists record with it.
    (key != 0 && key != OWN_VIEW_HEAP && key != BATCH).then(|| heap(key as This).hold())
}

/// The key of XTP's own view heap in [`Held`] (no title heap is at this
/// address).
#[cfg(windows)]
pub(crate) const OWN_VIEW_HEAP: usize = usize::MAX;

/// Dynamic slots in XTP's own view heap: root SRV tables of lists that
/// bind no view heap.
#[cfg(windows)]
const OWN_DYNAMIC: u32 = 1 << 14;

/// XTP's own view heap (made on first use), for lists that bind no view
/// heap of the title's but read root SRVs, which are bound as tables.
#[cfg(windows)]
pub(crate) fn own_view_heap() -> Option<&'static host::HostHeap> {
    static OWN: std::sync::OnceLock<Option<host::HostHeap>> = std::sync::OnceLock::new();
    OWN.get_or_init(|| host::HostHeap::own(OWN_DYNAMIC))
        .as_ref()
}

/// Gives back what a list held, in one batch per heap: each host heap is
/// locked once, not once per range. Heaps the title released since are
/// passed over (their slots went with them), and so is a heap made since at
/// a released one's address: what was taken from the first goes back to
/// it alone ([`Held::range_heaps`]).
#[cfg(windows)]
pub(crate) fn give_back(held: &mut Held) {
    if held.ranges.is_empty() && held.shared.is_empty() {
        return;
    }
    // Usually one heap or two.
    let mut keys: Vec<usize> = held
        .ranges
        .iter()
        .map(|r| r.0)
        .chain(held.shared.iter().map(|s| s.0))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    // Held throughout: a heap's drop waits for it, so a live one stays so.
    let heaps = SHADER_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    for key in keys {
        let (host, life) = if key == OWN_VIEW_HEAP {
            (own_view_heap(), None)
        } else if heaps.iter().any(|&(_, _, h)| h == key) {
            // SAFETY: registered heaps are live (removed when dropped).
            let heap = unsafe { &*(key as *const Heap) };
            (heap.host.get().and_then(Option::as_ref), Some(&heap.life))
        } else {
            (None, None)
        };
        let Some(host) = host else { continue };
        // Taken from the heap at the address now, or of no heap known.
        let ours = |taken: Option<&Option<HeapHold>>| match (taken.and_then(Option::as_ref), life) {
            (Some(taken), Some(life)) => std::sync::Arc::ptr_eq(&taken.0, life),
            _ => true,
        };
        let ranges: Vec<(u32, u32)> = held
            .ranges
            .iter()
            .enumerate()
            .filter(|&(i, r)| r.0 == key && ours(held.range_heaps.get(i)))
            .map(|(_, &(_, first, n))| (first, n))
            .collect();
        host.release_all(&ranges);
        let shared: Vec<u32> = held
            .shared
            .iter()
            .enumerate()
            .filter(|&(i, s)| s.0 == key && ours(held.shared_heaps.get(i)))
            .map(|(_, s)| s.1)
            .collect();
        host.give_back(&shared);
    }
    held.ranges.clear();
    held.shared.clear();
    held.range_heaps.clear();
    held.shared_heaps.clear();
}

#[cfg(windows)]
impl Drop for Heap {
    fn drop(&mut self) {
        let at = std::ptr::from_ref::<Heap>(self) as usize;
        // First, before any of the heap goes: work holding it ends, and
        // work after finds it gone ([`HeapHold`]). Done already when the
        // title's last release drops it ([`release`]).
        if RELEASING.get() != at {
            *self.life.heap.write().unwrap_or_else(|e| e.into_inner()) = 0;
        }
        let mut heaps = SHADER_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
        heaps.retain(|&(_, _, h)| h != at);
    }
}

/// Bytes per descriptor, by heap type (views, samplers, render targets,
/// depth).
fn increment(kind: u32) -> Option<u32> {
    [32, 16, 128, 128].get(kind as usize).copied()
}

/// Device method.
pub(crate) extern "system" fn increment_size(_: This, kind: u32) -> u32 {
    increment(kind).unwrap_or(0)
}

/// What our render target views hold: the resource and the view
/// description, turned into a host view when a command uses them.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RtvRecord {
    tag: u32,
    described: u32,
    pub(crate) resource: This,
    desc: [u32; 6],
}
const RTV_TAG: u32 = u32::from_le_bytes(*b"vRTV");
const _: () = assert!(size_of::<RtvRecord>() <= 112);

impl RtvRecord {
    /// The view description in the host's type (the same layout).
    #[cfg(windows)]
    pub(crate) fn desc(
        &self,
    ) -> Option<windows::Win32::Graphics::Direct3D12::D3D12_RENDER_TARGET_VIEW_DESC> {
        const TEXTURE_2D: u32 = 4;
        self.host_desc(TEXTURE_2D)
    }

    /// The depth view description in the host's type (the same layout).
    #[cfg(windows)]
    pub(crate) fn depth_desc(
        &self,
    ) -> Option<windows::Win32::Graphics::Direct3D12::D3D12_DEPTH_STENCIL_VIEW_DESC> {
        const TEXTURE_2D: u32 = 3;
        self.host_desc(TEXTURE_2D)
    }

    /// The description words as a host type, the format as the host has it.
    /// Without one, a typeless host copy gets a 2D view (`dimension`) in
    /// the title's format, which the host needs spelled out.
    #[cfg(windows)]
    fn host_desc<T>(&self, dimension: u32) -> Option<T> {
        assert!(size_of::<T>() <= size_of::<[u32; 6]>());
        let mut words = if self.described != 0 {
            self.desc
        } else if self.resource.is_null() {
            return None;
        } else {
            [
                crate::resource::view_format(self.resource)?,
                dimension,
                0,
                0,
                0,
                0,
            ]
        };
        words[0] = crate::resource::pc_format(words[0]);
        // SAFETY: both host view descriptions are 24 bytes in this layout.
        Some(unsafe { (&raw const words).cast::<T>().read_unaligned() })
    }
}

/// Device method: records the view at `handle` (descriptor memory).
pub(crate) extern "system" fn create_rtv(
    _: This,
    resource: This,
    desc: *const [u32; 6],
    handle: u64,
) {
    target_record(RTV_TAG, resource, desc, handle);
}

/// Device method: a depth view, recorded as render target views are.
pub(crate) extern "system" fn create_dsv(
    _: This,
    resource: This,
    desc: *const [u32; 6],
    handle: u64,
) {
    target_record(DSV_TAG, resource, desc, handle);
}

/// Records a render target or depth view (`tag`) at `handle`, in one write
/// under [`RECORDS`] (a depth view's tag was written over a render target
/// record's after it, so a reader could find the latter).
fn target_record(tag: u32, resource: This, desc: *const [u32; 6], handle: u64) {
    if handle == 0 {
        return;
    }
    // SAFETY: the title passes a description or null.
    let described = unsafe { desc.as_ref() };
    let record = RtvRecord {
        tag,
        described: u32::from(described.is_some()),
        resource,
        desc: described.copied().unwrap_or_default(),
    };
    let _records = records_written();
    // SAFETY: the handle is a descriptor slot, at least 112 bytes.
    unsafe { (handle as *mut RtvRecord).write_unaligned(record) };
}

/// The one lock of every access XTP makes to the records it keeps in
/// descriptor memory (Codex review of 244cf78, finding 7): view creation
/// writes a record's header (tag, view index, generation) under it, the
/// device's descriptor copies copy under it, and every read of a header
/// ([`header`], [`view`], render target and depth records) takes it, so no
/// read finds half of one record and half of another (two threads at the
/// same slot were a data race: `record` wrote under [`VIEWS`] only, and
/// copies took no lock at all). Readers share it; it is held for the
/// access alone, never while taking another lock.
/// shortcut: a title that copies descriptors with its own stores, not the
/// device's methods, takes no lock; what it leaves is read as before (a
/// record of another view, or none).
static RECORDS: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// [`RECORDS`] for a read.
fn records_read() -> std::sync::RwLockReadGuard<'static, ()> {
    RECORDS.read().unwrap_or_else(|e| e.into_inner())
}

/// [`RECORDS`] for a write.
fn records_written() -> std::sync::RwLockWriteGuard<'static, ()> {
    RECORDS.write().unwrap_or_else(|e| e.into_inner())
}

/// The record header at `handle` (tag, view index, generation), read under
/// [`RECORDS`]: one the title's view creation or descriptor copies wrote
/// whole, never part of one.
pub(crate) fn header(handle: u64) -> [u32; 3] {
    let _records = records_read();
    // SAFETY: the handle is a descriptor slot in title memory, at least 16
    // bytes.
    unsafe { (handle as *const [u32; 3]).read_unaligned() }
}
const DSV_TAG: u32 = u32::from_le_bytes(*b"vDSV");

/// A shader view or sampler as the title described it (descriptions in the
/// host's layout, kept as words), for the host to build its own from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum View {
    /// Constant buffer: GPU address and size.
    Constant(u64, u32),
    /// Shader resource: resource, description (none: the resource's own).
    Shader(usize, Option<[u32; 10]>),
    /// Unordered access: resource, counter resource, description.
    Unordered(usize, usize, Option<[u32; 10]>),
    Sampler([u32; 13]),
    /// The console's own sampler description: flags, filters per axis,
    /// addressing, LOD and anisotropy settings, border color, then
    /// console-only tuning words.
    ConsoleSampler([u32; 27]),
    /// A view of memory without a resource object: whether it is unordered
    /// access, the resource description, then the view's (the plain one
    /// followed by the memory address and console fields).
    Placed(bool, [u32; 14], [u32; 16]),
}

impl View {
    /// Whether the view reads buffer memory (constant buffers, buffer views
    /// of resources or of memory) or memory the title placed a texture in:
    /// such views get host slots only for draws.
    #[cfg(windows)]
    fn is_buffer(&self) -> bool {
        match self {
            View::Constant(..) | View::Placed(..) => true,
            View::Shader(r, _) | View::Unordered(r, _, _) => {
                crate::resource::buffer_bytes(*r as This).is_some()
            }
            View::Sampler(_) | View::ConsoleSampler(_) => false,
        }
    }
}

/// Every distinct view in use; a descriptor holds a tag, an index into it
/// and the entry's generation, which survive titles copying descriptors as
/// bytes. A view of a resource lives as long as the resource: when the
/// title releases it, its entries are freed for reuse under a new
/// generation, so descriptors (and host slots made from them) naming the
/// old one go stale instead of naming whatever is made at the same address
/// next.
/// shortcut: views without a resource object (constant buffers, memory
/// views, samplers) are kept for the run; they are bounded by the title's
/// memory and sampler states.
struct Views {
    /// Each entry's view and generation.
    list: Vec<(View, u32)>,
    index: std::collections::BTreeMap<View, u32>,
    /// Entries naming each resource.
    by_resource: std::collections::BTreeMap<usize, Vec<u32>>,
    free: Vec<u32>,
}

static VIEWS: std::sync::Mutex<Views> = std::sync::Mutex::new(Views {
    list: Vec::new(),
    index: std::collections::BTreeMap::new(),
    by_resource: std::collections::BTreeMap::new(),
    free: Vec::new(),
});
const VIEW_TAG: u32 = u32::from_le_bytes(*b"vVIE");

impl View {
    /// The resource objects the view names.
    fn resources(&self) -> [usize; 2] {
        match *self {
            View::Shader(r, _) => [r, 0],
            View::Unordered(r, counter, _) => [r, counter],
            _ => [0, 0],
        }
    }
}

fn record(handle: u64, view: View) {
    if handle == 0 {
        return;
    }
    let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let i = match views.index.get(&view) {
        Some(&i) => i,
        None => {
            let i = match views.free.pop() {
                Some(i) => {
                    views.list[i as usize].0 = view.clone();
                    i
                }
                None => {
                    views.list.push((view.clone(), 1));
                    views.list.len() as u32 - 1
                }
            };
            for r in view.resources().into_iter().filter(|&r| r != 0) {
                views.by_resource.entry(r).or_default().push(i);
            }
            views.index.insert(view, i);
            i
        }
    };
    let generation = views.list[i as usize].1;
    {
        let _records = records_written();
        // SAFETY: the handle is a descriptor slot, at least 16 bytes.
        unsafe { (handle as *mut [u32; 3]).write_unaligned([VIEW_TAG, i, generation]) };
    }
    drop(views);
    #[cfg(windows)]
    mirror(handle);
}

/// The title released `resource`: the views naming it go stale.
pub(crate) fn forget(resource: usize) {
    let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(entries) = views.by_resource.remove(&resource) else {
        return;
    };
    for i in entries {
        let (view, generation) = views.list[i as usize].clone();
        // Entries freed through a view's other resource may hold another
        // view by now.
        if views.index.get(&view) != Some(&i) || !view.resources().contains(&resource) {
            continue;
        }
        views.index.remove(&view);
        views.list[i as usize].1 = generation.wrapping_add(1).max(1);
        // The view's other resource (a counter) keeps a stale entry, which
        // `forget` of that resource skips.
        views.free.push(i);
    }
}

/// The view of entry `i` if `generation` is still its own.
#[cfg(windows)]
fn view_at(i: u32, generation: u32) -> Option<View> {
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let (view, current) = views.list.get(i as usize)?;
    (*current == generation).then(|| view.clone())
}

/// The view recorded at `handle`, if one is (and its resource is alive).
pub(crate) fn view(handle: u64) -> Option<View> {
    if handle == 0 {
        return None;
    }
    let [tag, i, generation] = header(handle);
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let (view, current) = views.list.get(i as usize)?;
    (tag == VIEW_TAG && *current == generation).then(|| view.clone())
}

/// A description of `N` words, if the title passes one.
fn words<const N: usize>(desc: *const [u32; N]) -> Option<[u32; N]> {
    // SAFETY: the title passes a description or null.
    (!desc.is_null()).then(|| unsafe { desc.read_unaligned() })
}

/// Device method.
/// Without a description it makes a null constant view, as D3D12's does:
/// `View::Constant(0, 0)`, which the host makes a null CBV (reads zeros),
/// not the view the slot held before.
pub(crate) extern "system" fn create_cbv(_: This, desc: *const [u32; 4], handle: u64) {
    let [lo, hi, size, _] = words(desc).unwrap_or_default();
    let address = u64::from(lo) | u64::from(hi) << 32;
    record(handle, View::Constant(address, size));
}

/// Device method.
pub(crate) extern "system" fn create_srv(
    _: This,
    resource: This,
    desc: *const [u32; 10],
    handle: u64,
) {
    record(handle, View::Shader(resource as usize, words(desc)));
}

/// Device method.
pub(crate) extern "system" fn create_uav(
    _: This,
    resource: This,
    counter: This,
    desc: *const [u32; 10],
    handle: u64,
) {
    let view = View::Unordered(resource as usize, counter as usize, words(desc));
    record(handle, view);
}

/// Device methods 0x230 and 0x238: shader resource and unordered access
/// views of memory the title placed itself, `(resource desc, view desc,
/// handle)`.
pub(crate) extern "system" fn create_placed_srv(
    _: This,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(resource), Some(desc)) = (words(resource), words(desc)) {
        record(handle, View::Placed(false, resource, desc));
    }
}

pub(crate) extern "system" fn create_placed_uav(
    _: This,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(resource), Some(desc)) = (words(resource), words(desc)) {
        record(handle, View::Placed(true, resource, desc));
    }
}

/// Device method.
pub(crate) extern "system" fn create_sampler(_: This, desc: *const [u32; 13], handle: u64) {
    if let Some(desc) = words(desc) {
        record(handle, View::Sampler(desc));
    }
}

/// Device method 0x1e8.
pub(crate) extern "system" fn create_sampler_x(_: This, desc: *const [u32; 27], handle: u64) {
    if let Some(desc) = words(desc) {
        record(handle, View::ConsoleSampler(desc));
    }
}

/// Device method: `count` descriptors of `kind` from `src` to `dst`.
pub(crate) extern "system" fn copy_simple(_: This, count: u32, dst: u64, src: u64, kind: u32) {
    let step = u64::from(increment(kind).unwrap_or(0));
    let len = u64::from(count) * step;
    if dst != 0 && src != 0 {
        {
            let _records = records_written();
            // SAFETY: both ranges are descriptor slots the title owns.
            unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len as usize) };
        }
        #[cfg(windows)]
        for i in 0..u64::from(count) {
            mirror(dst + i * step);
        }
    }
}

/// Device method: ranges of descriptors (start and count each) gathered
/// from the sources and scattered to the destinations, in order.
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) extern "system" fn copy(
    _: This,
    dst_ranges: u32,
    dst_starts: *const u64,
    dst_sizes: *const u32,
    src_ranges: u32,
    src_starts: *const u64,
    src_sizes: *const u32,
    kind: u32,
) {
    let Some(step) = increment(kind) else {
        return;
    };
    // Each range as (start, count); a null size array means one descriptor
    // per range.
    let ranges = |n: u32, starts: *const u64, sizes: *const u32| -> Vec<(u64, u32)> {
        (0..n as usize)
            .map(|i| {
                // SAFETY: the title passes `n` starts and sizes (or null sizes).
                unsafe {
                    (
                        *starts.add(i),
                        if sizes.is_null() { 1 } else { *sizes.add(i) },
                    )
                }
            })
            .collect()
    };
    if dst_starts.is_null() || src_starts.is_null() {
        return;
    }
    let src = ranges(src_ranges, src_starts, src_sizes);
    let mut dst = ranges(dst_ranges, dst_starts, dst_sizes).into_iter();
    let (mut to, mut room) = dst.next().unwrap_or((0, 0));
    for (mut from, mut left) in src {
        while left > 0 {
            while room == 0 {
                let Some(next) = dst.next() else { return };
                (to, room) = next;
            }
            let n = left.min(room);
            copy_simple(std::ptr::null(), n, to, from, kind);
            let bytes = u64::from(n) * u64::from(step);
            (to, from, room, left) = (to + bytes, from + bytes, room - n, left - n);
        }
    }
}

/// The view recorded at `handle`, if one is.
pub(crate) fn rtv_record(handle: u64) -> Option<RtvRecord> {
    tagged_record(handle, RTV_TAG)
}

/// The depth view recorded at `handle`, if one is.
pub(crate) fn dsv_record(handle: u64) -> Option<RtvRecord> {
    tagged_record(handle, DSV_TAG)
}

fn tagged_record(handle: u64, tag: u32) -> Option<RtvRecord> {
    if handle == 0 {
        return None;
    }
    let _records = records_read();
    // SAFETY: the handle is a descriptor slot the title passes back.
    let record = unsafe { (handle as *const RtvRecord).read_unaligned() };
    (record.tag == tag).then_some(record)
}

/// Device method.
pub(crate) extern "system" fn create_heap(
    _: This,
    desc_ptr: *const HeapDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc_ptr.as_ref() }) else {
        return E_INVALIDARG;
    };
    let Some(size) = increment(desc.kind).map(|i| u64::from(i) * u64::from(desc.count.max(1)))
    else {
        return E_INVALIDARG;
    };
    // SAFETY: read only as a hint: a title that passes the plain
    // description has its own stack or data after it.
    let placement = unsafe { desc_ptr.add(1).cast::<Placement>().read_unaligned() };
    // shortcut: a placement is recognised by its size matching the count,
    // since the plain and extended descriptions share the slot and the
    // interface version; find the flag that tells them apart if a title
    // trips over this.
    let placed = placement.address != 0 && placement.size == size;
    tracing::debug!(?desc, placed, "descriptor heap");
    let (start, memory) = if placed {
        (placement.address, None)
    } else {
        match Memory::zeroed(size, 64) {
            Ok(memory) => (memory.address(), Some(memory)),
            Err(hr) => return hr,
        }
    };
    let visible = desc.flags & SHADER_VISIBLE != 0 && matches!(desc.kind, VIEWS_KIND | SAMPLERS);
    let heap = Box::new(Heap {
        object: Object::new(&FUNCTIONS),
        desc,
        start,
        memory,
        #[cfg(windows)]
        host: std::sync::OnceLock::new(),
        #[cfg(windows)]
        pending: Default::default(),
        #[cfg(windows)]
        life: std::sync::Arc::new(Life {
            heap: std::sync::RwLock::new(0),
        }),
        #[cfg(windows)]
        watched: (placed && visible).then(Default::default),
    });
    // The box does not move: its address is the heap's for its life.
    #[cfg(windows)]
    {
        *heap.life.heap.write().unwrap_or_else(|e| e.into_inner()) =
            std::ptr::from_ref::<Heap>(&heap) as usize;
    }
    #[cfg(windows)]
    if visible {
        let at = std::ptr::from_ref::<Heap>(&heap) as usize;
        let mut heaps = SHADER_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
        heaps.push((start, start + size, at));
    }
    #[cfg(not(windows))]
    let _ = visible;
    object::hand_out(DESCRIPTOR_HEAP, heap, iid, &HEAP_IIDS, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &HEAP_IIDS).then_some(this);
    object::answer(DESCRIPTOR_HEAP, iid, found, out)
}
/// The title's release of a heap. Work holding the heap ([`HeapHold`])
/// ends, and the heap is marked gone for work after, before the last
/// release rebuilds its box: before, the heap's drop did that, once the box
/// was rebuilt, so the box and that work's `&Heap` were alive together
/// (stride4c, Codex review of stride4b, P1). A release that is not the last
/// leaves the heap as it was; work waiting meanwhile finds it there.
extern "system" fn release(this: This) -> u32 {
    #[cfg(windows)]
    {
        let life = heap(this).life.clone();
        let mut at = life.heap.write().unwrap_or_else(|e| e.into_inner());
        let address = std::mem::replace(&mut *at, 0);
        #[cfg(test)]
        if let Some(between) = RELEASE_BETWEEN.with(|b| b.borrow_mut().take()) {
            between();
        }
        let before = RELEASING.replace(this as usize);
        let left = object::release_boxed::<Heap>(this);
        RELEASING.set(before);
        if left != 0 {
            *at = address;
        }
        left
    }
    #[cfg(not(windows))]
    object::release_boxed::<Heap>(this)
}

#[cfg(windows)]
thread_local! {
    /// The heap whose release this thread runs ([`release`]), which holds
    /// the lock of its life for writing: the heap's drop leaves it alone.
    static RELEASING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, windows))]
thread_local! {
    /// Run once by the next [`release`] on this thread, once the heap is
    /// marked gone and before the box may be rebuilt (tests: work asking
    /// for the heap then).
    static RELEASE_BETWEEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}
extern "system" fn desc(this: This, out: *mut HeapDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(heap(this).desc) };
    }
}
/// The same address for the CPU and the GPU start.
extern "system" fn start(this: This) -> u64 {
    heap(this).start
}

/// IUnknown 3, object 4, device child 1, heap 3.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<DESCRIPTOR_HEAP, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = desc as *const (); // 0x40
    f[9] = start as *const (); // 0x48 CPU
    f[10] = start as *const (); // 0x50 GPU
    Table(f)
};

/// A shader-visible title view heap for tests, released when dropped.
#[cfg(all(test, windows))]
pub(crate) struct TestHeap(pub(crate) This);

#[cfg(all(test, windows))]
impl TestHeap {
    /// A heap of `count` view slots, made as the title makes one.
    pub(crate) fn views(count: u32) -> TestHeap {
        /// What newer titles pass, so that reading the placement stays
        /// inside the value.
        #[repr(C)]
        struct Extended(HeapDesc, Placement);
        let extended = Extended(
            HeapDesc {
                kind: VIEWS_KIND,
                count,
                flags: SHADER_VISIBLE,
                nodes: 0,
            },
            Placement {
                address: 0,
                size: 0,
            },
        );
        let mut out = std::ptr::null();
        let made = create_heap(std::ptr::null(), &extended.0, &HEAP_IIDS[0], &mut out);
        assert_eq!(made, crate::object::S_OK, "a title view heap");
        TestHeap(out)
    }
}

#[cfg(all(test, windows))]
impl Drop for TestHeap {
    fn drop(&mut self) {
        release(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, slot};

    /// What newer titles pass; tests use it throughout, so that reading the
    /// placement stays inside the value.
    #[repr(C)]
    struct Extended(HeapDesc, Placement);

    fn create(desc: HeapDesc, placement: Placement, out: &mut This) -> HRESULT {
        let extended = Extended(desc, placement);
        create_heap(std::ptr::null(), &extended.0, &HEAP_IIDS[0], out)
    }

    #[test]
    fn heaps_hold_count_descriptors_from_their_start() {
        let null = std::ptr::null();
        let desc = HeapDesc {
            kind: 0,
            count: 1000,
            flags: 1,
            nodes: 0,
        };
        let none = Placement {
            address: 0,
            size: 0,
        };
        let mut h = null;
        assert_eq!(create(desc, none, &mut h), S_OK);
        let cpu = slot::<extern "system" fn(This) -> u64>(h, 0x48)(h);
        assert_eq!(cpu % 64, 0);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), cpu);
        assert_eq!(increment_size(null, 0), 32);
        // SAFETY: the heap holds 1000 descriptors of 32 bytes.
        unsafe { (cpu as *mut u8).add(999 * 32 + 31).write(1) };
        assert_eq!(increment_size(null, 9), 0);
        let bad = HeapDesc { kind: 9, ..desc };
        assert_eq!(create(bad, none, &mut h), E_INVALIDARG);
    }

    /// A constant view made without a description is a null one, as on
    /// D3D12: it replaces the view the slot held, which the host would
    /// otherwise go on reading.
    #[test]
    fn a_constant_view_without_a_description_is_a_null_one() {
        let null = std::ptr::null();
        let heap = [0u64; 4]; // one 32-byte slot
        let s0 = heap.as_ptr() as u64;
        create_cbv(null, &[0x2000, 1, 512, 0], s0);
        assert_eq!(view(s0), Some(View::Constant(0x1_0000_2000, 512)));
        create_cbv(null, std::ptr::null(), s0);
        assert_eq!(view(s0), Some(View::Constant(0, 0)));
    }

    #[test]
    fn views_survive_copies() {
        let null = std::ptr::null();
        let mut heap = vec![0u64; 4 * 8]; // four 32-byte slots
        let at = |i: usize| heap.as_ptr() as u64 + 32 * i as u64;
        let (s0, s1, s2, s3) = (at(0), at(1), at(2), at(3));
        create_cbv(null, &[0x1000, 1, 256, 0], s0);
        assert_eq!(view(s0), Some(View::Constant(0x1_0000_1000, 256)));
        create_srv(null, null, std::ptr::null(), s1);
        assert_eq!(view(s1), Some(View::Shader(0, None)));
        // The same view twice is one entry.
        create_srv(null, null, std::ptr::null(), s2);
        // SAFETY: both slots are in `heap`.
        assert_eq!(unsafe { *(s1 as *const [u32; 3]) }, unsafe {
            *(s2 as *const [u32; 3])
        });
        // Two ranges of one into one range of two.
        let (dst, dst_n, src, src_n) = ([s2], [2u32], [s0, s1], [1u32, 1]);
        copy(
            null,
            1,
            dst.as_ptr(),
            dst_n.as_ptr(),
            2,
            src.as_ptr(),
            src_n.as_ptr(),
            0,
        );
        assert_eq!(view(s2), view(s0));
        assert_eq!(view(s3), view(s1));
        assert_eq!(view(at(0) + 8), None);
        heap[0] = 0;
    }

    /// A released resource's views go stale; the same view of a new
    /// resource at the same address is a new record.
    #[test]
    fn views_die_with_their_resource() {
        let null = std::ptr::null();
        let slots = [0u64; 2 * 4];
        let (a, b) = (slots.as_ptr() as u64, slots.as_ptr() as u64 + 32);
        let resource = 0x7777_0000usize as This;
        create_srv(null, resource, std::ptr::null(), a);
        assert_eq!(view(a), Some(View::Shader(0x7777_0000, None)));
        forget(0x7777_0000);
        assert_eq!(view(a), None, "stale after release");
        create_srv(null, resource, std::ptr::null(), b);
        assert_eq!(view(b), Some(View::Shader(0x7777_0000, None)));
        // SAFETY: both slots are in `slots`.
        let (old, new) = unsafe { (*(a as *const [u32; 3]), *(b as *const [u32; 3])) };
        assert_ne!(old, new, "a new generation");
    }

    /// Codex review of a090f22, P1: the work a list runs as it closes kept
    /// the title heap's address, and took a heap made at that address after
    /// the title released the first for the first. A hold is of one heap:
    /// gone with it, whatever is made at its address after.
    #[cfg(windows)]
    #[test]
    fn a_hold_ends_with_its_heap_not_with_its_address() {
        let first = TestHeap::views(8);
        let at = first.0 as usize;
        let hold = heap(first.0).hold();
        assert_eq!(hold.with(|h| h.start), Some(heap(first.0).start));
        drop(first);
        assert_eq!(hold.with(|h| h.start), None, "released");
        // Heaps made until one takes the address (the allocator usually
        // gives it back at once), and a few more if none does.
        let mut later = Vec::new();
        for _ in 0..64 {
            let h = TestHeap::views(8);
            let same = h.0 as usize == at;
            later.push(h);
            if same {
                break;
            }
        }
        assert_eq!(hold.with(|h| h.start), None, "another heap");
        let last = later.last().expect("a heap");
        assert!(heap(last.0).hold().with(|_| ()).is_some());
    }

    /// Codex review of the port #1331 (task 69adac42, point 1): a slot a
    /// list took from a heap goes back to that heap alone. Given back after
    /// the title released the heap and made another at its address, it
    /// leaves the slot the new heap gave another list since taken.
    #[cfg(windows)]
    #[test]
    fn a_slot_goes_back_to_its_own_heap_not_to_one_made_at_its_address() {
        let test =
            "descriptor::tests::a_slot_goes_back_to_its_own_heap_not_to_one_made_at_its_address";
        let Some(_gpu) = crate::testgpu::device(test) else {
            return;
        };
        let first = TestHeap::views(8);
        let at = first.0 as usize;
        let host = heap(first.0).host().expect("a host heap");
        let slot = host.dynamic(1).expect("a dynamic slot");
        // List A's lease.
        let mut a = Held::default();
        a.add_range(at, slot, 1);
        drop(first);
        // Heaps made until one takes the address (the allocator usually
        // gives it back at once).
        let mut later = Vec::new();
        for _ in 0..64 {
            let h = TestHeap::views(8);
            let same = h.0 as usize == at;
            later.push(h);
            if same {
                break;
            }
        }
        let second = later.last().expect("a heap");
        if second.0 as usize != at {
            // No heap at the address: nothing to tell apart.
            eprintln!("{test}: no heap made at the released one's address; not exercised");
            return;
        }
        let host = heap(second.0).host().expect("a host heap");
        // List B's slot: the same one, of the new heap.
        assert_eq!(host.dynamic(1), Some(slot));
        give_back(&mut a);
        assert_eq!(a, Held::default());
        assert_ne!(host.dynamic(1), Some(slot), "B's slot stays taken");
        // B's own lease goes back to its heap.
        let mut b = Held::default();
        b.add_range(at, slot, 1);
        give_back(&mut b);
        assert_eq!(host.dynamic(1), Some(slot), "given back");
    }

    /// Codex review of a090f22, P1: the close-time work checked the heap
    /// under the registry's lock, let it go, then used the heap, which a
    /// concurrent last release could free between the two. A release now
    /// waits for the work that holds the heap.
    #[cfg(windows)]
    #[test]
    fn a_release_waits_for_work_that_holds_the_heap() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let made = TestHeap::views(8);
        let this = made.0 as usize;
        // The thread below releases it.
        std::mem::forget(made);
        let hold = heap(this as This).hold();
        let released = std::sync::Arc::new(AtomicBool::new(false));
        let releaser = {
            let released = released.clone();
            move || {
                release(this as This);
                released.store(true, Ordering::SeqCst);
            }
        };
        let mut thread = None;
        let start = hold.with(|h| {
            thread = Some(std::thread::spawn(releaser));
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert!(!released.load(Ordering::SeqCst), "the release waits");
            h.start
        });
        assert!(start.is_some_and(|s| s != 0));
        thread
            .expect("a releasing thread")
            .join()
            .expect("released");
        assert!(released.load(Ordering::SeqCst));
        assert!(hold.with(|_| ()).is_none());
    }

    /// stride4c, Codex review of stride4b, P1: the last release rebuilt the
    /// heap's box before its drop marked it gone, while work holding the
    /// heap still had it. Work asking for the heap once a release has begun
    /// waits until the release knows whether it was the last: then it finds
    /// the heap gone (the box is rebuilt only after), or, for a release
    /// that was not the last, there as before.
    #[cfg(windows)]
    #[test]
    fn work_asking_during_a_release_finds_the_heap_gone_only_after_the_last() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let made = TestHeap::views(8);
        let this = made.0;
        // The last release below is the test's own.
        std::mem::forget(made);
        let start = heap(this).start;
        let hold = heap(this).hold();
        // Releases `this` while another thread asks for the heap: what it
        // found, and the release's count.
        let release_asked = |hold: HeapHold| {
            let asker = std::sync::Arc::new(std::sync::Mutex::new(None));
            let done = std::sync::Arc::new(AtomicBool::new(false));
            let (spawned, finished) = (asker.clone(), done.clone());
            RELEASE_BETWEEN.with(|b| {
                *b.borrow_mut() = Some(Box::new(move || {
                    let waited = finished.clone();
                    let thread = std::thread::spawn(move || {
                        let found = hold.with(|h| h.start);
                        waited.store(true, Ordering::SeqCst);
                        found
                    });
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    assert!(!finished.load(Ordering::SeqCst), "the work waits");
                    *spawned.lock().unwrap() = Some(thread);
                }));
            });
            let left = release(this);
            let thread = asker.lock().unwrap().take().expect("asked meanwhile");
            (thread.join().expect("the asking thread"), left)
        };
        object::add_ref(this);
        assert_eq!(release_asked(hold.clone()), (Some(start), 1), "not the last");
        assert_eq!(release_asked(hold.clone()), (None, 0), "the last");
        assert!(hold.with(|_| ()).is_none());
    }

    /// A record across a page's start or end is looked at from either page;
    /// pages before and past the heap hold none of its slots.
    #[cfg(windows)]
    #[test]
    fn slots_of_a_changed_page_reach_across_its_ends() {
        use crate::watch::PAGE;
        // 16 bytes into a page: slot 2047 lies across the next page's start.
        let start = 5 * PAGE + 16;
        let count = 3 * PAGE / 32;
        assert_eq!(page_slots(5, start, 32, count), 0..2048);
        assert_eq!(page_slots(6, start, 32, count), 2047..4096);
        // The heap's last 16 bytes are on page 8.
        assert_eq!(page_slots(8, start, 32, count), 6143..6144);
        assert_eq!(page_slots(4, start, 32, count), 0..0);
        assert_eq!(page_slots(9, start, 32, count), 0..0);
        // Samplers, 16 bytes each, from a page's start.
        assert_eq!(page_slots(2, 2 * PAGE, 16, 8192), 0..4096);
        assert_eq!(page_slots(3, 2 * PAGE, 16, 8192), 4096..8192);
    }

    /// Title memory of `watched` write-watched blocks (as the runtime
    /// reserves the title's GPU memory), then `plain` blocks no write watch
    /// answers for at the addresses after them; freed when dropped.
    #[cfg(windows)]
    struct TitlePages {
        at: usize,
        parts: Vec<usize>,
    }

    #[cfg(windows)]
    impl TitlePages {
        fn new(watched: usize, plain: usize) -> TitlePages {
            use windows_sys::Win32::System::Memory::{
                MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
                VirtualFree,
            };
            const MEM_WRITE_WATCH: u32 = 0x20_0000;
            let page = crate::watch::PAGE;
            for _ in 0..64 {
                // SAFETY: allocations of our own, where the system finds
                // room, then at the addresses it gave (which another thread
                // may take meanwhile: then again).
                unsafe {
                    let room = VirtualAlloc(
                        std::ptr::null(),
                        (watched + plain) * page,
                        MEM_RESERVE,
                        PAGE_NOACCESS,
                    );
                    assert!(!room.is_null());
                    VirtualFree(room, 0, MEM_RELEASE);
                    let at = room as usize;
                    let first = VirtualAlloc(
                        room,
                        watched * page,
                        MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                        PAGE_READWRITE,
                    );
                    if first.is_null() {
                        continue;
                    }
                    let mut parts = vec![first as usize];
                    if plain > 0 {
                        let second = VirtualAlloc(
                            (at + watched * page) as *const _,
                            plain * page,
                            MEM_RESERVE | MEM_COMMIT,
                            PAGE_READWRITE,
                        );
                        if second.is_null() {
                            VirtualFree(first, 0, MEM_RELEASE);
                            continue;
                        }
                        parts.push(second as usize);
                    }
                    return TitlePages { at, parts };
                }
            }
            panic!("no room for the title's pages");
        }
    }

    #[cfg(windows)]
    impl Drop for TitlePages {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
            for &part in &self.parts {
                // SAFETY: our own allocations, no longer used.
                unsafe { VirtualFree(part as *mut _, 0, MEM_RELEASE) };
            }
        }
    }

    /// A shader-visible view heap of `count` slots the title placed at
    /// `address`, which [`sync_heaps`] on other tests' threads leaves to
    /// this test; [`release_placed`] releases it.
    #[cfg(windows)]
    fn placed_views(address: u64, count: u32) -> This {
        let desc = HeapDesc {
            kind: VIEWS_KIND,
            count,
            flags: SHADER_VISIBLE,
            nodes: 1,
        };
        let placement = Placement {
            address,
            size: u64::from(count) * 32,
        };
        let mut h = std::ptr::null();
        assert_eq!(create(desc, placement, &mut h), S_OK);
        assert!(heap(h).watched.is_some(), "placed: looked at");
        LOOKED_AT_BY_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(h as usize);
        h
    }

    #[cfg(windows)]
    fn release_placed(h: This) {
        LOOKED_AT_BY_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|&at| at != h as usize);
        release(h);
    }

    /// One look at `title` for records copied in as bytes.
    #[cfg(windows)]
    fn look(title: &Heap) -> Copies {
        let mut found = Copies::default();
        title.look_for_copies(&mut found);
        found
    }

    /// Copies the 32 bytes of title slot `from` into slot `to` with plain
    /// stores, as the title copies records: no call sees it.
    #[cfg(windows)]
    fn copy_bytes(title: &Heap, from: u32, to: u32) {
        let at = |i: u32| (title.start + u64::from(i) * 32) as usize;
        // SAFETY: both are slots of the heap, in memory the test holds.
        unsafe { std::ptr::copy_nonoverlapping(at(from) as *const u8, at(to) as *mut u8, 32) };
    }

    /// The counter of records the title copies into its placed heap as
    /// bytes (the first step of the port of VIctory 86a5ed6 and the fork's
    /// a26859f): a record copied in with plain stores is counted once, in
    /// whichever block it lands, with what it holds, and so is one whose
    /// bytes changed only in the second of the two blocks it lies across
    /// (fork #802: such records were left out); records written through the
    /// device's view calls and descriptor copies are not.
    #[cfg(windows)]
    #[test]
    fn records_copied_as_bytes_are_counted_once_and_calls_are_not() {
        use crate::resource::tests_support::{committed, sampled};
        use crate::watch::PAGE;
        let test = "descriptor::tests::records_copied_as_bytes_are_counted_once_and_calls_are_not";
        let Some(_gpu) = crate::testgpu::device(test) else {
            return;
        };
        let null = std::ptr::null();
        let memory = TitlePages::new(3, 0);
        // 24 bytes into the first block: the header of slot 2047 lies across
        // the first two blocks, its generation in the second.
        let h = placed_views(memory.at as u64 + 24, ((3 * PAGE - 24) / 32) as u32);
        let title = heap(h);
        let host = title.host().expect("a host heap");
        let at = |i: u32| title.start + u64::from(i) * 32;
        // Through the device: a constant buffer view, a view of a texture
        // the title fills, a descriptor copy of it, and the same view in the
        // slot across the blocks.
        let texture = committed(&sampled(16, 16, 28));
        create_cbv(null, &[0x1000, 1, 256, 0], at(1));
        let texture_2d = [28, 4, 0x1688, 0, 0, 1, 0, 0, 0, 0];
        create_srv(null, texture, &texture_2d, at(2));
        copy_simple(null, 1, at(3), at(2), VIEWS_KIND);
        create_srv(null, texture, &texture_2d, at(2047));
        let first = look(title);
        assert!(first.pages >= 2, "{first:?}");
        assert_eq!(first.copied, 0, "the title's calls: {first:?}");
        // The same records with plain stores: the constant buffer in the
        // first block, the texture in the second, and in the third into a
        // slot a draw read through a table.
        copy_bytes(title, 1, 10);
        copy_bytes(title, 2, 4000);
        host.texture_slot_tabled(5000);
        copy_bytes(title, 2, 5000);
        let second = look(title);
        let kinds = (second.copied, second.textures, second.tabled, second.other);
        assert_eq!(kinds, (3, 2, 1, 1), "{second:?}");
        assert_eq!(second.picked(), 1, "the texture no table read: {second:?}");
        assert_eq!(second.unmade, 3, "their host slots are as they were");
        assert_eq!(second.pages, 3, "a page in each block: {second:?}");
        // Nothing since, then the same record copied again: nothing new.
        assert_eq!(look(title).copied, 0);
        copy_bytes(title, 2, 4000);
        assert_eq!(look(title).copied, 0, "the record it had");
        // The generation of the record across the blocks rewritten with a
        // plain store: only the second block changed, and the record is
        // found from it (a stale generation: no view of ours).
        let [_, _, generation] = header(at(2047));
        assert_eq!(
            (at(2047) + 8) % PAGE as u64,
            0,
            "the second block's first word"
        );
        // SAFETY: the slot's third word, in memory the test holds.
        unsafe { ((at(2047) + 8) as *mut u32).write_unaligned(generation ^ 0xffff) };
        let across = look(title);
        let found = (across.pages, across.copied, across.none);
        assert_eq!(found, (1, 1, 1), "{across:?}");
        // Bytes that hold no record of ours over a copied slot: counted.
        // SAFETY: a slot of the heap.
        unsafe { (at(10) as *mut u8).write_bytes(0, 32) };
        let cleared = look(title);
        assert_eq!((cleared.copied, cleared.none), (1, 1), "{cleared:?}");
        // A view call over a copied slot: the title's own write.
        create_cbv(null, &[0x2000, 1, 256, 0], at(4000));
        assert_eq!(look(title).copied, 0);
        release_placed(h);
        type Release = extern "system" fn(This) -> u32;
        slot::<Release>(texture, 0x10)(texture);
        drop(memory);
    }

    /// The write watch decides for a range by its first block (`watch.rs`
    /// `with_writes` and `guard`): asked for a heap over a write-watched
    /// block and blocks it does not watch, it answers nothing at all, and a
    /// look asking once for the heap (as the fork's `Tracked::take` did)
    /// would never see the watched block change. The look asks per run of
    /// blocks it answers for: a record copied into the watched block is
    /// found, and the other blocks are counted.
    #[cfg(windows)]
    #[test]
    fn a_heap_the_watch_answers_for_in_part_is_looked_at_where_it_does() {
        use crate::watch::PAGE;
        let test =
            "descriptor::tests::a_heap_the_watch_answers_for_in_part_is_looked_at_where_it_does";
        let Some(_gpu) = crate::testgpu::device(test) else {
            return;
        };
        let null = std::ptr::null();
        let memory = TitlePages::new(1, 2);
        assert_eq!(
            crate::watch::writes(memory.at, memory.at + 3 * PAGE, true),
            None,
            "no answer for the whole range"
        );
        let h = placed_views(memory.at as u64, (3 * PAGE / 32) as u32);
        let title = heap(h);
        title.host().expect("a host heap");
        create_cbv(null, &[0x3000, 1, 256, 0], title.start + 32);
        let first = look(title);
        assert_eq!((first.copied, first.unwatched), (0, 2), "{first:?}");
        copy_bytes(title, 1, 20);
        let second = look(title);
        assert_eq!(second.copied, 1, "found in the watched block: {second:?}");
        release_placed(h);
        drop(memory);
    }

    /// BTP 6 places its shader-visible heap in its own memory
    /// and checks that the heap starts there.
    #[test]
    fn placed_heaps_start_at_the_title_memory() {
        let mut memory = vec![0u8; 64 * 32];
        let address = memory.as_mut_ptr() as u64;
        let desc = HeapDesc {
            kind: 0,
            count: 64,
            flags: 1,
            nodes: 1,
        };
        let mut h = std::ptr::null();
        assert_eq!(
            create(
                desc,
                Placement {
                    address,
                    size: 64 * 32
                },
                &mut h
            ),
            S_OK
        );
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), address);
        // A size that does not match the count is not a placement.
        assert_eq!(create(desc, Placement { address, size: 7 }, &mut h), S_OK);
        assert_ne!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
    }
}
