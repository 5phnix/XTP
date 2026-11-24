//! The autopilot's script language, one step per line (docs/dev/autopilot.md
//! has the whole of it):
//!
//! ```text
//! build <BLAKE3 of the title executable>
//! wait <condition> [timeout <duration>]
//! press <buttons> [[hold] <duration>]
//! repeat <buttons> [hold <duration>] every <duration> until <condition> [timeout <duration>]
//! release
//! exit after <duration>
//! ```
//!
//! A condition is `time`, `elapsed` or `frames` compared with a number, or a
//! title value (`u8`, `u16`, `u32`, `u64` at an RVA) compared, `changed` or
//! `nonzero`; joined by `and`, `or`, `not` and parentheses. `#` starts a
//! comment; keywords take any case.

use std::time::Duration;

/// How long a press holds its buttons when the step does not say.
pub(crate) const HOLD: Duration = Duration::from_millis(200);

/// A script, checked: every step is valid and nothing is pressed after a
/// `release`.
#[derive(Debug, PartialEq)]
pub(crate) struct Script {
    /// The title build its RVAs belong to: the BLAKE3 of the executable,
    /// hex (at least its first 16 digits). Required when a step reads title
    /// memory.
    pub(crate) build: Option<String>,
    pub(crate) steps: Vec<Step>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Step {
    /// Its line in the file, from 1.
    pub(crate) line: usize,
    /// The line as written, without its comment.
    pub(crate) text: String,
    pub(crate) action: Action,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Action {
    /// Until the condition holds; `None` waits as long as it takes.
    Wait {
        until: Condition,
        timeout: Option<Duration>,
    },
    /// The buttons down for `hold`, then up (`engine::GAP`).
    Press { buttons: Buttons, hold: Duration },
    /// A press every `every` until the condition holds.
    Repeat {
        buttons: Buttons,
        hold: Duration,
        every: Duration,
        until: Condition,
        timeout: Option<Duration>,
    },
    /// Control back to the player: nothing is pressed from here on.
    Release,
    /// The process ends that long after this step.
    ExitAfter(Duration),
}

/// Pad buttons pressed together (`a+b`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Buttons {
    /// As written, for the log.
    pub(crate) names: String,
    /// GameInput button bits (`gameinput::button`).
    pub(crate) bits: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Condition {
    /// Since the autopilot started (the title's start).
    Time(Compare, Duration),
    /// Since the step began.
    Elapsed(Compare, Duration),
    /// Frames the title presented since it started.
    Frames(Compare, u64),
    Memory(Memory, Test),
    Not(Box<Condition>),
    And(Vec<Condition>),
    Or(Vec<Condition>),
}

/// A title value: an unsigned integer at an RVA of the title executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Memory {
    pub(crate) rva: u64,
    pub(crate) width: Width,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Width {
    U8,
    U16,
    U32,
    U64,
}

impl Width {
    pub(crate) fn bytes(self) -> u64 {
        match self {
            Width::U8 => 1,
            Width::U16 => 2,
            Width::U32 => 4,
            Width::U64 => 8,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Width::U8 => "u8",
            Width::U16 => "u16",
            Width::U32 => "u32",
            Width::U64 => "u64",
        }
    }

    fn max(self) -> u64 {
        u64::MAX >> (64 - 8 * self.bytes())
    }
}

impl std::fmt::Display for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {:#x}", self.width.name(), self.rva)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Test {
    Compare(Compare, u64),
    /// Not what it was when the step began.
    Changed,
    NonZero,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compare {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Compare {
    pub(crate) fn holds<T: PartialOrd>(self, a: T, b: T) -> bool {
        match self {
            Compare::Eq => a == b,
            Compare::Ne => a != b,
            Compare::Lt => a < b,
            Compare::Le => a <= b,
            Compare::Gt => a > b,
            Compare::Ge => a >= b,
        }
    }
}

impl Condition {
    /// Every title value it reads, each once, in order.
    pub(crate) fn memory(&self) -> Vec<Memory> {
        let mut all = Vec::new();
        self.visit(&mut |memory, _| all.push(memory));
        all.sort();
        all.dedup();
        all
    }

    /// The title values it tests for `changed`.
    pub(crate) fn changed(&self) -> Vec<Memory> {
        let mut all = Vec::new();
        self.visit(&mut |memory, test| {
            if test == Test::Changed {
                all.push(memory);
            }
        });
        all.sort();
        all.dedup();
        all
    }

    fn visit(&self, f: &mut impl FnMut(Memory, Test)) {
        match self {
            Condition::Memory(memory, test) => f(*memory, *test),
            Condition::Not(c) => c.visit(f),
            Condition::And(all) | Condition::Or(all) => all.iter().for_each(|c| c.visit(f)),
            Condition::Time(..) | Condition::Elapsed(..) | Condition::Frames(..) => {}
        }
    }
}

impl Action {
    fn condition(&self) -> Option<&Condition> {
        match self {
            Action::Wait { until, .. } | Action::Repeat { until, .. } => Some(until),
            _ => None,
        }
    }

    fn presses(&self) -> bool {
        matches!(self, Action::Press { .. } | Action::Repeat { .. })
    }
}

impl Script {
    /// Every title value the script reads, with the first line reading it.
    pub(crate) fn memory(&self) -> Vec<(usize, Memory)> {
        let mut all: Vec<(usize, Memory)> = Vec::new();
        for step in &self.steps {
            for memory in step
                .action
                .condition()
                .map(Condition::memory)
                .unwrap_or_default()
            {
                if !all.iter().any(|&(_, m)| m == memory) {
                    all.push((step.line, memory));
                }
            }
        }
        all
    }
}

/// Parses and checks a script. `button` gives the bit of a pad button by
/// name. Errors name the line.
pub(crate) fn parse(text: &str, button: impl Fn(&str) -> Option<u32>) -> Result<Script, String> {
    let mut build: Option<String> = None;
    let mut steps = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let text = raw.split('#').next().unwrap_or_default().trim();
        if text.is_empty() {
            continue;
        }
        let error = |e: String| format!("line {line}: {e} (`{text}`)");
        let words = tokens(text);
        if words[0].eq_ignore_ascii_case("build") {
            if build.is_some() {
                return Err(error("a second `build` line".into()));
            }
            build = Some(hash(&words[1..]).map_err(error)?);
            continue;
        }
        let action = action(&words, &button).map_err(error)?;
        steps.push(Step {
            line,
            text: text.to_string(),
            action,
        });
    }
    let script = Script { build, steps };
    if let (None, Some(&(line, memory))) = (&script.build, script.memory().first()) {
        return Err(format!(
            "line {line}: reads title memory ({memory}), so the script needs a `build <BLAKE3 \
             of the executable>` line: RVAs belong to one build of the title"
        ));
    }
    let released = script
        .steps
        .iter()
        .find(|s| s.action == Action::Release)
        .map(|s| s.line);
    if let Some(released) = released
        && let Some(late) = script
            .steps
            .iter()
            .find(|s| s.line > released && s.action.presses())
    {
        return Err(format!(
            "line {}: presses after `release` (line {released}), which handed control back to \
             the player",
            late.line
        ));
    }
    Ok(script)
}

/// The words of a line: names and numbers, comparison operators,
/// parentheses.
fn tokens(text: &str) -> Vec<&str> {
    let operator = |c: char| matches!(c, '=' | '!' | '<' | '>');
    let mut words = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        let Some(c) = rest.chars().next() else {
            return words;
        };
        let end = if c == '(' || c == ')' {
            1
        } else if operator(c) {
            rest.find(|c| !operator(c)).unwrap_or(rest.len())
        } else {
            rest.find(|c: char| c.is_whitespace() || c == '(' || c == ')' || operator(c))
                .unwrap_or(rest.len())
        };
        words.push(&rest[..end]);
        rest = &rest[end..];
    }
}

fn hash(words: &[&str]) -> Result<String, String> {
    match words {
        [hash]
            if (16..=64).contains(&hash.len()) && hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            Ok(hash.to_ascii_lowercase())
        }
        _ => Err("`build` takes the BLAKE3 of the title executable: 16 to 64 hex digits".into()),
    }
}

/// The words of one step, read from the front.
struct Words<'a> {
    words: &'a [&'a str],
}

