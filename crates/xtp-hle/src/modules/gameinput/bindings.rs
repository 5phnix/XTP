//! Which key or mouse input drives which control of the keyboard's pad.
//! Keys are positions (scan codes), not characters: the key in W's place
//! moves the left stick on every layout, also where it reads Z (AZERTY).
//! Names, in `[input.bindings]` and `XTP_KEYS`, are the US layout's labels
//! for those places.

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

/// A key place or a mouse input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Input {
    /// A key by its scan code (0xE0xx when extended), as `KeyState` has it.
    Key(u32),
    /// Mouse buttons (`MOUSE_*` bits).
    Mouse(u8),
    /// One notch of the wheel, a press each (`mouse::Wheel`).
    WheelUp,
    WheelDown,
}

/// Mouse buttons, as `MouseInput` reports them.
pub(super) const MOUSE_LEFT: u8 = 0x1;
pub(super) const MOUSE_RIGHT: u8 = 0x2;
pub(super) const MOUSE_MIDDLE: u8 = 0x4;
pub(super) const MOUSE_BACK: u8 = 0x8;
pub(super) const MOUSE_FORWARD: u8 = 0x10;

/// What an input does on the pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Button(u32),
    LeftTrigger,
    RightTrigger,
    /// A stick pushed one way: which stick, then x and y (up positive).
    Stick(Side, i8, i8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

use Control::{Button, LeftTrigger, RightTrigger, Stick};

/// The pad's controls by their `[input.bindings]` names, each with its
/// inputs by default (README lists them).
const CONTROLS: [(&str, Control, &[&str]); 24] = [
    ("left_stick_up", Stick(Side::Left, 0, 1), &["W"]),
    ("left_stick_down", Stick(Side::Left, 0, -1), &["S"]),
    ("left_stick_left", Stick(Side::Left, -1, 0), &["A"]),
    ("left_stick_right", Stick(Side::Left, 1, 0), &["D"]),
    // The mouse moves the right stick too (`mouse::Curve`).
    ("right_stick_up", Stick(Side::Right, 0, 1), &["I"]),
    ("right_stick_down", Stick(Side::Right, 0, -1), &["K"]),
    ("right_stick_left", Stick(Side::Right, -1, 0), &["J"]),
    ("right_stick_right", Stick(Side::Right, 1, 0), &["L"]),
    ("dpad_up", Button(UP), &["Up"]),
    ("dpad_down", Button(DOWN), &["Down"]),
    ("dpad_left", Button(LEFT), &["Left"]),
    ("dpad_right", Button(RIGHT), &["Right"]),
    ("a", Button(A), &["Enter", "Space"]),
    ("b", Button(B), &["Backspace", "R"]),
    ("x", Button(X), &["X"]),
    ("y", Button(Y), &["Y"]),
    ("left_shoulder", Button(LEFT_SHOULDER), &["Q", "WheelDown"]),
    ("right_shoulder", Button(RIGHT_SHOULDER), &["E", "WheelUp"]),
    ("left_trigger", LeftTrigger, &["Z", "MouseRight"]),
    ("right_trigger", RightTrigger, &["C", "MouseLeft"]),
    ("left_stick_click", Button(LEFT_STICK), &["F"]),
    (
        "right_stick_click",
        Button(RIGHT_STICK),
        &["H", "MouseMiddle"],
    ),
    ("menu", Button(MENU), &["Esc"]),
    ("view", Button(VIEW), &["Tab"]),
];

/// Key rows of the main block: the scan code of each row's first key.
const ROWS: [(u32, &str); 4] = [
    (0x02, "1234567890"),
    (0x10, "QWERTYUIOP"),
    (0x1E, "ASDFGHJKL"),
    (0x2C, "ZXCVBNM"),
];

/// The other keys by name.
const NAMED: [(&str, u32); 37] = [
    ("Esc", 0x01),
    ("Minus", 0x0C),
    ("Equals", 0x0D),
    ("Backspace", 0x0E),
    ("Back", 0x0E),
    ("Tab", 0x0F),
    ("LeftBracket", 0x1A),
    ("RightBracket", 0x1B),
    ("Enter", 0x1C),
    ("LeftCtrl", 0x1D),
    ("Semicolon", 0x27),
    ("Apostrophe", 0x28),
    ("Grave", 0x29),
    ("LeftShift", 0x2A),
    ("Backslash", 0x2B),
    ("Comma", 0x33),
    ("Period", 0x34),
    ("Slash", 0x35),
    ("RightShift", 0x36),
    ("LeftAlt", 0x38),
    ("Space", 0x39),
    ("CapsLock", 0x3A),
    ("F11", 0x57),
    ("F12", 0x58),
    ("RightCtrl", 0xE01D),
    ("RightAlt", 0xE038),
    ("Home", 0xE047),
    ("Up", 0xE048),
    ("PageUp", 0xE049),
    ("Left", 0xE04B),
    ("Right", 0xE04D),
    ("End", 0xE04F),
    ("Down", 0xE050),
    ("PageDown", 0xE051),
    ("Insert", 0xE052),
    ("Delete", 0xE053),
    // The key left of Z on ISO keyboards (< > on AZERTY).
    ("Iso", 0x56),
];

const MICE: [(&str, Input); 7] = [
    ("MouseLeft", Input::Mouse(MOUSE_LEFT)),
    ("MouseRight", Input::Mouse(MOUSE_RIGHT)),
    ("MouseMiddle", Input::Mouse(MOUSE_MIDDLE)),
    ("MouseBack", Input::Mouse(MOUSE_BACK)),
    ("MouseForward", Input::Mouse(MOUSE_FORWARD)),
    ("WheelUp", Input::WheelUp),
    ("WheelDown", Input::WheelDown),
];

/// The scan code of the key in the place `name` labels on a US keyboard
/// (any case): a letter or digit, `F1` to `F12`, or a name in `NAMED`.
pub(super) fn scan_code(name: &str) -> Option<u32> {
    if let [c] = name.as_bytes() {
        let c = c.to_ascii_uppercase();
        return ROWS.iter().find_map(|&(first, row)| {
            let at = row.bytes().position(|k| k == c)?;
            Some(first + at as u32)
        });
    }
    if let Some(n) = name
        .strip_prefix(|c: char| c.eq_ignore_ascii_case(&'f'))
        .and_then(|n| n.parse::<u32>().ok())
        && (1..=10).contains(&n)
    {
        return Some(0x3A + n);
    }
    NAMED
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|&(_, code)| code)
}

