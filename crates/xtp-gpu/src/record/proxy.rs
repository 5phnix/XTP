//! The recorder: the device the title gets while recording, the tables of
//! the objects it makes, and what their calls write.
//!
//! The title calls our objects through their tables. While recording, the
//! device it gets is a copy of ours with a table of its own: the methods
//! kept ([`super::methods`]) record and forward, the others are ours. Every
//! object that device makes gets, at once, a table of its kind made the same
//! way from its own (the word before the first slot points at the original
//! table). Objects keep their memory and layout, so the title's inline code
//! and our methods see no difference; only `resource::is_resource`, which
//! knows resources by their table, asks [`is_proxy_of`].

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use super::memory::{self, Hashes};
use super::methods::{self, DEVICE, FENCE, LIST, QUEUE, Role, Word};
use super::store::{self, BLOCK, StreamWriter};
use super::wire::{Arg, Event, Writer};
use crate::object::{self, HRESULT, Iid, This};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// When the recorded frames start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    /// Before frame `n` (once frame `n - 1` was presented).
    Frame(u64),
    /// Before the first frame `XTP_CAPTURE_WORLD` or `XTP_CAPTURE_FRAMES`
    /// lists (diagnostics builds), so that the session's own captures are
    /// of the recorded frames.
    Capture,
    /// At once (tests): the window then ends with [`Recorder::finish`].
    Now,
}

/// What to record and where.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) dir: PathBuf,
    pub(crate) start: Start,
    /// Frames recorded.
    pub(crate) frames: u64,
    /// Whether title memory (below 2^40) is snapshotted and captured; tests
    /// leave it out, since their process's memory is not a title's.
    pub(crate) title_memory: bool,
}

impl Options {
    /// `XTP_RECORD=<dir>` and `XTP_RECORD_FRAMES=<n>[:<count>]` or
    /// `capture[:<count>]` (3 frames by default; the first frame `XTP_CAPTURE_WORLD`
    /// lists by default).
    pub(crate) fn from_env() -> Result<Option<Options>, String> {
        let Some(dir) = std::env::var_os("XTP_RECORD").filter(|d| !d.is_empty()) else {
            return Ok(None);
        };
        let frames = std::env::var("XTP_RECORD_FRAMES").unwrap_or_default();
        let (start, frames) = parse_frames(&frames)?;
        Ok(Some(Options {
            dir: PathBuf::from(dir),
            start,
            frames,
            title_memory: true,
        }))
    }
}

/// `<n>[:<count>]`, `capture[:<count>]`, or empty (the capture's frames).
pub(crate) fn parse_frames(text: &str) -> Result<(Start, u64), String> {
    let wrong =
        || format!("XTP_RECORD_FRAMES={text}: expected <frame>[:<count>] or capture[:<count>]");
    let mut fields = text.trim().splitn(2, ':');
    let first = fields.next().unwrap_or("").trim();
    let start = match first {
        "" | "capture" => Start::Capture,
        n => Start::Frame(n.parse().map_err(|_| wrong())?),
    };
    let count = match fields.next() {
        Some(c) => c
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|&c| c > 0)
            .ok_or_else(wrong)?,
        None => 3,
    };
    Ok((start, count))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Before the recorded frames: objects and views only.
    Journal,
    /// Recording frames; the window ends once frame `last` is presented.
    Window {
        last: u64,
    },
    Done,
}

/// An object the title holds, and its references.
struct Live {
    kind: u8,
    refs: i64,
}

/// What a recording counts, for its summary.
#[derive(Default, Debug, Clone)]
pub(crate) struct Counts {
    pub(crate) events: u64,
    pub(crate) calls: u64,
    pub(crate) pages: u64,
    pub(crate) gpu_pages: u64,
    pub(crate) snapshot_pages: u64,
    pub(crate) snapshot_bytes: u64,
    pub(crate) stream_bytes: u64,
    pub(crate) capture_ms: u64,
}

