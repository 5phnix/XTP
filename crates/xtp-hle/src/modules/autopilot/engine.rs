//! Runs a script one tick at a time. The caller says what time it is and
//! how to read the title; the engine says which buttons to hold and what to
//! log. It neither sleeps nor touches the host, so tests drive it with a
//! fake title and a fake clock.

use std::collections::BTreeMap;
use std::time::Duration;

use super::script::{Action, Condition, Memory, Script, Step, Test};

/// How long a press leaves its buttons up before the next step, so that a
/// press right after it is a new press.
pub(crate) const GAP: Duration = Duration::from_millis(100);

/// What the engine reads: the title's memory and how far it got.
pub(crate) trait Title {
    /// The value at `memory` now; `None` when it is outside the title.
    fn read(&self, memory: Memory) -> Option<u64>;
    /// Frames presented since the title started.
    fn frames(&self) -> u64;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// A step is under way.
    Running,
    /// Every step ran; nothing is held.
    Done,
    /// A wait timed out or the title could not be read: the script
    /// stopped, nothing is held.
    Stopped,
    /// An `exit after` came due: the caller ends the process.
    Exit,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Log {
    pub(crate) error: bool,
    pub(crate) text: String,
}

/// What a tick decided.
#[derive(Debug, PartialEq)]
pub(crate) struct Tick {
    /// Pad buttons to hold down until the next tick.
    pub(crate) buttons: u32,
    pub(crate) status: Status,
    pub(crate) logs: Vec<Log>,
}

/// The phrase that says the player has control; `cargo xtask play` waits
/// for it (`xtask/src/play.rs`).
pub(crate) const HANDED_BACK: &str = "control handed back to the player";
/// The phrase that says the script gave up; `cargo xtask play` stops the
/// title on it.
pub(crate) const STOPS: &str = "the script stops here";

pub(crate) struct Engine {
    steps: Vec<Step>,
    /// The step under way (or next).
    next: usize,
    step: Option<Running>,
    held: u32,
    released: bool,
    exit_at: Option<Duration>,
    status: Status,
    logs: Vec<Log>,
}

/// The step under way.
struct Running {
    since: Duration,
    /// For `changed`: each value as it was when the step began.
    baseline: BTreeMap<Memory, u64>,
    /// A press or a repeat's press: when its buttons come up.
    up: Duration,
    /// A press: when the step ends. A repeat: when it presses next.
    next: Duration,
    presses: u32,
}

/// What became of the step under way this tick.
enum Progress {
    Stay,
    Next,
    Stop(String),
}

/// Where a condition is evaluated.
struct Context<'a> {
    now: Duration,
    since: Duration,
    frames: u64,
    title: &'a dyn Title,
    baseline: &'a BTreeMap<Memory, u64>,
}

fn outside(memory: Memory) -> String {
    format!("{memory} is outside the title's image")
}

fn holds(condition: &Condition, at: &Context) -> Result<bool, String> {
    Ok(match condition {
        Condition::Time(compare, time) => compare.holds(at.now, *time),
        Condition::Elapsed(compare, time) => compare.holds(at.now - at.since, *time),
        Condition::Frames(compare, frames) => compare.holds(at.frames, *frames),
        Condition::Memory(memory, test) => {
            let value = at.title.read(*memory).ok_or_else(|| outside(*memory))?;
            match test {
                Test::Compare(compare, expected) => compare.holds(value, *expected),
                Test::NonZero => value != 0,
                Test::Changed => at.baseline.get(memory) != Some(&value),
            }
        }
        Condition::Not(inner) => !holds(inner, at)?,
        Condition::And(all) => {
            for c in all {
                if !holds(c, at)? {
                    return Ok(false);
                }
            }
            true
        }
        Condition::Or(all) => {
            for c in all {
                if holds(c, at)? {
                    return Ok(true);
                }
            }
            false
        }
    })
}