/// The bit of the pad button called `name` in `CONTROLS` (any case).
pub(super) fn button(name: &str) -> Option<u32> {
    CONTROLS
        .iter()
        .find_map(|&(known, control, _)| match control {
            Button(bit) if known.eq_ignore_ascii_case(name) => Some(bit),
            _ => None,
        })
}

/// A key or mouse input by name.
fn input(name: &str) -> Option<Input> {
    scan_code(name).map(Input::Key).or_else(|| {
        MICE.iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|&(_, input)| input)
    })
}

/// Every control of the keyboard's pad with the inputs that drive it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Bindings(Vec<(Control, Vec<Input>)>);

impl Default for Bindings {
    fn default() -> Bindings {
        let inputs = |names: &[&str]| names.iter().map(|n| input(n).unwrap()).collect();
        Bindings(
            CONTROLS
                .iter()
                .map(|&(_, control, names)| (control, inputs(names)))
                .collect(),
        )
    }
}

impl Bindings {
    /// Gives `control` (a name from `CONTROLS`) these inputs instead of its
    /// own; none leaves it unbound.
    pub(super) fn set(&mut self, control: &str, inputs: &[String]) -> Result<(), String> {
        let Some(at) = CONTROLS
            .iter()
            .position(|(name, ..)| name.eq_ignore_ascii_case(control))
        else {
            let known: Vec<_> = CONTROLS.iter().map(|(name, ..)| *name).collect();
            return Err(format!(
                "unknown pad control `{control}` (known: {})",
                known.join(", ")
            ));
        };
        self.0[at].1 = inputs
            .iter()
            .map(|name| {
                input(name).ok_or_else(|| {
                    format!(
                        "unknown key or mouse input `{name}` for `{control}` (keys by their US \
                         label, such as W, Space, LeftShift, Up, F1; mouse: MouseLeft, \
                         MouseRight, MouseMiddle, MouseBack, MouseForward, WheelUp, WheelDown)"
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(())
    }

    /// The pad while `held` says which inputs are down. Both directions of
    /// an axis cancel; a diagonal stays on the stick's circle.
    pub(super) fn pad(&self, held: impl Fn(Input) -> bool) -> PadState {
        let mut pad = PadState::default();
        let mut sticks = [(0.0_f32, 0.0_f32); 2];
        for (control, inputs) in &self.0 {
            if !inputs.iter().any(|&input| held(input)) {
                continue;
            }
            match *control {
                Button(bit) => pad.buttons |= bit,
                LeftTrigger => pad.left_trigger = 1.0,
                RightTrigger => pad.right_trigger = 1.0,
                Stick(side, x, y) => {
                    let stick = &mut sticks[side as usize];
                    stick.0 += f32::from(x);
                    stick.1 += f32::from(y);
                }
            }
        }
        let round = |(x, y): (f32, f32)| {
            let length = x.hypot(y);
            if length > 1.0 {
                (x / length, y / length)
            } else {
                (x, y)
            }
        };
        (pad.left_x, pad.left_y) = round(sticks[Side::Left as usize]);
        (pad.right_x, pad.right_y) = round(sticks[Side::Right as usize]);
        pad
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_us_key_places() {
        let codes: Vec<_> = [
            "W", "a", "Z", "m", "1", "0", "F1", "f10", "F11", "Esc", "up",
        ]
        .iter()
        .map(|n| scan_code(n))
        .collect();
        let want = [
            0x11, 0x1E, 0x2C, 0x32, 0x02, 0x0B, 0x3B, 0x44, 0x57, 0x01, 0xE048,
        ];
        assert_eq!(codes, want.map(Some));
        for unknown in ["", "F0", "F13", "Ü", "WW", "Mouse"] {
            assert_eq!(scan_code(unknown), None, "{unknown}");
        }
        assert_eq!(scan_code("Back"), scan_code("Backspace"));
        // Every control has inputs by default, and they all resolve.
        let defaults = Bindings::default();
        assert_eq!(defaults.0.len(), CONTROLS.len());
        assert!(defaults.0.iter().all(|(_, inputs)| !inputs.is_empty()));
    }

    #[test]
    fn bindings_replace_one_control() {
        let mut bindings = Bindings::default();
        bindings
            .set("A", &["MouseBack".into(), "leftshift".into()])
            .unwrap();
        bindings.set("menu", &[]).unwrap();
        let held = |down: Input| move |input: Input| input == down;
        assert_eq!(bindings.pad(held(Input::Key(0x2A))).buttons, A);
        assert_eq!(bindings.pad(held(Input::Mouse(MOUSE_BACK))).buttons, A);
        // Enter no longer presses A, Esc no longer opens the menu; the
        // others keep their keys.
        assert_eq!(bindings.pad(held(Input::Key(0x1C))).buttons, 0);
        assert_eq!(bindings.pad(held(Input::Key(0x01))).buttons, 0);
        assert_eq!(bindings.pad(held(Input::Key(0x0E))).buttons, B);

        let err = bindings.set("jump", &[]).unwrap_err();
        assert!(err.contains("unknown pad control `jump`"), "{err}");
        let err = bindings.set("b", &["Shift".into()]).unwrap_err();
        assert!(err.contains("`Shift`"), "{err}");
    }

    #[test]
    fn sticks_cancel_and_stay_round() {
        let bindings = Bindings::default();
        let keys = |names: &'static [&'static str]| {
            let codes: Vec<_> = names.iter().map(|n| scan_code(n).unwrap()).collect();
            bindings.pad(move |input| matches!(input, Input::Key(c) if codes.contains(&c)))
        };
        let pad = keys(&["W", "D"]);
        let half = std::f32::consts::FRAC_1_SQRT_2;
        assert!((pad.left_x - half).abs() < 1e-6 && (pad.left_y - half).abs() < 1e-6);
        assert_eq!(
            (keys(&["W", "S"]).left_x, keys(&["W", "S"]).left_y),
            (0.0, 0.0)
        );
        let pad = keys(&["J", "Z", "C", "Space"]);
        assert_eq!(
            pad,
            PadState {
                buttons: A,
                left_trigger: 1.0,
                right_trigger: 1.0,
                right_x: -1.0,
                ..Default::default()
            }
        );
    }
}