struct State {
    phase: Phase,
    out: Writer,
    stream: Option<StreamWriter>,
    seq: u64,
    objects: HashMap<u64, Live>,
    /// A list's calls since its last reset, before the recorded frames:
    /// what an open list holds when they start.
    tails: HashMap<u64, Vec<(u64, Vec<u8>)>>,
    /// Memory our objects own outside title memory: start → (length, owner).
    owned: BTreeMap<u64, (u64, u64)>,
    /// Heaps' sizes, for their memory once a placed resource shows where it is.
    heaps: HashMap<u64, u64>,
    /// Title memory at the last capture.
    ranges: Vec<(u64, u64)>,
    counts: Counts,
    error: Option<String>,
}

/// One recording.
pub(crate) struct Recorder {
    options: Options,
    state: Mutex<State>,
    /// Captures one at a time.
    capturing: Mutex<Hashes>,
    device: OnceLock<usize>,
}

static CURRENT: AtomicPtr<Recorder> = AtomicPtr::new(std::ptr::null_mut());

/// The recording under way, if any.
pub(crate) fn current() -> Option<&'static Recorder> {
    let p = CURRENT.load(Ordering::Acquire);
    // SAFETY: recorders are leaked once installed, never freed.
    unsafe { p.as_ref() }
}

impl Recorder {
    /// Starts a recording into `options.dir` (made if needed) and makes it
    /// the current one.
    pub(crate) fn begin(options: Options) -> Result<&'static Recorder, String> {
        std::fs::create_dir_all(&options.dir)
            .map_err(|e| format!("{}: {e}", options.dir.display()))?;
        let stream = StreamWriter::create(&options.dir.join("calls.bin"))
            .map_err(|e| format!("{}: {e}", options.dir.display()))?;
        let recorder = Box::leak(Box::new(Recorder {
            options,
            state: Mutex::new(State {
                phase: Phase::Journal,
                out: Writer::default(),
                stream: Some(stream),
                seq: 0,
                objects: HashMap::new(),
                tails: HashMap::new(),
                owned: BTreeMap::new(),
                heaps: HashMap::new(),
                ranges: Vec::new(),
                counts: Counts::default(),
                error: None,
            }),
            capturing: Mutex::new(Hashes::default()),
            device: OnceLock::new(),
        }));
        CURRENT.store(recorder, Ordering::Release);
        recorder.note_setup();
        tracing::info!(dir = %recorder.options.dir.display(), start = ?recorder.options.start, frames = recorder.options.frames, "recording the title's GPU work");
        Ok(recorder)
    }

    /// `info.txt`: what the replay must set up as the session had it (the
    /// console model, the `XTP_*` switches); the recorded frames are added
    /// when they start.
    fn note_setup(&self) {
        let console = match crate::console::current() {
            crate::console::Console::SeriesS => "series-s",
            crate::console::Console::SeriesX => "series-x",
        };
        let mut info = format!(
            "console={console}
"
        );
        let mut vars: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| k.starts_with("XTP_"))
            .collect();
        vars.sort();
        for (k, v) in vars {
            info.push_str(&format!(
                "env {k}={v}
"
            ));
        }
        if let Err(e) = std::fs::write(self.options.dir.join("info.txt"), info) {
            lock(&self.state).error = Some(format!("info.txt: {e}"));
        }
    }

    /// Adds the recorded frames to `info.txt`.
    fn note_window(&self, frame: u64, last: u64) {
        use std::io::Write;
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.options.dir.join("info.txt"));
        let wrote = file.and_then(|mut f| {
            writeln!(
                f,
                "window={frame}
last={last}"
            )
        });
        if let Err(e) = wrote {
            tracing::warn!(error = %e, "recording: info.txt");
        }
    }

    /// The device the title gets, made as `iid`: ours, with a table that
    /// records. A recording that starts at once ([`Start::Now`]) starts
    /// here.
    pub(crate) fn device(&'static self, iid: &Iid) -> This {
        {
            let mut state = self.state();
            let event = Event::Device {
                iid: *iid,
                version: crate::device::CHECKED_VERSION,
            };
            self.emit(&mut state, &event, None);
        }
        if self.options.start == Start::Now && self.state().phase == Phase::Journal {
            self.open_window(queue_frames(), u64::MAX);
        }
        *self.device.get_or_init(|| {
            let real = crate::device::device();
            // SAFETY: our device's first word is its table.
            let table = unsafe { *real.cast::<*const *const ()>() };
            let proxy = proxy_table(table, DEVICE).expect("the device's table");
            // The device's layout: the header, then fields the title reads
            // (0x58 bytes in all, `crate::device::Device`), copied.
            let mut copy = vec![0u64; 0x58 / 8].into_boxed_slice();
            // SAFETY: our device is 0x58 bytes; the copy has room for them.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    real.cast::<u8>(),
                    copy.as_mut_ptr().cast::<u8>(),
                    0x58,
                )
            };
            copy[0] = proxy as u64;
            Box::leak(copy).as_ptr() as usize
        }) as This
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Whether calls of `role` are kept now.
    fn keeps(&self, role: Role) -> bool {
        let phase = self.state().phase;
        match role {
            Role::Journal | Role::List => phase != Phase::Done,
            Role::Window | Role::Capture | Role::Present => matches!(phase, Phase::Window { .. }),
        }
    }

    /// Appends an event to the stream (or to `tail`'s list before the
    /// recorded frames).
    fn emit(&self, state: &mut State, event: &Event, tail: Option<u64>) {
        state.seq += 1;
        state.counts.events += 1;
        if let (Some(list), Phase::Journal) = (tail, state.phase) {
            let mut w = Writer::default();
            w.event(event);
            let seq = state.seq;
            state.tails.entry(list).or_default().push((seq, w.out));
            return;
        }
        state.out.event(event);
        if state.out.out.len() >= BLOCK {
            let block = std::mem::take(&mut state.out.out);
            if let Some(s) = &state.stream {
                s.push(block);
            }
        }
    }

    /// Notes a reference the title took (`+1`) or dropped (`-1`); its last
    /// one goes before the object.
    fn references(&self, this: This, delta: i64) {
        let mut state = self.state();
        if state.phase == Phase::Done {
            return;
        }
        let key = this as u64;
        let Some(live) = state.objects.get_mut(&key) else {
            return;
        };
        live.refs += delta;
        if live.refs > 0 || delta > 0 {
            return;
        }
        state.objects.remove(&key);
        state.tails.remove(&key);
        state.owned.retain(|_, &mut (_, owner)| owner != key);
        state.heaps.remove(&key);
        self.emit(&mut state, &Event::Destroy { this: key }, None);
    }

    /// Records a call made and its arguments, once it returned `ret`.
    fn called(&self, kind: u8, slot: usize, role: Role, this: This, mut args: Vec<Arg>, ret: u64) {
        let ok = ret as u32 as i32 >= 0;
        let mut made = Vec::new();
        for a in &mut args {
            if let Arg::Out { kind, made: at } = a {
                let p = *at as *const This;
                *at = 0;
                if ok && !p.is_null() {
                    // SAFETY: the title passed room for an object pointer.
                    let object = unsafe { p.read() };
                    if !object.is_null() && adopt(object, *kind) {
                        *at = object as u64;
                        made.push((object as u64, *kind));
                    }
                }
            }
        }
        let note = note(kind, slot, &args, &made, ret);
        let mut state = self.state();
        if state.phase == Phase::Done {
            return;
        }
        for &(object, kind) in &made {
            state.objects.insert(object, Live { kind, refs: 1 });
        }
        let list = (role == Role::List).then_some(this as u64);
        if let Some(l) = list
            && state.phase == Phase::Journal
            && is_reset(slot)
        {
            // Older calls of the list are done with.
            state.tails.remove(&l);
            state.owned.retain(|_, &mut (_, owner)| owner != l);
        }
        let event = Event::Call {
            kind,
            slot: slot as u16,
            this: this as u64,
            args,
            ret,
            note,
        };
        state.counts.calls += 1;
        self.emit(&mut state, &event, list);
        self.owns(&mut state, kind, slot, this, &event, &made);
    }

    /// Notes and records memory a new object owns (see [`Event::Owned`]).
    fn owns(
        &self,
        state: &mut State,
        kind: u8,
        slot: usize,
        this: This,
        event: &Event,
        made: &[(u64, u8)],
    ) {
        let Event::Call {
            args, note, ret, ..
        } = event
        else {
            return;
        };
        let add = |state: &mut State, owner: u64, at: u64, len: u64| {
            if at == 0 || len == 0 {
                return;
            }
            state.owned.insert(at, (len, owner));
            self.emit(
                state,
                &Event::Owned {
                    this: owner,
                    at,
                    len,
                },
                None,
            );
        };
        let first = made.first().map(|m| m.0);
        match (kind, slot, first) {
            (DEVICE, 14, Some(heap)) => {
                // (kind, count, flags, nodes): the heap's descriptors.
                if let Some(Arg::Blob(desc)) = args.first() {
                    let word = |i: usize| {
                        u32::from_le_bytes(
                            desc.bytes[i * 4..i * 4 + 4].try_into().expect("4 bytes"),
                        )
                    };
                    let step = crate::descriptor::increment_size(std::ptr::null(), word(0));
                    add(
                        state,
                        heap,
                        *note,
                        u64::from(step) * u64::from(word(1).max(1)),
                    );
                }
            }
            (DEVICE, 27 | 76 | 127, Some(resource)) => {
                add(state, resource, *note, committed_size(resource as This));
            }
            (DEVICE, 28, Some(heap)) => {
                if let Some(Arg::Blob(desc)) = args.first() {
                    let size = u64::from_le_bytes(desc.bytes[..8].try_into().expect("8 bytes"));
                    state.heaps.insert(heap, size);
                }
            }
            (DEVICE, 29, Some(_)) => {
                // The heap's memory, now that a resource shows where it is.
                if let Some(&Arg::Obj(heap)) = args.first()
                    && let Some(size) = state.heaps.remove(&heap)
                {
                    add(state, heap, *note, size);
                }
            }
            (DEVICE, 36, Some(fence)) => {
                // Only noted (fence values are kept as values, not memory).
                self.emit(
                    state,
                    &Event::Owned {
                        this: fence,
                        at: *note,
                        len: 8,
                    },
                    None,
                );
            }
            (LIST, 148, None) => {
                // With the call: in the list's calls before the frames.
                if let Some(&Arg::Word(size)) = args.first()
                    && *ret != 0
                {
                    let (owner, at) = (this as u64, *ret);
                    state.owned.insert(at, (size, owner));
                    let owned = Event::Owned {
                        this: owner,
                        at,
                        len: size,
                    };
                    self.emit(state, &owned, Some(owner));
                }
            }
            _ => {}
        }
    }

    /// The recorded frames start after frame `frame`: the open lists' calls,
    /// the fences' values and the memory snapshot.
    fn open_window(&self, frame: u64, last: u64) {
        let started = std::time::Instant::now();
        if let Some(host) = crate::host::made() {
            host.drain();
        }
        // Captures, then the state: the order `capture` takes them in.
        let hashes = lock(&self.capturing);
        let mut state = self.state();
        state.phase = Phase::Window { last };
        self.emit(&mut state, &Event::Window { frame }, None);
        let fences: Vec<u64> = state
            .objects
            .iter()
            .filter(|(_, l)| l.kind == FENCE)
            .map(|(&f, _)| f)
            .collect();
        for fence in fences {
            // SAFETY: a live fence; slot 8 is its completed value.
            let get: extern "system" fn(This) -> u64 =
                unsafe { std::mem::transmute(original(fence as This, 8)) };
            let value = get(fence as This);
            self.emit(&mut state, &Event::Fence { this: fence, value }, None);
        }
        // The memory, then the open lists' calls (made before, run after).
        let ranges = if self.options.title_memory {
            memory::title_ranges()
        } else {
            Vec::new()
        };
        let threads = memory::threads();
        let read = |at: u64, out: &mut [u8]| {
            let ok = memory::read_page(at, out);
            hashes.set(at, if ok { memory::hash(out) } else { 0 });
            ok
        };
        match store::write_memory(
            &self.options.dir.join("memory.bin"),
            &ranges,
            threads,
            &read,
        ) {
            Ok((pages, bytes)) => {
                state.counts.snapshot_pages = pages;
                state.counts.snapshot_bytes = bytes;
            }
            Err(e) => state.error = Some(format!("memory snapshot: {e}")),
        }
        state.ranges = ranges;
        let owned = owned_ranges(&state);
        let pages = hashes.changes(&owned, threads, &memory::read_page);
        state.counts.pages += pages.len() as u64;
        self.emit(&mut state, &Event::Pages { gpu: false, pages }, None);
        let mut tails: Vec<(u64, Vec<u8>)> = state.tails.drain().flat_map(|(_, t)| t).collect();
        tails.sort_unstable_by_key(|t| t.0);
        for (_, bytes) in tails {
            state.out.out.extend_from_slice(&bytes);
        }
        state.counts.capture_ms += started.elapsed().as_millis() as u64;
        drop(state);
        drop(hashes);
        self.note_window(frame, last);
        let state = self.state();
        tracing::info!(
            frame,
            pages = state.counts.snapshot_pages,
            bytes = state.counts.snapshot_bytes,
            ms = started.elapsed().as_millis() as u64,
            "recording: the recorded frames start"
        );
    }

    /// Captures memory around a submission (`gpu`: after it, once the
    /// GPU finished and wrote back).
    fn capture(&self, gpu: bool) {
        let started = std::time::Instant::now();
        if gpu && let Some(host) = crate::host::made() {
            host.drain();
        }
        let hashes = lock(&self.capturing);
        let ranges = if self.options.title_memory {
            memory::title_ranges()
        } else {
            Vec::new()
        };
        let (owned, appeared) = {
            let state = self.state();
            let appeared: Vec<(u64, u64)> = ranges
                .iter()
                .filter(|r| {
                    !state
                        .ranges
                        .iter()
                        .any(|o| o.0 <= r.0 && r.0 + r.1 <= o.0 + o.1)
                })
                .copied()
                .collect();
            (owned_ranges(&state), appeared)
        };
        let mut all = ranges.clone();
        all.extend(owned);
        let pages = hashes.changes(&all, memory::threads(), &memory::read_page);
        drop(hashes);
        let mut state = self.state();
        if !matches!(state.phase, Phase::Window { .. }) {
            return;
        }
        for (at, len) in appeared {
            self.emit(&mut state, &Event::Owned { this: 0, at, len }, None);
        }
        state.ranges = ranges;
        if gpu {
            state.counts.gpu_pages += pages.len() as u64;
        } else {
            state.counts.pages += pages.len() as u64;
        }
        if !pages.is_empty() {
            self.emit(&mut state, &Event::Pages { gpu, pages }, None);
        }
        state.counts.capture_ms += started.elapsed().as_millis() as u64;
    }

    /// After every present: starts or ends the recorded frames.
    fn presented(&self) {
        let frame = queue_frames();
        let phase = self.state().phase;
        match phase {
            Phase::Journal if self.due(frame + 1) => {
                self.open_window(frame, frame + self.options.frames);
            }
            Phase::Window { last } => {
                {
                    let mut state = self.state();
                    self.emit(&mut state, &Event::Presented { frame }, None);
                }
                if frame >= last {
                    let _ = self.finish();
                }
            }
            _ => {}
        }
    }

    /// Whether frame `next` is the first one recorded.
    fn due(&self, next: u64) -> bool {
        match self.options.start {
            Start::Frame(n) => next >= n,
            Start::Now => true,
            #[cfg(feature = "diagnostics")]
            Start::Capture => crate::capture::listed(next),
            #[cfg(not(feature = "diagnostics"))]
            Start::Capture => false,
        }
    }

    /// Ends the recording: writes what is left and its summary; the
    /// summary, or what went wrong.
    pub(crate) fn finish(&self) -> Result<Counts, String> {
        let mut state = self.state();
        if state.phase == Phase::Done {
            return Err("the recording was finished already".into());
        }
        self.emit(&mut state, &Event::End, None);
        state.phase = Phase::Done;
        let block = std::mem::take(&mut state.out.out);
        let stream = state.stream.take();
        if let Some(s) = stream {
            s.push(block);
            match s.finish() {
                Ok(bytes) => state.counts.stream_bytes = bytes,
                Err(e) => state.error = Some(format!("calls.bin: {e}")),
            }
        }
        let counts = state.counts.clone();
        let error = state.error.clone();
        drop(state);
        let summary = format!(
            "{counts:#?}\nerror: {}\n",
            error.as_deref().unwrap_or("none")
        );
        let _ = std::fs::write(self.options.dir.join("summary.txt"), summary);
        tracing::info!(?counts, error = error.as_deref().unwrap_or("none"), dir = %self.options.dir.display(), "recording: done");
        match error {
            Some(e) => Err(e),
            None => Ok(counts),
        }
    }
}

