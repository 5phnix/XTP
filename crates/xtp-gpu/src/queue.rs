//! Command queues. Submitted lists run on the host GPU in submission order
//! (all queues on one host queue); a fence signal takes effect once the
//! submissions before it have finished.
//!
//! A queue told to wait for a fence holds what it is given after the wait
//! ([`Held`]) until the wait is over: the fence is there, or a queue signal
//! made before gets it there (what is submitted after that signal runs
//! after the work before it, on the one host queue). That is how the
//! console orders work between queues: the title's compute queue waits for
//! what its graphics queue draws.
//!
//! Ported from fxceless's VIctory fork (commit 578d459): its `Held`,
//! `hold`, `release_held`, `wait` and `signal`, reworked for submissions
//! that run while the title goes on. There a wait was over only once the
//! fence was set, which here would wait for the GPU before every held
//! submission; it held whole submissions for waits for memory values too,
//! and forced every queue at each present.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUEUE, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const QUEUE_IIDS: [Iid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

/// The title reads the list type right after the header.
#[repr(C)]
struct Queue {
    object: Object,
    kind: u32,
    desc: QueueDesc,
    /// What the queue holds back. Locked while what it runs is submitted,
    /// so that the queue's order holds whichever thread submits.
    #[cfg(windows)]
    line: Mutex<Line>,
}
const _: () = assert!(std::mem::offset_of!(Queue, kind) == 0x10);

/// What a queue holds back behind a wait for a fence, in the order it was
/// given.
#[cfg(windows)]
#[derive(Default)]
struct Line {
    held: std::collections::VecDeque<Held>,
    /// When the queue last moved, while it holds something.
    since: Option<std::time::Instant>,
    /// Whether it is in [`WAITING`], with a reference to the queue (so a
    /// queue the title releases runs what it holds first).
    listed: bool,
}

/// One thing a queue holds back, with a reference kept to each object it
/// names.
#[cfg(windows)]
enum Held {
    /// A wait for the fence (its address) to reach the value.
    Wait(usize, u64),
    /// A submission: the title's lists, whose host sides run when the
    /// queue gets to it. The title records into a list again only after a
    /// reset, and a reset runs a held list first ([`list_reset`]).
    Run(Vec<usize>),
    /// A signal of the queue's, made once what it holds before it has run.
    Signal(usize, u64),
}

/// The queues that hold something (their addresses).
#[cfg(windows)]
static WAITING: Mutex<Vec<usize>> = Mutex::new(Vec::new());

#[cfg(windows)]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// One of our objects' methods by its slot, as the title calls it.
#[cfg(windows)]
fn method<F: Copy>(object: This, slot: usize) -> F {
    // SAFETY: one of our objects, whose first field is its table, which has
    // the slot; the caller names the method's signature.
    unsafe {
        let table = *object.cast::<*const *const ()>();
        std::mem::transmute_copy(&*table.add(slot))
    }
}

/// A fence's value (`GetCompletedValue`, slot 8).
#[cfg(windows)]
fn completed(fence: usize) -> u64 {
    method::<extern "system" fn(This) -> u64>(fence as This, 8)(fence as This)
}

/// Drops a reference kept to one of our objects (`Release`, slot 2: the
/// last one frees it).
#[cfg(windows)]
fn release_object(object: usize) {
    method::<extern "system" fn(This) -> u32>(object as This, 2)(object as This);
}

/// Queue signals made and not done yet: the fence (its address) and the
/// value it is set to once the submissions before the signal have finished.
#[cfg(windows)]
static ISSUED: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());

/// Whether a wait for `fence` to reach `value` is over for what is
/// submitted from now on: the fence is there, or a queue signal made so
/// far gets it there (what is submitted now runs after the work before
/// that signal: one host queue runs every queue's submissions in order).
#[cfg(windows)]
fn over(fence: usize, value: u64) -> bool {
    completed(fence) >= value || lock(&ISSUED).iter().any(|&(f, v)| f == fence && v >= value)
}

/// Whether a queue holds what follows a wait that is not over (on by
/// default). `XTP_QUEUE_WAIT=0`: it runs it at once, as before, and such
/// waits are only counted (to compare frame times).
#[cfg(windows)]
fn holds() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XTP_QUEUE_WAIT").is_none_or(|v| v != "0"))
}

/// For the frame line: waits for fences; those not over when made (with
/// [`holds`], the queue held what came next); title lists held; and waits
/// passed before they were over ([`Force`]).
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
const WAITS: usize = 0;
const HOLDS: usize = 1;
const HELD_LISTS: usize = 2;
const FORCED: usize = 3;

/// A queue that has not moved for this long while its wait is not over
/// runs on past it.
///
/// shortcut: on the console it would wait for ever. A signal XTP does not
/// see as one (a title write we miss) would then stop the queue for good;
/// past this the queue runs on, logged, as it did before waits held.
#[cfg(windows)]
const STUCK: std::time::Duration = std::time::Duration::from_secs(4);

