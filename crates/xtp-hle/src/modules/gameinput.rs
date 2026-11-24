//! `gameinput.dll`: input devices from the host. Gamepads come from XInput
//! (host slots 0-3), with their trigger motors through
//! Windows.Gaming.Input (`triggers`). The keyboard and mouse play as a pad
//! too (`Kbm`): keys by position (`bindings`), the mouse as the right stick
//! (`mouse`, read in the title's window by `window`). They are a stand-in
//! pad of their own while no pad is connected, and play on top of the
//! first pad otherwise. The keyboard is a device of its own as well. A poll
//! thread turns host state into readings (a new one per change) and runs
//! device callbacks on connect and disconnect.
#![allow(non_snake_case)]
// Title code calls these through the objects, passing pointers the real API
// would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod bindings;
mod mouse;
#[cfg(windows)]
mod triggers;
#[cfg(windows)]
mod window;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::ops::{Deref, RangeInclusive};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

#[cfg(windows)]
pub(crate) use window::{
    created as window_created, message as window_message, release as release_mouse,
};

use self::bindings::{Bindings, Input};
use self::mouse::{Curve, Look, Wheel};
use super::xasync::{E_INVALIDARG, E_NOTIMPL};
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleExport, HleModule};

/// The documented errors.
pub const DEVICE_DISCONNECTED: HRESULT = 0x838A_0001_u32 as i32;
pub const DEVICE_NOT_FOUND: HRESULT = 0x838A_0002_u32 as i32;
pub const READING_NOT_FOUND: HRESULT = 0x838A_0003_u32 as i32;
pub const REFERENCE_TOO_OLD: HRESULT = 0x838A_0004_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;

/// Input kinds (bits) our devices have.
const KEYBOARD_KIND: u32 = 0x10;
const GAMEPAD_KIND: u32 = 0x4_0000;
/// Device status of a connected device: connected, input enabled.
const STATUS_CONNECTED: u32 = 0x3;
const ASYNC_ENUMERATION: u32 = 1;

/// Gamepad buttons; the title maps each bit to one of its pad buttons.
const MENU: u32 = 0x1;
const VIEW: u32 = 0x2;
const A: u32 = 0x4;
const B: u32 = 0x8;
const X: u32 = 0x10;
const Y: u32 = 0x20;
const UP: u32 = 0x40;
const DOWN: u32 = 0x80;
const LEFT: u32 = 0x100;
const RIGHT: u32 = 0x200;
const LEFT_SHOULDER: u32 = 0x400;
const RIGHT_SHOULDER: u32 = 0x800;
const LEFT_STICK: u32 = 0x1000;
const RIGHT_STICK: u32 = 0x2000;

/// Devices: host pad slots, the keyboard and mouse's stand-in pad, the
/// keyboard.
const PADS: usize = 4;
const FALLBACK: usize = PADS;
const KEYBOARD: usize = PADS + 1;
const DEVICES: usize = PADS + 2;

/// Readings kept per device for next/previous lookups.
const HISTORY: usize = 32;
const POLL: Duration = Duration::from_millis(8);

/// A gamepad reading: triggers 0 to 1, sticks -1 to 1 (up positive).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PadState {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}

/// One key held down: scan code (0xE0xx when extended), character, key.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct KeyState {
    scan_code: u32,
    code_point: u32,
    virtual_key: u8,
    dead_key: bool,
}

const _: () = assert!(size_of::<KeyState>() == 12 && size_of::<PadState>() == 28);

/// What the mouse did since the last poll.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MouseInput {
    /// The cursor is held in the title's window: the mouse plays.
    captured: bool,
    /// Buttons down (`bindings::MOUSE_*`); none while not captured.
    buttons: u8,
    /// Counts, x right and y down.
    dx: i32,
    dy: i32,
    /// Wheel movement, `mouse::NOTCH` a notch, up positive.
    wheel: i32,
}

/// Host input, or a fake in tests.
trait Source: Send {
    /// The pad in host slot `slot`, `None` when the slot is empty.
    fn pad(&mut self, slot: usize) -> Option<PadState>;
    /// Keys held down, scan codes by place; none while the title's window
    /// lacks focus.
    fn keys(&mut self) -> Vec<KeyState>;
    /// The mouse since the last call.
    fn mouse(&mut self) -> MouseInput;
    /// Motor levels, 0 to 1: low frequency (left grip), high frequency
    /// (right grip), left trigger, right trigger. A host pad without some
    /// of them drops those.
    fn rumble(&mut self, slot: usize, motors: [f32; 4]);
}

/// `[input]` in the config; anything unset keeps its default (README).
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// The keyboard and mouse play as a pad (default true).
    pub keyboard_mouse: Option<bool>,
    /// Mouse speed to right stick deflection: 2 turns twice as fast
    /// (default 1).
    pub mouse_sensitivity: Option<f32>,
    /// Exponent of that curve: 1 linear, above 1 finer slow aiming
    /// (default 1).
    pub mouse_curve: Option<f32>,
    /// The deflection the slowest movement starts at, past the title's dead
    /// zone (default 0.2).
    pub mouse_dead_zone: Option<f32>,
    /// Moving the mouse up looks down (default false).
    pub invert_y: Option<bool>,
    /// Pad controls given other inputs than their own: control name to key
    /// and mouse input names.
    pub bindings: BTreeMap<String, Vec<String>>,
}

/// The settings in force.
#[derive(Clone, Debug, PartialEq)]
struct Tuning {
    keyboard_mouse: bool,
    curve: Curve,
    bindings: Bindings,
}

impl Default for Tuning {
    fn default() -> Tuning {
        Tuning {
            keyboard_mouse: true,
            curve: Curve::default(),
            bindings: Bindings::default(),
        }
    }
}

impl Tuning {
    fn new(settings: &Settings) -> Result<Tuning, String> {
        let mut tuning = Tuning::default();
        let within =
            |name: &str, value: Option<f32>, range: RangeInclusive<f32>, default| match value {
                None => Ok(default),
                Some(v) if range.contains(&v) => Ok(v),
                Some(v) => Err(format!(
                    "{name} = {v}: must be from {} to {}",
                    range.start(),
                    range.end()
                )),
            };
        let curve = &mut tuning.curve;
        curve.sensitivity = within(
            "mouse_sensitivity",
            settings.mouse_sensitivity,
            0.01..=100.0,
            curve.sensitivity,
        )?;
        curve.exponent = within(
            "mouse_curve",
            settings.mouse_curve,
            0.1..=10.0,
            curve.exponent,
        )?;
        curve.dead_zone = within(
            "mouse_dead_zone",
            settings.mouse_dead_zone,
            0.0..=0.95,
            curve.dead_zone,
        )?;
        curve.invert_y = settings.invert_y.unwrap_or(curve.invert_y);
        tuning.keyboard_mouse = settings.keyboard_mouse.unwrap_or(tuning.keyboard_mouse);
        for (control, inputs) in &settings.bindings {
            tuning
                .bindings
                .set(control, inputs)
                .map_err(|e| format!("bindings: {e}"))?;
        }
        Ok(tuning)
    }
}

static TUNING: OnceLock<Tuning> = OnceLock::new();

fn tuning() -> &'static Tuning {
    TUNING.get_or_init(Tuning::default)
}

/// Takes `[input]` from the config; before the title reads any input.
pub fn configure(settings: &Settings) -> Result<(), String> {
    let tuning = Tuning::new(settings)?;
    tracing::info!(
        keyboard_mouse = tuning.keyboard_mouse,
        curve = ?tuning.curve,
        rebound = settings.bindings.len(),
        "input settings"
    );
    match TUNING.set(tuning) {
        Err(tuning) if self::tuning() != &tuning => {
            Err("input already started with other settings".to_string())
        }
        _ => Ok(()),
    }
}

/// The keyboard and the mouse as one pad.
struct Kbm {
    bindings: Bindings,
    curve: Curve,
    look: Look,
    wheel: Wheel,
}

impl Kbm {
    fn new(tuning: &Tuning) -> Kbm {
        Kbm {
            bindings: tuning.bindings.clone(),
            curve: tuning.curve,
            look: Look::default(),
            wheel: Wheel::default(),
        }
    }