/// Frames presented so far (`crate::queue::presented`).
fn queue_frames() -> u64 {
    crate::queue::presented()
}

/// Whether a list slot resets the list (either layout).
fn is_reset(slot: usize) -> bool {
    slot == 10 || slot == 137 || slot == methods::newer_list(137)
}

/// Our objects' memory outside title memory, exact.
fn owned_ranges(state: &State) -> Vec<(u64, u64)> {
    let inside = |at: u64, len: u64| {
        state
            .ranges
            .iter()
            .any(|&(a, l)| a <= at && at + len <= a + l)
    };
    state
        .owned
        .iter()
        .map(|(&at, &(len, _))| (at, len))
        .filter(|&(at, len)| len > 8 && !inside(at, len))
        .collect()
}

/// The bytes a committed resource's memory spans.
fn committed_size(resource: This) -> u64 {
    if let Some((_, size)) = crate::resource::buffer_bytes(resource) {
        return size;
    }
    // A texture: its description's allocation (`(size, alignment)`).
    let mut desc = std::mem::MaybeUninit::<[u64; 7]>::zeroed();
    // SAFETY: slot 10 writes the 56-byte description.
    let get: extern "system" fn(This, *mut [u64; 7]) =
        unsafe { std::mem::transmute(original(resource, 10)) };
    get(resource, desc.as_mut_ptr());
    let mut info = [0u64; 2];
    crate::resource::allocation_info(
        info.as_mut_ptr().cast(),
        std::ptr::null(),
        0,
        1,
        desc.as_ptr().cast(),
    );
    info[0]
}

