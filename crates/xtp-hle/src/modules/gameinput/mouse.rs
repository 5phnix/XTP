//! The mouse as the right stick. A stick sets how fast the camera turns, a
//! mouse how far: the mouse's mean speed over the last `WINDOW` becomes the
//! stick's deflection (`Look`, then `Curve`), so the camera follows the
//! hand. Wheel notches become presses one at a time (`Wheel`). The mouse
//! plays only while the cursor is held in the title's window (`Capture`).
//! All of it is plain arithmetic over what the host reports; times are
//! microseconds on the input clock.

use std::collections::VecDeque;

/// The span the mouse's speed is averaged over: several polls, so bursts
/// of counts read as a steady speed, and under two frames at 30 fps.
const WINDOW: u64 = 48_000;
/// Slower than this (counts a second), the stick rests: fewer than two
/// counts in the window are a hand resting on the mouse.
const REST: f32 = 1.5e6 / WINDOW as f32;
/// Speed (counts a second) at full deflection with sensitivity 1: about
/// three inches a second with a 1600 dpi mouse.
const FULL_SPEED: f32 = 5000.0;

/// The mouse's speed, averaged over the last `WINDOW`.
#[derive(Debug, Default)]
pub(super) struct Look {
    /// The counts each poll brought, by poll time, oldest first.
    polls: VecDeque<(u64, i32, i32)>,
    /// Where the oldest poll's counts began; `None` after a rest.
    since: Option<u64>,
}

impl Look {
    /// Adds the counts since the last call (x right, y down, as the mouse
    /// reports them); the mean speed, counts a second. The mouse was still
    /// before the first call.
    pub(super) fn speed(&mut self, now: u64, dx: i32, dy: i32) -> (f32, f32) {
        let since = self.since.get_or_insert(now.saturating_sub(WINDOW));
        self.polls.push_back((now, dx, dy));
        while let Some(&(end, ..)) = self.polls.front()
            && end + WINDOW <= now
        {
            *since = end;
            self.polls.pop_front();
        }
        let span = now.saturating_sub(*since).max(1) as f32 / 1e6;
        let (x, y) = self.polls.iter().fold((0_i64, 0_i64), |(x, y), p| {
            (x + i64::from(p.1), y + i64::from(p.2))
        });
        (x as f32 / span, y as f32 / span)
    }
}

/// Mouse speed to stick deflection; `[input]` sets it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Curve {
    /// Scales the speed: 2 reaches full deflection at half the speed.
    pub sensitivity: f32,
    /// Shape: 1 is linear, above 1 finer at low speeds, below 1 coarser
    /// (to undo a title's own curve).
    pub exponent: f32,
    /// The deflection the slowest movement starts at: titles ignore a stick
    /// barely off centre, so slow aiming has to jump that dead zone.
    pub dead_zone: f32,
    /// Moving the mouse up pulls the stick down.
    pub invert_y: bool,
}

impl Default for Curve {
    fn default() -> Curve {
        Curve {
            sensitivity: 1.0,
            exponent: 1.0,
            dead_zone: 0.2,
            invert_y: false,
        }
    }
}

impl Curve {
    /// The right stick (up positive) for a mouse speed in counts a second
    /// (y grows downwards); the stick points where the mouse goes.
    pub(super) fn stick(&self, (vx, vy): (f32, f32)) -> (f32, f32) {
        let speed = vx.hypot(vy);
        if speed.is_nan() || speed < REST {
            return (0.0, 0.0);
        }
        let amount = (speed * self.sensitivity / FULL_SPEED)
            .min(1.0)
            .powf(self.exponent);
        let deflection = self.dead_zone + (1.0 - self.dead_zone) * amount;
        let up = if self.invert_y { vy } else { -vy };
        (vx / speed * deflection, up / speed * deflection)
    }
}

/// How long a notch's press lasts, and the release after it: a title
/// reading the pad once a frame at 30 fps sees both.
const PRESS: u64 = 50_000;
/// Notches waiting at most: a fast spin does not press for seconds.
const QUEUE: i32 = 5;
/// Wheel movement in a notch, as Windows counts it (`WHEEL_DELTA`).
pub(super) const NOTCH: i32 = 120;

/// Wheel notches as presses of `WheelUp` / `WheelDown`, one after the other.
#[derive(Debug, Default)]
pub(super) struct Wheel {
    /// Movement short of a whole notch (fine-grained wheels).
    partial: i32,
    /// Whole notches waiting, up positive.
    queued: i32,
    /// The notch being pressed: up or down, and since when.
    pressing: Option<(bool, u64)>,
}

impl Wheel {
    /// Adds wheel movement (`NOTCH` a notch, up positive).
    pub(super) fn turn(&mut self, movement: i32) {
        self.partial += movement;
        let notches = self.partial / NOTCH;
        self.partial -= notches * NOTCH;
        self.queued = (self.queued + notches).clamp(-QUEUE, QUEUE);
    }

