//! Exclusive preparation times. The submitting thread publishes a whole
//! submission at once so the frame meter always sees its parts and total
//! in the same window.

use std::cell::RefCell;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Copies,
    Fills,
    Textures,
    Snapshots,
    Watch,
    Steps,
    Other,
}

const PHASES: usize = Phase::Other as usize + 1;

struct Clock {
    phase: Phase,
    since: Instant,
    waited: u64,
    times: [Duration; PHASES],
}

impl Clock {
    fn advance(&mut self, now: Instant, waited: u64) {
        self.times[self.phase as usize] +=
            (now - self.since).saturating_sub(Duration::from_micros(waited - self.waited));
        self.since = now;
        self.waited = waited;
    }
}

thread_local! {
    static CLOCK: RefCell<Option<Clock>> = const { RefCell::new(None) };
}

/// A fault can interrupt a clock update on this thread.
#[cfg(feature = "diagnostics")]
pub(crate) fn active() -> bool {
    CLOCK.with(|clock| !clock.try_borrow().is_ok_and(|c| c.is_none()))
}

impl Phase {
    fn switch(self) -> Option<Self> {
        CLOCK.with_borrow_mut(|clock| {
            let clock = clock.as_mut()?;
            clock.advance(Instant::now(), super::waited_here());
            Some(std::mem::replace(&mut clock.phase, self))
        })
    }

    pub(crate) fn enter(self) -> Region {
        // A copy-back inside a wait can ask the write watch too. Its
        // entire wait is already taken off the enclosing region.
        Region(if super::WAITING.get().0 == 0 {
            self.switch()
        } else {
            None
        })
    }
}

#[must_use]
pub(crate) struct Region(Option<Phase>);

impl Drop for Region {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            previous.switch();
        }
    }
}

pub(super) struct Recording;

impl Recording {
    pub(super) fn start(since: Instant, waited: u64) -> Self {
        CLOCK.replace(Some(Clock {
            phase: Phase::Other,
            since,
            waited,
            times: [Duration::ZERO; PHASES],
        }));
        Self
    }

    pub(super) fn finish(self, now: Instant, waited: u64, preparing: Duration, work: &Work) {
        let mut clock = CLOCK.take().expect("active submission");
        clock.advance(now, waited);
        let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        frame.total += preparing;
        for (sum, time) in frame.times.iter_mut().zip(clock.times) {
            *sum += time;
        }
        frame.work.copy_bytes += work.copy_bytes;
        frame.work.fill_bytes += work.fill_bytes;
        frame.work.merged_fills += work.merged_fills;
        frame.work.texture_bytes += work.texture_bytes;
        for (sum, count) in frame.work.steps.iter_mut().zip(work.steps) {
            *sum += count;
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        CLOCK.take();
    }
}

#[derive(Default)]
pub(super) struct Work {
    pub copy_bytes: u64,
    pub fill_bytes: u64,
    pub merged_fills: u64,
    pub texture_bytes: u64,
    pub steps: [u64; Kind::Texture as usize + 1],
}

pub(super) enum Kind {
    Copy,
    Fill,
    Refresh,
    Run,
    Host,
    GpuWrite,
    GpuCopy,
    GpuWord,
    Texture,
}

#[derive(Default)]
pub(super) struct Frame {
    pub total: Duration,
    pub times: [Duration; PHASES],
    pub work: Work,
}

static FRAME: LazyLock<Mutex<Frame>> = LazyLock::new(|| Mutex::new(Frame::default()));

pub(super) fn take() -> Frame {
    std::mem::take(&mut *FRAME.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_inside_a_submission_wait_stay_off_the_preparation_clock() {
        let Some(host) = crate::testgpu::device("phases_inside_a_submission_wait") else {
            return;
        };
        let start = Instant::now();
        let waited = super::super::waited_here();
        let _recording = Recording::start(start, waited);
        {
            let _copy = Phase::Copies.enter();
            host.waiting(|| {
                let _watch = Phase::Watch.enter();
                host.waiting(|| {
                    let _snapshot = Phase::Snapshots.enter();
                    std::thread::sleep(Duration::from_millis(1));
                });
            });
        }
        let now = Instant::now();
        let after = super::super::waited_here();
        let mut clock = CLOCK.take().unwrap();
        clock.advance(now, after);
        assert!(after > waited);
        assert_eq!(clock.times[Phase::Watch as usize], Duration::ZERO);
        assert_eq!(clock.times[Phase::Snapshots as usize], Duration::ZERO);
        assert_eq!(
            clock.times.iter().sum::<Duration>(),
            now - start - Duration::from_micros(after - waited)
        );
    }

    #[test]
    fn nested_watch_and_staging_wait_are_exclusive() {
        let start = Instant::now();
        let mut clock = Clock {
            phase: Phase::Textures,
            since: start,
            waited: 100,
            times: [Duration::ZERO; PHASES],
        };
        let us = Duration::from_micros;
        clock.advance(start + us(3), 100);
        clock.phase = Phase::Watch;
        clock.advance(start + us(7), 100);
        clock.phase = Phase::Textures;
        clock.advance(start + us(28), 120);
        assert_eq!(clock.times[Phase::Textures as usize], us(4));
        assert_eq!(clock.times[Phase::Watch as usize], us(4));
        assert_eq!(clock.times.iter().sum::<Duration>(), us(28 - 20));
    }
}