impl<'a> Words<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.words.first().copied()
    }

    fn next(&mut self) -> Option<&'a str> {
        let (first, rest) = self.words.split_first()?;
        self.words = rest;
        Some(first)
    }

    /// Takes `keyword` (any case) if it comes next.
    fn eat(&mut self, keyword: &str) -> bool {
        let found = self.peek().is_some_and(|w| w.eq_ignore_ascii_case(keyword));
        if found {
            self.next();
        }
        found
    }

    fn expect(&mut self, keyword: &str) -> Result<(), String> {
        if self.eat(keyword) {
            return Ok(());
        }
        Err(match self.peek() {
            Some(w) => format!("expected `{keyword}`, found `{w}`"),
            None => format!("expected `{keyword}` at the end"),
        })
    }

    fn end(&self) -> Result<(), String> {
        match self.peek() {
            None => Ok(()),
            Some(w) => Err(format!("unexpected `{w}`")),
        }
    }

    /// A number with an optional unit, `ms` or `s` (joined or apart);
    /// without one, `unit` (nanoseconds per unit).
    fn duration(&mut self, unit: f64) -> Result<Duration, String> {
        let word = self.next().ok_or("a duration is missing")?;
        let split = word
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(word.len());
        let (number, mut suffix) = word.split_at(split);
        if suffix.is_empty()
            && let Some(next) = self.peek()
            && (next.eq_ignore_ascii_case("ms") || next.eq_ignore_ascii_case("s"))
        {
            suffix = next;
            self.next();
        }
        let scale = match suffix.to_ascii_lowercase().as_str() {
            "" => unit,
            "ms" => MS,
            "s" => S,
            _ => return Err(format!("`{word}` is no duration (a number, then ms or s)")),
        };
        // In whole nanoseconds, so that 0.05 s is 50 ms exactly.
        let nanos = number.parse::<f64>().ok().map(|n| (n * scale).round());
        match nanos {
            Some(n) if (0.0..u64::MAX as f64).contains(&n) => Ok(Duration::from_nanos(n as u64)),
            _ => Err(format!("`{word}` is no duration (a number, then ms or s)")),
        }
    }

    fn integer(&mut self) -> Result<u64, String> {
        let word = self.next().ok_or("a number is missing")?;
        let digits = word.replace('_', "");
        let value = match digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("0X"))
        {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => digits.parse(),
        };
        value.map_err(|_| format!("`{word}` is no number (decimal, or hex with 0x)"))
    }

    fn compare(&mut self) -> Result<Compare, String> {
        let word = self.next().ok_or("a comparison is missing")?;
        Ok(match word {
            "==" => Compare::Eq,
            "!=" => Compare::Ne,
            "<" => Compare::Lt,
            "<=" => Compare::Le,
            ">" => Compare::Gt,
            ">=" => Compare::Ge,
            _ => return Err(format!("expected ==, !=, <, <=, > or >=, found `{word}`")),
        })
    }
}