    /// The pad at `now` for the keys down and what the mouse did.
    fn pad(&mut self, keys: &[KeyState], mouse: &MouseInput, now: u64) -> PadState {
        let mut look = (0.0, 0.0);
        let mut wheel = (false, false);
        if mouse.captured {
            look = self.curve.stick(self.look.speed(now, mouse.dx, mouse.dy));
            self.wheel.turn(mouse.wheel);
            wheel = self.wheel.held(now);
        } else {
            // Nothing carries over a release: no drift, no late notches.
            self.look = Look::default();
            self.wheel = Wheel::default();
        }
        let mut pad = self.bindings.pad(|input| match input {
            Input::Key(code) => keys.iter().any(|k| k.scan_code == code),
            Input::Mouse(button) => mouse.buttons & button != 0,
            Input::WheelUp => wheel.0,
            Input::WheelDown => wheel.1,
        });
        (pad.right_x, pad.right_y) = stronger((pad.right_x, pad.right_y), look);
        pad
    }
}

/// The stick pushed further.
fn stronger(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    if b.0.hypot(b.1) > a.0.hypot(a.1) {
        b
    } else {
        a
    }
}

/// The keyboard and mouse's pad on top of a host pad: the buttons of both,
/// each trigger and stick as far as either pushes it.
fn merge(pad: &mut PadState, kbm: &PadState) {
    pad.buttons |= kbm.buttons;
    pad.left_trigger = pad.left_trigger.max(kbm.left_trigger);
    pad.right_trigger = pad.right_trigger.max(kbm.right_trigger);
    (pad.left_x, pad.left_y) = stronger((pad.left_x, pad.left_y), (kbm.left_x, kbm.left_y));
    (pad.right_x, pad.right_y) = stronger((pad.right_x, pad.right_y), (kbm.right_x, kbm.right_y));
}

#[repr(C)]
struct Table<const N: usize>([*const (); N]);
// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Table<N> {}

/// The input object; one per hub.
#[repr(C)]
pub struct Object {
    vtable: &'static Table<22>,
    hub: *const Hub,
}

#[repr(C)]
pub struct Device {
    vtable: &'static Table<21>,
    hub: *const Hub,
    index: usize,
    info: DeviceInfo,
}

/// What a device says about itself. The title reads `id` (+0x20); the rest
/// follows the public layout and is zero where we have nothing to say.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // read by title code
struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// Counts and optional detail pointers.
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

fn kind(index: usize) -> u32 {
    if index == KEYBOARD {
        KEYBOARD_KIND
    } else {
        GAMEPAD_KIND
    }
}

impl Device {
    fn new(index: usize) -> Device {
        let mut id = [0; 32];
        id[..3].copy_from_slice(b"xtp");
        id[31] = index as u8 + 1;
        let info = DeviceInfo {
            size: size_of::<DeviceInfo>() as u32,
            id,
            root_id: id,
            supported_input: kind(index),
            // Low and high frequency, left and right trigger, as on every
            // console pad; a host pad without some drops them.
            rumble_motors: if index < PADS { 0xF } else { 0 },
            ..Default::default()
        };
        Device {
            vtable: &DEVICE,
            hub: std::ptr::null(),
            index,
            info,
        }
    }

    fn hub(&self) -> &'static Hub {
        // SAFETY: set by `Hub::new`; hubs are never freed.
        unsafe { &*self.hub }
    }
}

#[derive(PartialEq)]
enum Value {
    Pad(PadState),
    Keys(Vec<KeyState>),
}

#[repr(C)]
pub struct Reading {
    vtable: &'static Table<25>,
    refs: AtomicU32,
    device: *const Device,
    sequence: u64,
    timestamp: u64,
    value: Value,
}

impl Reading {
    fn kind(&self) -> u32 {
        match self.value {
            Value::Pad(_) => GAMEPAD_KIND,
            Value::Keys(_) => KEYBOARD_KIND,
        }
    }
}

/// A counted reference to a reading.
struct Ref(NonNull<Reading>);

impl Ref {
    fn new(reading: Reading) -> Ref {
        Ref(NonNull::from(Box::leak(Box::new(reading))))
    }

    /// Another reference, for the title.
    fn share(&self) -> *const Reading {
        reading_add_ref(self.0.as_ptr());
        self.0.as_ptr()
    }
}

impl Deref for Ref {
    type Target = Reading;
    fn deref(&self) -> &Reading {
        // SAFETY: we hold a reference, so it is alive.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        reading_release(self.0.as_ptr());
    }
}

type DeviceCallback = extern "system" fn(u64, *mut c_void, *const Device, u64, u32, u32);

#[derive(Clone, Copy)]
struct Registration {
    token: u64,
    /// One device's address, or 0 for any.
    device: usize,
    kind: u32,
    status: u32,
    context: usize,
    callback: DeviceCallback,
}

impl Registration {
    fn matches(&self, devices: &[Device; DEVICES], index: usize) -> bool {
        kind(index) & self.kind != 0
            && (self.device == 0 || self.device == &devices[index] as *const Device as usize)
    }

    fn event(&self, device: usize, timestamp: u64, current: u32, previous: u32) -> Event {
        Event {
            registration: *self,
            device,
            timestamp,
            current,
            previous,
        }
    }
}

struct Event {
    registration: Registration,
    device: usize,
    timestamp: u64,
    current: u32,
    previous: u32,
}

#[derive(Default)]
struct Slot {
    connected: bool,
    sequence: u64,
    history: VecDeque<Ref>,
}

struct State {
    slots: [Slot; DEVICES],
    callbacks: Vec<Registration>,
    /// Asynchronous enumerations, delivered by the next poll.
    pending: Vec<Event>,
    /// Device callbacks running now: token and thread.
    running: Vec<(u64, ThreadId)>,
    /// The keyboard and mouse's pad, unless `[input]` turned it off.
    kbm: Option<Kbm>,
    /// Buttons the autopilot holds (`hold_buttons`).
    autopilot: u32,
}

pub struct Hub {
    object: Object,
    devices: [Device; DEVICES],
    /// Apart from the state: XInput can be slow, and the title reads
    /// readings every frame.
    source: Mutex<Box<dyn Source>>,
    state: Mutex<State>,
    /// A device callback returned.
    returned: Condvar,
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// SAFETY: the back pointers are set before the hub is shared and never
// change; what changes sits behind the mutex or is atomic.
unsafe impl Send for Hub {}
// SAFETY: as above.
unsafe impl Sync for Hub {}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Microseconds, the clock of every timestamp we hand out.
fn now() -> u64 {
    START.elapsed().as_micros() as u64
}

impl Hub {
    fn new(source: Box<dyn Source>) -> &'static Hub {
        let hub = Box::into_raw(Box::new(Hub {
            object: Object {
                vtable: &INPUT,
                hub: std::ptr::null(),
            },
            devices: std::array::from_fn(Device::new),
            source: Mutex::new(source),
            state: Mutex::new(State {
                slots: Default::default(),
                callbacks: Vec::new(),
                pending: Vec::new(),
                running: Vec::new(),
                kbm: tuning().keyboard_mouse.then(|| Kbm::new(tuning())),
                autopilot: 0,
            }),
            returned: Condvar::new(),
        }));
        // SAFETY: just allocated and never freed; nothing else sees it yet.
        unsafe {
            (*hub).object.hub = hub;
            for device in &mut (*hub).devices {
                device.hub = hub;
            }
            &*hub
        }
    }

    /// A hub over the host's input, polled from now on.
    fn start(source: Box<dyn Source>) -> &'static Hub {
        let hub = Hub::new(source);
        hub.poll();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL);
                hub.poll();
            }
        });
        hub
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Reads the host once; device callbacks run on the calling thread.
    fn poll(&self) {
        let (pads, keys, mouse) = {
            let mut source = lock(&self.source);
            let pads: [_; PADS] = std::array::from_fn(|slot| source.pad(slot));
            (pads, source.keys(), source.mouse())
        };
        let events = self.state().poll(&self.devices, pads, keys, mouse);
        self.deliver(events);
    }

    /// Runs callbacks, never under our lock: they call back into us. One
    /// unregistered since the event was taken does not run, and
    /// `UnregisterCallback` waits for one that runs.
    fn deliver(&self, events: Vec<Event>) {
        let me = std::thread::current().id();
        for e in events {
            let r = e.registration;
            {
                let mut state = self.state();
                if !state.callbacks.iter().any(|c| c.token == r.token) {
                    continue;
                }
                state.running.push((r.token, me));
            }
            (r.callback)(
                r.token,
                r.context as *mut c_void,
                &self.devices[e.device],
                e.timestamp,
                e.current,
                e.previous,
            );
            let mut state = self.state();
            if let Some(i) = state.running.iter().position(|&x| x == (r.token, me)) {
                state.running.swap_remove(i);
            }
            self.returned.notify_all();
        }
    }

    fn index(&self, device: *const Device) -> Option<usize> {
        self.devices.iter().position(|d| std::ptr::eq(d, device))
    }
}

