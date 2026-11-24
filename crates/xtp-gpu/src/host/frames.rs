//! One frame in flight (`XTP_FRAMES_IN_FLIGHT`, docs/analysis/frames-in-flight.md):
//! the title prepares frame N+1 while the GPU runs frame N. A present no
//! longer finishes every submission before it returns; the frame before it
//! is finished instead, before its copy is recorded ([`super::Host::flip`]),
//! and so is the frame presented last before a flip wait ends
//! (`crate::frame::wait`), as a console's flip follows the GPU. The title
//! is then never more than one frame ahead of the GPU.
//!
//! Ported from simnJS/AnyPS5 (7b85afc1aa: a flip completes once its work is
//! submitted and the previous flip's work is done; 4328d9d796: the title
//! never more than one frame ahead of the GPU, after eight frames ahead let
//! a title rewrite tables the GPU had not read yet).
//!
//! `XTP_FRAMES_IN_FLIGHT=ab` alternates the two within one run, to compare
//! them on the same scene: phases of 20 s (`ab:<seconds>`), off first, the
//! first 3 s of each not measured (`ab:<seconds>:<settle>`), a line per
//! phase. Ported from simnJS/AnyPS5 (2eadda9b42, `KYTY_AB`: listed settings
//! alternate at flips within one session, with settling time and per-phase
//! frame counts).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// What `XTP_FRAMES_IN_FLIGHT` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Asked {
    /// Every submission finished at each present, as before (`0`).
    Off,
    /// One frame in flight (`1`).
    On,
    /// Off and on in turn within the run.
    Ab { phase: Duration, settle: Duration },
}

/// Without `XTP_FRAMES_IN_FLIGHT`: on, once a Series S world session of
/// 2 minutes or more ran with it without corruption, crash or GPU reset
/// (docs/analysis/frames-in-flight.md).
const DEFAULT: Asked = Asked::Off;

/// `XTP_FRAMES_IN_FLIGHT`'s value read: `0`, `1`, `ab`, `ab:<phase>` or
/// `ab:<phase>:<settle>` (seconds, phases of 2 to 3600 s, settling shorter
/// than the phase); unset or empty, the default.
pub(crate) fn parse(value: Option<&str>) -> Result<Asked, String> {
    let Some(text) = value.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(DEFAULT);
    };
    match text {
        "0" => return Ok(Asked::Off),
        "1" => return Ok(Asked::On),
        _ => {}
    }
    let bad = || format!("XTP_FRAMES_IN_FLIGHT={text}: 0, 1 or ab[:<phase s>[:<settle s>]]");
    let mut fields = text.split(':');
    if fields.next() != Some("ab") {
        return Err(bad());
    }
    let mut seconds = |default: u64| match fields.next() {
        None => Ok(default),
        Some(f) => f.trim().parse::<u64>().map_err(|_| bad()),
    };
    let (phase, settle) = (seconds(20)?, seconds(3)?);
    if fields.next().is_some() || !(2..=3600).contains(&phase) || settle >= phase {
        return Err(bad());
    }
    Ok(Asked::Ab {
        phase: Duration::from_secs(phase),
        settle: Duration::from_secs(settle),
    })
}

/// What was asked, read once (logged, and a bad value logged and taken as
/// the default).
pub(crate) fn asked() -> Asked {
    static ASKED: OnceLock<Asked> = OnceLock::new();
    *ASKED.get_or_init(|| {
        let value = std::env::var("XTP_FRAMES_IN_FLIGHT").ok();
        let asked = parse(value.as_deref()).unwrap_or_else(|e| {
            tracing::warn!("{e}; taken as unset");
            DEFAULT
        });
        tracing::info!(?asked, "frames in flight");
        AHEAD.store(asked == Asked::On, Ordering::Relaxed);
        asked
    })
}

/// Whether the frame being presented may run on past its present (in an
/// A/B run, as its phase says).
static AHEAD: AtomicBool = AtomicBool::new(false);

/// Whether a present lets its frame run on while the title prepares the
/// next one.
pub(crate) fn ahead() -> bool {
    asked();
    AHEAD.load(Ordering::Relaxed)
}

/// Submissions in flight at most: a submission past them waits for the
/// oldest first. A world frame of BTP 6 makes 20 to 40 (run of
/// 2026-11-24 18:11), so one frame in flight needs room for two.
pub(crate) fn cap() -> usize {
    if ahead() { 128 } else { 16 }
}

/// A second of frames as the frame-rate line measures it, per frame (ms).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Second {
    /// How long it lasted and the frames presented in it.
    pub(crate) seconds: f64,
    pub(crate) frames: f64,
    pub(crate) gpu: f64,
    pub(crate) waited: f64,
    /// Of `waited`, at flip waits and presents.
    pub(crate) at_ends: f64,
    pub(crate) overlap: f64,
    /// The longest time between two presents in it.
    pub(crate) longest: f64,
}

/// The in-run A/B: phase 0 off, 1 on, and so on; each measured past its
/// first `settle`.
pub(crate) struct Ab {
    phase: Duration,
    settle: Duration,
    /// The phase and when it began, from the first second on.
    at: Option<(u64, Instant)>,
    /// The phase's measured seconds, summed (per-frame values weighted by
    /// their frames).
    sum: Second,
}

impl Ab {
    pub(crate) fn new(phase: Duration, settle: Duration) -> Ab {
        Ab {
            phase,
            settle,
            at: None,
            sum: Second::default(),
        }
    }