/// What the replay needs to know of a call's result besides its arguments
/// ([`Event::Call`]'s `note`).
fn note(kind: u8, slot: usize, args: &[Arg], made: &[(u64, u8)], ret: u64) -> u64 {
    let Some(&(object, _)) = made.first() else {
        return if kind == LIST && slot == 148 { ret } else { 0 };
    };
    let object = object as This;
    let word = |slot: usize| {
        // SAFETY: the method at `slot` of this kind takes only the object
        // and returns a word.
        let f: extern "system" fn(This) -> u64 =
            unsafe { std::mem::transmute(original(object, slot)) };
        f(object)
    };
    match (kind, slot) {
        // A descriptor heap's start.
        (DEVICE, 14) => word(9),
        // A committed resource's memory.
        (DEVICE, 27 | 76 | 127) => word(11),
        // A placed resource's heap's memory.
        (DEVICE, 29) => match args.get(1) {
            Some(&Arg::Word(offset)) => word(11).wrapping_sub(offset),
            _ => 0,
        },
        // A fence's value.
        (DEVICE, 36) => word(12),
        _ => 0,
    }
}

/// The original method at `slot` of `this`, whose table is one of ours.
pub(crate) fn original(this: This, slot: usize) -> *const () {
    // SAFETY: `this` is one of our objects with a proxy table: its first
    // word is the table, whose word before slot 0 is the original table.
    unsafe {
        let table = *this.cast::<*const *const ()>();
        let original = (*table.sub(1)).cast::<*const ()>();
        *original.add(slot)
    }
}