impl State {
    fn poll(
        &mut self,
        devices: &[Device; DEVICES],
        mut pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        mouse: MouseInput,
    ) -> Vec<Event> {
        let now = now();
        let mut events = std::mem::take(&mut self.pending);
        let mut kbm = self.kbm.as_mut().map(|kbm| kbm.pad(&keys, &mouse, now));
        if self.autopilot != 0 {
            kbm.get_or_insert_default().buttons |= self.autopilot;
        }
        // On the first pad, or as the stand-in while there is none.
        let stand_in = match (pads.iter_mut().flatten().next(), kbm) {
            (Some(pad), Some(kbm)) => {
                merge(pad, &kbm);
                None
            }
            (Some(_), None) => None,
            (None, kbm) => kbm.map(Value::Pad),
        };
        for (slot, pad) in pads.into_iter().enumerate() {
            self.update(devices, slot, pad.map(Value::Pad), now, &mut events);
        }
        self.update(devices, FALLBACK, stand_in, now, &mut events);
        self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
        events
    }

    /// Records a device's state: a reading when it changed, callbacks when
    /// the device came or went.
    fn update(
        &mut self,
        devices: &[Device; DEVICES],
        index: usize,
        value: Option<Value>,
        now: u64,
        events: &mut Vec<Event>,
    ) {
        let slot = &mut self.slots[index];
        let was = slot.connected;
        slot.connected = value.is_some();
        if let Some(value) = value
            && (!was || slot.history.back().is_none_or(|r| r.value != value))
        {
            slot.sequence += 1;
            if slot.history.len() == HISTORY {
                slot.history.pop_front();
            }
            slot.history.push_back(Ref::new(Reading {
                vtable: &READING,
                refs: AtomicU32::new(1),
                device: &devices[index],
                sequence: slot.sequence,
                timestamp: now,
                value,
            }));
        }
        if was != slot.connected {
            tracing::info!(device = index, connected = !was, "input device");
            let (current, previous) = if was {
                (0, STATUS_CONNECTED)
            } else {
                (STATUS_CONNECTED, 0)
            };
            events.extend(
                self.callbacks
                    .iter()
                    .filter(|r| r.matches(devices, index) && r.status & STATUS_CONNECTED != 0)
                    .map(|r| r.event(index, now, current, previous)),
            );
        }
    }
}

#[cfg(windows)]
fn host() -> Box<dyn Source> {
    Box::new(host::Host::default())
}

#[cfg(not(windows))]
fn host() -> Box<dyn Source> {
    struct Nothing;
    impl Source for Nothing {
        fn pad(&mut self, _: usize) -> Option<PadState> {
            None
        }
        fn keys(&mut self) -> Vec<KeyState> {
            Vec::new()
        }
        fn mouse(&mut self) -> MouseInput {
            MouseInput::default()
        }
        fn rumble(&mut self, _: usize, _: [f32; 4]) {}
    }
    Box::new(Nothing)
}

static HUB: LazyLock<&'static Hub> = LazyLock::new(|| Hub::start(host()));

/// Holds pad buttons (bits from [`button`]) down for the autopilot until
/// the next call, 0 for none: on the pad that plays, as the keyboard's are.
pub(crate) fn hold_buttons(buttons: u32) {
    HUB.state().autopilot = buttons;
}

/// The bit of the pad button `[input.bindings]` names `name` (`a`, `menu`,
/// `dpad_up`, ...; any case).
pub(crate) fn button(name: &str) -> Option<u32> {
    bindings::button(name)
}

type This = *const Object;
type Out = *mut *const c_void;

/// The DLL's only export (ordinal 1): the input object, static and shared.
pub extern "system" fn GameInputCreate(out: *mut This) -> HRESULT {
    tracing::info!("GameInputCreate");
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a pointer.
    unsafe { out.write(&HUB.object) };
    S_OK
}

fn hub(this: This) -> &'static Hub {
    // SAFETY: `this` is a hub's object; hubs are never freed.
    unsafe { &*(*this).hub }
}

/// Clears a COM out pointer and returns `hr`.
fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(std::ptr::null()) };
    }
    hr
}

/// Gives the title a reference to `reading`, or fails as with none.
fn hand(out: Out, reading: Option<&Ref>) -> HRESULT {
    match reading {
        _ if out.is_null() => E_INVALIDARG,
        Some(reading) => {
            // SAFETY: checked non-null; the title passes room for a pointer.
            unsafe { out.write(reading.share().cast()) };
            S_OK
        }
        None => fail(out, READING_NOT_FOUND),
    }
}

extern "system" fn no_interface(_: *const c_void, _iid: *const c_void, out: Out) -> HRESULT {
    tracing::warn!("input QueryInterface not implemented");
    fail(out, E_NOINTERFACE)
}
/// The input object and devices live as long as the process.
extern "system" fn add_ref(_: *const c_void) -> u32 {
    1
}

extern "system" fn timestamp(_: This) -> u64 {
    now()
}

extern "system" fn current_reading(
    this: This,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let state = hub.state();
    let newest = |i: usize| {
        state.slots[i]
            .history
            .back()
            .filter(|r| r.kind() & kind != 0)
    };
    let found = if device.is_null() {
        (0..DEVICES)
            .filter(|&i| state.slots[i].connected)
            .filter_map(newest)
            .max_by_key(|r| r.timestamp)
    } else {
        match hub.index(device) {
            None => return fail(out, E_INVALIDARG),
            Some(i) if !state.slots[i].connected => return fail(out, DEVICE_DISCONNECTED),
            Some(i) => newest(i),
        }
    };
    tracing::trace!(
        kind = format_args!("{kind:#x}"),
        device = ?hub.index(device),
        sequence = found.map(|r| r.sequence),
        "current reading"
    );
    hand(out, found)
}

/// The reading after (`forward`) or before `reference` on its device.
fn relative(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
    forward: bool,
) -> HRESULT {
    if reference.is_null() {
        return fail(out, E_INVALIDARG);
    }
    let hub = hub(this);
    // SAFETY: a reading we handed out and the title still holds.
    let reference = unsafe { &*reference };
    // shortcut: with no device given only the reference's device is walked,
    // not every device's readings in time order; Grand Theft Auto VI walks
    // only keyboard readings (one device) and the current pad's previous one.
    let Some(index) = hub.index(reference.device) else {
        return fail(out, E_INVALIDARG);
    };
    if !device.is_null() && !std::ptr::eq(device, reference.device) {
        return fail(out, E_INVALIDARG);
    }
    let state = hub.state();
    let history = &state.slots[index].history;
    if history
        .front()
        .is_none_or(|oldest| reference.sequence < oldest.sequence)
    {
        return fail(out, REFERENCE_TOO_OLD);
    }
    let wanted = |r: &&Ref| r.kind() & kind != 0;
    let found = if forward {
        history
            .iter()
            .filter(|r| r.sequence > reference.sequence)
            .find(wanted)
    } else {
        history
            .iter()
            .rev()
            .filter(|r| r.sequence < reference.sequence)
            .find(wanted)
    };
    hand(out, found)
}
extern "system" fn next_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, true)
}
extern "system" fn previous_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, false)
}
/// The device's reading at `time`, among those still kept.
extern "system" fn temporal_reading(
    this: This,
    time: u64,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let Some(index) = hub.index(device) else {
        return fail(out, E_INVALIDARG);
    };
    let state = hub.state();
    let history = &state.slots[index].history;
    hand(out, history.iter().rev().find(|r| r.timestamp <= time))
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn token(out: *mut u64) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)) };
    }
    S_OK
}

// shortcut: reading callbacks never run; Grand Theft Auto VI registers none
// (it polls).
extern "system" fn register_reading(
    _: This,
    _device: *const c_void,
    _kind: u32,
    _threshold: f32,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    tracing::warn!("input reading callbacks are never called");
    token(out)
}

