//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: BTP 6 polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! A console flips a presented frame once the GPU is done with it, so a
//! blocking flip wait also ends only once the frame presented last has
//! finished on the host GPU (with frames in flight it may still run when
//! the present returns: `XTP_FRAMES_IN_FLIGHT`); then at the next tick.
//!
//! shortcut: event offsets are ignored (every event is at the tick); tie
//! the clock to presentation if a title paces by them.
//!
//! shortcut: a flip wait with a time-out does not wait for the GPU (BTP 6's
//! has none); it would need a host fence wait that gives up.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// The flip event: a presented frame reaches the display.
const FLIP: u32 = 2;
/// A wait's time-out when it has none.
const INFINITE: u32 = u32::MAX;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);

fn clock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let interval = match length {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    tracing::info!(?interval, "frame interval");
    // A new interval starts from now, after the last tick handed out.
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

/// Device method: events are scheduled at every tick.
pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// Frames begun so far (frame waits that ended).
pub(crate) fn current() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// Per event kind (origin, vblank, flip and later kinds): the last tick a
/// wait for it ended on.
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

/// The next tick after now and after the last one a wait for `kind` ended
/// on (an event happens once per tick: a wait woken a little early must not
/// let the next one end on the same tick), and how long until it.
fn next_tick(kind: u32) -> (u64, Duration) {
    let mut clock = clock();
    let c = clock.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: SIXTIETH,
        first: 0,
    });
    let elapsed = c.start.elapsed().as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let tick =
        (c.first + (elapsed / interval + 1) as u64).max(last(kind).load(Ordering::Relaxed) + 1);
    let at = u128::from(tick - c.first) * interval;
    (
        tick,
        Duration::from_nanos(at.saturating_sub(elapsed) as u64),
    )
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes. Ticks are absolute (start plus whole intervals), so
/// lateness never accumulates.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let origin = kind == ORIGIN;
    #[cfg(windows)]
    if kind == FLIP
        && timeout_ms == INFINITE
        && let Some(host) = crate::host::made()
    {
        host.flip();
    }
    let (tick, wait) = next_tick(kind);
    let limit = Duration::from_millis(u64::from(timeout_ms));
    let hr = if wait > limit {
        std::thread::sleep(limit);
        TIMEOUT
    } else {
        std::thread::sleep(wait);
        last(kind).fetch_max(tick, Ordering::Relaxed);
        S_OK
    };
    // A frame begun on a time-out takes no tick: its flip still does.
    if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        // Tokens count frames (consecutive), not ticks.
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        if !token.is_null() {
            // SAFETY: checked non-null; the title passes its token.
            unsafe { token.write(frame) };
        }
    }
    #[cfg(feature = "diagnostics")]
    tracing::debug!(
        kind,
        timeout_ms,
        flags,
        hr,
        tick,
        wait_us = wait.as_micros() as u64,
        frame = current(),
        "frame wait"
    );
    hr
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 2_000, 1, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut first), S_OK);
        // The next wait ends on the next tick, 2 ms on, not at its time-out
        // (1 s). A loaded machine may wake the thread late, which only ever
        // adds time: one wait of a few ending within 50 ms is enough, and a
        // wait that sleeps its time-out never does.
        let mut second = first;
        let quick = (0..8).any(|_| {
            let before = Instant::now();
            let mut token = 0;
            assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut token), S_OK);
            assert!(token > second);
            second = token;
            before.elapsed() <= Duration::from_millis(50)
        });
        assert!(quick, "no wait ended within 50 ms of a 2 ms tick");
        assert_eq!(wait(null, 0, 0, std::ptr::null(), 0, &mut second), TIMEOUT);
        // Tokens count frames, a frame started early on a time-out too.
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(
            wait(null, 0, 0, std::ptr::null(), flags, &mut early),
            TIMEOUT
        );
        let mut next = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut next), S_OK);
        assert!(early == second + 1 && next == early + 1);
        assert!(LAST[0].load(Ordering::Relaxed) >= 2);

        // The title's loop (the clock is global, so in the same test): an
        // origin poll that times out (and starts the frame), then a
        // blocking flip wait. Each flip takes its own tick, the next one
        // unless the thread woke late (a loaded machine oversleeps): then
        // it skips only the ticks that passed meanwhile.
        const INTERVAL: Duration = Duration::from_millis(5);
        // The test's own clock: the interval starts between `t0` and `t1`,
        // counting from the last tick handed out. Set again until they are
        // close (a few µs unless the thread is preempted in between): our
        // reading of a tick may then differ from the wait's only when a
        // boundary falls in that gap, not on every late flip.
        let base = LAST
            .iter()
            .map(|t| t.load(Ordering::Relaxed))
            .max()
            .unwrap();
        let (t0, t1) = (0..100)
            .find_map(|_| {
                let t0 = Instant::now();
                assert_eq!(
                    set_interval(null, null, INTERVAL.as_micros() as u32, 3, 0),
                    S_OK
                );
                let t1 = Instant::now();
                (t1 - t0 <= INTERVAL / 50).then_some((t0, t1))
            })
            .expect("setting the interval took over 100 µs 100 times");
        let tick_after = |since: Instant, at: Instant| {
            base + (at.duration_since(since).as_nanos() / INTERVAL.as_nanos()) as u64 + 1
        };
        let mut token = 0;
        let mut flips: Vec<u64> = Vec::new();
        let mut exact = 0;
        // Four flips, and more (up to 32) until one lands exactly on the
        // tick our clock expects: a tick boundary may fall between our
        // reading and the wait's own.
        while flips.len() < 4 || (exact == 0 && flips.len() < 32) {
            let _ = wait(null, 0, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut token);
            let prev = flips.last().map_or(0, |&f| f + 1);
            let asked = Instant::now();
            let due = tick_after(t1, asked).max(prev);
            assert_eq!(
                wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
                S_OK
            );
            let woke = Instant::now();
            let latest = tick_after(t0, woke).max(prev);
            let flip = LAST[2].load(Ordering::Relaxed);
            // The wait slept until its tick (the interval starts at `t0` or
            // later), so it never ends early. 1 ms covers a sleep that wakes
            // a little before its deadline.
            assert!(
                woke + Duration::from_millis(1) >= t0 + INTERVAL * (flip - base) as u32,
                "{flips:?}: woke early for {flip}"
            );
            // A new tick, not before the one due when the wait was called,
            // and no later than the ticks that passed during the call (a
            // late wake-up skips those).
            assert!(
                flip >= due && flip <= latest && flip >= prev,
                "{flips:?}: {flip} not in {due}..={latest}"
            );
            exact += usize::from(flip == due);
            flips.push(flip);
        }
        assert!(exact > 0, "no flip on the expected tick: {flips:?}");
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, _) = next_tick(FLIP);
        assert!(tick > *flips.last().unwrap());
    }
}