    /// Whether up, or down, is pressed at `now`.
    pub(super) fn held(&mut self, now: u64) -> (bool, bool) {
        if let Some((up, since)) = self.pressing {
            match now.saturating_sub(since) {
                t if t < PRESS => return (up, !up),
                t if t < 2 * PRESS => return (false, false),
                _ => self.pressing = None,
            }
        }
        if self.queued == 0 {
            return (false, false);
        }
        let up = self.queued > 0;
        self.queued -= self.queued.signum();
        self.pressing = Some((up, now));
        (up, !up)
    }
}

/// Something that moves the cursor into or out of the game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    /// The title's window became active (true) or lost it: Alt+Tab, another
    /// window, a dialog of ours, minimising.
    Active(bool),
    /// The user drags or sizes the window (true) or stopped.
    Moving(bool),
    /// Esc: the title opens a menu, where the cursor is the player's.
    Escape,
    /// A mouse button went down in the window.
    Click,
}

/// Whether the cursor is held in the title's window, hidden, and the mouse
/// plays: while the window is active, unless Esc let the cursor go or the
/// window is being moved. A click in the window takes the mouse back.
#[derive(Debug)]
pub(super) struct Capture {
    active: bool,
    /// Esc let the cursor go; a click ends that.
    let_go: bool,
    moving: bool,
    /// Buttons ignored until released: those down when the capture began
    /// (the click that took the mouse back, or activated the window, is
    /// not a shot).
    ignored: u8,
    /// The capture began since buttons were last read.
    fresh: bool,
}

impl Capture {
    pub(super) const fn new() -> Capture {
        Capture {
            active: false,
            let_go: false,
            moving: false,
            ignored: 0,
            fresh: false,
        }
    }

    pub(super) fn held(&self) -> bool {
        self.active && !self.let_go && !self.moving
    }

    /// Applies `event`; true when that holds or frees the cursor.
    pub(super) fn on(&mut self, event: Event) -> bool {
        let was = self.held();
        match event {
            Event::Active(active) => self.active = active,
            Event::Moving(moving) => self.moving = moving,
            Event::Escape => self.let_go = true,
            Event::Click if self.active && !self.moving => self.let_go = false,
            Event::Click => {}
        }
        let held = self.held();
        if held && !was {
            self.fresh = true;
        }
        if !held {
            self.fresh = false;
            self.ignored = 0;
        }
        held != was
    }

