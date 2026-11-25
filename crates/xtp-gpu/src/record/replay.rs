//! The replay: a recording's calls made again on our device, with no title
//! code running, in the order they were recorded.
//!
//! - Title memory is reserved where the session had it (the snapshot's
//!   ranges, before anything else allocates), restored when the recorded
//!   frames start, then changed as the captures say: the CPU's pages before
//!   each submission; after it, the pages the session wrote back are
//!   compared with what the replay wrote back ([`Gpu`]).
//! - Objects are made again by their recorded calls; calls name them by
//!   their session address, mapped to the replay's. Memory our objects own
//!   elsewhere (descriptor heaps, heaps, uploads) is at other addresses:
//!   addresses and descriptor handles inside it are translated.
//! - Before the recorded frames, only what the frames need is made again:
//!   objects alive when they start, and what those were made from. Objects
//!   the title released before are left out, with the views naming them.
//! - The frame count is brought to the session's before the recorded
//!   frames (empty presents), so that frame-numbered diagnostics
//!   (`XTP_CAPTURE_FRAMES`, `XTP_PASS_DUMP`, `XTP_FRAME_PROBE`) name the
//!   session's frames.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::memory;
use super::methods::{self, DEVICE, LIST, QUEUE};
use super::proxy;
use super::store::{self, StreamReader};
use super::wire::{Arg, Blob, Event, Fix, Page, Reader};
use crate::object::This;

/// A value written into memory at every point the replay changes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Poke {
    pub at: u64,
    pub bytes: Vec<u8>,
    /// Only while this frame is replayed; every frame if `None`.
    pub frame: Option<u64>,
}

/// What to replay.
#[derive(Debug, Clone)]
pub struct Options {
    pub dir: PathBuf,
    /// Recorded frames to report, by session number (all if `None`); the
    /// frames before are replayed all the same.
    pub frames: Option<(u64, u64)>,
    /// Times the recorded frames are replayed (memory restored each time).
    pub loops: u32,
    pub pokes: Vec<Poke>,
    /// Pages the session wrote back after a submission are written over the
    /// replay's own, as recorded (exact session memory, but GPU results the
    /// replay computes differently are overwritten).
    pub session_gpu_pages: bool,
    /// Runs as the only user of this process's memory and our allocator
    /// hooks: title memory reserved at its addresses, committed resources
    /// placed where the session had them. Off in tests (another test's
    /// memory may sit there).
    pub own_process: bool,
}

/// A replayed frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// The session's number.
    pub frame: u64,
    /// The loop it belongs to (from 0).
    pub pass: u32,
    pub calls: u64,
    pub draws: u64,
    pub dispatches: u64,
    /// Draws and dispatches run on the host.
    pub drawn: u64,
    pub dispatched: u64,
    /// CPU pages written (changed ones only).
    pub pages: u64,
    /// Pages the session wrote back after its submissions: the same in the
    /// replay, written as recorded (the replay left them as they were
    /// before), or different (the replay's own result kept).
    pub gpu_same: u64,
    pub gpu_written: u64,
    pub gpu_different: u64,
    /// Calls on objects the replay does not have.
    pub missing: u64,
    pub ms: u64,
}

/// What a replay did in all.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub frames: Vec<Frame>,
    /// Calls made before the recorded frames, and those left out.
    pub journal_calls: u64,
    pub journal_skipped: u64,
    /// Snapshot memory that could not be reserved at its address.
    pub unplaced_pages: u64,
    pub setup_ms: u64,
    /// The objects the replay made, by session address: (session, replay).
    pub objects: Vec<(u64, usize)>,
}

/// Replays a recording; `frame` is told each frame as it is presented.
pub fn run(options: &Options, mut frame: impl FnMut(&Frame)) -> Result<Summary, String> {
    let started = std::time::Instant::now();
    let calls = options.dir.join("calls.bin");
    let mut replay = Replay::new(options);
    if options.own_process {
        // First, before reading the calls allocates anything.
        replay.reserve_title_memory()?;
        crate::resource::use_allocator(allocate, free);
        crate::resource::use_page_mapper(map_pages);
    }
    let plan = Plan::make(&calls)?;
    let mut window = Vec::new();
    let mut summary = Summary::default();
    let mut reader = Events::open(&calls)?;
    let mut index = 0usize;
    while let Some(event) = reader.next()? {
        if index >= plan.window {
            window.push(event);
            index += 1;
            continue;
        }
        if plan.keep(index) {
            replay.journal(&event)?;
            summary.journal_calls += u64::from(matches!(event, Event::Call { .. }));
        } else {
            summary.journal_skipped += 1;
        }
        index += 1;
    }
    summary.setup_ms = started.elapsed().as_millis() as u64;
    for pass in 0..options.loops.max(1) {
        replay.pass = pass;
        for event in &window {
            if let Some(done) = replay.window(event)? {
                let wanted = options
                    .frames
                    .is_none_or(|(a, b)| (a..=b).contains(&done.frame));
                if wanted {
                    frame(&done);
                    summary.frames.push(done);
                }
            }
        }
    }
    summary.unplaced_pages = replay.unplaced;
    summary.objects = replay
        .objects
        .iter()
        .map(|(&s, &r)| (s, r as usize))
        .collect();
    Ok(summary)
}