/// Proxy tables by original table, and the originals by proxy.
struct Tables {
    proxies: HashMap<usize, usize>,
    originals: HashMap<usize, usize>,
}

static TABLES: OnceLock<Mutex<Tables>> = OnceLock::new();

fn tables() -> MutexGuard<'static, Tables> {
    lock(TABLES.get_or_init(|| {
        Mutex::new(Tables {
            proxies: HashMap::new(),
            originals: HashMap::new(),
        })
    }))
}

/// Whether `table` is in the newer layout (lists and queues), or `None` if
/// it cannot be told. Lists: the two memory waits sit side by side, at 71
/// and 72 in the older layout and 73 and 74 in the newer. Queues: signal.
pub(crate) fn newer_layout(kind: u8, table: &[*const ()]) -> Option<bool> {
    match kind {
        LIST => {
            let waits = [
                crate::queue::wait32 as *const (),
                crate::queue::wait64 as *const (),
            ];
            if table[71..73] == waits {
                Some(false)
            } else if table[73..75] == waits {
                Some(true)
            } else {
                None
            }
        }
        QUEUE => {
            let signal = crate::queue::signal as *const ();
            if table[14] == signal {
                Some(false)
            } else if table[methods::newer_queue(14)] == signal {
                Some(true)
            } else {
                None
            }
        }
        _ => Some(false),
    }
}