/// What a queue runs before its waits are over.
#[cfg(windows)]
#[derive(Clone, Copy)]
enum Force {
    /// All the queue (its address) holds, and why.
    Queue(usize, &'static str),
    /// What a queue holds up to the submission naming the list (its
    /// address), which the title resets to record into it again.
    List(usize),
}

/// Puts `what` behind what queue `this` holds (`line`, locked), listing
/// the queue in [`WAITING`] if it held nothing.
#[cfg(windows)]
fn push(this: This, line: &mut Line, what: Held) {
    if !line.listed {
        line.listed = true;
        line.since = Some(std::time::Instant::now());
        object::add_ref(this);
        lock(&WAITING).push(this as usize);
        watch();
    }
    line.held.push_back(what);
}

/// Sets `fence` to `value` once every submission made so far has finished,
/// and drops the reference kept to it.
#[cfg(windows)]
fn issue(host: &crate::host::Host, fence: usize, value: u64) {
    // Not locked across `after`, which runs it at once when nothing is in
    // flight.
    lock(&ISSUED).push((fence, value));
    host.after(move || {
        crate::fence::signal(fence as This, value);
        let mut issued = lock(&ISSUED);
        if let Some(i) = issued.iter().position(|&s| s == (fence, value)) {
            issued.swap_remove(i);
        }
        drop(issued);
        crate::fence::release_kept(fence as This);
    });
}

/// Runs title lists (their host sides) as one title submission.
#[cfg(windows)]
fn run(host: &crate::host::Host, lists: impl Iterator<Item = This>) {
    let host_lists: Vec<_> = lists.filter_map(crate::list::host_list).collect();
    host.submit(&host_lists);
}

/// Runs what waiting queues hold as far as their waits are over, each
/// queue's in its own order, until nothing moves; with `force`, also what
/// it names. Called where a wait may have ended: after a queue's signal,
/// wait and submission, and from [`watch`]'s thread for what moves fences
/// without a queue (the title's CPU signals, bottom-of-pipe writes into a
/// fence once their submission has finished).
#[cfg(windows)]
fn release_held(force: Option<Force>) {
    /// One at a time: each queue's things are taken in order, and a listed
    /// queue stays alive meanwhile (only this takes queues off the list).
    static RELEASING: Mutex<()> = Mutex::new(());
    if lock(&WAITING).is_empty() {
        return;
    }
    let Some(host) = crate::host::made() else {
        return;
    };
    let _one = lock(&RELEASING);
    loop {
        let queues = lock(&WAITING).clone();
        let mut moved = false;
        for q in queues {
            moved |= run_line(host, q, force);
        }
        if !moved {
            break;
        }
    }
}

/// Runs what queue `q` holds as far as its waits are over, and what
/// `force` names: whether anything ran.
#[cfg(windows)]
fn run_line(host: &crate::host::Host, q: usize, force: Option<Force>) -> bool {
    // SAFETY: a listed queue, kept alive by the list's reference.
    let queue = unsafe { &*(q as *const Queue) };
    let mut line = lock(&queue.line);
    // How many of its things go whatever their waits.
    let (mut forced, mut why) = match force {
        Some(Force::Queue(at, why)) if at == q => (usize::MAX, why),
        Some(Force::List(list)) => {
            let names = |h: &Held| matches!(h, Held::Run(lists) if lists.contains(&list));
            let n = line.held.iter().position(names).map_or(0, |i| i + 1);
            (n, "the title resets a list it holds")
        }
        _ => (0, ""),
    };
    let mut moved = false;
    while let Some(next) = line.held.pop_front() {
        let forcing = forced > 0;
        forced = forced.saturating_sub(1);
        if let Held::Wait(fence, value) = next
            && !over(fence, value)
        {
            if !forcing && !line.since.is_some_and(|t| t.elapsed() >= STUCK) {
                line.held.push_front(next);
                break;
            }
            if !forcing {
                why = "it stood still too long";
            }
            let n = COUNTS[FORCED].fetch_add(1, Ordering::Relaxed);
            if n < 16 {
                tracing::warn!(
                    kind = queue.kind,
                    value,
                    now = completed(fence),
                    why,
                    "a queue runs on past a wait for a fence that is not there yet"
                );
            }
        }
        moved = true;
        match next {
            Held::Wait(fence, _) => crate::fence::release_kept(fence as This),
            Held::Run(lists) => {
                run(host, lists.iter().map(|&l| l as This));
                lists.into_iter().for_each(release_object);
            }
            Held::Signal(fence, value) => issue(host, fence, value),
        }
    }
    if moved {
        line.since = Some(std::time::Instant::now());
    }
    let unlisted = line.held.is_empty() && std::mem::take(&mut line.listed);
    if unlisted {
        line.since = None;
        lock(&WAITING).retain(|&w| w != q);
    }
    drop(line);
    if unlisted {
        // The list's reference: the last one frees the queue.
        release(q as This);
    }
    moved
}

/// Whether the queue holds anything.
#[cfg(windows)]
fn holding(this: This) -> bool {
    !lock(&queue(this).line).held.is_empty()
}

/// Before the title resets `list` to record into it again: a queue that
/// holds it runs what it holds up to it first, waits over or not (its
/// commands are about to be replaced).
#[cfg(windows)]
pub(crate) fn list_reset(list: This) {
    if !lock(&WAITING).is_empty() {
        release_held(Some(Force::List(list as usize)));
    }
}

/// Wakes the thread [`watch`] starts.
#[cfg(windows)]
static WAKE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// Starts, once, the thread that runs what queues hold when fences move
/// without a queue: it looks every millisecond while a queue holds
/// something, and sleeps otherwise. Wakes it now.
///
/// shortcut: it looks instead of being told. A hook in the fence's own
/// signal would release at once, for the CPU's signals.
#[cfg(windows)]
fn watch() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("xtp-gpu-queue-waits".into())
            .spawn(|| {
                loop {
                    let holding = !lock(&WAITING).is_empty();
                    let mut woken = lock(&WAKE.0);
                    if !*woken {
                        woken = if holding {
                            let look = std::time::Duration::from_millis(1);
                            let waited = WAKE.1.wait_timeout(woken, look);
                            waited.unwrap_or_else(|e| e.into_inner()).0
                        } else {
                            WAKE.1.wait(woken).unwrap_or_else(|e| e.into_inner())
                        };
                    }
                    *woken = false;
                    drop(woken);
                    release_held(None);
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no thread for queue waits: held lists run at the queues' next calls");
        }
    });
    *lock(&WAKE.0) = true;
    WAKE.1.notify_one();
}