/// Device callbacks run on our poll thread when a device comes or goes.
/// Enumeration reports the devices already there: blocking on the caller's
/// thread before we return (the title registers holding a lock its callback
/// takes again), asynchronous on the next poll.
extern "system" fn register_device(
    this: This,
    device: *const Device,
    kind: u32,
    status: u32,
    enumeration: u32,
    context: *mut c_void,
    callback: Option<DeviceCallback>,
    out: *mut u64,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let hub = hub(this);
    let registration = Registration {
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        device: device as usize,
        kind,
        status,
        context: context as usize,
        callback,
    };
    let mut found = Vec::new();
    {
        let mut state = hub.state();
        if enumeration != 0 && status & STATUS_CONNECTED != 0 {
            let now = now();
            found = (0..DEVICES)
                .filter(|&i| state.slots[i].connected && registration.matches(&hub.devices, i))
                .map(|i| registration.event(i, now, STATUS_CONNECTED, 0))
                .collect();
        }
        tracing::info!(
            kind = format_args!("{kind:#x}"),
            enumeration,
            devices = found.len(),
            "input device callback"
        );
        state.callbacks.push(registration);
        if enumeration == ASYNC_ENUMERATION {
            state.pending.append(&mut found);
        }
    }
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(registration.token) };
    }
    hub.deliver(found);
    S_OK
}
/// Guide button and keyboard layout callbacks: neither ever changes here.
extern "system" fn register_other(
    _: This,
    _device: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    token(out)
}

fn forget(this: This, token: u64) {
    let mut state = hub(this).state();
    state.callbacks.retain(|r| r.token != token);
    state.pending.retain(|e| e.registration.token != token);
}
extern "system" fn stop_callback(this: This, token: u64) {
    forget(this, token);
}
/// Stops the callback, then waits up to `timeout` µs for a call of it that
/// is running; false on time-out, or from inside that callback (it cannot
/// wait for itself), as documented. Either way it is never called again.
extern "system" fn unregister_callback(this: This, token: u64, timeout: u64) -> bool {
    forget(this, token);
    let hub = hub(this);
    let me = std::thread::current().id();
    let deadline = Instant::now().checked_add(Duration::from_micros(timeout));
    let mut state = hub.state();
    loop {
        let running = state.running.iter().filter(|r| r.0 == token);
        if running.clone().any(|r| r.1 == me) {
            return false;
        }
        if running.count() == 0 {
            return true;
        }
        state = match deadline {
            None => hub
                .returned
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                let waited = hub.returned.wait_timeout(state, left);
                waited.unwrap_or_else(PoisonError::into_inner).0
            }
        };
    }
}

/// Dispatcher and aggregate device: no title of ours asks for them.
extern "system" fn create_dispatcher(_: This, out: Out) -> HRESULT {
    tracing::warn!("input dispatcher not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn create_aggregate(_: This, _kind: u32, out: Out) -> HRESULT {
    tracing::warn!("aggregate input device not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn find_by_id(this: This, id: *const [u8; 32], out: Out) -> HRESULT {
    if id.is_null() || out.is_null() {
        return fail(out, E_INVALIDARG);
    }
    // SAFETY: checked non-null; the title passes a device id.
    let id = unsafe { &*id };
    match hub(this).devices.iter().find(|d| d.info.id == *id) {
        Some(device) => {
            // SAFETY: checked non-null.
            unsafe { out.write((device as *const Device).cast()) };
            S_OK
        }
        None => fail(out, DEVICE_NOT_FOUND),
    }
}
/// Lookup by object, platform handle or platform string: our devices have
/// none of these.
extern "system" fn find_device(_: This, _value: *const c_void, out: Out) -> HRESULT {
    fail(out, DEVICE_NOT_FOUND)
}
extern "system" fn enable_oem_device(
    _: This,
    _vendor: u16,
    _product: u16,
    _interface: u8,
    _collection: u8,
) -> HRESULT {
    S_OK
}
extern "system" fn set_focus_policy(_: This, _policy: u32) {}

static INPUT: Table<22> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    timestamp as *const (),           // 0x18
    current_reading as *const (),     // 0x20
    next_reading as *const (),        // 0x28
    previous_reading as *const (),    // 0x30
    temporal_reading as *const (),    // 0x38
    register_reading as *const (),    // 0x40
    register_device as *const (),     // 0x48
    register_other as *const (),      // 0x50 guide button
    register_other as *const (),      // 0x58 keyboard layout
    stop_callback as *const (),       // 0x60
    unregister_callback as *const (), // 0x68
    create_dispatcher as *const (),   // 0x70
    create_aggregate as *const (),    // 0x78
    find_by_id as *const (),          // 0x80
    find_device as *const (),         // 0x88 by object
    find_device as *const (),         // 0x90 by platform handle
    find_device as *const (),         // 0x98 by platform string
    enable_oem_device as *const (),   // 0xa0
    set_focus_policy as *const (),    // 0xa8
]);

extern "system" fn device_info(this: *const Device) -> *const DeviceInfo {
    // SAFETY: one of our devices.
    unsafe { &(*this).info }
}
extern "system" fn device_status(this: *const Device) -> u32 {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    if device.hub().state().slots[device.index].connected {
        STATUS_CONNECTED
    } else {
        0
    }
}
/// Four motor levels: low and high frequency, left and right trigger
/// (Grand Theft Auto VI sends trigger levels too); null stops them all.
extern "system" fn set_rumble(this: *const Device, params: *const [f32; 4]) {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    if device.index >= PADS {
        return;
    }
    let levels = if params.is_null() {
        [0.0; 4]
    } else {
        // SAFETY: checked non-null; four motor levels.
        unsafe { *params }
    };
    // Within 0 to 1; NaN is 0.
    let motors = levels.map(|level| if level > 0.0 { level.min(1.0) } else { 0.0 });
    lock(&device.hub().source).rumble(device.index, motors);
}
extern "system" fn unsupported<const SLOT: usize>(_: *const c_void) -> HRESULT {
    tracing::warn!(
        slot = format_args!("{SLOT:#x}"),
        "input device method not implemented"
    );
    E_NOTIMPL
}

static DEVICE: Table<21> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    device_info as *const (),         // 0x18
    device_status as *const (),       // 0x20
    unsupported::<0x28> as *const (), // battery
    unsupported::<0x30> as *const (), // force feedback
    unsupported::<0x38> as *const (), //
    unsupported::<0x40> as *const (), //
    unsupported::<0x48> as *const (), // haptics
    set_rumble as *const (),          // 0x50
    unsupported::<0x58> as *const (), // input synchronization
    unsupported::<0x60> as *const (), //
    unsupported::<0x68> as *const (), // power off
    unsupported::<0x70> as *const (), // raw device access
    unsupported::<0x78> as *const (), //
    unsupported::<0x80> as *const (), //
    unsupported::<0x88> as *const (), //
    unsupported::<0x90> as *const (), //
    unsupported::<0x98> as *const (), //
    unsupported::<0xa0> as *const (), //
]);

fn reading<'a>(this: *const Reading) -> &'a Reading {
    // SAFETY: a reading we handed out and the title still holds.
    unsafe { &*this }
}
extern "system" fn reading_add_ref(this: *const Reading) -> u32 {
    reading(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
extern "system" fn reading_release(this: *const Reading) -> u32 {
    let left = reading(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference is gone; `Ref::new` boxed it.
        drop(unsafe { Box::from_raw(this.cast_mut()) });
    }
    left
}
extern "system" fn input_kind(this: *const Reading) -> u32 {
    reading(this).kind()
}
extern "system" fn sequence_number(this: *const Reading, kind: u32) -> u64 {
    let reading = reading(this);
    if reading.kind() & kind != 0 {
        reading.sequence
    } else {
        0
    }
}
extern "system" fn reading_timestamp(this: *const Reading) -> u64 {
    reading(this).timestamp
}
extern "system" fn reading_device(this: *const Reading, out: *mut *const Device) {
    if !out.is_null() {
        // SAFETY: checked non-null; devices need no reference.
        unsafe { out.write(reading(this).device) };
    }
}
extern "system" fn key_count(this: *const Reading) -> u32 {
    match &reading(this).value {
        Value::Keys(keys) => keys.len() as u32,
        Value::Pad(_) => 0,
    }
}
extern "system" fn key_state(this: *const Reading, count: u32, out: *mut KeyState) -> u32 {
    let Value::Keys(keys) = &reading(this).value else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let n = keys.len().min(count as usize);
    // SAFETY: the title passes room for `count` keys.
    unsafe { std::ptr::copy_nonoverlapping(keys.as_ptr(), out, n) };
    n as u32
}
extern "system" fn gamepad_state(this: *const Reading, out: *mut PadState) -> bool {
    match &reading(this).value {
        Value::Pad(state) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(*state) };
            true
        }
        _ => false,
    }
}
/// Kinds our devices do not have: no count, no state.
extern "system" fn nothing(_: *const c_void) -> usize {
    0
}

