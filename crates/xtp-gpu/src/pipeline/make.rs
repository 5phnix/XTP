//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and BTP 6's
//! first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline ahead of those started early and waits
//! for it within a budget per title frame ([`Budget`]): a few
//! milliseconds for graphics pipelines, more for compute ones, whose
//! results later passes read (docs/analysis/gpu.md, G12). The cheap ones
//! (most, and every one the pipeline library holds) are there at once;
//! past the budget, a draw or dispatch whose pipeline is not ready is
//! dropped, as an untranslated one is, until a worker has made it: a few
//! frames, instead of up to half a second of waiting per frame (the budget
//! before).
//!
//! shortcut: a pass the title runs only once and whose pipeline takes
//! longer than the budget is lost; `XTP_PIPELINE_WAIT_MS` raises it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    /// [`IDLE`], [`EARLY`], [`URGENT`] or [`MAKING`]: the queue it was
    /// handed to last, or taken out of it. Changed under the queues' lock.
    queued: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    /// Set by the call that made it.
    finished: AtomicBool,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw has used it yet.
    key: String,
    used: AtomicBool,
    /// Why the host refused it, once it has (diagnostics).
    #[cfg(feature = "diagnostics")]
    refusal: OnceLock<String>,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

/// Not queued yet; queued as started early; queued for a first use; taken
/// by the worker that makes it (VIctory #546: a pipeline is never handed to
/// a second worker, which would wait on the first's compile).
const IDLE: u8 = 0;
const EARLY: u8 = 1;
const URGENT: u8 = 2;
const MAKING: u8 = 3;

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(
            graphics
                .as_ref()
                .map(|g| g.describe())
                .unwrap_or_default()
                .as_bytes(),
        );
        Pending {
            shaders,
            graphics,
            queued: AtomicU8::new(IDLE),
            made: OnceLock::new(),
            finished: AtomicBool::new(false),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            #[cfg(feature = "diagnostics")]
            refusal: OnceLock::new(),
        }
    }

    /// Makes the host pipeline (once); true for the call that finished it.
    fn make(&self) -> bool {
        self.made.get_or_init(|| {
            #[cfg(feature = "diagnostics")]
            let _ = super::host::take_refusal();
            let started = Instant::now();
            let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
            let ms = started.elapsed().as_millis() as u64;
            if ms >= SLOW_MS {
                tracing::info!(
                    ms,
                    shaders = ?self.shaders,
                    made = made.is_some(),
                    "host pipeline slow to make (its translations and the driver's compile)"
                );
            }
            #[cfg(feature = "diagnostics")]
            if made.is_none()
                && let Some(why) = super::host::take_refusal()
            {
                let _ = self.refusal.set(why);
            }
            made
        });
        let first = !self.finished.swap(true, Ordering::Relaxed);
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
        first
    }

    fn compute(&self) -> bool {
        matches!(&*self.shaders, [cs] if cs.stage == "compute")
    }
}

/// Why a pipeline has no host pipeline, and which shaders that concerns
/// (diagnostics): "being made"; "does not translate", with its shaders
/// that do not; "refused" and the host's reason, with all its shaders.
/// `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<(String, Option<String>)> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some(("being made".into(), None)),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                let why = p
                    .refusal
                    .get()
                    .map_or("refused".into(), |r| format!("refused, {r}"));
                (why, Some(format!("{:?}", p.shaders)))
            } else {
                ("does not translate".into(), Some(failed.join(" + ")))
            })
        }
    }
}

/// Signalled whenever a pipeline has been made.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// A pipeline that takes a worker this long or more is logged.
const SLOW_MS: u64 = 100;

/// Waiting allowed per title frame, over all threads: for graphics
/// pipelines, then for compute ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Budget([Duration; 2]);

impl Budget {
    const DEFAULT: Budget = Budget([Duration::from_millis(2), Duration::from_millis(8)]);

    /// `XTP_PIPELINE_WAIT_MS`: `<graphics>,<compute>` milliseconds, or one
    /// number for both.
    fn parse(text: &str) -> Option<Budget> {
        let ms: Vec<u64> = text
            .split(',')
            .map(|n| n.trim().parse().ok())
            .collect::<Option<_>>()?;
        let [graphics, compute] = match ms[..] {
            [both] => [both, both],
            [graphics, compute] => [graphics, compute],
            _ => return None,
        };
        Some(Budget([graphics, compute].map(Duration::from_millis)))
    }