/// The table recording `kind`'s calls in place of `original`.
fn proxy_table(original: *const *const (), kind: u8) -> Option<*const *const ()> {
    let mut t = tables();
    if let Some(&p) = t.proxies.get(&(original as usize)) {
        return Some(p as *const *const ());
    }
    let len = methods::table_len(kind)?;
    // SAFETY: our tables of this kind have `len` slots.
    let slots = unsafe { std::slice::from_raw_parts(original, len) };
    let Some(newer) = newer_layout(kind, slots) else {
        tracing::warn!(
            kind,
            "recording: a table of unknown layout; its calls are not recorded"
        );
        return None;
    };
    let mut proxy: Vec<*const ()> = Vec::with_capacity(len + 1);
    proxy.push(original.cast());
    proxy.extend_from_slice(slots);
    for p in methods::PLACES
        .iter()
        .filter(|p| p.kind == kind && p.applies(newer))
    {
        proxy[1 + p.slots[usize::from(newer)]] = p.wrap[usize::from(newer)];
    }
    if kind != DEVICE {
        proxy[1] = query as *const ();
        proxy[2] = add_ref as *const ();
        proxy[3] = release as *const ();
        proxy[1 + 7] = parent as *const ();
    }
    let proxy = Box::leak(proxy.into_boxed_slice());
    let table = proxy[1..].as_ptr();
    t.proxies.insert(original as usize, table as usize);
    t.originals.insert(table as usize, original as usize);
    Some(table)
}