static READING: Table<25> = Table([
    no_interface as *const (),      // 0x00
    reading_add_ref as *const (),   // 0x08
    reading_release as *const (),   // 0x10
    input_kind as *const (),        // 0x18
    sequence_number as *const (),   // 0x20
    reading_timestamp as *const (), // 0x28
    reading_device as *const (),    // 0x30
    nothing as *const (),           // 0x38 raw report
    nothing as *const (),           // 0x40 controller axes, buttons, switches
    nothing as *const (),           // 0x48
    nothing as *const (),           // 0x50
    nothing as *const (),           // 0x58
    nothing as *const (),           // 0x60
    nothing as *const (),           // 0x68
    key_count as *const (),         // 0x70
    key_state as *const (),         // 0x78
    nothing as *const (),           // 0x80 mouse
    nothing as *const (),           // 0x88 touch
    nothing as *const (),           // 0x90
    nothing as *const (),           // 0x98 motion
    nothing as *const (),           // 0xa0 arcade stick
    nothing as *const (),           // 0xa8 flight stick
    gamepad_state as *const (),     // 0xb0
    nothing as *const (),           // 0xb8 racing wheel
    nothing as *const (),           // 0xc0 UI navigation
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "gameinput",
        dlls: &["gameinput.dll"],
        // Titles import it by ordinal only.
        exports: vec![HleExport {
            name: "#1",
            address: GameInputCreate as *const () as usize,
        }],
    }
}