/// Duration units, in nanoseconds.
const MS: f64 = 1e6;
const S: f64 = 1e9;

fn action(words: &[&str], button: &impl Fn(&str) -> Option<u32>) -> Result<Action, String> {
    let mut w = Words { words: &words[1..] };
    let step = words[0].to_ascii_lowercase();
    let action = match step.as_str() {
        "wait" => {
            let until = or(&mut w)?;
            Action::Wait {
                until,
                timeout: timeout(&mut w)?,
            }
        }
        "press" => {
            let buttons = buttons(&mut w, button)?;
            let hold = if w.eat("hold") || w.peek().is_some() {
                w.duration(MS)?
            } else {
                HOLD
            };
            if hold.is_zero() {
                return Err("a press must hold its buttons for some time".into());
            }
            Action::Press { buttons, hold }
        }
        "repeat" => {
            let buttons = buttons(&mut w, button)?;
            let hold = if w.eat("hold") { w.duration(MS)? } else { HOLD };
            w.expect("every")?;
            let every = w.duration(MS)?;
            if hold.is_zero() || every <= hold {
                return Err(format!(
                    "every ({} ms) must be longer than hold ({} ms), or the buttons never come up",
                    every.as_millis(),
                    hold.as_millis()
                ));
            }
            w.expect("until")?;
            let until = or(&mut w)?;
            Action::Repeat {
                buttons,
                hold,
                every,
                until,
                timeout: timeout(&mut w)?,
            }
        }
        "release" => Action::Release,
        "exit" => {
            w.expect("after")?;
            Action::ExitAfter(w.duration(S)?)
        }
        _ => {
            return Err(format!(
                "unknown step `{}` (steps: build, wait, press, repeat, release, exit after)",
                words[0]
            ));
        }
    };
    w.end()?;
    Ok(action)
}