    /// A second measured, ending at `now`: counted for its phase unless it
    /// began while the phase settled. Returns whether frames now run
    /// ahead, and the line closing a phase that ended (the next one starts
    /// with the next present).
    pub(crate) fn second(&mut self, now: Instant, s: Second) -> (bool, Option<String>) {
        let Some((index, began)) = self.at else {
            // The run's first second: its frames came before the A/B began.
            self.at = Some((0, now));
            return (false, None);
        };
        let start = now
            .checked_sub(Duration::from_secs_f64(s.seconds))
            .unwrap_or(now);
        if start >= began + self.settle {
            let w = s.frames;
            let sum = &mut self.sum;
            sum.seconds += s.seconds;
            sum.frames += w;
            sum.gpu += s.gpu * w;
            sum.waited += s.waited * w;
            sum.at_ends += s.at_ends * w;
            sum.overlap += s.overlap * w;
            sum.longest = sum.longest.max(s.longest);
        }
        if now.duration_since(began) < self.phase {
            return (index % 2 == 1, None);
        }
        let line = summary(index, &std::mem::take(&mut self.sum));
        self.at = Some((index + 1, now));
        ((index + 1) % 2 == 1, Some(line))
    }
}

/// The line closing phase `index`.
fn summary(index: u64, sum: &Second) -> String {
    let arm = if index % 2 == 1 { "B on" } else { "A off" };
    let frames = sum.frames.max(1.0);
    format!(
        "frames in flight A/B: phase {index} ({arm}), {seconds:.0} s measured: {fps:.2} fps, \
         {ms:.1} ms/frame, longest frame {longest:.0} ms; per frame {gpu:.1} ms on the GPU, \
         {waited:.1} waiting ({at_ends:.1} at flips and presents), {overlap:.1} overlapped",
        seconds = sum.seconds,
        fps = sum.frames / sum.seconds.max(1e-9),
        ms = sum.seconds * 1000.0 / frames,
        longest = sum.longest,
        gpu = sum.gpu / frames,
        waited = sum.waited / frames,
        at_ends = sum.at_ends / frames,
        overlap = sum.overlap / frames,
    )
}

/// The run's A/B, if asked for: the frame-rate meter hands it each second.
pub(crate) fn measured(now: Instant, s: Second) {
    static AB: std::sync::Mutex<Option<Ab>> = std::sync::Mutex::new(None);
    let Asked::Ab { phase, settle } = asked() else {
        return;
    };
    let mut ab = AB.lock().unwrap_or_else(|e| e.into_inner());
    let (ahead, line) = ab
        .get_or_insert_with(|| Ab::new(phase, settle))
        .second(now, s);
    if let Some(line) = line {
        tracing::info!("{line}");
    }
    AHEAD.store(ahead, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_takes_0_1_and_ab() {
        assert_eq!(parse(None), Ok(DEFAULT));
        assert_eq!(parse(Some(" ")), Ok(DEFAULT));
        assert_eq!(parse(Some("0")), Ok(Asked::Off));
        assert_eq!(parse(Some("1")), Ok(Asked::On));
        let s = Duration::from_secs;
        assert_eq!(
            parse(Some("ab")),
            Ok(Asked::Ab {
                phase: s(20),
                settle: s(3)
            })
        );
        assert_eq!(
            parse(Some("ab:30:5")),
            Ok(Asked::Ab {
                phase: s(30),
                settle: s(5)
            })
        );
        for bad in ["yes", "2", "ab:1", "ab:10:10", "ab:x", "ab:10:2:1", "ba"] {
            assert!(parse(Some(bad)).is_err(), "{bad}");
        }
    }

    /// Phases alternate off and on at the second that ends one; the
    /// seconds that begin while a phase settles are not counted, and a
    /// phase's line gives its measured seconds per frame.
    #[test]
    fn an_ab_run_alternates_and_measures_past_the_settling() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut ab = Ab::new(Duration::from_secs(4), Duration::from_secs(1));
        let second = |frames: f64, waited: f64| Second {
            seconds: 1.0,
            frames,
            gpu: 100.0,
            waited,
            at_ends: waited / 2.0,
            overlap: 100.0 - waited,
            longest: 1000.0 / frames,
        };
        // Phase 0 (off) begins with the first second, which is not counted
        // (its frames came before the run's A/B began).
        assert_eq!(ab.second(at(0), second(1.0, 0.0)), (false, None));
        // 0..1 settles; 1..4 counted.
        assert_eq!(ab.second(at(1), second(9.0, 0.0)), (false, None));
        for s in 2..4 {
            assert_eq!(ab.second(at(s), second(2.0, 80.0)), (false, None));
        }
        let (ahead, line) = ab.second(at(4), second(2.0, 80.0));
        assert!(ahead, "phase 1 runs ahead");
        let line = line.expect("phase 0 ends");
        assert!(line.starts_with("frames in flight A/B: phase 0 (A off), 3 s measured: 2.00 fps"));
        assert!(
            line.contains("500.0 ms/frame, longest frame 500 ms"),
            "{line}"
        );
        assert!(line.contains("80.0 waiting (40.0 at flips and presents), 20.0 overlapped"));
        // Phase 1 (on): the second ending at 5 began as it did: settling.
        assert_eq!(ab.second(at(5), second(9.0, 0.0)), (true, None));
        for s in 6..8 {
            assert_eq!(ab.second(at(s), second(4.0, 10.0)), (true, None));
        }
        let (ahead, line) = ab.second(at(8), second(4.0, 10.0));
        assert!(!ahead, "phase 2 is off again");
        let line = line.unwrap();
        assert!(
            line.contains("phase 1 (B on), 3 s measured: 4.00 fps"),
            "{line}"
        );
        assert!(line.contains("10.0 waiting (5.0 at flips and presents)"));
    }
}