fn queue<'a>(this: This) -> &'a Queue {
    // SAFETY: called from queue methods only.
    unsafe { &*this.cast::<Queue>() }
}

pub(crate) extern "system" fn create(
    _: This,
    desc: *const QueueDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "command queue");
    let queue = Box::new(Queue {
        object: if crate::device::newer() {
            Object::new(&NEWER_FUNCTIONS)
        } else {
            Object::new(&FUNCTIONS)
        },
        kind: desc.kind,
        desc,
        #[cfg(windows)]
        line: Mutex::default(),
    });
    object::hand_out(QUEUE, queue, iid, &QUEUE_IIDS, out)
}

/// The console's queue description: an engine or pipe and a queue on it
/// in place of PC's priority and node mask.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDescX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

/// Device method `CreateCommandQueueX`. The host picks engines itself, so
/// the queue is created as on PC.
pub(crate) extern "system" fn create_x(
    this: This,
    desc: *const QueueDescX,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&x) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(desc = ?x, "console command queue");
    let desc = QueueDesc {
        kind: x.kind,
        priority: 0,
        flags: x.flags,
        nodes: 1,
    };
    create(this, &desc, iid, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUEUE,
        iid,
        object::is(iid, &QUEUE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Queue>(this)
}

/// Runs the lists' host lists (decision 0007), counted as one title
/// submission for the frame-rate line; behind what the queue holds, if it
/// holds anything ([`Held`]).
extern "system" fn execute(this: This, count: u32, lists: *const This) {
    if lists.is_null() {
        return;
    }
    #[cfg(windows)]
    if let Some(host) = crate::host::host() {
        // SAFETY: the title passes `count` lists.
        let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
        if this.is_null() {
            run(host, lists.iter().copied());
            return;
        }
        let mut line = lock(&queue(this).line);
        if line.held.is_empty() {
            // Under the lock: after what the queue ran before.
            run(host, lists.iter().copied());
            return;
        }
        let lists: Vec<usize> = lists
            .iter()
            .filter(|l| !l.is_null())
            .map(|&l| {
                object::add_ref(l);
                l as usize
            })
            .collect();
        COUNTS[HELD_LISTS].fetch_add(lists.len() as u64, Ordering::Relaxed);
        push(this, &mut line, Held::Run(lists));
        drop(line);
        // The wait may be over already.
        release_held(None);
        return;
    }
    let _ = this;
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) {
        tracing::warn!(count, "command lists submitted; no host GPU runs them");
    }
}

/// The start of a present plane: frame token, then the plane's resources.
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
}
/// Submission tagged with the frame it belongs to `(count, lists, frame
/// token, flags)`; frames are paced elsewhere, so it runs as a plain one.
extern "system" fn execute_x(this: This, count: u32, lists: *const This, _token: u64, _flags: u32) {
    execute(this, count, lists);
}
/// Signal with sync settings for the top and bottom of the pipe: as a plain
/// signal, once earlier submissions have finished.
extern "system" fn signal2_x(
    this: This,
    fence: This,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> HRESULT {
    signal(this, fence, value)
}
/// Markers and events only matter to profilers.
extern "system" fn marker(_: This) {}
/// Sets the fence once every submission so far has finished (written back,
/// bottom-of-pipe writes done): at once if none is in flight. A queue that
/// holds something signals after it.
pub(crate) extern "system" fn signal(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    #[cfg(windows)]
    if let Some(host) = crate::host::made() {
        // Kept until it is set.
        object::add_ref(fence);
        let at = fence as usize;
        if this.is_null() {
            issue(host, at, value);
        } else {
            let mut line = lock(&queue(this).line);
            if line.held.is_empty() {
                // Under the lock: after what the queue ran before.
                issue(host, at, value);
            } else {
                push(this, &mut line, Held::Signal(at, value));
            }
        }
        // What waits for it may run now.
        release_held(None);
        return S_OK;
    }
    let _ = this;
    crate::fence::signal(fence, value)
}
/// From here on the queue runs nothing it is given until `fence` reaches
/// `value`, or a queue signal made before gets it there ([`over`]); what
/// it is given meanwhile is held ([`Held`]).
extern "system" fn wait(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    COUNTS[WAITS].fetch_add(1, Ordering::Relaxed);
    #[cfg(windows)]
    if !this.is_null() && crate::host::made().is_some() && !over(fence as usize, value) {
        let n = COUNTS[HOLDS].fetch_add(1, Ordering::Relaxed);
        if n < 8 {
            tracing::info!(
                kind = queue(this).kind,
                now = completed(fence as usize),
                value,
                held = holds(),
                "a queue waits for a fence no signal made so far gets to: what it is given next waits"
            );
        }
        if holds() {
            object::add_ref(fence);
            let mut line = lock(&queue(this).line);
            push(this, &mut line, Held::Wait(fence as usize, value));
            drop(line);
            // The fence may have got there meanwhile.
            release_held(None);
        }
    }
    let _ = (this, value);
    S_OK
}
/// Ticks per second of GPU timestamps (see [`Written::GpuTimestamp`]).
extern "system" fn timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(TIMESTAMP_FREQUENCY) };
    S_OK
}
const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