/// Gives a new object of `kind` a recording table; false if it keeps its
/// own (a layout we cannot tell).
fn adopt(object: This, kind: u8) -> bool {
    // SAFETY: a new object of ours; its first word is its table.
    let slot = object.cast_mut().cast::<*const *const ()>();
    // SAFETY: as above; not yet handed to the title.
    let table = unsafe { slot.read() };
    if tables().originals.contains_key(&(table as usize)) {
        return true;
    }
    match proxy_table(table, kind) {
        Some(proxy) => {
            // SAFETY: as above; no one else knows the object yet.
            unsafe { slot.write(proxy) };
            true
        }
        None => false,
    }
}

/// Whether `this`'s table is our recording table of `table` (for checks
/// that know objects by their table).
pub(crate) fn is_proxy_of(this: This, table: *const ()) -> bool {
    // No recording made tables: nothing to look up (every run without one).
    if TABLES.get().is_none() {
        return false;
    }
    let mut first = [0u8; 8];
    if this.is_null() || !crate::host::read_into(this.cast(), &mut first) {
        return false;
    }
    let proxy = usize::from_le_bytes(first);
    tables().originals.get(&proxy) == Some(&(table as usize))
}

/// What every kept method runs: the call, recorded when its role is kept
/// now, with memory captured around submissions and the window moved on at
/// presents.
pub(crate) fn call<R: Word>(
    kind: u8,
    slot: usize,
    role: Role,
    this: This,
    capture: impl FnOnce() -> Vec<Arg>,
    forward: impl FnOnce() -> R,
) -> R {
    let Some(recorder) = current() else {
        return forward();
    };
    if !recorder.keeps(role) {
        let ret = forward();
        if role == Role::Present {
            recorder.presented();
        }
        return ret;
    }
    let around = matches!(role, Role::Capture | Role::Present);
    if around {
        recorder.capture(false);
    }
    let args = capture();
    let ret = forward();
    let word = ret.word();
    // A present's own record (the window may end at it).
    recorder.called(kind, slot, role, this, args, word);
    if around {
        recorder.capture(true);
    }
    if role == Role::Present {
        recorder.presented();
    }
    R::from_word(word)
}