/// A stream's events, a block at a time.
struct Events {
    stream: StreamReader,
    block: Vec<u8>,
    at: usize,
}

impl Events {
    fn open(path: &Path) -> Result<Events, String> {
        Ok(Events {
            stream: StreamReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?,
            block: Vec::new(),
            at: 0,
        })
    }

    fn next(&mut self) -> Result<Option<Event>, String> {
        while self.at >= self.block.len() {
            match self.stream.block().map_err(|e| e.to_string())? {
                Some(b) => {
                    self.block = b;
                    self.at = 0;
                }
                None => return Ok(None),
            }
        }
        let mut r = Reader::new(&self.block[self.at..]);
        let event = r.event().map_err(|e| e.to_string())?;
        self.at = self.block.len() - r.rest();
        Ok(Some(event))
    }
}

/// What the journal's events are, for [`Plan`]: an object made (its
/// incarnation is the event's index), a call naming objects, a release.
enum Use {
    Made(Vec<usize>),
    Names(Vec<usize>),
    Gone(usize),
    Owned(Option<usize>),
    Other,
}

/// Which journal events the replay makes again (see the module).
struct Plan {
    /// The index of the event that starts the recorded frames (the end if
    /// there is none).
    window: usize,
    uses: Vec<Use>,
    kept: HashSet<usize>,
    /// Objects the recorded frames name whose release came before: kept.
    named_late: HashSet<usize>,
}

impl Plan {
    fn make(path: &Path) -> Result<Plan, String> {
        let mut events = Events::open(path)?;
        let mut current: HashMap<u64, usize> = HashMap::new();
        let mut uses = Vec::new();
        let mut alive: HashSet<usize> = HashSet::new();
        let mut window = None;
        let mut named_late = HashSet::new();
        let mut later_names: HashSet<usize> = HashSet::new();
        let mut index = 0usize;
        while let Some(event) = events.next()? {
            let in_window = window.is_some();
            let mut named = Vec::new();
            let objects =
                |names: &mut Vec<usize>, current: &HashMap<u64, usize>, this: u64, args: &[Arg]| {
                    for o in std::iter::once(this).chain(objects_in(args)) {
                        if let Some(&i) = current.get(&o) {
                            names.push(i);
                        }
                    }
                };
            let used = match &event {
                Event::Window { .. } if window.is_none() => {
                    window = Some(index);
                    Use::Other
                }
                Event::Call { this, args, .. } => {
                    objects(&mut named, &current, *this, args);
                    let made: Vec<u64> = args
                        .iter()
                        .filter_map(|a| match a {
                            Arg::Out { made, .. } if *made != 0 => Some(*made),
                            _ => None,
                        })
                        .collect();
                    for &m in &made {
                        current.insert(m, index);
                        alive.insert(index);
                    }
                    if made.is_empty() {
                        Use::Names(named.clone())
                    } else {
                        Use::Made(named.clone())
                    }
                }
                Event::Destroy { this } => match current.remove(this) {
                    Some(i) => {
                        if in_window {
                            Use::Other
                        } else {
                            alive.remove(&i);
                            Use::Gone(i)
                        }
                    }
                    None => Use::Other,
                },
                Event::Owned { this, .. } => Use::Owned(current.get(this).copied()),
                _ => Use::Other,
            };
            if in_window {
                later_names.extend(named);
            } else {
                uses.push(used);
            }
            index += 1;
        }
        let window = window.unwrap_or(index);
        // What the frames need: objects alive when they start, those they
        // name, and what those were made from.
        let mut kept: HashSet<usize> = alive.iter().copied().filter(|&i| i < window).collect();
        for &i in &later_names {
            if i < window && kept.insert(i) {
                named_late.insert(i);
            }
        }
        let mut todo: Vec<usize> = kept.iter().copied().collect();
        while let Some(i) = todo.pop() {
            if let Some(Use::Made(from)) = uses.get(i) {
                for &f in from {
                    if kept.insert(f) {
                        todo.push(f);
                    }
                }
            }
        }
        named_late.retain(|i| !alive.contains(i));
        Ok(Plan {
            window,
            uses,
            kept,
            named_late,
        })
    }