fn timeout(w: &mut Words) -> Result<Option<Duration>, String> {
    if w.eat("timeout") {
        return w.duration(S).map(Some);
    }
    Ok(None)
}

fn buttons(w: &mut Words, button: &impl Fn(&str) -> Option<u32>) -> Result<Buttons, String> {
    let names = w.next().ok_or("the buttons to press are missing")?;
    let mut bits = 0;
    for name in names.split('+') {
        bits |= button(name).ok_or_else(|| {
            format!(
                "unknown pad button `{name}` (named as in [input.bindings]: a, b, x, y, menu, \
                 view, dpad_up, dpad_down, dpad_left, dpad_right, left_shoulder, \
                 right_shoulder, left_stick_click, right_stick_click)"
            )
        })?;
    }
    Ok(Buttons {
        names: names.to_string(),
        bits,
    })
}

fn or(w: &mut Words) -> Result<Condition, String> {
    let mut all = vec![and(w)?];
    while w.eat("or") {
        all.push(and(w)?);
    }
    Ok(if all.len() == 1 {
        all.remove(0)
    } else {
        Condition::Or(all)
    })
}

fn and(w: &mut Words) -> Result<Condition, String> {
    let mut all = vec![not(w)?];
    while w.eat("and") {
        all.push(not(w)?);
    }
    Ok(if all.len() == 1 {
        all.remove(0)
    } else {
        Condition::And(all)
    })
}

fn not(w: &mut Words) -> Result<Condition, String> {
    if w.eat("not") {
        return Ok(Condition::Not(Box::new(not(w)?)));
    }
    atom(w)
}