/// Host input: XInput pads (and Windows.Gaming.Input for their trigger
/// motors), the keyboard while our process has focus, the mouse while the
/// title's window holds it.
#[cfg(windows)]
mod host {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, GetKeyboardLayout, HKL, MAPVK_VK_TO_VSC_EX, MapVirtualKeyExW, VK_DELETE,
        VK_DOWN, VK_END, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_UP,
    };
    use windows_sys::Win32::UI::Input::XboxController::{
        XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
        XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
        XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
        XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
        XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState,
        XInputSetState,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    use super::*;

    /// Asking XInput about an empty slot is slow; look again this often.
    const EMPTY_SLOT_RECHECK: Duration = Duration::from_secs(1);

    const BUTTONS: [(u16, u32); 14] = [
        (XINPUT_GAMEPAD_START, MENU),
        (XINPUT_GAMEPAD_BACK, VIEW),
        (XINPUT_GAMEPAD_A, A),
        (XINPUT_GAMEPAD_B, B),
        (XINPUT_GAMEPAD_X, X),
        (XINPUT_GAMEPAD_Y, Y),
        (XINPUT_GAMEPAD_DPAD_UP, UP),
        (XINPUT_GAMEPAD_DPAD_DOWN, DOWN),
        (XINPUT_GAMEPAD_DPAD_LEFT, LEFT),
        (XINPUT_GAMEPAD_DPAD_RIGHT, RIGHT),
        (XINPUT_GAMEPAD_LEFT_SHOULDER, LEFT_SHOULDER),
        (XINPUT_GAMEPAD_RIGHT_SHOULDER, RIGHT_SHOULDER),
        (XINPUT_GAMEPAD_LEFT_THUMB, LEFT_STICK),
        (XINPUT_GAMEPAD_RIGHT_THUMB, RIGHT_STICK),
    ];

    /// Raw values, no dead zone: titles apply their own.
    pub(super) fn convert(pad: &XINPUT_GAMEPAD) -> PadState {
        let stick = |v: i16| (f32::from(v) / 32767.0).max(-1.0);
        PadState {
            buttons: BUTTONS
                .iter()
                .filter(|(host, _)| pad.wButtons & host != 0)
                .fold(0, |all, (_, bit)| all | bit),
            left_trigger: f32::from(pad.bLeftTrigger) / 255.0,
            right_trigger: f32::from(pad.bRightTrigger) / 255.0,
            left_x: stick(pad.sThumbLX),
            left_y: stick(pad.sThumbLY),
            right_x: stick(pad.sThumbRX),
            right_y: stick(pad.sThumbRY),
        }
    }

    #[derive(Default)]
    pub(super) struct Host {
        empty_until: [Option<Instant>; PADS],
        triggers: super::triggers::Triggers,
    }

    impl Host {
        fn read(&mut self, slot: usize) -> Option<PadState> {
            if self.empty_until[slot].is_some_and(|until| Instant::now() < until) {
                return None;
            }
            let mut state = XINPUT_STATE::default();
            // SAFETY: room for the state.
            if unsafe { XInputGetState(slot as u32, &mut state) } != 0 {
                self.empty_until[slot] = Some(Instant::now() + EMPTY_SLOT_RECHECK);
                return None;
            }
            self.empty_until[slot] = None;
            Some(convert(&state.Gamepad))
        }
    }

    impl Source for Host {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            let state = self.read(slot);
            self.triggers.saw(slot, state.map(|s| s.buttons));
            state
        }

        /// Keys by virtual key, each with the scan code of its place on the
        /// layout of the window in front: on AZERTY, Z is the key in W's
        /// place.
        fn keys(&mut self) -> Vec<KeyState> {
            let layout = focused_layout();
            let key = |virtual_key: u8, layout: HKL| KeyState {
                scan_code: scan_code(virtual_key, layout),
                virtual_key,
                // shortcut: no characters (code point 0); Grand Theft
                // Auto VI reads scan codes and virtual keys only.
                ..Default::default()
            };
            #[cfg(feature = "diagnostics")]
            let mut down = scripted(layout);
            #[cfg(not(feature = "diagnostics"))]
            let mut down = Vec::new();
            if let Some(layout) = layout {
                // Left/right modifier keys only, as devices report them; no
                // mouse buttons (below 0x08).
                let held: Vec<KeyState> = (0x08..=0xFE_u8)
                    .filter(|vk| !(0x10..=0x12).contains(vk))
                    .filter(|vk| !down.iter().any(|k: &KeyState| k.virtual_key == *vk))
                    // SAFETY: plain queries.
                    .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
                    .map(|vk| key(vk, layout))
                    .collect();
                down.extend(held);
            }
            down
        }

        fn mouse(&mut self) -> MouseInput {
            super::window::take()
        }

        fn rumble(&mut self, slot: usize, motors: [f32; 4]) {
            if self.triggers.rumble(slot, motors) {
                return;
            }
            let speeds = XINPUT_VIBRATION {
                wLeftMotorSpeed: speed(motors[0]),
                wRightMotorSpeed: speed(motors[1]),
            };
            // SAFETY: a valid vibration; an empty slot just fails.
            unsafe { XInputSetState(slot as u32, &speeds) };
        }
    }

    /// The arrows and the six keys above them. Windows gives their virtual
    /// keys the number pad's scan codes (those keys with Num Lock off make
    /// the same virtual keys); their own keys are the extended ones.
    const NAVIGATION: [u16; 10] = [
        VK_PRIOR, VK_NEXT, VK_END, VK_HOME, VK_LEFT, VK_UP, VK_RIGHT, VK_DOWN, VK_INSERT, VK_DELETE,
    ];

    /// The scan code of `virtual_key`'s place on `layout`, 0xE0xx when
    /// extended.
    fn scan_code(virtual_key: u8, layout: HKL) -> u32 {
        // SAFETY: plain query.
        let code = unsafe { MapVirtualKeyExW(u32::from(virtual_key), MAPVK_VK_TO_VSC_EX, layout) };
        if code & 0xFF00 == 0 && code != 0 && NAVIGATION.contains(&u16::from(virtual_key)) {
            code | 0xE000
        } else {
            code
        }
    }

    /// XInput's motor speed for a level from 0 to 1.
    pub(super) fn speed(level: f32) -> u16 {
        (level * f32::from(u16::MAX)) as u16
    }

    /// Debugging aid: `XTP_KEYS=<seconds>:<key>,...` holds each key for
    /// 0.2 s that long after input starts, with or without focus. Keys are
    /// places, named as in `[input.bindings]` (`bindings::scan_code`):
    /// `W` is the key in W's place on any layout.
    #[cfg(feature = "diagnostics")]
    fn scripted(layout: Option<HKL>) -> Vec<KeyState> {
        static SCRIPT: LazyLock<(Instant, Vec<(Duration, u32)>)> = LazyLock::new(|| {
            let list = std::env::var("XTP_KEYS").unwrap_or_default();
            let keys = list.split(',').filter_map(|entry| {
                let (at, key) = entry.split_once(':')?;
                Some((
                    Duration::try_from_secs_f64(at.trim().parse().ok()?).ok()?,
                    super::bindings::scan_code(key.trim())?,
                ))
            });
            (Instant::now(), keys.collect())
        });
        // SAFETY: plain query; this thread's layout when no window of ours
        // is in front.
        let layout = layout.unwrap_or_else(|| unsafe { GetKeyboardLayout(0) });
        let (start, keys) = &*SCRIPT;
        let now = start.elapsed();
        keys.iter()
            .filter(|&&(at, _)| (at..at + Duration::from_millis(200)).contains(&now))
            .map(|&(_, scan_code)| KeyState {
                scan_code,
                // SAFETY: plain query.
                virtual_key: unsafe {
                    MapVirtualKeyExW(
                        scan_code,
                        windows_sys::Win32::UI::Input::KeyboardAndMouse::MAPVK_VSC_TO_VK_EX,
                        layout,
                    )
                } as u8,
                ..Default::default()
            })
            .collect()
    }

    /// The keyboard layout of the window in front when it is one of ours
    /// (the title's, or a dialog of ours); `None` otherwise.
    fn focused_layout() -> Option<HKL> {
        let mut process = 0;
        // SAFETY: plain queries; a null window gives process 0.
        unsafe {
            let thread = GetWindowThreadProcessId(GetForegroundWindow(), &mut process);
            if process == GetCurrentProcessId() {
                Some(GetKeyboardLayout(thread))
            } else {
                None
            }
        }
    }

    #[test]
    fn host_pads_convert_to_console_pads() {
        let pad = XINPUT_GAMEPAD {
            wButtons: XINPUT_GAMEPAD_A | XINPUT_GAMEPAD_START | XINPUT_GAMEPAD_DPAD_LEFT,
            bLeftTrigger: 255,
            sThumbLX: -32768,
            sThumbLY: 32767,
            ..Default::default()
        };
        let state = convert(&pad);
        assert_eq!(state.buttons, A | MENU | LEFT);
        assert_eq!(
            (
                state.left_trigger,
                state.left_x,
                state.left_y,
                state.right_x
            ),
            (1.0, -1.0, 1.0, 0.0)
        );
        assert_eq!((speed(0.0), speed(0.5), speed(1.0)), (0, 32767, 65535));
    }

    #[test]
    fn host_scan_codes_are_the_places_bindings_name() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse as k;
        // Keys in the same place on every layout, extended ones included.
        let fixed = [
            (k::VK_ESCAPE, "Esc"),
            (k::VK_RETURN, "Enter"),
            (k::VK_SPACE, "Space"),
            (k::VK_TAB, "Tab"),
            (k::VK_BACK, "Backspace"),
            (k::VK_LSHIFT, "LeftShift"),
            (k::VK_RCONTROL, "RightCtrl"),
            (k::VK_UP, "Up"),
            (k::VK_DOWN, "Down"),
            (k::VK_LEFT, "Left"),
            (k::VK_RIGHT, "Right"),
            (k::VK_HOME, "Home"),
            (k::VK_PRIOR, "PageUp"),
            (k::VK_DELETE, "Delete"),
            (k::VK_F5, "F5"),
            (k::VK_F12, "F12"),
        ];
        // SAFETY: a plain query: this thread's layout.
        let layout = unsafe { GetKeyboardLayout(0) };
        for (vk, name) in fixed {
            let code = scan_code(vk as u8, layout);
            assert_eq!(Some(code), super::bindings::scan_code(name), "{name}");
        }
        // The number pad keeps its own places.
        assert_eq!(scan_code(k::VK_NUMPAD8 as u8, layout), 0x48);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::HleResolver;
    use crate::HostPolicy;
    use xtp_loader::Provenance;
    use xtp_pe::ImportSymbol;

    #[derive(Default)]
    struct FakeHost {
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        /// Counts and wheel are taken by a poll; the rest stays.
        mouse: MouseInput,
        rumble: Vec<(usize, [f32; 4])>,
    }

    struct Fake(Arc<Mutex<FakeHost>>);

    impl Source for Fake {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            self.0.lock().unwrap().pads[slot]
        }
        fn keys(&mut self) -> Vec<KeyState> {
            self.0.lock().unwrap().keys.clone()
        }
        fn mouse(&mut self) -> MouseInput {
            let mouse = &mut self.0.lock().unwrap().mouse;
            let now = *mouse;
            (mouse.dx, mouse.dy, mouse.wheel) = (0, 0, 0);
            now
        }
        fn rumble(&mut self, slot: usize, motors: [f32; 4]) {
            self.0.lock().unwrap().rumble.push((slot, motors));
        }
    }

    fn fake() -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        let host = Arc::new(Mutex::new(FakeHost::default()));
        let hub = Hub::new(Box::new(Fake(Arc::clone(&host))));
        hub.poll();
        (hub, host)
    }

    /// The key in the place `name` labels on a US keyboard, reading
    /// `virtual_key` on the layout in use.
    fn key(name: &str, virtual_key: u8) -> KeyState {
        KeyState {
            scan_code: bindings::scan_code(name).unwrap(),
            virtual_key,
            ..Default::default()
        }
    }

    /// The stand-in pad as the title reads it now.
    fn stand_in(hub: &Hub) -> PadState {
        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        let state = pad(r);
        release(r);
        state
    }

    fn slot<F: Copy, T>(object: *const T, offset: usize) -> F {
        // SAFETY: our objects start with their table, which has the slot.
        let f = unsafe { *(*(object as *const *const *const ())).add(offset / 8) };
        // SAFETY: the caller names the slot's signature.
        unsafe { std::mem::transmute_copy(&f) }
    }

    /// Device callbacks seen: device index, current and previous status.
    type Log = Mutex<Vec<(usize, u32, u32)>>;

    extern "system" fn record(
        _token: u64,
        context: *mut c_void,
        device: *const Device,
        _timestamp: u64,
        current: u32,
        previous: u32,
    ) {
        // SAFETY: the test passes its log and one of the hub's devices.
        let (log, index) = unsafe { (&*(context as *const Log), (*device).index) };
        log.lock().unwrap().push((index, current, previous));
    }

    fn register(hub: &Hub, kind: u32, enumeration: u32, log: &Log) -> u64 {
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let mut token = 0;
        let context = log as *const Log as *mut c_void;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let hr = register(
            &hub.object,
            std::ptr::null(),
            kind,
            1,
            enumeration,
            context,
            Some(record),
            &mut token,
        );
        assert_eq!(hr, S_OK);
        assert_ne!(token, 0);
        token
    }

    fn result(hr: HRESULT, out: *const c_void) -> Result<*const Reading, HRESULT> {
        if hr == S_OK {
            Ok(out.cast())
        } else {
            assert!(out.is_null());
            Err(hr)
        }
    }

    fn current(hub: &Hub, kind: u32, device: usize) -> Result<*const Reading, HRESULT> {
        type Current = extern "system" fn(This, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let hr = slot::<Current, _>(&hub.object, 0x20)(
            &hub.object,
            kind,
            &hub.devices[device],
            &mut out,
        );
        result(hr, out)
    }

    fn walk(hub: &Hub, offset: usize, from: *const Reading) -> Result<*const Reading, HRESULT> {
        type Walk = extern "system" fn(This, *const Reading, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let device = reading(from).device;
        let hr =
            slot::<Walk, _>(&hub.object, offset)(&hub.object, from, GAMEPAD_KIND, device, &mut out);
        result(hr, out)
    }

    fn pad(reading: *const Reading) -> PadState {
        let mut state = PadState::default();
        let get =
            slot::<extern "system" fn(*const Reading, *mut PadState) -> bool, _>(reading, 0xb0);
        assert!(get(reading, &mut state));
        state
    }

    fn stamp(reading: *const Reading) -> (u64, u64) {
        let time = slot::<extern "system" fn(*const Reading) -> u64, _>(reading, 0x28);
        let sequence = slot::<extern "system" fn(*const Reading, u32) -> u64, _>(reading, 0x20);
        (time(reading), sequence(reading, GAMEPAD_KIND))
    }

    fn release(reading: *const Reading) {
        slot::<extern "system" fn(*const Reading) -> u32, _>(reading, 0x10)(reading);
    }

    #[test]
    fn ordinal_one_resolves_to_create() {
        let r = HleResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("GameInput.dll", &ImportSymbol::Ordinal(1)),
            Some(Provenance::Hle)
        );
        assert_eq!(r.classify("GameInput.dll", &ImportSymbol::Ordinal(2)), None);
    }

    #[test]
    fn pads_connect_report_and_disconnect() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        // Blocking enumeration ran before registration returned: with no
        // host pad the keyboard's stand-in is the only one.
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        host.lock().unwrap().pads[1] = Some(PadState {
            buttons: A,
            left_x: 0.5,
            ..Default::default()
        });
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[1..],
            [(1, STATUS_CONNECTED, 0), (FALLBACK, 0, STATUS_CONNECTED)]
        );

        let first = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(first).buttons, A);
        assert_eq!(pad(first).left_x, 0.5);
        // No change, no new reading.
        hub.poll();
        let again = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(again, first);
        release(again);

        std::thread::sleep(Duration::from_millis(2));
        host.lock().unwrap().pads[1].as_mut().unwrap().buttons = B;
        hub.poll();
        let second = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(second).buttons, B);
        let (t1, s1) = stamp(first);
        let (t2, s2) = stamp(second);
        assert!(t2 > t1 && s2 == s1 + 1);
        assert!(t2 <= now());

        // The title's edge check: previous of current, and back again.
        assert_eq!(walk(hub, 0x30, second), Ok(first));
        assert_eq!(walk(hub, 0x28, first), Ok(second));
        assert_eq!(walk(hub, 0x28, second), Err(READING_NOT_FOUND));
        assert_eq!(walk(hub, 0x30, first), Err(READING_NOT_FOUND));
        for r in [first, first, second, second] {
            release(r);
        }

        host.lock().unwrap().pads[1] = None;
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[3..],
            [(1, 0, STATUS_CONNECTED), (FALLBACK, STATUS_CONNECTED, 0)]
        );
        assert_eq!(current(hub, GAMEPAD_KIND, 1), Err(DEVICE_DISCONNECTED));
        // The stand-in is back, at rest.
        let rest = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(pad(rest), PadState::default());
        release(rest);
    }

    #[test]
    fn keys_feed_the_keyboard_and_the_stand_in_pad() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, KEYBOARD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(KEYBOARD, STATUS_CONNECTED, 0)]);

        let keys = vec![
            key("W", b'W'),
            key("Enter", 0x0D),
            key("L", b'L'),
            key("C", b'C'),
        ];
        host.lock().unwrap().keys = keys.clone();
        hub.poll();
        let r = current(hub, KEYBOARD_KIND, KEYBOARD).unwrap();
        let count = slot::<extern "system" fn(*const Reading) -> u32, _>(r, 0x70);
        let state =
            slot::<extern "system" fn(*const Reading, u32, *mut KeyState) -> u32, _>(r, 0x78);
        assert_eq!(count(r), 4);
        let mut got = [KeyState::default(); 8];
        assert_eq!(state(r, 8, got.as_mut_ptr()), 4);
        assert_eq!(got[..4], keys[..]);
        release(r);

        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(
            pad(r),
            PadState {
                buttons: A,
                left_y: 1.0,
                right_x: 1.0,
                right_trigger: 1.0,
                ..Default::default()
            }
        );
        release(r);
        // A keyboard reading is no gamepad reading.
        assert_eq!(current(hub, GAMEPAD_KIND, KEYBOARD), Err(READING_NOT_FOUND));
    }

    #[test]
    fn autopilot_buttons_play_on_the_pad_that_plays() {
        let (hub, host) = fake();
        hub.state().autopilot = button("a").unwrap() | button("Menu").unwrap();
        hub.poll();
        assert_eq!(stand_in(hub).buttons, A | MENU);
        // On top of the first host pad once there is one.
        host.lock().unwrap().pads[1] = Some(PadState {
            buttons: B,
            ..Default::default()
        });
        hub.poll();
        let r = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(r).buttons, A | B | MENU);
        release(r);
        hub.state().autopilot = 0;
        hub.poll();
        let r = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(r).buttons, B);
        release(r);
        // Buttons only: triggers and sticks are no buttons.
        assert_eq!(button("left_trigger"), None);
        assert_eq!(button("start"), None);
    }

    #[test]
    fn keys_play_by_place_on_qwerty_and_azerty() {
        // The same places pressed: W, Q, E, Z by their US labels, which
        // AZERTY labels Z, A, E, W.
        let qwerty = [
            key("W", b'W'),
            key("Q", b'Q'),
            key("E", b'E'),
            key("Z", b'Z'),
        ];
        let azerty = [
            key("W", b'Z'),
            key("Q", b'A'),
            key("E", b'E'),
            key("Z", b'W'),
        ];
        let want = PadState {
            buttons: LEFT_SHOULDER | RIGHT_SHOULDER,
            left_trigger: 1.0,
            left_y: 1.0,
            ..Default::default()
        };
        for keys in [qwerty, azerty] {
            let (hub, host) = fake();
            host.lock().unwrap().keys = keys.to_vec();
            hub.poll();
            assert_eq!(stand_in(hub), want, "{keys:?}");
            // The keyboard still reports the layout's keys.
            let r = current(hub, KEYBOARD_KIND, KEYBOARD).unwrap();
            let state =
                slot::<extern "system" fn(*const Reading, u32, *mut KeyState) -> u32, _>(r, 0x78);
            let mut got = [KeyState::default(); 4];
            assert_eq!(state(r, 4, got.as_mut_ptr()), 4);
            assert_eq!(got, keys);
            release(r);
        }
        // AZERTY's own W (US Z's place) is the left trigger, not the stick.
        let (hub, host) = fake();
        host.lock().unwrap().keys = vec![key("Z", b'W')];
        hub.poll();
        assert_eq!(
            stand_in(hub),
            PadState {
                left_trigger: 1.0,
                ..Default::default()
            }
        );
    }

    #[test]
    fn the_mouse_plays_while_captured() {
        let (hub, host) = fake();
        host.lock().unwrap().mouse = MouseInput {
            captured: true,
            buttons: bindings::MOUSE_LEFT,
            dx: 400,
            dy: -400,
            wheel: 0,
        };
        hub.poll();
        // Right and up, fast: the right stick well out that way; the left
        // button is the right trigger.
        let pad = stand_in(hub);
        assert!(pad.right_x > 0.5 && pad.right_y > 0.5, "{pad:?}");
        assert!((pad.right_x - pad.right_y).abs() < 1e-6, "{pad:?}");
        assert_eq!(pad.right_trigger, 1.0);

        // Let go (Esc, Alt+Tab): nothing plays, counts or not.
        host.lock().unwrap().mouse = MouseInput {
            captured: false,
            dx: 400,
            ..Default::default()
        };
        hub.poll();
        assert_eq!(stand_in(hub), PadState::default());
    }

    #[test]
    fn keyboard_and_mouse_play_on_top_of_a_pad() {
        let (hub, host) = fake();
        {
            let mut host = host.lock().unwrap();
            host.pads[1] = Some(PadState {
                buttons: A,
                left_x: 0.3,
                right_y: -0.9,
                left_trigger: 0.4,
                ..Default::default()
            });
            host.pads[3] = Some(PadState::default());
            host.keys = vec![key("D", b'D'), key("E", b'E')];
            host.mouse = MouseInput {
                captured: true,
                buttons: bindings::MOUSE_RIGHT,
                ..Default::default()
            };
        }
        hub.poll();
        // The stand-in is gone; the first pad has both, as far as either
        // pushes; the other pad is its own.
        assert_eq!(
            current(hub, GAMEPAD_KIND, FALLBACK),
            Err(DEVICE_DISCONNECTED)
        );
        let r = current(hub, GAMEPAD_KIND, 1).unwrap();
        let state = pad(r);
        release(r);
        assert_eq!(state.buttons, A | RIGHT_SHOULDER);
        assert_eq!(state.left_trigger, 1.0);
        assert_eq!((state.left_x, state.left_y), (1.0, 0.0));
        assert_eq!((state.right_x, state.right_y), (0.0, -0.9));
        let r = current(hub, GAMEPAD_KIND, 3).unwrap();
        assert_eq!(pad(r), PadState::default());
        release(r);
    }

    /// Polls `kbm` every 8 ms from `from`, the mouse captured with these
    /// counts each time; the last pad.
    fn moving(kbm: &mut Kbm, from: u64, polls: u64, dx: i32, dy: i32, wheel: i32) -> PadState {
        let mouse = MouseInput {
            captured: true,
            dx,
            dy,
            wheel,
            buttons: 0,
        };
        (1..=polls).fold(PadState::default(), |_, i| {
            kbm.pad(&[], &mouse, from + i * 8000)
        })
    }

    #[test]
    fn mouse_counts_become_a_steady_right_stick() {
        let mut kbm = Kbm::new(&Tuning::default());
        // 10 counts left every 8 ms: 1250 a second, a quarter of full speed.
        let first = moving(&mut kbm, 1_000_000, 10, -10, 0, 0);
        let again = moving(&mut kbm, 1_080_000, 10, -10, 0, 0);
        assert_eq!(first, again);
        assert!((first.right_x + 0.4).abs() < 1e-4, "{first:?}");
        assert_eq!(first.right_y, 0.0);
        // Stopping settles it at rest.
        let still = moving(&mut kbm, 1_160_000, 7, 0, 0, 0);
        assert_eq!(still, PadState::default());

        // A finer curve, inverted: the same speed deflects less, and up is
        // down.
        let mut settings = Settings {
            mouse_curve: Some(2.0),
            invert_y: Some(true),
            ..Default::default()
        };
        let mut kbm = Kbm::new(&Tuning::new(&settings).unwrap());
        let pad = moving(&mut kbm, 1_000_000, 10, 0, -10, 0);
        assert!((pad.right_y + 0.25).abs() < 1e-4, "{pad:?}");
        settings.mouse_sensitivity = Some(4.0);
        let mut kbm = Kbm::new(&Tuning::new(&settings).unwrap());
        let pad = moving(&mut kbm, 1_000_000, 10, 0, -10, 0);
        assert!((pad.right_y + 1.0).abs() < 1e-4, "{pad:?}");
    }

    #[test]
    fn wheel_notches_press_the_shoulders_and_focus_loss_forgets_everything() {
        let mut kbm = Kbm::new(&Tuning::default());
        let scrolled = |wheel| MouseInput {
            captured: true,
            wheel,
            ..Default::default()
        };
        // Three notches up at once: three presses of the right shoulder.
        let mut presses = 0;
        let mut was = 0;
        for i in 0..60 {
            let wheel = if i == 0 { 3 * mouse::NOTCH } else { 0 };
            let buttons = kbm.pad(&[], &scrolled(wheel), 1_000_000 + i * 8000).buttons;
            if buttons == RIGHT_SHOULDER && was == 0 {
                presses += 1;
            }
            assert!(buttons == 0 || buttons == RIGHT_SHOULDER);
            was = buttons;
        }
        assert_eq!(presses, 3);

        // Moving fast with notches queued, then Alt+Tab ...
        let pad = moving(&mut kbm, 2_000_000, 10, 200, 0, -5 * mouse::NOTCH);
        assert!(pad.right_x > 0.9);
        let free = MouseInput::default();
        assert_eq!(kbm.pad(&[], &free, 2_100_000), PadState::default());
        // ... and back: no motion and no notch left over.
        for i in 1..30 {
            assert_eq!(
                kbm.pad(&[], &scrolled(0), 3_000_000 + i * 8000),
                PadState::default()
            );
        }
    }

    #[test]
    fn settings_override_the_defaults_and_refuse_nonsense() {
        let settings = Settings {
            keyboard_mouse: Some(false),
            mouse_sensitivity: Some(2.5),
            mouse_dead_zone: Some(0.0),
            bindings: [("a".to_string(), vec!["MouseForward".to_string()])].into(),
            ..Default::default()
        };
        let tuning = Tuning::new(&settings).unwrap();
        assert!(!tuning.keyboard_mouse);
        assert_eq!(
            tuning.curve,
            Curve {
                sensitivity: 2.5,
                dead_zone: 0.0,
                ..Curve::default()
            }
        );
        let a = tuning
            .bindings
            .pad(|i| i == Input::Mouse(bindings::MOUSE_FORWARD));
        assert_eq!(a.buttons, A);
        assert_eq!(Tuning::new(&Settings::default()), Ok(Tuning::default()));

        let refused = |settings: Settings| Tuning::new(&settings).unwrap_err();
        let e = refused(Settings {
            mouse_sensitivity: Some(0.0),
            ..Default::default()
        });
        assert!(e.contains("mouse_sensitivity = 0"), "{e}");
        let e = refused(Settings {
            mouse_curve: Some(f32::NAN),
            ..Default::default()
        });
        assert!(e.contains("mouse_curve"), "{e}");
        let e = refused(Settings {
            mouse_dead_zone: Some(1.0),
            ..Default::default()
        });
        assert!(e.contains("mouse_dead_zone"), "{e}");
        let e = refused(Settings {
            bindings: [("shoot".to_string(), vec![])].into(),
            ..Default::default()
        });
        assert!(
            e.starts_with("bindings: unknown pad control `shoot`"),
            "{e}"
        );
    }

    #[test]
    fn async_enumeration_waits_for_the_poll_and_unregistering_stops_callbacks() {
        let (hub, host) = fake();
        let log = Log::default();
        let token = register(hub, GAMEPAD_KIND, ASYNC_ENUMERATION, &log);
        assert!(log.lock().unwrap().is_empty());
        hub.poll();
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(unregister(&hub.object, token, 5000));
        host.lock().unwrap().pads[0] = Some(PadState::default());
        hub.poll();
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn unregistering_waits_for_a_running_callback() {
        use std::sync::atomic::AtomicBool;
        static INSIDE: AtomicBool = AtomicBool::new(false);
        static LEAVE: AtomicBool = AtomicBool::new(false);
        static FROM_INSIDE: AtomicU32 = AtomicU32::new(0);
        extern "system" fn slow(
            token: u64,
            context: *mut c_void,
            _: *const Device,
            _: u64,
            _: u32,
            _: u32,
        ) {
            // From inside its own callback, unregistering fails at once.
            let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(context, 0x68);
            if !unregister(context.cast(), token, 0) {
                FROM_INSIDE.fetch_add(1, Ordering::SeqCst);
            }
            INSIDE.store(true, Ordering::SeqCst);
            while !LEAVE.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let (hub, host) = fake();
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let mut token = 0;
        let context = (&raw const hub.object).cast_mut().cast();
        assert_eq!(
            register(
                &hub.object,
                std::ptr::null(),
                GAMEPAD_KIND,
                1,
                0,
                context,
                Some(slow),
                &mut token
            ),
            S_OK
        );
        host.lock().unwrap().pads[1] = Some(PadState::default());
        let poller = std::thread::spawn(|| hub.poll());
        while !INSIDE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(
            !unregister(&hub.object, token, 5000),
            "times out while it runs"
        );
        let leave = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            LEAVE.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        assert!(unregister(&hub.object, token, u64::MAX), "waits for it");
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert_eq!(FROM_INSIDE.load(Ordering::SeqCst), 1);
        poller.join().unwrap();
        leave.join().unwrap();
    }

    #[test]
    fn devices_are_found_by_id_and_rumble_reaches_the_host() {
        let (hub, host) = fake();
        host.lock().unwrap().pads[2] = Some(PadState::default());
        hub.poll();
        let device = &hub.devices[2] as *const Device;
        let info = slot::<extern "system" fn(*const Device) -> *const u8, _>(device, 0x18);
        // SAFETY: the info has the id at +0x20.
        let id = unsafe { *(info(device).add(0x20) as *const [u8; 32]) };
        assert_ne!(id, [0; 32]);

        type Find = extern "system" fn(This, *const [u8; 32], Out) -> HRESULT;
        let find = slot::<Find, _>(&hub.object, 0x80);
        let mut out = std::ptr::null();
        assert_eq!(find(&hub.object, &id, &mut out), S_OK);
        assert_eq!(out, device.cast());
        assert_eq!(find(&hub.object, &[1; 32], &mut out), DEVICE_NOT_FOUND);
        assert!(out.is_null());

        // All four motors reach the host, within 0 to 1; the keyboard's pad
        // has none.
        type Rumble = extern "system" fn(*const Device, *const [f32; 4]);
        let rumble = slot::<Rumble, _>(device, 0x50);
        rumble(device, &[0.5, 1.0, 0.25, 0.75]);
        rumble(&hub.devices[FALLBACK], &[1.0; 4]);
        rumble(device, &[2.0, -1.0, f32::NAN, 0.0]);
        rumble(device, std::ptr::null());
        assert_eq!(
            host.lock().unwrap().rumble,
            [
                (2, [0.5, 1.0, 0.25, 0.75]),
                (2, [1.0, 0.0, 0.0, 0.0]),
                (2, [0.0; 4])
            ]
        );
        let at = std::mem::offset_of!(DeviceInfo, rumble_motors);
        // SAFETY: as above, at the motors' offset.
        let motors = unsafe { *(info(device).add(at) as *const u32) };
        assert_eq!(motors, 0xF);
    }
}