    /// Whether journal event `index` is made again.
    fn keep(&self, index: usize) -> bool {
        match self.uses.get(index) {
            Some(Use::Made(_)) => self.kept.contains(&index),
            Some(Use::Names(names)) => names.iter().all(|n| self.kept.contains(n)),
            Some(Use::Gone(i)) => self.kept.contains(i) && !self.named_late.contains(i),
            Some(Use::Owned(owner)) => owner.is_none_or(|o| self.kept.contains(&o)),
            Some(Use::Other) | None => true,
        }
    }
}

/// The objects an event's arguments name (session addresses).
fn objects_in(args: &[Arg]) -> Vec<u64> {
    fn blob(b: &Blob, out: &mut Vec<u64>) {
        for (at, fix) in &b.fixups {
            match fix {
                Fix::Obj => out.push(word(&b.bytes, *at as usize)),
                Fix::Blob(inner) => blob(inner, out),
                Fix::Addr | Fix::Handle => {}
            }
        }
    }
    let mut out = Vec::new();
    for a in args {
        match a {
            Arg::Obj(o) => out.push(*o),
            Arg::Blob(b) => blob(b, &mut out),
            _ => {}
        }
    }
    out.retain(|&o| o != 0);
    out
}

fn word(bytes: &[u8], at: usize) -> u64 {
    bytes
        .get(at..at + 8)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
}

/// The replay's state.
struct Replay<'a> {
    /// Memory of replay objects at their session addresses.
    identity: Vec<(u64, u64)>,
    options: &'a Options,
    /// Session object → replay object.
    objects: HashMap<u64, This>,
    kinds: HashMap<u64, u8>,
    /// Session memory → (length, replay memory), where they differ.
    moved: BTreeMap<u64, (u64, u64)>,
    /// The last call's session and replay notes (or results), which the
    /// `Owned` events after it are about.
    noted: (u64, u64),
    /// Title memory reserved by the replay.
    reserved: Vec<(u64, u64)>,
    unplaced: u64,
    frame: Frame,
    pass: u32,
    /// Counters at the frame's start.
    drawn: u64,
    dispatched: u64,
    started: std::time::Instant,
    device: This,
}