    /// The buttons that play, of those `down` now: none while the cursor is
    /// free.
    pub(super) fn buttons(&mut self, down: u8) -> u8 {
        if !self.held() {
            return 0;
        }
        if std::mem::take(&mut self.fresh) {
            self.ignored = down;
        }
        self.ignored &= down;
        down & !self.ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Polls every 8 ms from 1 s on, `dx`, `dy` counts each; the last speed.
    fn steady(look: &mut Look, polls: u64, dx: i32, dy: i32) -> (f32, f32) {
        (1..=polls).fold((0.0, 0.0), |_, i| look.speed(1_000_000 + i * 8000, dx, dy))
    }

    #[test]
    fn steady_counts_give_a_steady_speed_and_stillness_none() {
        let mut look = Look::default();
        // 16 counts right and 8 up every 8 ms: 2000 and -1000 a second,
        // exactly, once the window is full.
        let mut last = (0.0, 0.0);
        for i in 1..=40 {
            let speed = look.speed(1_000_000 + i * 8000, 16, -8);
            if i > 8 {
                assert!((speed.0 - 2000.0).abs() < 1e-2, "{i}: {speed:?}");
                assert!((speed.1 + 1000.0).abs() < 1e-2, "{i}: {speed:?}");
            }
            last = speed;
        }
        let curve = Curve::default();
        let (x, y) = curve.stick(last);
        assert!(x > 0.0 && y > 0.0, "right and up: {x}, {y}");
        // Uneven polls carrying counts in proportion read the same speed.
        let mut look = Look::default();
        let mut t = 1_000_000;
        for i in 0..40 {
            let gap = if i % 2 == 0 { 4000 } else { 12_000 };
            t += gap;
            let speed = look.speed(t, (gap / 500) as i32, 0);
            if i > 10 {
                assert!((speed.0 - 2000.0).abs() < 1e-2, "{i}: {speed:?}");
            }
        }
        // No counts: back to rest within the window and a poll, exactly.
        let mut look = Look::default();
        steady(&mut look, 20, 16, -8);
        let start = 1_000_000 + 20 * 8000;
        let rests: Vec<_> = (1..=8)
            .map(|i| curve.stick(look.speed(start + i * 8000, 0, 0)))
            .collect();
        assert!(rests[..5].iter().all(|&s| s != (0.0, 0.0)), "{rests:?}");
        assert!(rests[5..].iter().all(|&s| s == (0.0, 0.0)), "{rests:?}");
    }

    #[test]
    fn speed_maps_to_deflection_through_the_curve() {
        let curve = Curve::default();
        // At rest, and under two counts in the window.
        assert_eq!(curve.stick((0.0, 0.0)), (0.0, 0.0));
        assert_eq!(curve.stick((20.0, 0.0)), (0.0, 0.0));
        assert_eq!(curve.stick((f32::NAN, 0.0)), (0.0, 0.0));
        // Slow movement starts past the dead zone; the mouse going down
        // pulls the stick down.
        let (x, y) = curve.stick((0.0, 50.0));
        assert!(x == 0.0 && y < -0.2 && y > -0.21, "{y}");
        // Full deflection, no further.
        let (x, _) = curve.stick((FULL_SPEED, 0.0));
        assert!((x - 1.0).abs() < 1e-6);
        assert_eq!(curve.stick((-3.0 * FULL_SPEED, 0.0)), (-1.0, 0.0));
        // The direction is kept.
        let (x, y) = curve.stick((1000.0, -1000.0));
        assert!((x - y).abs() < 1e-6);

        let at = |c: Curve| c.stick((FULL_SPEED / 4.0, 0.0)).0;
        let linear = at(curve);
        assert!((linear - (0.2 + 0.8 * 0.25)).abs() < 1e-6);
        let faster = Curve {
            sensitivity: 2.0,
            ..curve
        };
        let finer = Curve {
            exponent: 2.0,
            ..curve
        };
        let no_dead_zone = Curve {
            dead_zone: 0.0,
            ..curve
        };
        assert!((at(faster) - (0.2 + 0.8 * 0.5)).abs() < 1e-6);
        assert!((at(finer) - (0.2 + 0.8 * 0.0625)).abs() < 1e-6);
        assert!((at(no_dead_zone) - 0.25).abs() < 1e-6);
        let inverted = Curve {
            invert_y: true,
            ..curve
        };
        assert_eq!(
            inverted.stick((0.0, 1000.0)).1,
            -curve.stick((0.0, 1000.0)).1
        );
    }

    #[test]
    fn three_notches_are_three_presses() {
        let mut wheel = Wheel::default();
        wheel.turn(3 * NOTCH);
        let mut presses = Vec::new();
        let mut was = (false, false);
        for i in 0..100 {
            let held = wheel.held(i * 8000);
            if held != was && held != (false, false) {
                presses.push(held);
            }
            was = held;
        }
        assert_eq!(presses, [(true, false); 3]);

        // A fine wheel's steps add up to notches; down is the other button.
        for _ in 0..4 {
            wheel.turn(-NOTCH / 2);
        }
        let held: Vec<_> = (0..30).map(|i| wheel.held(1_000_000 + i * 8000)).collect();
        let down = held.iter().filter(|&&h| h == (false, true)).count();
        // Two presses of 50 ms, read every 8 ms, with the release between.
        assert_eq!(down, 14, "{held:?}");
        assert_eq!(held[7], (false, false));

        // A spin queues a few presses, not dozens.
        wheel.turn(40 * NOTCH);
        let presses = std::iter::once((false, false))
            .chain((0..200).map(|i| wheel.held(2_000_000 + i * 8000)))
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[1] == (true, false) && w[0] != w[1])
            .count();
        assert_eq!(presses, QUEUE as usize);
    }

    #[test]
    fn the_cursor_is_held_while_playing_and_freed_otherwise() {
        let mut capture = Capture::new();
        assert!(!capture.held());
        // The window comes up: the mouse plays.
        assert!(capture.on(Event::Active(true)));
        assert!(capture.held());
        assert_eq!(capture.buttons(0), 0);
        assert_eq!(capture.buttons(0x1), 0x1);
        // Alt+Tab frees it, coming back holds it again.
        assert!(capture.on(Event::Active(false)));
        assert_eq!(capture.buttons(0x1), 0);
        assert!(capture.on(Event::Active(true)));
        // Esc in a menu frees it, and so it stays across Alt+Tab ...
        assert!(capture.on(Event::Escape));
        assert!(!capture.held());
        capture.on(Event::Active(false));
        capture.on(Event::Active(true));
        assert!(!capture.held());
        // ... until a click in the window, which is not a shot: that button
        // is ignored until released, others play.
        assert!(capture.on(Event::Click));
        assert!(capture.held());
        assert_eq!(capture.buttons(0x1), 0);
        assert_eq!(capture.buttons(0x3), 0x2);
        assert_eq!(capture.buttons(0x0), 0);
        assert_eq!(capture.buttons(0x1), 0x1);
        // Moving the window frees the cursor for the drag.
        assert!(capture.on(Event::Moving(true)));
        assert!(!capture.on(Event::Click));
        assert!(!capture.held());
        assert!(capture.on(Event::Moving(false)));
        // A click on an inactive window does nothing by itself.
        capture.on(Event::Active(false));
        capture.on(Event::Escape);
        assert!(!capture.on(Event::Click));
        assert!(!capture.held());
    }
}