/// The frame count and the values a condition reads, for the log.
fn state(condition: &Condition, title: &dyn Title) -> String {
    let mut text = format!("frame {}", title.frames());
    for memory in condition.memory() {
        match title.read(memory) {
            Some(value) => text += &format!(", {memory} = {value:#x}"),
            None => text += &format!(", {memory} unreadable"),
        }
    }
    text
}

fn seconds(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

impl Engine {
    pub(crate) fn new(script: Script) -> Engine {
        Engine {
            steps: script.steps,
            next: 0,
            step: None,
            held: 0,
            released: false,
            exit_at: None,
            status: Status::Running,
            logs: Vec::new(),
        }
    }

    /// An `exit after` is still to come.
    pub(crate) fn exit_pending(&self) -> bool {
        self.exit_at.is_some() && self.status != Status::Exit
    }

    /// Runs the script at `now` (since the autopilot started): every step
    /// that is due, up to one that has to wait.
    pub(crate) fn tick(&mut self, now: Duration, title: &dyn Title) -> Tick {
        self.run(now, title);
        Tick {
            buttons: self.held,
            status: self.status,
            logs: std::mem::take(&mut self.logs),
        }
    }

    fn info(&mut self, text: String) {
        self.logs.push(Log { error: false, text });
    }

    fn run(&mut self, now: Duration, title: &dyn Title) {
        loop {
            if self.status == Status::Exit {
                return;
            }
            if let Some(at) = self.exit_at
                && now >= at
            {
                self.held = 0;
                self.status = Status::Exit;
                self.info("exit after: time is up, ending the title".into());
                return;
            }
            if self.status != Status::Running {
                return;
            }
            if self.next == self.steps.len() {
                self.held = 0;
                self.status = Status::Done;
                let text = if self.released {
                    "script done".to_string()
                } else {
                    format!("script done: {HANDED_BACK}")
                };
                self.info(text);
                return;
            }
            if self.step.is_none()
                && let Err(e) = self.begin(now, title)
            {
                self.stop(e);
                return;
            }
            match self.advance(now, title) {
                Progress::Stay => return,
                Progress::Next => {
                    self.step = None;
                    self.next += 1;
                }
                Progress::Stop(why) => {
                    self.stop(why);
                    return;
                }
            }
        }
    }

    fn stop(&mut self, why: String) {
        self.held = 0;
        self.status = Status::Stopped;
        self.logs.push(Log {
            error: true,
            text: format!("{why}; {STOPS} and presses nothing more: the title is yours"),
        });
    }

    fn begin(&mut self, now: Duration, title: &dyn Title) -> Result<(), String> {
        let step = &self.steps[self.next];
        let (line, text) = (step.line, format!("line {}: {}", step.line, step.text));
        let mut baseline = BTreeMap::new();
        let (mut up, mut next) = (now, now);
        match &step.action {
            Action::Wait { until, .. } | Action::Repeat { until, .. } => {
                for memory in until.changed() {
                    let value = title
                        .read(memory)
                        .ok_or_else(|| format!("line {line}: {}", outside(memory)))?;
                    baseline.insert(memory, value);
                }
            }
            Action::Press { buttons, hold } => {
                self.held = buttons.bits;
                up = now + *hold;
                next = up + GAP;
            }
            Action::Release | Action::ExitAfter(_) => {}
        }
        self.info(text);
        self.step = Some(Running {
            since: now,
            baseline,
            up,
            next,
            presses: 0,
        });
        Ok(())
    }

    fn advance(&mut self, now: Duration, title: &dyn Title) -> Progress {
        let step = &self.steps[self.next];
        let line = step.line;
        let Some(run) = self.step.as_mut() else {
            return Progress::Stay;
        };
        let (until, timeout) = match &step.action {
            Action::Press { buttons, .. } => {
                if now < run.up {
                    self.held = buttons.bits;
                    return Progress::Stay;
                }
                self.held = 0;
                return if now < run.next {
                    Progress::Stay
                } else {
                    Progress::Next
                };
            }
            Action::Release => {
                self.held = 0;
                self.released = true;
                self.logs.push(Log {
                    error: false,
                    text: format!("line {line}: {HANDED_BACK}"),
                });
                return Progress::Next;
            }
            Action::ExitAfter(after) => {
                let at = now + *after;
                self.exit_at = Some(self.exit_at.map_or(at, |was| was.min(at)));
                self.logs.push(Log {
                    error: false,
                    text: format!("line {line}: the title ends in {}", seconds(*after)),
                });
                return Progress::Next;
            }
            Action::Wait { until, timeout } | Action::Repeat { until, timeout, .. } => {
                (until, *timeout)
            }
        };
        let at = Context {
            now,
            since: run.since,
            frames: title.frames(),
            title,
            baseline: &run.baseline,
        };
        let waited = now - run.since;
        match holds(until, &at) {
            Err(e) => return Progress::Stop(format!("line {line}: {e}")),
            Ok(true) => {
                self.held = 0;
                let presses = match run.presses {
                    0 => String::new(),
                    1 => ", 1 press".to_string(),
                    n => format!(", {n} presses"),
                };
                let text = format!(
                    "line {line}: met after {}{presses} ({})",
                    seconds(waited),
                    state(until, title)
                );
                self.info(text);
                return Progress::Next;
            }
            Ok(false) => {}
        }
        if let Some(timeout) = timeout
            && waited >= timeout
        {
            return Progress::Stop(format!(
                "line {line}: timed out after {} ({})",
                seconds(timeout),
                state(until, title)
            ));
        }
        if let Action::Repeat {
            buttons,
            hold,
            every,
            ..
        } = &step.action
        {
            if self.held != 0 {
                // Up for at least a tick before the next press.
                if now >= run.up {
                    self.held = 0;
                }
            } else if now >= run.next {
                run.presses += 1;
                run.up = now + *hold;
                run.next = now + *every;
                self.held = buttons.bits;
                let text = format!("line {line}: press {} of {}", run.presses, buttons.names);
                self.info(text);
            }
        }
        Progress::Stay
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::super::script::parse;
    use super::*;

    /// A title whose memory and frame count the test sets.
    #[derive(Default)]
    struct Fake {
        memory: RefCell<BTreeMap<u64, u8>>,
        frames: Cell<u64>,
        /// Bytes past it are outside the image.
        size: u64,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                size: 0x1000,
                ..Default::default()
            }
        }

        fn set(&self, rva: u64, bytes: &[u8]) {
            let mut memory = self.memory.borrow_mut();
            for (i, &b) in bytes.iter().enumerate() {
                memory.insert(rva + i as u64, b);
            }
        }
    }

    impl Title for Fake {
        fn read(&self, at: Memory) -> Option<u64> {
            if at.rva + at.width.bytes() > self.size {
                return None;
            }
            let memory = self.memory.borrow();
            let bytes = (0..at.width.bytes()).map(|i| *memory.get(&(at.rva + i)).unwrap_or(&0));
            Some(bytes.rev().fold(0, |value, b| (value << 8) | u64::from(b)))
        }

        fn frames(&self) -> u64 {
            self.frames.get()
        }
    }

    fn engine(text: &str) -> Engine {
        let text = format!("build 0123456789abcdef\n{text}");
        Engine::new(parse(&text, crate::modules::gameinput::button).unwrap())
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn a() -> u32 {
        crate::modules::gameinput::button("a").unwrap()
    }

    /// The log lines' text.
    fn texts(tick: &Tick) -> Vec<&str> {
        tick.logs.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn autopilot_waits_for_title_memory_then_hands_control_back() {
        let title = Fake::new();
        let mut e = engine(
            "wait u32 0x10 == 0x1000001 timeout 10\npress a 100\nwait u8 0x20 changed\nrelease",
        );
        let t = e.tick(ms(0), &title);
        assert_eq!((t.buttons, t.status), (0, Status::Running));
        assert_eq!(texts(&t), ["line 2: wait u32 0x10 == 0x1000001 timeout 10"]);
        // Little-endian, as the title stores it.
        title.set(0x10, &[1, 0, 0, 1]);
        title.frames.set(42);
        let t = e.tick(ms(1500), &title);
        assert_eq!(
            texts(&t),
            [
                "line 2: met after 1.5 s (frame 42, u32 0x10 = 0x1000001)",
                "line 3: press a 100"
            ]
        );
        assert_eq!(t.buttons, a());
        assert_eq!(e.tick(ms(1599), &title).buttons, a());
        // Up after the hold; the next step waits for the gap.
        let t = e.tick(ms(1600), &title);
        assert_eq!((t.buttons, t.logs.len()), (0, 0));
        let t = e.tick(ms(1700), &title);
        assert_eq!(texts(&t), ["line 4: wait u8 0x20 changed"]);
        // `changed`: from the value when the step began.
        title.set(0x20, &[7]);
        let t = e.tick(ms(1800), &title);
        assert_eq!(
            texts(&t),
            [
                "line 4: met after 0.1 s (frame 42, u8 0x20 = 0x7)",
                "line 5: release",
                "line 5: control handed back to the player",
                "script done",
            ]
        );
        assert_eq!((t.buttons, t.status), (0, Status::Done));
        assert!(!e.exit_pending());
    }

    #[test]
    fn autopilot_repeats_a_press_until_the_title_took_it() {
        let title = Fake::new();
        let mut e = engine("repeat a hold 200 every 1000 until u8 0x30 == 1 timeout 60");
        let t = e.tick(ms(0), &title);
        assert_eq!(t.buttons, a());
        assert_eq!(texts(&t)[1], "line 2: press 1 of a");
        assert_eq!(e.tick(ms(150), &title).buttons, a());
        assert_eq!(e.tick(ms(200), &title).buttons, 0);
        assert_eq!(e.tick(ms(999), &title).buttons, 0);
        let t = e.tick(ms(1000), &title);
        assert_eq!((t.buttons, texts(&t)), (a(), vec!["line 2: press 2 of a"]));
        // Taken while held: let go at once and go on.
        title.set(0x30, &[1]);
        let t = e.tick(ms(1100), &title);
        assert_eq!(t.buttons, 0);
        assert_eq!(
            texts(&t),
            [
                "line 2: met after 1.1 s, 2 presses (frame 0, u8 0x30 = 0x1)",
                "script done: control handed back to the player"
            ]
        );
    }

    #[test]
    fn autopilot_presses_come_up_for_a_tick_even_when_ticks_are_late() {
        let title = Fake::new();
        let mut e = engine("repeat a every 300 until frames >= 10");
        assert_eq!(e.tick(ms(0), &title).buttons, a());
        // Late past both the hold and the next press: up first.
        assert_eq!(e.tick(ms(900), &title).buttons, 0);
        assert_eq!(e.tick(ms(905), &title).buttons, a());
    }

    #[test]
    fn autopilot_does_not_press_when_the_condition_already_holds() {
        let title = Fake::new();
        title.frames.set(10);
        let mut e = engine("repeat a every 300 until frames >= 10");
        let t = e.tick(ms(0), &title);
        assert_eq!((t.buttons, t.status), (0, Status::Done));
        assert_eq!(texts(&t)[1], "line 2: met after 0.0 s (frame 10)");
    }

    #[test]
    fn autopilot_timeout_stops_the_script_with_what_it_saw() {
        let title = Fake::new();
        title.set(0x40, &[3]);
        let mut e = engine("repeat a every 500 until u8 0x40 == 1 timeout 2\nrelease");
        assert_eq!(e.tick(ms(0), &title).buttons, a());
        assert_eq!(e.tick(ms(1999), &title).status, Status::Running);
        let t = e.tick(ms(2000), &title);
        assert_eq!((t.buttons, t.status), (0, Status::Stopped));
        assert_eq!(
            t.logs,
            [Log {
                error: true,
                text: "line 2: timed out after 2.0 s (frame 0, u8 0x40 = 0x3); the script stops \
                       here and presses nothing more: the title is yours"
                    .into()
            }]
        );
        // It stays stopped, with nothing held.
        let t = e.tick(ms(3000), &title);
        assert_eq!((t.buttons, t.status, t.logs.len()), (0, Status::Stopped, 0));
    }

    #[test]
    fn autopilot_stops_on_memory_outside_the_title() {
        let title = Fake::new();
        let mut e = engine("wait u8 0x10 == 1 or u32 0x2000 == 0");
        let t = e.tick(ms(0), &title);
        assert_eq!(t.status, Status::Stopped);
        assert!(
            t.logs[1]
                .text
                .starts_with("line 2: u32 0x2000 is outside the title's image; the script stops"),
            "{:?}",
            t.logs
        );
        // A `changed` baseline is read when the step begins.
        let mut e = engine("wait u32 0x2000 changed");
        let t = e.tick(ms(0), &title);
        assert_eq!(t.status, Status::Stopped);
        assert!(t.logs[0].text.starts_with("line 2: u32 0x2000 is outside"));
    }

    #[test]
    fn autopilot_conditions_combine_time_frames_and_memory() {
        let title = Fake::new();
        let mut e = engine(
            "wait time >= 5 and (frames > 100 or u16 0x50 nonzero) and not u8 0x52 == 1\n\
             wait elapsed >= 250ms",
        );
        title.frames.set(101);
        title.set(0x52, &[1]);
        assert_eq!(e.tick(ms(4999), &title).logs.len(), 1);
        // Time is up and frames are there, but 0x52 is still 1.
        assert_eq!(e.tick(ms(6000), &title).logs.len(), 0);
        title.set(0x52, &[0]);
        title.frames.set(0);
        assert_eq!(e.tick(ms(6000), &title).logs.len(), 0);
        title.set(0x50, &[0, 1]);
        let t = e.tick(ms(7000), &title);
        assert_eq!(
            texts(&t),
            [
                "line 2: met after 2.0 s (frame 0, u16 0x50 = 0x100, u8 0x52 = 0x0)",
                "line 3: wait elapsed >= 250ms"
            ]
        );
        assert_eq!(e.tick(ms(7249), &title).status, Status::Running);
        assert_eq!(e.tick(ms(7250), &title).status, Status::Done);
    }

    #[test]
    fn autopilot_exit_after_comes_due_even_once_the_script_ended_or_stopped() {
        let title = Fake::new();
        let mut e = engine("exit after 10\nrelease");
        let t = e.tick(ms(1000), &title);
        assert_eq!(t.status, Status::Done);
        assert_eq!(texts(&t)[1], "line 2: the title ends in 10.0 s");
        assert!(e.exit_pending());
        assert_eq!(e.tick(ms(10_999), &title).status, Status::Done);
        let t = e.tick(ms(11_000), &title);
        assert_eq!(t.status, Status::Exit);
        assert!(!e.exit_pending());

        // The earliest exit wins; a timeout does not cancel it.
        let mut e = engine("exit after 60\nexit after 5\nwait frames > 1 timeout 1");
        assert_eq!(e.tick(ms(0), &title).status, Status::Running);
        assert_eq!(e.tick(ms(1000), &title).status, Status::Stopped);
        assert!(e.exit_pending());
        assert_eq!(e.tick(ms(5000), &title).status, Status::Exit);
    }

    #[test]
    fn autopilot_a_wait_without_timeout_waits_as_long_as_it_takes() {
        let title = Fake::new();
        let mut e = engine("wait u64 0x100 == 0xffffffffffffffff");
        assert_eq!(e.tick(ms(0), &title).status, Status::Running);
        assert_eq!(e.tick(ms(86_400_000), &title).status, Status::Running);
        title.set(0x100, &[0xff; 8]);
        assert_eq!(e.tick(ms(86_400_001), &title).status, Status::Done);
    }
}