impl<'a> Replay<'a> {
    fn new(options: &'a Options) -> Replay<'a> {
        Replay {
            identity: Vec::new(),
            options,
            objects: HashMap::new(),
            kinds: HashMap::new(),
            moved: BTreeMap::new(),
            noted: (0, 0),
            reserved: Vec::new(),
            unplaced: 0,
            frame: Frame::default(),
            pass: 0,
            drawn: 0,
            dispatched: 0,
            started: std::time::Instant::now(),
            device: std::ptr::null(),
        }
    }

    /// Reserves the snapshot's memory at its addresses, before anything
    /// else may take them.
    fn reserve_title_memory(&mut self) -> Result<(), String> {
        let path = self.options.dir.join("memory.bin");
        let mut ranges = Vec::new();
        store::read_memory(&path, 1, |u| {
            ranges.push((u.at, (u.count * super::wire::PAGE) as u64));
        })
        .map_err(|e| format!("{}: {e}", path.display()))?;
        for (at, len) in memory::merge(ranges) {
            if reserve(at, len) {
                self.reserved.push((at, len));
            } else {
                self.unplaced += len / super::wire::PAGE as u64;
                tracing::warn!(
                    at = format_args!("{at:#x}"),
                    len,
                    "replay: title memory taken here; its pages are left out"
                );
            }
        }
        Ok(())
    }

    fn is_reserved(&self, at: u64, len: u64) -> bool {
        self.reserved
            .iter()
            .any(|&(a, l)| a <= at && at + len <= a + l)
    }

    /// Where session memory `at` is in the replay.
    fn translate(&self, at: u64) -> u64 {
        match self.moved.range(..=at).next_back() {
            Some((&start, &(len, to))) if at < start + len => at - start + to,
            _ => at,
        }
    }

    fn object(&mut self, session: u64) -> This {
        if session == 0 {
            return std::ptr::null();
        }
        match self.objects.get(&session) {
            Some(&o) => o,
            None => {
                self.frame.missing += 1;
                std::ptr::null()
            }
        }
    }

    /// A blob in replay memory, its fixups applied; kept in `keep`.
    fn blob(&mut self, b: &Blob, keep: &mut Vec<Vec<u64>>) -> u64 {
        let mut words = vec![0u64; b.bytes.len().div_ceil(8).max(1)];
        // SAFETY: the vector has room for the bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                b.bytes.as_ptr(),
                words.as_mut_ptr().cast::<u8>(),
                b.bytes.len(),
            );
        }
        let base = words.as_mut_ptr().cast::<u8>();
        for (at, fix) in &b.fixups {
            let at = *at as usize;
            let session = word(&b.bytes, at);
            let value = match fix {
                Fix::Obj => self.object(session) as u64,
                Fix::Addr | Fix::Handle => self.translate(session),
                Fix::Blob(inner) => self.blob(inner, keep),
            };
            // SAFETY: fixups lie inside the blob (checked when read).
            unsafe { base.add(at).cast::<u64>().write_unaligned(value) };
        }
        let p = base as u64;
        keep.push(words);
        p
    }

    /// Makes a recorded call: its result, the objects it made.
    fn call(&mut self, kind: u8, slot: u16, this: u64, args: &[Arg]) -> Result<Called, String> {
        let object = if kind == DEVICE {
            self.device
        } else {
            self.object(this)
        };
        if object.is_null() {
            return Ok((0, Vec::new(), ""));
        }
        // SAFETY: one of our objects; its first word is its table.
        let table = unsafe { *object.cast::<*const *const ()>() };
        let len = methods::table_len(kind).ok_or("an unknown object kind")?;
        // SAFETY: our tables of this kind have `len` slots.
        let slots = unsafe { std::slice::from_raw_parts(table, len) };
        let newer = proxy::newer_layout(kind, slots).unwrap_or(false);
        let Some(place) = methods::find(kind, newer, usize::from(slot)) else {
            return Err(format!("no method {slot} of kind {kind} to replay"));
        };
        let mut keep = Vec::new();
        let mut outs = Vec::new();
        let mut words = Vec::with_capacity(args.len());
        for a in args {
            words.push(match a {
                Arg::Word(v) => *v,
                Arg::Obj(o) => self.object(*o) as u64,
                Arg::Addr(a) | Arg::Handle(a) => self.translate(*a),
                Arg::Blob(b) => self.blob(b, &mut keep),
                Arg::Out { made, .. } => {
                    let room = vec![0u64; 1];
                    let p = room.as_ptr() as u64;
                    outs.push((*made, p));
                    keep.push(room);
                    p
                }
                Arg::Scratch(n) => {
                    let room = vec![0u64; (*n as usize).div_ceil(8).max(1)];
                    let p = room.as_ptr() as u64;
                    keep.push(room);
                    p
                }
            });
        }
        // SAFETY: the method at this slot, with the arguments it was
        // recorded with; pointers into `keep`, alive through the call.
        let ret = unsafe { (place.replay)(slots[usize::from(slot)], object, &words) };
        let made = outs
            .into_iter()
            // SAFETY: room for an object pointer, written by the call.
            .map(|(session, p)| (session, unsafe { *(p as *const This) }))
            .collect();
        drop(keep);
        Ok((ret, made, place.name))
    }

    /// Notes the objects a call made, and what it says of their memory.
    fn made(
        &mut self,
        kind: u8,
        slot: u16,
        args: &[Arg],
        made: &[(u64, This)],
        note: u64,
        ret: u64,
    ) {
        for &(session, replay) in made {
            if session != 0 && !replay.is_null() {
                self.objects.insert(session, replay);
                let k = args.iter().find_map(|a| match a {
                    Arg::Out { kind, made } if *made == session => Some(*kind),
                    _ => None,
                });
                self.kinds.insert(session, k.unwrap_or(0));
            }
        }
        // What the replay's own notes are (as `proxy::note` takes them).
        let replay_note = match (made.first(), kind, slot) {
            (Some(&(_, o)), DEVICE, 14) if !o.is_null() => method_word(o, 9),
            (Some(&(_, o)), DEVICE, 27 | 76 | 127) if !o.is_null() => method_word(o, 11),
            (Some(&(_, o)), DEVICE, 29) if !o.is_null() => match args.get(1) {
                Some(&Arg::Word(offset)) => method_word(o, 11).wrapping_sub(offset),
                _ => 0,
            },
            (Some(&(_, o)), DEVICE, 36) if !o.is_null() => method_word(o, 12),
            _ => 0,
        };
        self.noted = if kind == LIST && slot == 148 {
            (note, ret)
        } else {
            (note, replay_note)
        };
    }

    /// Memory an object owns: translated when it moved.
    fn owned(&mut self, this: u64, at: u64, len: u64) {
        if this == 0 {
            // Title memory that appeared during the recorded frames.
            if self.options.own_process && !self.is_reserved(at, len) && reserve(at, len) {
                self.reserved.push((at, len));
            }
            return;
        }
        let (session, replay) = self.noted;
        if session != at || replay == 0 {
            return;
        }
        if replay == at {
            self.identity.push((at, len));
        } else {
            self.moved.insert(at, (len, replay));
        }
    }

    /// An event before the recorded frames.
    fn journal(&mut self, event: &Event) -> Result<(), String> {
        match event {
            Event::Device { iid, version } => {
                let parameters = [*version, 0, 0, 0, 0, 0, 0, 0];
                let mut out = std::ptr::null();
                let hr =
                    crate::device::create(std::ptr::null(), parameters.as_ptr(), iid, &mut out);
                if hr < 0 || out.is_null() {
                    return Err(format!("the replay's device: {hr:#x}"));
                }
                self.device = out;
            }
            Event::Call {
                kind,
                slot,
                this,
                args,
                ret,
                note,
            } => {
                if *kind == DEVICE && matches!(slot, 27 | 76 | 127) {
                    set_next_allocation(*note);
                }
                let (replayed, made, _) = self.call(*kind, *slot, *this, args)?;
                set_next_allocation(0);
                let ret = if *kind == LIST && *slot == 148 {
                    replayed
                } else {
                    *ret
                };
                self.made(*kind, *slot, args, &made, *note, ret);
            }
            Event::Destroy { this } => self.destroy(*this),
            Event::Owned { this, at, len } => self.owned(*this, *at, *len),
            _ => {}
        }
        Ok(())
    }

    fn destroy(&mut self, this: u64) {
        if let Some(o) = self.objects.remove(&this) {
            self.kinds.remove(&this);
            // SAFETY: slot 2 of every table is the release.
            let release: extern "system" fn(This) -> u32 =
                unsafe { std::mem::transmute(slot(o, 2)) };
            release(o);
        }
    }

    /// An event of the recorded frames; the frame, when one was presented.
    fn window(&mut self, event: &Event) -> Result<Option<Frame>, String> {
        match event {
            Event::Window { frame } => {
                self.catch_up(*frame);
                self.restore()?;
                self.start_frame(frame + 1);
            }
            Event::Fence { this, value } => {
                let f = self.object(*this);
                if !f.is_null() {
                    // SAFETY: slot 10 of a fence is its signal.
                    let signal: extern "system" fn(This, u64) -> i32 =
                        unsafe { std::mem::transmute(slot(f, 10)) };
                    signal(f, *value);
                }
            }
            Event::Pages { gpu: false, pages } => {
                self.write_pages(pages);
                self.poke();
            }
            Event::Pages { gpu: true, pages } => {
                if let Some(host) = crate::host::made() {
                    host.drain();
                }
                self.compare_pages(pages);
                self.poke();
            }
            Event::Call {
                kind,
                slot,
                this,
                args,
                ret,
                note,
            } => {
                self.frame.calls += 1;
                if *kind == DEVICE && matches!(slot, 27 | 76 | 127) {
                    set_next_allocation(*note);
                }
                let (replayed, made, name) = self.call(*kind, *slot, *this, args)?;
                match (*kind, name) {
                    (LIST, "draw" | "draw_indexed" | "dispatch_mesh_x" | "execute_indirect") => {
                        self.frame.draws += 1;
                    }
                    (LIST, "dispatch" | "dispatch_x") => self.frame.dispatches += 1,
                    _ => {}
                }
                set_next_allocation(0);
                let ret = if *kind == LIST && *slot == 148 {
                    replayed
                } else {
                    *ret
                };
                self.made(*kind, *slot, args, &made, *note, ret);
            }
            Event::Destroy { this } => self.destroy(*this),
            Event::Owned { this, at, len } => self.owned(*this, *at, *len),
            Event::Presented { frame } => {
                if let Some(host) = crate::host::made() {
                    host.drain();
                }
                let mut done = std::mem::take(&mut self.frame);
                done.frame = *frame;
                done.pass = self.pass;
                done.drawn = crate::draw::drawn() - self.drawn;
                done.dispatched = crate::draw::dispatched() - self.dispatched;
                done.ms = self.started.elapsed().as_millis() as u64;
                self.start_frame(frame + 1);
                return Ok(Some(done));
            }
            Event::End if self.frame.calls > 0 => {
                // A recording that ends between presents (tests): its last
                // frame as far as it went.
                if let Some(host) = crate::host::made() {
                    host.drain();
                }
                let mut done = std::mem::take(&mut self.frame);
                done.pass = self.pass;
                done.drawn = crate::draw::drawn() - self.drawn;
                done.dispatched = crate::draw::dispatched() - self.dispatched;
                done.ms = self.started.elapsed().as_millis() as u64;
                return Ok(Some(done));
            }
            Event::Device { .. } | Event::End => {}
        }
        Ok(None)
    }

    fn start_frame(&mut self, frame: u64) {
        self.frame = Frame {
            frame,
            pass: self.pass,
            ..Frame::default()
        };
        self.drawn = crate::draw::drawn();
        self.dispatched = crate::draw::dispatched();
        self.started = std::time::Instant::now();
    }

    /// Presents with no planes until the frame count is the session's.
    fn catch_up(&mut self, frame: u64) {
        let queue = self
            .kinds
            .iter()
            .find(|&(_, &k)| k == QUEUE)
            .and_then(|(s, _)| self.objects.get(s).copied());
        let Some(queue) = queue else {
            return;
        };
        // SAFETY: one of our queues; its first word is its table.
        let table = unsafe { *queue.cast::<*const *const ()>() };
        // SAFETY: queue tables have 36 slots.
        let slots = unsafe { std::slice::from_raw_parts(table, 36) };
        let newer = proxy::newer_layout(QUEUE, slots).unwrap_or(false);
        let at = if newer { methods::newer_queue(30) } else { 30 };
        // SAFETY: the present method of a queue (planes, parameters, rest).
        let present: extern "system" fn(This, u32, *const u8, *const u8) -> i32 =
            unsafe { std::mem::transmute(slots[at]) };
        while crate::queue::presented() < frame {
            present(queue, 0, std::ptr::null(), std::ptr::null());
        }
    }

    /// Restores the snapshot's title memory.
    fn restore(&mut self) -> Result<(), String> {
        let path = self.options.dir.join("memory.bin");
        if !self.options.own_process {
            return Ok(());
        }
        let mut unplaced = 0;
        let zeros = [0u8; super::wire::PAGE];
        store::read_memory(&path, memory::threads(), |unit| {
            for (at, bytes) in unit.pages() {
                if !self.is_reserved(at, super::wire::PAGE as u64) {
                    unplaced += 1;
                    continue;
                }
                let bytes = bytes.unwrap_or(&zeros);
                // SAFETY: memory the replay reserved and committed.
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len())
                };
            }
        })
        .map_err(|e| format!("{}: {e}", path.display()))?;
        for &(at, len) in &self.reserved {
            crate::title_memory_remapped(at as usize, len as usize);
        }
        if self.pass == 0 {
            self.unplaced += unplaced;
        }
        Ok(())
    }

    /// Whether `len` bytes at replay address `at` are memory the replay may
    /// write: title memory it reserved, or memory of its objects.
    fn writable(&self, session: u64, len: u64) -> Option<u64> {
        let at = self.translate(session);
        if at != session {
            return Some(at);
        }
        let inside = |r: &[(u64, u64)]| r.iter().any(|&(a, l)| a <= at && at + len <= a + l);
        (inside(&self.reserved) || inside(&self.identity)).then_some(at)
    }

    /// Writes the CPU's pages (the changed ones).
    fn write_pages(&mut self, pages: &[Page]) {
        for p in pages {
            let Some(at) = self.writable(p.at, p.bytes.len() as u64) else {
                continue;
            };
            // SAFETY: memory the replay owns, `len` bytes.
            let now = unsafe { std::slice::from_raw_parts(at as *const u8, p.bytes.len()) };
            if now != p.bytes.as_slice() {
                // SAFETY: as above.
                unsafe {
                    std::ptr::copy_nonoverlapping(p.bytes.as_ptr(), at as *mut u8, p.bytes.len())
                };
                self.frame.pages += 1;
            }
        }
    }

    /// Compares what the session wrote back after a submission with the
    /// replay's memory (see [`Frame`]).
    fn compare_pages(&mut self, pages: &[Page]) {
        for p in pages {
            let Some(at) = self.writable(p.at, p.bytes.len() as u64) else {
                continue;
            };
            // SAFETY: memory the replay owns.
            let now = unsafe { std::slice::from_raw_parts(at as *const u8, p.bytes.len()) };
            if now == p.bytes.as_slice() {
                self.frame.gpu_same += 1;
            } else if self.options.session_gpu_pages || memory::hash(now) == p.before {
                // SAFETY: as above.
                unsafe {
                    std::ptr::copy_nonoverlapping(p.bytes.as_ptr(), at as *mut u8, p.bytes.len())
                };
                self.frame.gpu_written += 1;
            } else {
                self.frame.gpu_different += 1;
            }
        }
    }

    /// Writes the pokes of this frame where memory differs from them.
    fn poke(&mut self) {
        for p in &self.options.pokes {
            if p.frame.is_some_and(|f| f != self.frame.frame) {
                continue;
            }
            let Some(at) = self.writable(p.at, p.bytes.len() as u64) else {
                continue;
            };
            // SAFETY: memory the replay owns.
            let now = unsafe { std::slice::from_raw_parts(at as *const u8, p.bytes.len()) };
            if now != p.bytes.as_slice() {
                // SAFETY: as above.
                unsafe {
                    std::ptr::copy_nonoverlapping(p.bytes.as_ptr(), at as *mut u8, p.bytes.len())
                };
            }
        }
    }
}