    /// The run's, read once.
    fn get() -> Budget {
        static BUDGET: OnceLock<Budget> = OnceLock::new();
        *BUDGET.get_or_init(|| {
            let Ok(text) = std::env::var("XTP_PIPELINE_WAIT_MS") else {
                return Budget::DEFAULT;
            };
            match Budget::parse(&text) {
                Some(budget) => {
                    tracing::info!(?budget, "XTP_PIPELINE_WAIT_MS: pipeline waits per frame");
                    budget
                }
                None => {
                    tracing::warn!(
                        text,
                        "XTP_PIPELINE_WAIT_MS expects <graphics>,<compute> milliseconds"
                    );
                    Budget::DEFAULT
                }
            }
        })
    }
}

/// The waiting done in one title frame, per kind (graphics, compute).
struct Frame {
    frame: AtomicU64,
    waited_us: [AtomicU64; 2],
}

impl Frame {
    const fn new() -> Frame {
        Frame {
            frame: AtomicU64::new(u64::MAX),
            waited_us: [AtomicU64::new(0), AtomicU64::new(0)],
        }
    }

    /// What is left of `budget` for `kind` in title frame `frame`.
    fn left(&self, frame: u64, kind: usize, budget: Budget) -> Duration {
        if self.frame.swap(frame, Ordering::Relaxed) != frame {
            for w in &self.waited_us {
                w.store(0, Ordering::Relaxed);
            }
        }
        let waited = Duration::from_micros(self.waited_us[kind].load(Ordering::Relaxed));
        budget.0[kind].saturating_sub(waited)
    }

    fn add(&self, kind: usize, waited: Duration) {
        let us = waited.as_micros() as u64;
        self.waited_us[kind].fetch_add(us, Ordering::Relaxed);
        WAITED_US[kind].fetch_add(us, Ordering::Relaxed);
    }
}

/// Waiting since the last [`take_wait_us`], in microseconds (graphics,
/// compute).
static WAITED_US: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Microseconds title threads waited for pipelines (graphics, compute)
/// since the last call (for the frame line).
pub(crate) fn take_wait_us() -> [u64; 2] {
    [0, 1].map(|k| WAITED_US[k].swap(0, Ordering::Relaxed))
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it ahead of the early ones on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    static FRAME: Frame = Frame::new();
    if p.made.get().is_none() {
        start(p, URGENT);
        wait(p, &FRAME, crate::frame::current(), Budget::get());
    }
    let made = p.made.get()?.as_ref();
    if made.is_some() && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    made
}