fn atom(w: &mut Words) -> Result<Condition, String> {
    let Some(word) = w.next() else {
        return Err("a condition is missing".into());
    };
    let width = match word.to_ascii_lowercase().as_str() {
        "(" => {
            let inner = or(w)?;
            w.expect(")")?;
            return Ok(inner);
        }
        "time" => return Ok(Condition::Time(w.compare()?, w.duration(S)?)),
        "elapsed" => return Ok(Condition::Elapsed(w.compare()?, w.duration(S)?)),
        "frames" => return Ok(Condition::Frames(w.compare()?, w.integer()?)),
        "u8" => Width::U8,
        "u16" => Width::U16,
        "u32" => Width::U32,
        "u64" => Width::U64,
        _ => {
            return Err(format!(
                "expected a condition (time, elapsed, frames, or u8/u16/u32/u64 <rva>), found \
                 `{word}`"
            ));
        }
    };
    let rva = w.integer()?;
    let memory = Memory { rva, width };
    if rva % width.bytes() != 0 {
        return Err(format!(
            "{memory} is not aligned to {} bytes (read the byte with u8 instead)",
            width.bytes()
        ));
    }
    let test = if w.eat("changed") {
        Test::Changed
    } else if w.eat("nonzero") {
        Test::NonZero
    } else {
        let compare = w.compare()?;
        let value = w.integer()?;
        if value > width.max() {
            return Err(format!("{value:#x} does not fit in {}", width.name()));
        }
        Test::Compare(compare, value)
    };
    Ok(Condition::Memory(memory, test))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pad buttons as `gameinput::button` names them.
    fn button(name: &str) -> Option<u32> {
        crate::modules::gameinput::button(name)
    }

    fn parse(text: &str) -> Result<Script, String> {
        super::parse(text, button)
    }

    fn one(text: &str) -> Action {
        let script = parse(&format!("build 0123456789abcdef\n{text}")).unwrap();
        assert_eq!(script.steps.len(), 1);
        script.steps[0].action.clone()
    }

    fn error(text: &str) -> String {
        parse(text).unwrap_err()
    }

    fn u8_at(rva: u64) -> Memory {
        Memory {
            rva,
            width: Width::U8,
        }
    }

    fn u32_at(rva: u64) -> Memory {
        Memory {
            rva,
            width: Width::U32,
        }
    }

    #[test]
    fn autopilot_script_reads_every_step() {
        let a = button("a").unwrap();
        let text = "\
# From the golden save.
BUILD B99F876B74D4A77F03AF5F878223B27BD647B5C35A79D5DECD9F716504239E93

wait u8 0x74e007c == 1 timeout 60s   # init done
repeat a every 2500 until u8 0x74e007f != 0 timeout 2 min
";
        // `2 min` is no unit.
        assert!(error(text).starts_with("line 5:"), "{}", error(text));
        let script = parse(&text.replace("2 min", "120")).unwrap();
        assert_eq!(
            script.build.as_deref(),
            Some("b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93")
        );
        assert_eq!(
            script.steps,
            [
                Step {
                    line: 4,
                    text: "wait u8 0x74e007c == 1 timeout 60s".into(),
                    action: Action::Wait {
                        until: Condition::Memory(u8_at(0x74e007c), Test::Compare(Compare::Eq, 1)),
                        timeout: Some(Duration::from_secs(60)),
                    },
                },
                Step {
                    line: 5,
                    text: "repeat a every 2500 until u8 0x74e007f != 0 timeout 120".into(),
                    action: Action::Repeat {
                        buttons: Buttons {
                            names: "a".into(),
                            bits: a,
                        },
                        hold: HOLD,
                        every: Duration::from_millis(2500),
                        until: Condition::Memory(u8_at(0x74e007f), Test::Compare(Compare::Ne, 0)),
                        timeout: Some(Duration::from_secs(120)),
                    },
                },
            ]
        );

        let both = button("a").unwrap() | button("menu").unwrap();
        let press = |hold| Action::Press {
            buttons: Buttons {
                names: "a+menu".into(),
                bits: both,
            },
            hold,
        };
        assert_eq!(one("press a+menu"), press(HOLD));
        assert_eq!(one("press a+menu 50"), press(Duration::from_millis(50)));
        assert_eq!(
            one("Press A+Menu hold 1.5 s"),
            Action::Press {
                buttons: Buttons {
                    names: "A+Menu".into(),
                    bits: both,
                },
                hold: Duration::from_millis(1500),
            }
        );
        assert_eq!(one("release"), Action::Release);
        assert_eq!(
            one("exit after 30"),
            Action::ExitAfter(Duration::from_secs(30))
        );
        assert_eq!(
            one("exit after 250ms"),
            Action::ExitAfter(Duration::from_millis(250))
        );
        // A wait without a timeout waits as long as it takes.
        assert_eq!(
            one("wait frames >= 100"),
            Action::Wait {
                until: Condition::Frames(Compare::Ge, 100),
                timeout: None,
            }
        );
    }

    #[test]
    fn autopilot_conditions_nest_with_not_before_and_before_or() {
        let Action::Wait { until, .. } = one(
            "wait u32 0xab43664==0 and not (u32 0x9e58138 changed or u8 0x74dfa9d nonzero) or \
             time > 2.5 and elapsed <= 500ms",
        ) else {
            unreachable!()
        };
        assert_eq!(
            until,
            Condition::Or(vec![
                Condition::And(vec![
                    Condition::Memory(u32_at(0xab43664), Test::Compare(Compare::Eq, 0)),
                    Condition::Not(Box::new(Condition::Or(vec![
                        Condition::Memory(u32_at(0x9e58138), Test::Changed),
                        Condition::Memory(u8_at(0x74dfa9d), Test::NonZero),
                    ]))),
                ]),
                Condition::And(vec![
                    Condition::Time(Compare::Gt, Duration::from_millis(2500)),
                    Condition::Elapsed(Compare::Le, Duration::from_millis(500)),
                ]),
            ])
        );
        assert_eq!(
            until.memory(),
            [u8_at(0x74dfa9d), u32_at(0x9e58138), u32_at(0xab43664)]
        );
        assert_eq!(until.changed(), [u32_at(0x9e58138)]);
    }

    #[test]
    fn autopilot_scripts_say_what_is_wrong_and_where() {
        let cases = [
            ("wait u8 0x10 == 1", "line 1: reads title memory (u8 0x10)"),
            ("build 12ab\n", "line 1: `build` takes"),
            (
                "build 0123456789abcdef\nbuild 0123456789abcdef",
                "line 2: a second",
            ),
            (
                "wait time >= 1\nrelease\npress a",
                "line 3: presses after `release` (line 2)",
            ),
            ("jump", "line 1: unknown step `jump`"),
            ("press start", "line 1: unknown pad button `start`"),
            ("press a 0", "line 1: a press must hold"),
            (
                "repeat a hold 300 every 300 until time > 1",
                "line 1: every (300 ms)",
            ),
            (
                "repeat a every 300 time > 1",
                "line 1: expected `until`, found `time`",
            ),
            ("wait frames >= 1 timeout", "line 1: a duration is missing"),
            (
                "wait frames >= 1 timeout 5 extra",
                "line 1: unexpected `extra`",
            ),
            ("wait frames = 1", "line 1: expected ==, !="),
            ("wait (frames > 1", "line 1: expected `)` at the end"),
            ("wait", "line 1: a condition is missing"),
            ("wait loaded", "line 1: expected a condition"),
            (
                "build 0123456789abcdef\nwait u32 0x2 == 0",
                "line 2: u32 0x2 is not aligned",
            ),
            (
                "build 0123456789abcdef\nwait u8 0x2 == 0x100",
                "line 2: 0x100 does not fit",
            ),
            (
                "build 0123456789abcdef\nwait u16 0xg == 0",
                "line 2: `0xg` is no number",
            ),
        ];
        for (text, start) in cases {
            let e = error(text);
            assert!(e.starts_with(start), "{text:?}: {e}");
        }
        // The line as written is in the message, without its comment.
        assert_eq!(
            error("\n  jump high  # no"),
            "line 2: unknown step `jump` (steps: build, wait, press, repeat, release, exit \
             after) (`jump high`)"
        );
    }

    #[test]
    fn autopilot_script_lists_what_it_reads_with_the_first_line_reading_it() {
        let script = parse(
            "build 0123456789abcdef\nwait u8 0x10 == 1\nwait u8 0x10 == 2 or u32 0x20 changed\n\
             wait time > 1",
        )
        .unwrap();
        assert_eq!(script.memory(), [(2, u8_at(0x10)), (3, u32_at(0x20))]);
        // Without title reads, no build is needed: the script runs on any.
        assert_eq!(parse("wait time > 1\npress a").unwrap().build, None);
    }

    #[test]
    fn autopilot_shipped_scripts_parse() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../autopilot");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "txt") {
                let text = std::fs::read_to_string(&path).unwrap();
                let script = parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert!(script.build.is_some(), "{}", path.display());
                seen += 1;
            }
        }
        assert!(seen > 0, "no script in {}", dir.display());
    }
}