/// What a replayed call returned, the objects it made (session, replay)
/// and the method's name.
type Called = (u64, Vec<(u64, This)>, &'static str);

/// The method at `slot` of one of our objects.
fn slot(object: This, slot: usize) -> *const () {
    // SAFETY: our objects start with their table, which has the slot.
    unsafe { *(*object.cast::<*const *const ()>()).add(slot) }
}

/// A method taking only the object and returning a word.
fn method_word(object: This, at: usize) -> u64 {
    // SAFETY: as the callers say of the slot.
    let f: extern "system" fn(This) -> u64 = unsafe { std::mem::transmute(slot(object, at)) };
    f(object)
}

/// Reserves and commits `len` bytes at `at` exactly (write-watched, as the
/// runtime's title memory); false if they are taken.
fn reserve(at: u64, len: u64) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    // SAFETY: a new reservation at an address of our choice; the system
    // refuses one that is taken.
    let p = unsafe {
        VirtualAlloc(
            at as *const _,
            len as usize,
            MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
            PAGE_READWRITE,
        )
    };
    p as u64 == at
}

/// The address the next committed resource's memory must have (0: any).
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn set_next_allocation(at: u64) {
    NEXT.store(at, std::sync::atomic::Ordering::Relaxed);
}

/// Committed resources' memory in the replay: where the session had it.
extern "system" fn allocate(size: usize, _attributes: u64) -> *mut std::ffi::c_void {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };
    let at = NEXT.swap(0, std::sync::atomic::Ordering::Relaxed);
    if at != 0 {
        // SAFETY: commits within memory the replay reserved, or reserves
        // anew; the system refuses memory that is taken.
        let p = unsafe { VirtualAlloc(at as *const _, size, MEM_COMMIT, PAGE_READWRITE) };
        if p as u64 == at & !0xfff {
            return at as *mut _;
        }
        // SAFETY: as above.
        let p = unsafe {
            VirtualAlloc(
                at as *const _,
                size,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        if p as u64 == at {
            return p;
        }
    }
    // SAFETY: fresh memory anywhere.
    unsafe {
        VirtualAlloc(
            std::ptr::null(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        )
    }
}

/// Committed memory stays: the replay ends with the process.
extern "system" fn free(_: *mut std::ffi::c_void, _: u64) {}

/// The queues' page mappings in the replay: the replay's memory has no
/// physical pages to share, so what the session would see through the new
/// mapping is copied there (zeros where pages are unmapped).
fn map_pages(to: u64, from: u64, len: u64) -> bool {
    // SAFETY: both ranges are title memory the replay reserved (the session
    // mapped them); `from` 0 unmaps.
    unsafe {
        if from == 0 {
            std::ptr::write_bytes(to as *mut u8, 0, len as usize);
        } else {
            std::ptr::copy(from as *const u8, to as *mut u8, len as usize);
        }
    }
    crate::title_memory_remapped(to as usize, len as usize);
    true
}

/// Reads a poke: `<address>=<values>[@<frame>]`, values `f32:1,0.5`,
/// `u32:7,0x10`, `u8:1,2` or `hex:0a0b0c`; addresses in hex (`0x…`) or
/// decimal, with an optional `+<offset>`.
pub fn parse_poke(text: &str) -> Result<Poke, String> {
    let wrong = |why: &str| {
        format!(
            "poke {text:?}: {why} (expected <address>[+<offset>]=<f32|u32|u8|hex>:<values>[@<frame>])"
        )
    };
    let (place, rest) = text.split_once('=').ok_or_else(|| wrong("no ="))?;
    let (values, frame) = match rest.rsplit_once('@') {
        Some((v, f)) => (v, Some(number(f).ok_or_else(|| wrong("bad frame"))?)),
        None => (rest, None),
    };
    let mut at = 0u64;
    for part in place.split('+') {
        at = at
            .checked_add(number(part).ok_or_else(|| wrong("bad address"))?)
            .ok_or_else(|| wrong("address too large"))?;
    }
    let (kind, list) = values.split_once(':').ok_or_else(|| wrong("no type"))?;
    let items = list.split(',').map(str::trim).filter(|s| !s.is_empty());
    let mut bytes = Vec::new();
    match kind.trim() {
        "f32" => {
            for v in items {
                let f: f32 = v.parse().map_err(|_| wrong("bad f32"))?;
                bytes.extend_from_slice(&f.to_le_bytes());
            }
        }
        "u32" => {
            for v in items {
                let n = u32::try_from(number(v).ok_or_else(|| wrong("bad u32"))?)
                    .map_err(|_| wrong("u32 too large"))?;
                bytes.extend_from_slice(&n.to_le_bytes());
            }
        }
        "u8" => {
            for v in items {
                bytes.push(
                    u8::try_from(number(v).ok_or_else(|| wrong("bad u8"))?)
                        .map_err(|_| wrong("u8 too large"))?,
                );
            }
        }
        "hex" => {
            let digits: String = list.chars().filter(|c| !c.is_whitespace()).collect();
            if !digits.len().is_multiple_of(2) {
                return Err(wrong("odd hex digits"));
            }
            for i in (0..digits.len()).step_by(2) {
                bytes
                    .push(u8::from_str_radix(&digits[i..i + 2], 16).map_err(|_| wrong("bad hex"))?);
            }
        }
        _ => return Err(wrong("unknown type")),
    }
    if bytes.is_empty() {
        return Err(wrong("no values"));
    }
    Ok(Poke { at, bytes, frame })
}

fn number(text: &str) -> Option<u64> {
    let t = text.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => t.parse().ok(),
    }
}

/// Pokes from a file: one per line, `#` comments and blank lines skipped.
pub fn read_pokes(path: &Path) -> Result<Vec<Poke>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(parse_poke)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_pokes_read_floats_words_bytes_and_frames() {
        let p = parse_poke("0x14626b80+300=f32:1,0.5,0.25@618").unwrap();
        assert_eq!(p.at, 0x1462_6b80 + 300);
        assert_eq!(p.frame, Some(618));
        assert_eq!(p.bytes.len(), 12);
        assert_eq!(f32::from_le_bytes(p.bytes[4..8].try_into().unwrap()), 0.5);
        let p = parse_poke("4096=u32:7,0x10").unwrap();
        assert_eq!((p.at, p.frame), (4096, None));
        assert_eq!(p.bytes, [7, 0, 0, 0, 0x10, 0, 0, 0]);
        assert_eq!(parse_poke("0x10=hex:0a ff").unwrap().bytes, [0x0a, 0xff]);
        assert_eq!(parse_poke("0x10=u8:1,2").unwrap().bytes, [1, 2]);
        for bad in [
            "0x10",
            "0x10=f32:",
            "0x10=f64:1",
            "zz=u8:1",
            "0x10=hex:abc",
            "0x10=u8:300",
        ] {
            assert!(parse_poke(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn record_poke_files_skip_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pokes.txt");
        std::fs::write(
            &path,
            "# light colour\n0x100=f32:1,1,1 # all frames\n\n0x200=u8:5@3\n",
        )
        .unwrap();
        let pokes = read_pokes(&path).unwrap();
        assert_eq!(pokes.len(), 2);
        assert_eq!(pokes[1].frame, Some(3));
    }
}