/// Hands `p` to the workers: once, and once more ahead of the early ones on
/// a first use, unless a worker has taken it already.
fn start(p: &Arc<Pending>, how: u8) {
    if p.queued.load(Ordering::Relaxed) >= how {
        return;
    }
    if workers() == 0 {
        // No worker: made here instead.
        let was = {
            let _held = QUEUES.0.lock().unwrap_or_else(|e| e.into_inner());
            p.queued.swap(MAKING, Ordering::Relaxed)
        };
        if was == IDLE {
            WAITING.fetch_add(1, Ordering::Relaxed);
            finish(p);
        }
        return;
    }
    let (lock, ready) = &QUEUES;
    let mut queues = lock.lock().unwrap_or_else(|e| e.into_inner());
    match queues.push(p, how) {
        Some(true) => {
            PEAK.fetch_max(
                WAITING.fetch_add(1, Ordering::Relaxed) + 1,
                Ordering::Relaxed,
            );
        }
        Some(false) => {}
        None => return,
    }
    ready.notify_one();
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// Waits for `p` while title frame `frame` has budget left for its kind.
fn wait(p: &Pending, frames: &Frame, frame: u64, budget: Budget) {
    let kind = usize::from(p.compute());
    let left = frames.left(frame, kind, budget);
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done
            .wait_timeout(held, rest)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    drop(held);
    frames.add(kind, started.elapsed());
}

/// Pipelines for the workers: first uses first, then those started early.
/// A pipeline started early and then used is in both; the early entry is
/// passed over once the pipeline is no longer [`EARLY`].
struct Queues {
    urgent: VecDeque<Arc<Pending>>,
    early: VecDeque<Arc<Pending>>,
}

impl Queues {
    const fn new() -> Queues {
        Queues {
            urgent: VecDeque::new(),
            early: VecDeque::new(),
        }
    }

    /// Queues `p` as `how` ([`EARLY`] or [`URGENT`]) unless it is queued
    /// that way already, or taken: whether it was not queued before, or
    /// `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, how: u8) -> Option<bool> {
        let was = p.queued.load(Ordering::Relaxed);
        if was >= how {
            return None;
        }
        p.queued.store(how, Ordering::Relaxed);
        if how == URGENT {
            self.urgent.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(was == IDLE)
    }

    /// The next pipeline to make, taken ([`MAKING`]).
    fn pop(&mut self) -> Option<Arc<Pending>> {
        let early = &mut self.early;
        let p = self.urgent.pop_front().or_else(|| {
            std::iter::from_fn(|| early.pop_front())
                .find(|p| p.queued.load(Ordering::Relaxed) == EARLY)
        })?;
        p.queued.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

static QUEUES: (Mutex<Queues>, Condvar) = (Mutex::new(Queues::new()), Condvar::new());

/// Stack of each pipeline worker: room for the driver's shader compiler,
/// which runs on it (reserved address space; committed only as used).
/// Every title pipeline is compiled by [`Pending::make`], from [`finish`]:
/// on a worker, or, when none could start, on the title's thread with the
/// title's stack.
const PIPELINE_STACK: usize = 64 << 20;

/// A pipeline worker's thread, before it starts: its name and its stack.
fn worker(i: usize) -> std::thread::Builder {
    std::thread::Builder::new()
        .name(format!("xtp-pipelines-{i}"))
        // The driver compiles pipelines on this thread, recursively:
        // NVIDIA's compiler (nvgpucomp64) overflowed the default 2 MB
        // on a world compute program and refused it as out of memory.
        .stack_size(PIPELINE_STACK)
}

/// The workers (half the logical processors, 2 to 8), started on first
/// use: how many there are.
fn workers() -> usize {
    static WORKERS: OnceLock<usize> = OnceLock::new();
    *WORKERS.get_or_init(|| {
        let wanted = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        (0..wanted)
            .filter(|&i| {
                let spawned = worker(i).spawn(work);
                if let Err(e) = &spawned {
                    tracing::warn!(error = %e, "no pipeline worker");
                }
                spawned.is_ok()
            })
            .count()
    })
}

/// A worker: makes pipelines, first uses first, below the title's own
/// threads (loading, recording).
fn work() {
    // SAFETY: a plain call on our own thread.
    unsafe {
        use windows::Win32::System::Threading::*;
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    let (lock, ready) = &QUEUES;
    loop {
        let next = {
            let mut queues = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(p) = queues.pop() {
                    break p;
                }
                queues = ready.wait(queues).unwrap_or_else(|e| e.into_inner());
            }
        };
        finish(&next);
    }
}

/// Makes `p` and keeps the counts.
fn finish(p: &Pending) {
    if !p.make() {
        return;
    }
    if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
        drained();
    } else {
        super::library::save(false);
    }
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn until_finished(p: &Pending) {
        let started = Instant::now();
        while !p.finished.load(Ordering::Relaxed) && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        // Queued for a first use, or taken by a worker already.
        assert!(p.queued.load(Ordering::Relaxed) >= URGENT);
        until_finished(&p);
        assert!(matches!(p.made.get(), Some(None)));
    }

    /// A pipeline that is not made in time costs a frame its budget once:
    /// later uses in that frame drop at once, the next frame waits again.
    #[test]
    fn waits_stop_at_the_frames_budget() {
        let never = Pending::new(Vec::new(), None);
        let frames = Frame::new();
        let budget = Budget([Duration::from_millis(30), Duration::from_millis(60)]);
        let timed = |frame| {
            let started = Instant::now();
            wait(&never, &frames, frame, budget);
            started.elapsed()
        };
        let first = timed(7);
        assert!(first >= Duration::from_millis(30), "{first:?}");
        assert!(timed(7) < Duration::from_millis(10), "the budget is spent");
        assert!(
            timed(8) >= Duration::from_millis(30),
            "a new frame waits again"
        );
        // Compute pipelines have their own.
        assert_eq!(frames.left(8, 1, budget), Duration::from_millis(60));
        assert!(take_wait_us()[0] >= 60_000);
    }

    /// A worker's stack, as the thread itself sees it, is the 64 MiB kept
    /// for the driver's compiler, not Rust's default 2 MiB (VIctory #1129),
    /// and only reserved: what is committed at its start is less than the
    /// whole default stack.
    #[test]
    fn workers_have_room_for_the_drivers_compiler() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery,
        };
        let (reserved, committed) = worker(99)
            .spawn(|| {
                let (mut low, mut high) = (0, 0);
                // SAFETY: two out-pointers to this thread's own locals.
                unsafe {
                    windows::Win32::System::Threading::GetCurrentThreadStackLimits(
                        &mut low, &mut high,
                    );
                }
                let (mut at, mut committed) = (low, 0);
                while at < high {
                    // SAFETY: a query of our own stack's pages into a local.
                    let info = unsafe {
                        let mut info = std::mem::zeroed::<MEMORY_BASIC_INFORMATION>();
                        let n = VirtualQuery(at as *const _, &mut info, size_of_val(&info));
                        assert_ne!(n, 0, "VirtualQuery");
                        info
                    };
                    let end = (info.BaseAddress as usize + info.RegionSize).min(high);
                    if info.State == MEM_COMMIT {
                        committed += end - at;
                    }
                    at = end;
                }
                (high - low, committed)
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(reserved >= PIPELINE_STACK, "{reserved} bytes reserved");
        assert!(
            committed < 2 << 20,
            "{committed} bytes committed at the start"
        );
    }

    #[test]
    fn budgets_read_one_or_two_numbers() {
        let ms = |g, c| Some(Budget([Duration::from_millis(g), Duration::from_millis(c)]));
        assert_eq!(Budget::parse("5"), ms(5, 5));
        assert_eq!(Budget::parse(" 1, 20"), ms(1, 20));
        assert_eq!(Budget::parse("0,0"), ms(0, 0));
        assert_eq!(Budget::parse("1,2,3"), None);
        assert_eq!(Budget::parse("fast"), None);
        assert_eq!(Budget::parse(""), None);
    }

    /// A pipeline started early and then used is queued again, ahead, and
    /// made (and counted) once.
    #[test]
    fn early_then_used_is_made_once() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        start(&p, EARLY);
        start(&p, URGENT);
        start(&p, EARLY);
        assert!(p.queued.load(Ordering::Relaxed) >= URGENT);
        until_finished(&p);
        assert_eq!(p.queued.load(Ordering::Relaxed), MAKING);
        assert!(matches!(p.made.get(), Some(None)));
        assert!(!p.make(), "already finished");
    }

    /// First uses come out first; a pipeline started early and then used
    /// comes out once, at its first use's place, and is not handed out
    /// again once taken (VIctory #546's test).
    #[test]
    fn first_uses_come_first_and_each_pipeline_once() {
        let pending = || Arc::new(Pending::new(Vec::new(), None));
        let (earlier, promoted, used) = (pending(), pending(), pending());
        let mut queues = Queues::new();
        assert_eq!(queues.push(&earlier, EARLY), Some(true));
        assert_eq!(queues.push(&promoted, EARLY), Some(true));
        assert_eq!(queues.push(&used, URGENT), Some(true));
        assert_eq!(queues.push(&promoted, URGENT), Some(false));
        assert_eq!(queues.push(&promoted, URGENT), None);
        assert_eq!(queues.push(&promoted, EARLY), None);

        assert!(Arc::ptr_eq(&queues.pop().unwrap(), &used));
        assert!(Arc::ptr_eq(&queues.pop().unwrap(), &promoted));
        assert_eq!(promoted.queued.load(Ordering::Relaxed), MAKING);
        assert_eq!(queues.push(&promoted, URGENT), None, "taken already");
        assert!(Arc::ptr_eq(&queues.pop().unwrap(), &earlier));
        assert!(queues.pop().is_none(), "promoted's early entry passed over");
    }
}