/// What a list's bottom-of-pipe write stores when the list has run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    /// The GPU clock: [`TIMESTAMP_FREQUENCY`] ticks per second since the
    /// process started (our GPU work is done on the CPU's timeline).
    GpuTimestamp,
    /// The CPU's performance counter.
    SystemTimestamp,
}

impl Written {
    /// Stores the low `bytes` (4 or 8) of the value at `at`.
    pub(crate) fn write(self, at: *mut u8, bytes: usize) {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => {
                static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
                (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64
            }
            Written::SystemTimestamp => system_timestamp(),
        };
        // SAFETY: the title names writable GPU memory (unified with ours).
        unsafe {
            if bytes == 4 {
                at.cast::<u32>().write_unaligned(value as u32);
            } else {
                at.cast::<u64>().write_unaligned(value);
            }
        }
    }
}

fn system_timestamp() -> u64 {
    #[cfg(windows)]
    {
        let mut count = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }
    #[cfg(not(windows))]
    0
}

/// Frames presented so far.
static FRAMES: AtomicU64 = AtomicU64::new(0);

/// Frames the title presented so far.
pub fn presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

/// Shows the first plane's first resource in our window.
///
/// shortcut: one plane, no scaling or placement, the source expected in
/// the common state.
extern "system" fn present(
    this: This,
    planes: u32,
    plane_parameters: *const Plane,
    _parameters: *const u8,
) -> HRESULT {
    // The frame shows what the queue was given before the present: what it
    // still holds runs first, as far as its waits are over, then the rest.
    #[cfg(windows)]
    if !this.is_null() && holding(this) {
        release_held(None);
        release_held(Some(Force::Queue(this as usize, "it presents")));
    }
    let _ = this;
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if frame.is_power_of_two() {
        let (draws, dispatches) = crate::list::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::drawn()),
                since(&DISPATCHED, crate::draw::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "frame presented (since the last line: draws and dispatches, and those run on the host)"
        );
        let [waits, held, lists, forced] = COUNTS.each_ref().map(|n| n.load(Ordering::Relaxed));
        tracing::info!(
            waits,
            held,
            lists,
            forced,
            "queue waits for fences so far (held: no signal made yet got there, so what came next waited; lists: title lists held; forced: waits run past before they were over)"
        );
        let [mappings, waited, past_writers] = REMAPS.each_ref().map(|n| n.load(Ordering::Relaxed));
        tracing::info!(
            mappings,
            waited,
            past_writers,
            "page-mapping copies so far (waited: for submissions in flight touching the pages; past writers: further than the newest one writing them, for one reading them)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    if crate::capture::listed(frame) {
        crate::capture::dump_targets(frame);
    }
    // XTP_PRESENT_TARGET: a target of the frame shown instead (`passdump`).
    #[cfg(all(windows, feature = "diagnostics"))]
    if crate::passdump::present(frame) {
        return S_OK;
    }
    // Several planes: composed into one image (`dxgi::planes`).
    #[cfg(windows)]
    let composed = crate::dxgi::planes::present(planes, plane_parameters.cast(), frame);
    #[cfg(all(windows, feature = "diagnostics"))]
    if let Some((resource, shown)) = composed {
        blinks(frame, resource, 0x4, shown);
    }
    #[cfg(windows)]
    if composed.is_none()
        && planes > 0
        // SAFETY: the title passes `planes` plane descriptions.
        && let Some(plane) = unsafe { plane_parameters.as_ref() }
        && plane.count > 0
        && !plane.resources.is_null()
        // SAFETY: the plane lists `count` resources.
        && let resource = unsafe { *plane.resources }
        && let Some(texture) = crate::resource::host_resource(resource)
        && let Some(host) = crate::host::host()
    {
        const COPY_SOURCE: u32 = 0x800;
        let state = crate::resource::host_transition(resource, COPY_SOURCE)
            .map_or(COPY_SOURCE, |(_, before)| before);
        let shown = host.present(texture, state, frame);
        crate::resource::host_transition(resource, state);
        #[cfg(feature = "diagnostics")]
        blinks(frame, resource, state, shown);
        #[cfg(not(feature = "diagnostics"))]
        let _ = shown;
    }
    #[cfg(not(windows))]
    let _ = (planes, plane_parameters);
    S_OK
}
/// Diagnostics: logs a presented frame that is black after one that was
/// not, with what was presented and how many draws ran since.
#[cfg(all(windows, feature = "diagnostics"))]
fn blinks(frame: u64, resource: This, state: u32, shown: Option<bool>) {
    static LAST: std::sync::Mutex<(bool, u64, u64)> = std::sync::Mutex::new((false, 0, 0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let drawn = crate::draw::drawn();
    let black = shown.unwrap_or(false);
    let draws = crate::draw::take_frame();
    if black && !last.0 {
        last.2 += 1;
        tracing::warn!(
            frame,
            resource = ?resource,
            state = format_args!("{state:#x}"),
            draws = drawn - last.1,
            copied = shown.is_some(),
            black_frames = last.2,
            levels = ?crate::capture::recent_levels(),
            "a black frame after a frame that was not"
        );
        for d in draws {
            tracing::warn!("black frame {frame}: {d}");
        }
        AFTER.store(frame + 6, Ordering::Relaxed);
    }
    // Six frames on: the levels around it (a fade or a single drop).
    static AFTER: AtomicU64 = AtomicU64::new(0);
    if AFTER.load(Ordering::Relaxed) == frame {
        tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
    }
    *last = (black, drawn, last.2);
}

/// A run of destination pages: how they are mapped (0 pool pages from
/// `start` on, 1 pool page `start` each, 2 unmapped, 3 left as they are)
/// and how many.
#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// Bytes per mapped page.
const PAGE: u64 = 64 << 10;

/// Page-mapping copies so far: all, those that waited for submissions in
/// flight (that touch the pages), and of those, the ones that waited past
/// the newest submission writing the pages (for one that only reads them).
static REMAPS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
const REMAPPED: usize = 0;
const REMAP_WAITS: usize = 1;
const REMAP_READERS: usize = 2;

/// `CopyPageMappingsBatchX(count, batches, pool, pool pages, flags)`: maps
/// pages of a pool (memory the title mapped physical pages into) at the
/// batches' addresses. BTP 6 streams textures into pool pages
/// and maps them into its tiled heap this way (title+0xf2c470, ranges of
/// one constant page or of unmapped pages). Done once the earlier
/// submissions whose GPU work touches the pages it maps have finished,
/// what the queue holds included (`Host::settle_remap`): what they write
/// goes to the pages mapped when they were submitted, and the host copies
/// they read keep what those pages held. Submissions elsewhere run on.
///
/// shortcut: what the queue holds runs first, waits over or not, instead
/// of the mapping being held behind it.
extern "system" fn copy_page_mappings(
    this: This,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> HRESULT {
    let Some(map) = crate::resource::page_mapper() else {
        return E_INVALIDARG;
    };
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    let batches = (count > 0).then(|| {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    });
    // The mappings to make: `times` runs of `bytes` from `at` on, one after
    // the other, each of the pages at `source` (0 unmaps).
    let mut maps: Vec<(u64, u64, u64, u32)> = Vec::new();
    let mut ok = true;
    for b in batches.into_iter().flatten() {
        if b.ranges.is_null() {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            let len = u64::from(r.count) * PAGE;
            let inside = |pages: u32| r.start.checked_add(pages).is_some_and(|e| e <= pool_pages);
            let source = pool + u64::from(r.start) * PAGE;
            match r.kind {
                0 if inside(r.count) => maps.push((at, source, len, 1)),
                1 if inside(1) => maps.push((at, source, PAGE, r.count)),
                2 => maps.push((at, 0, len, 1)),
                3 => {}
                _ => ok = false,
            }
            at += len;
        }
    }
    #[cfg(windows)]
    if !this.is_null() && holding(this) {
        release_held(None);
        release_held(Some(Force::Queue(this as usize, "it maps pages")));
    }
    let _ = this;
    REMAPS[REMAPPED].fetch_add(1, Ordering::Relaxed);
    #[cfg(windows)]
    if let Some(host) = crate::host::made() {
        let pages = maps
            .iter()
            .map(|&(at, _, bytes, times)| (at as usize, (at + bytes * u64::from(times)) as usize));
        let (waited, readers) = host.settle_remap(pages.collect());
        if waited {
            REMAPS[REMAP_WAITS].fetch_add(1, Ordering::Relaxed);
        }
        if readers {
            REMAPS[REMAP_READERS].fetch_add(1, Ordering::Relaxed);
        }
    }
    for (at, source, bytes, times) in maps {
        ok &= (0..u64::from(times)).all(|k| map(at + k * bytes, source, bytes));
    }
    if !ok {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(pool = format_args!("{pool:#x}"), "a page mapping failed")
        });
    }
    S_OK
}

extern "system" fn desc(this: This, out: *mut QueueDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(queue(this).desc) };
    }
}