type Unknown = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
type Count = extern "system" fn(This) -> u32;

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    // SAFETY: slot 0 of every table is the interface query.
    let f: Unknown = unsafe { std::mem::transmute(original(this, 0)) };
    let hr = f(this, iid, out);
    if hr >= 0
        // SAFETY: the title passes room for an object (null is tolerated).
        && unsafe { out.as_ref() }.is_some_and(|&o| o == this)
        && let Some(r) = current()
    {
        r.references(this, 1);
    }
    hr
}

extern "system" fn add_ref(this: This) -> u32 {
    if let Some(r) = current() {
        r.references(this, 1);
    }
    // SAFETY: slot 1 of every table.
    let f: Count = unsafe { std::mem::transmute(original(this, 1)) };
    f(this)
}

extern "system" fn release(this: This) -> u32 {
    // Before the object may go (and its address be used again).
    if let Some(r) = current() {
        r.references(this, -1);
    }
    // SAFETY: slot 2 of every table.
    let f: Count = unsafe { std::mem::transmute(original(this, 2)) };
    f(this)
}

/// The device an object reports is the recording one.
extern "system" fn parent(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    // SAFETY: slot 7 of every device child's table.
    let f: Unknown = unsafe { std::mem::transmute(original(this, 7)) };
    let hr = f(this, iid, out);
    if hr >= 0
        && let Some(r) = current()
        && let Some(&device) = r.device.get()
        // SAFETY: the title passes room for an object.
        && let Some(slot) = unsafe { out.as_mut() }
        && *slot == crate::device::device()
    {
        object::release(*slot);
        *slot = device as This;
        object::add_ref(*slot);
    }
    hr
}

/// Device creation while recording (`XTP_RECORD`, or a recording a test
/// began): the recording device, as `iid`. `None` when not recording.
pub(crate) fn hand_out_device(iid: *const Iid, out: *mut This) -> Option<HRESULT> {
    let recorder = match current() {
        Some(r) => r,
        None => {
            static FROM_ENV: OnceLock<Option<&'static Recorder>> = OnceLock::new();
            (*FROM_ENV.get_or_init(|| match Options::from_env() {
                Ok(Some(options)) => {
                    match check_space(&options.dir).and_then(|()| Recorder::begin(options)) {
                        Ok(r) => Some(r),
                        Err(e) => {
                            tracing::error!(error = %e, "XTP_RECORD: not recording");
                            None
                        }
                    }
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::error!(error = %e, "XTP_RECORD: not recording");
                    None
                }
            }))?
        }
    };
    if recorder.state().phase == Phase::Done {
        return None;
    }
    // SAFETY: the title passes an interface id (`device::create` checked it).
    let id = unsafe { iid.as_ref() }?;
    let device = recorder.device(id);
    Some(object::answer(object::DEVICE, iid, Some(device), out))
}

/// Refuses a recording on a volume with less than
/// `XTP_RECORD_MIN_FREE_GB` (16 by default) free, and one inside the
/// repository (recordings are game data).
fn check_space(dir: &Path) -> Result<(), String> {
    xtp_core::ensure_outside_repo(dir).map_err(|e| e.to_string())?;
    let want: u64 = std::env::var("XTP_RECORD_MIN_FREE_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    match free_bytes(dir) {
        Some(free) if free < want << 30 => Err(format!(
            "{} has {} GiB free, under the {want} GiB a recording may take (XTP_RECORD_MIN_FREE_GB)",
            dir.display(),
            free >> 30
        )),
        _ => Ok(()),
    }
}

/// Free bytes on `dir`'s volume, if the system says.
pub(crate) fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            available: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut available = 0u64;
    // SAFETY: a NUL-terminated path and out pointers of ours (null for
    // the totals we do not need).
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}
