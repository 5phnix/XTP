//! The trigger motors. XInput drives the two motors in the grips only; the
//! triggers' own motors (Xbox One and Series pads) answer to
//! Windows.Gaming.Input (`Gamepad.Vibration`), which knows its pads apart
//! from XInput's slots. A slot is taken for one of its pads when each side
//! has a single pad left, or when one pad alone holds the buttons the slot
//! holds; that slot's rumble then goes there, all four motors. A slot not
//! matched keeps XInput's two motors, and its trigger levels are dropped;
//! so are they on a matched pad without trigger motors.

use std::time::{Duration, Instant};

use windows::Gaming::Input::{Gamepad, GamepadVibration};

use super::PADS;

/// Windows.Gaming.Input's pads are listed again this often (they come and
/// go, and the first list may be empty while it looks).
const RELIST: Duration = Duration::from_secs(1);
/// The buttons both APIs have, with the same bits (no paddles).
const BUTTONS: u32 = 0x3FFF;

#[derive(Default)]
pub(super) struct Triggers {
    /// Windows.Gaming.Input's pads, as last listed.
    pads: Vec<Gamepad>,
    listed: Option<Instant>,
    /// The pad behind each host slot, once known.
    slots: [Option<Gamepad>; PADS],
    /// The slots that had a pad when last read.
    present: [bool; PADS],
    /// Windows.Gaming.Input does not answer: XInput alone.
    broken: bool,
}

impl Triggers {
    /// Host slot `slot` was read: its buttons, `None` when it is empty.
    pub(super) fn saw(&mut self, slot: usize, buttons: Option<u32>) {
        self.present[slot] = buttons.is_some();
        let Some(buttons) = buttons else {
            self.slots[slot] = None;
            return;
        };
        if self.broken {
            return;
        }
        self.list();
        if let Some(pad) = &self.slots[slot] {
            if self.pads.contains(pad) {
                return;
            }
            self.slots[slot] = None;
        }
        let free: Vec<&Gamepad> = self
            .pads
            .iter()
            .filter(|pad| !self.slots.iter().flatten().any(|taken| taken == *pad))
            .collect();
        if free.is_empty() {
            return;
        }
        let unmatched = (0..PADS)
            .filter(|&s| self.present[s] && self.slots[s].is_none())
            .count();
        let held: Vec<u32> = free
            .iter()
            .map(|pad| pad.GetCurrentReading().map_or(0, |r| r.Buttons.0 & BUTTONS))
            .collect();
        if let Some(at) = pick(buttons & BUTTONS, unmatched, &held) {
            tracing::info!(
                slot,
                "pad rumble through Windows.Gaming.Input, trigger motors too"
            );
            self.slots[slot] = Some(free[at].clone());
        }
    }

    /// Motor levels (grips low and high, triggers left and right, 0 to 1)
    /// to `slot`'s pad; false when it has none here.
    pub(super) fn rumble(&mut self, slot: usize, motors: [f32; 4]) -> bool {
        let Some(pad) = &self.slots[slot] else {
            return false;
        };
        let [low, high, left, right] = motors.map(f64::from);
        pad.SetVibration(GamepadVibration {
            LeftMotor: low,
            RightMotor: high,
            LeftTrigger: left,
            RightTrigger: right,
        })
        .is_ok()
    }

    fn list(&mut self) {
        if self.listed.is_some_and(|at| at.elapsed() < RELIST) {
            return;
        }
        self.listed = Some(Instant::now());
        match Gamepad::Gamepads() {
            Ok(pads) => self.pads = pads.into_iter().collect(),
            Err(e) => {
                tracing::warn!(%e, "no Windows.Gaming.Input: no trigger rumble");
                self.broken = true;
                self.pads.clear();
                self.slots = Default::default();
            }
        }
    }
}

/// Which of the free pads (the buttons each holds) is a slot holding
/// `buttons`, `unmatched` slots (this one too) having no pad yet.
fn pick(buttons: u32, unmatched: usize, free: &[u32]) -> Option<usize> {
    if unmatched == 1 && free.len() == 1 {
        return Some(0);
    }
    if buttons == 0 {
        return None;
    }
    let mut same = free
        .iter()
        .enumerate()
        .filter(|&(_, &held)| held == buttons);
    match (same.next(), same.next()) {
        (Some((at, _)), None) => Some(at),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_matched_to_pads_by_what_they_hold() {
        // One of each: the same pad, held or not.
        assert_eq!(pick(0, 1, &[0]), Some(0));
        assert_eq!(pick(0x4, 1, &[0]), Some(0));
        // Several: only the pad holding the same buttons, alone.
        assert_eq!(pick(0, 2, &[0, 0]), None);
        assert_eq!(pick(0x4, 2, &[0, 0x4]), Some(1));
        assert_eq!(pick(0x4, 2, &[0x4, 0x4]), None);
        assert_eq!(pick(0x4, 2, &[0x8, 0x10]), None);
        // Two slots, one pad left: not known yet which slot it is.
        assert_eq!(pick(0, 2, &[0]), None);
        assert_eq!(pick(0x20, 2, &[0x20]), Some(0));
        assert_eq!(pick(0x4, 1, &[]), None);
    }
}