/// The queue methods by their slot in older titles' queues (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, queue 26 (12 as on PC,
/// then console ones).
const METHODS: [(usize, *const ()); 15] = [
    (0, query as *const ()),
    (2, release as *const ()),
    (7, object::parent_device as *const ()), // 0x38
    (10, execute as *const ()),              // 0x50
    (11, marker as *const ()),               // 0x58 set marker
    (12, marker as *const ()),               // 0x60 begin event
    (13, marker as *const ()),               // 0x68 end event
    (14, signal as *const ()),               // 0x70
    (15, wait as *const ()),                 // 0x78
    (16, timestamp_frequency as *const ()),  // 0x80
    (26, desc as *const ()),                 // 0xd0
    (29, copy_page_mappings as *const ()),   // 0xe8 page mapping batch
    (30, present as *const ()),              // 0xf0
    (33, execute_x as *const ()),            // 0x108
    (32, signal2_x as *const ()),            // 0x100
];

/// Newer titles' queues have two more methods before Signal, which moved
/// from 0x70 to 0x80: every slot from 0x70 on is two later (taken here as
/// starting exactly at 0x70, the methods before it kept in place).
/// Inferred from BTP 6's calls: Signal 0x80 `(fence,
/// value)`, timestamp frequency 0x90 (title+0x24841a6), suspend and resume
/// 0xb8/0xc0, PresentX 0x100, the frame-token submission 0x118 `(count,
/// lists, token, flags)` (title+0x24821dd).
const fn newer_slot(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

const fn queue_table(newer: bool) -> [*const (); 36] {
    let mut f = object::d3d12_object(object::missing_table::<QUEUE, 36>());
    let mut i = 0;
    while i < METHODS.len() {
        let (slot, method) = METHODS[i];
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    f
}

static FUNCTIONS: Table<36> = Table(queue_table(false));
static NEWER_FUNCTIONS: Table<36> = Table(queue_table(true));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    /// Where a queue's signal is: newer layouts have it two slots later.
    /// Another test may switch layouts while this one runs, so the queue's
    /// own table decides.
    fn signal_slot(q: This) -> usize {
        // SAFETY: one of our queues; its first field is the table.
        let table = unsafe { *q.cast::<*const *const ()>() };
        if table == NEWER_FUNCTIONS.0.as_ptr() {
            0x80
        } else {
            0x70
        }
    }

    #[test]
    fn queue_signals_complete_at_once() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        // SAFETY: the title reads the type at +0x10.
        assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
        let mut fence = null;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let signal = slot::<extern "system" fn(This, This, u64) -> HRESULT>(q, signal_slot(q));
        assert_eq!(signal(q, fence, 7), S_OK);
        // Other tests' submissions may be in flight: the signal is behind
        // them.
        #[cfg(windows)]
        if let Some(host) = crate::host::made() {
            host.drain();
        }
        assert_eq!(
            slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
            7
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    /// BTP 6's compute queue: `{COMPUTE, 0, pipe 1, queue 0}`.
    #[test]
    fn console_queues_are_created_as_on_pc() {
        let null = std::ptr::null();
        let desc = QueueDescX {
            kind: 2,
            flags: 0,
            engine_or_pipe: 1,
            queue: 0,
        };
        let mut q = null;
        assert_eq!(create_x(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        assert_eq!(queue(q).desc.kind, 2);
        let none = std::ptr::null();
        assert_eq!(create_x(null, none, &QUEUE_IIDS[0], &mut q), E_INVALIDARG);
    }

    #[test]
    fn newer_queues_have_their_methods_two_slots_later() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(old[10], new[10]); // submission stays at 0x50
        assert_eq!(old[14], new[16]); // signal 0x70 → 0x80
        assert_eq!(old[30], new[32]); // present 0xf0 → 0x100
        assert_eq!(new[35], execute_x as *const ()); // 0x118
    }

    /// Queues that wait for fences, on the host GPU (`XTP_TEST_ADAPTER=warp`
    /// for the software adapter).
    #[cfg(windows)]
    mod waits {
        use super::*;
        use crate::testgpu::{Kernel, Pass, TitleBuffer, TitleList, device, pattern};
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        /// A title queue: 0 direct (graphics), 2 compute.
        fn new_queue(kind: u32) -> This {
            let null = std::ptr::null();
            let desc = QueueDesc {
                kind,
                priority: 0,
                flags: 0,
                nodes: 0,
            };
            let mut q = null;
            assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
            q
        }

        fn new_fence() -> This {
            let null = std::ptr::null();
            let mut fence = null;
            let iid = &crate::fence::FENCE_IIDS[0];
            assert_eq!(crate::fence::create(null, 0, 0, iid, &mut fence), S_OK);
            fence
        }

        /// Closes the list and submits it on `queue`, as the title does.
        fn submit(queue: This, list: &TitleList) {
            let this = list.this();
            assert_eq!(
                slot::<extern "system" fn(This) -> HRESULT>(this, 0x48)(this),
                S_OK
            );
            execute(queue, 1, &this);
        }

        fn release_all(objects: &[This]) {
            for &o in objects {
                slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
            }
        }

        /// What a fill writes.
        fn filled() -> Vec<u32> {
            (0..64).map(pattern).collect()
        }

        /// The title's compute queue waits for the depth its graphics queue
        /// draws: its list, submitted first, runs behind the graphics list
        /// the signal follows, and reads what that list wrote.
        #[test]
        fn a_compute_list_behind_a_fence_sees_what_the_graphics_list_signalling_it_wrote() {
            let test = "queue::tests::waits::a_compute_list_behind_a_fence_sees_what_the_graphics_list_signalling_it_wrote";
            let Some(host) = device(test) else { return };
            let kernel = Kernel::new(host);
            let (scratch, depth, read) = (
                TitleBuffer::new(256),
                TitleBuffer::new(256),
                TitleBuffer::new(256),
            );
            let (graphics, compute, fence) = (new_queue(0), new_queue(2), new_fence());
            assert_eq!(wait(compute, fence, 1), S_OK);
            let reader = TitleList::new();
            kernel.dispatch(reader.host(), Pass::Copy, depth.address(), read.address());
            submit(compute, &reader);
            assert!(holding(compute), "held until the fence gets there");
            let drawer = TitleList::new();
            kernel.dispatch(
                drawer.host(),
                Pass::Fill,
                scratch.address(),
                depth.address(),
            );
            submit(graphics, &drawer);
            assert_eq!(signal(graphics, fence, 1), S_OK);
            assert!(!holding(compute), "run once the signal is made");
            host.drain();
            assert_eq!(read.words(), filled());
            release_all(&[graphics, compute, fence]);
        }

        /// A wait for a value the fence has, or that a queue signal made
        /// before gets it to, holds nothing.
        #[test]
        fn a_wait_for_a_value_already_reached_runs_the_next_list_at_once() {
            let test = "queue::tests::waits::a_wait_for_a_value_already_reached_runs_the_next_list_at_once";
            let Some(host) = device(test) else { return };
            let kernel = Kernel::new(host);
            let (scratch, out) = (TitleBuffer::new(256), TitleBuffer::new(256));
            let (graphics, compute, fence) = (new_queue(0), new_queue(2), new_fence());
            assert_eq!(crate::fence::signal(fence, 3), S_OK);
            assert_eq!(wait(compute, fence, 2), S_OK);
            assert!(!holding(compute), "the fence is there");
            assert_eq!(signal(graphics, fence, 5), S_OK);
            assert_eq!(wait(compute, fence, 5), S_OK);
            assert!(!holding(compute), "a signal made before gets it there");
            let list = TitleList::new();
            kernel.dispatch(list.host(), Pass::Fill, scratch.address(), out.address());
            submit(compute, &list);
            assert!(!holding(compute), "run at once");
            host.drain();
            assert_eq!(out.words(), filled());
            release_all(&[graphics, compute, fence]);
        }

        /// A queue waiting for a fence the CPU signals runs on once it is
        /// signalled, with no queue call after it (the title then only waits
        /// for the queue's own signal), and reads what the CPU wrote before.
        #[test]
        fn a_queue_waiting_for_a_cpu_signal_runs_on_without_another_call() {
            let test = "queue::tests::waits::a_queue_waiting_for_a_cpu_signal_runs_on_without_another_call";
            let Some(host) = device(test) else { return };
            let kernel = Kernel::new(host);
            let (data, read) = (TitleBuffer::new(256), TitleBuffer::new(256));
            // A host copy, as if a draw had read the buffer before.
            let _ = data.shadow();
            let (compute, fence, done) = (new_queue(2), new_fence(), new_fence());
            assert_eq!(wait(compute, fence, 1), S_OK);
            let reader = TitleList::new();
            kernel.dispatch(reader.host(), Pass::Copy, data.address(), read.address());
            submit(compute, &reader);
            assert_eq!(signal(compute, done, 1), S_OK);
            assert!(holding(compute));
            // SAFETY: plain auto-reset event.
            let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
            assert!(!event.is_null());
            let on_completion =
                slot::<extern "system" fn(This, u64, *mut std::ffi::c_void) -> HRESULT>(done, 0x48);
            assert_eq!(on_completion(done, 1, event), S_OK);
            data.set_words(pattern);
            assert_eq!(crate::fence::signal(fence, 1), S_OK);
            // SAFETY: a valid event.
            let set = unsafe { WaitForSingleObject(event, 30_000) };
            assert_eq!(set, WAIT_OBJECT_0, "the queue's signal is made");
            assert_eq!(read.words(), filled(), "what the CPU wrote first");
            // SAFETY: ours.
            unsafe { CloseHandle(event) };
            release_all(&[compute, fence, done]);
        }

        /// The title resets a list a queue holds to record into it again:
        /// the list runs first, as it was recorded.
        #[test]
        fn a_held_list_the_title_resets_runs_first() {
            const ALLOCATOR_IID: Iid = guid(
                0x6102dee4,
                0xaf59,
                0x4b09,
                [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
            );
            let test = "queue::tests::waits::a_held_list_the_title_resets_runs_first";
            let Some(host) = device(test) else { return };
            let kernel = Kernel::new(host);
            let (scratch, out) = (TitleBuffer::new(256), TitleBuffer::new(256));
            let (compute, fence) = (new_queue(2), new_fence());
            // Never signalled.
            assert_eq!(wait(compute, fence, 1), S_OK);
            let list = TitleList::new();
            kernel.dispatch(list.host(), Pass::Fill, scratch.address(), out.address());
            submit(compute, &list);
            assert!(holding(compute));
            let null = std::ptr::null();
            let mut allocator = null;
            let made = crate::list::create_allocator(null, 0, &ALLOCATOR_IID, &mut allocator);
            assert_eq!(made, S_OK);
            let reset = slot::<extern "system" fn(This, This, This) -> HRESULT>(list.this(), 0x50);
            assert_eq!(reset(list.this(), allocator, null), S_OK);
            assert!(!holding(compute), "run before the reset");
            host.drain();
            assert_eq!(out.words(), filled());
            release_all(&[compute, fence, allocator]);
        }

        /// A present shows what its queue was given before it: what the
        /// queue holds runs first.
        #[test]
        fn a_present_runs_what_its_queue_holds() {
            let test = "queue::tests::waits::a_present_runs_what_its_queue_holds";
            let Some(host) = device(test) else { return };
            let kernel = Kernel::new(host);
            let (scratch, out) = (TitleBuffer::new(256), TitleBuffer::new(256));
            let (graphics, fence) = (new_queue(0), new_fence());
            assert_eq!(wait(graphics, fence, 1), S_OK);
            let list = TitleList::new();
            kernel.dispatch(list.host(), Pass::Fill, scratch.address(), out.address());
            submit(graphics, &list);
            assert!(holding(graphics));
            assert_eq!(
                present(graphics, 0, std::ptr::null(), std::ptr::null()),
                S_OK
            );
            assert!(!holding(graphics), "run before the present");
            host.drain();
            assert_eq!(out.words(), filled());
            release_all(&[graphics, fence]);
        }

        /// Where the test's page mapper mapped, and the first 64 words there
        /// just before (what the old pages held).
        static MAPPED: Mutex<Vec<(u64, Vec<u32>)>> = Mutex::new(Vec::new());

        /// The runtime's page mapper as the tests see it: notes the mapping,
        /// then shows the new pages at `to` (zeros for an unmapping, else
        /// the source's bytes), so that a write-back made after it would
        /// land on them.
        fn note_mapping(to: u64, from: u64, len: u64) -> bool {
            // SAFETY: the tests' own memory, `len` bytes at each address.
            unsafe {
                let old = (0..64).map(|i| (to as *const u32).add(i).read_volatile());
                lock(&MAPPED).push((to, old.collect()));
                if from == 0 {
                    std::ptr::write_bytes(to as *mut u8, 0, len as usize);
                } else {
                    std::ptr::copy(from as *const u8, to as *mut u8, len as usize);
                }
            }
            true
        }

        /// What the old pages at `at` held when a mapping replaced them.
        fn mapped(at: u64) -> Option<Vec<u32>> {
            let mapped = lock(&MAPPED);
            mapped.iter().find(|m| m.0 == at).map(|m| m.1.clone())
        }

        /// A page-mapping copy on `queue` that unmaps the page at `at`.
        fn unmap_page(queue: This, at: u64) -> HRESULT {
            let range = PageRange {
                kind: 2,
                start: 0,
                count: 1,
            };
            let batch = PageBatch {
                destination: at,
                count: 1,
                ranges: &range,
            };
            copy_page_mappings(queue, 1, &batch, 0, 0, 0)
        }

        /// A page mapping waits for the submission in flight whose shaders
        /// write the old pages, which get what they wrote (the new pages
        /// nothing); a mapping elsewhere does not wait for it.
        #[test]
        fn a_page_mapping_waits_only_for_the_submissions_touching_its_pages() {
            let test = "queue::tests::waits::a_page_mapping_waits_only_for_the_submissions_touching_its_pages";
            let Some(host) = device(test) else { return };
            crate::resource::use_page_mapper(note_mapping);
            let kernel = Kernel::new(host);
            // A page each: buffers are 64 KiB aligned.
            let [scratch, old, elsewhere] = [(); 3].map(|()| TitleBuffer::new(PAGE as usize));
            let queue = new_queue(0);
            let writer = TitleList::new();
            kernel.dispatch(writer.host(), Pass::Fill, scratch.address(), old.address());
            let open = host.hold_queue();
            submit(queue, &writer);
            assert_eq!(unmap_page(queue, elsewhere.address()), S_OK);
            assert!(mapped(elsewhere.address()).is_some(), "mapped at once");
            assert!(writer.host().running(), "the writer not waited for");
            assert!(old.words().iter().all(|&w| w == 0), "nothing written back");
            let (q, at) = (queue as usize, old.address());
            let (done, returned) = std::sync::mpsc::channel();
            std::thread::scope(|s| {
                s.spawn(move || {
                    let _ = done.send(unmap_page(q as This, at));
                });
                let waited = returned.recv_timeout(std::time::Duration::from_millis(300));
                assert!(waited.is_err(), "the mapping waits for the writer");
                assert_eq!(mapped(at), None);
                open();
                assert_eq!(returned.recv().unwrap(), S_OK);
            });
            assert_eq!(
                mapped(at),
                Some(filled()),
                "the old pages got what it wrote"
            );
            assert!(old.words().iter().all(|&w| w == 0), "the new pages nothing");
            assert!(!writer.host().running());
            release_all(&[queue]);
        }
    }

    #[test]
    fn timestamps_and_values_are_written() {
        let mut at = [0u64; 2];
        Written::Value(0x1_0000_0002).write(at.as_mut_ptr().cast(), 4);
        assert_eq!(at[0], 2);
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        let first = at[1];
        std::thread::sleep(std::time::Duration::from_millis(2));
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        assert!(at[1] > first);
    }
}
