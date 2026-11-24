//! The title's window as the mouse sees it. Raw Input brings the mouse's
//! own counts there (the cursor stops at the screen's edges, the counts do
//! not), and while the mouse plays the cursor is held at the window's
//! centre, hidden; `mouse::Capture` says when. Messages come from the
//! window procedure `lifecycle` puts in front of the title's, on the
//! title's window thread; the poll thread takes what they gathered.

use std::sync::{Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;

use super::MouseInput;
use super::bindings::{MOUSE_BACK, MOUSE_FORWARD, MOUSE_LEFT, MOUSE_MIDDLE, MOUSE_RIGHT};
use super::mouse::{Capture, Event};

struct Window {
    /// The title's window the capture is about: the last one active, or 0.
    hwnd: usize,
    /// A title window made active while another program was in front: it
    /// takes the mouse once it comes to the front.
    waiting: usize,
    /// The window Raw Input reports the mouse to (one per process).
    target: usize,
    capture: Capture,
    /// Counts (x right, y down) and wheel movement since the last poll.
    dx: i32,
    dy: i32,
    wheel: i32,
}

static WINDOW: Mutex<Window> = Mutex::new(Window {
    hwnd: 0,
    waiting: 0,
    target: 0,
    capture: Capture::new(),
    dx: 0,
    dy: 0,
    wheel: 0,
});

fn window() -> MutexGuard<'static, Window> {
    super::lock(&WINDOW)
}

fn foreground() -> usize {
    // SAFETY: a plain query.
    unsafe { wm::GetForegroundWindow() as usize }
}

/// The top-level window `hwnd` is in: clicks and keys may go to a child.
fn root(hwnd: HWND) -> HWND {
    // SAFETY: a plain query; a window that is gone gives null.
    unsafe { wm::GetAncestor(hwnd, wm::GA_ROOT) }
}

/// A title window was created: one already in front (created visible) was
/// activated before our procedure could see it.
pub(crate) fn created(hwnd: HWND) {
    if super::tuning().keyboard_mouse && foreground() == hwnd as usize {
        listen(hwnd);
        event(hwnd, Event::Active(true));
    }
}

/// A message to one of the title's windows, before the title sees it;
/// `Some` answers it here instead.
pub(crate) fn message(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if !super::tuning().keyboard_mouse {
        return None;
    }
    match message {
        wm::WM_ACTIVATE => {
            // Low word: how it became active, or not; high word: minimised.
            // A window active on its thread but not in front (another
            // program is) waits before it takes the cursor.
            let active =
                wparam & 0xFFFF != wm::WA_INACTIVE as usize && (wparam >> 16) & 0xFFFF == 0;
            let front = foreground() == hwnd as usize;
            if active {
                listen(hwnd);
            }
            {
                let mut w = window();
                if active && !front {
                    w.waiting = hwnd as usize;
                } else if w.waiting == hwnd as usize {
                    w.waiting = 0;
                }
            }
            event(hwnd, Event::Active(active && front));
        }
        wm::WM_ENTERSIZEMOVE => event(hwnd, Event::Moving(true)),
        wm::WM_EXITSIZEMOVE => event(hwnd, Event::Moving(false)),
        // The centre moved.
        wm::WM_SIZE | wm::WM_MOVE => {
            let held = {
                let w = window();
                w.hwnd == hwnd as usize && w.capture.held()
            };
            if held {
                hold(hwnd, true);
            }
        }
        wm::WM_LBUTTONDOWN | wm::WM_RBUTTONDOWN | wm::WM_MBUTTONDOWN | wm::WM_XBUTTONDOWN => {
            let top = root(hwnd);
            if foreground() == top as usize {
                listen(top);
                event(top, Event::Active(true));
                event(top, Event::Click);
            }
        }
        // The first press, not the repeats.
        wm::WM_KEYDOWN if wparam == usize::from(VK_ESCAPE) && lparam & (1 << 30) == 0 => {
            event(root(hwnd), Event::Escape);
        }
        wm::WM_INPUT => gather(hwnd, lparam),
        wm::WM_SETCURSOR if lparam & 0xFFFF == wm::HTCLIENT as isize => {
            let top = root(hwnd);
            let w = window();
            if w.hwnd == top as usize && w.capture.held() {
                // SAFETY: hides the cursor over our window.
                unsafe { wm::SetCursor(std::ptr::null_mut()) };
                return Some(1);
            }
        }
        wm::WM_DESTROY => {
            let ours = {
                let mut w = window();
                if w.waiting == hwnd as usize {
                    w.waiting = 0;
                }
                w.hwnd == hwnd as usize
            };
            if ours {
                event(hwnd, Event::Active(false));
                window().hwnd = 0;
            }
        }
        _ => {}
    }
    None
}

/// Applies `event` from `hwnd`; holds or frees the cursor when that
/// changes. Events from a window other than the active one are stale.
fn event(hwnd: HWND, event: Event) {
    let held = {
        let mut w = window();
        if event == Event::Active(true) {
            w.hwnd = hwnd as usize;
        } else if w.hwnd != hwnd as usize {
            return;
        }
        if !w.capture.on(event) {
            return;
        }
        (w.dx, w.dy, w.wheel) = (0, 0, 0);
        w.capture.held()
    };
    tracing::debug!(?event, held, "mouse capture");
    hold(hwnd, held);
    if !held {
        // SAFETY: the arrow over our window until the window's own cursor
        // takes over; another window sets its own.
        unsafe { wm::SetCursor(wm::LoadCursorW(std::ptr::null_mut(), wm::IDC_ARROW)) };
    }
}

/// Holds the cursor at `hwnd`'s centre, hidden, or lets it go.
fn hold(hwnd: HWND, on: bool) {
    if !on {
        // SAFETY: frees the cursor.
        unsafe { wm::ClipCursor(std::ptr::null()) };
        return;
    }
    let mut info = wm::WINDOWINFO {
        cbSize: size_of::<wm::WINDOWINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: room for the info; its client area is in screen pixels.
    if unsafe { wm::GetWindowInfo(hwnd, &mut info) } == 0 {
        return;
    }
    let area = info.rcClient;
    let (x, y) = ((area.left + area.right) / 2, (area.top + area.bottom) / 2);
    let at = RECT {
        left: x,
        top: y,
        right: x + 1,
        bottom: y + 1,
    };
    // SAFETY: a one-pixel rectangle; the cursor over our window, hidden.
    unsafe {
        wm::ClipCursor(&at);
        wm::SetCursor(std::ptr::null_mut());
    }
}

/// Raw Input from the mouse goes to `hwnd` from now on.
fn listen(hwnd: HWND) {
    let before = std::mem::replace(&mut window().target, hwnd as usize);
    if before == hwnd as usize {
        return;
    }
    // Generic desktop page, mouse; only while our window is in front.
    let mouse = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 2,
        dwFlags: 0,
        hwndTarget: hwnd,
    };
    // SAFETY: one device description.
    if unsafe { RegisterRawInputDevices(&mouse, 1, size_of::<RAWINPUTDEVICE>() as u32) } != 0 {
        tracing::info!("the mouse plays as the right stick");
    } else {
        // SAFETY: a plain query.
        let error = unsafe { GetLastError() };
        tracing::warn!(
            error,
            "no Raw Input: the mouse does not move the right stick"
        );
    }
}

/// Adds a Raw Input report's movement and wheel, while the mouse plays.
fn gather(hwnd: HWND, lparam: LPARAM) {
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    // SAFETY: room for a mouse report, which is all we registered for.
    let read = unsafe {
        GetRawInputData(
            lparam as HRAWINPUT,
            RID_INPUT,
            (&raw mut raw).cast(),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if read == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE {
        return;
    }
    // SAFETY: a mouse report, as its header says.
    let (mouse, buttons) = unsafe { (raw.data.mouse, raw.data.mouse.Anonymous.Anonymous) };
    let mut w = window();
    if w.hwnd != hwnd as usize || !w.capture.held() {
        return;
    }
    // Absolute positions (tablets, remote desktops) are no movement.
    if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 {
        w.dx = w.dx.saturating_add(mouse.lLastX);
        w.dy = w.dy.saturating_add(mouse.lLastY);
    }
    if u32::from(buttons.usButtonFlags) & wm::RI_MOUSE_WHEEL != 0 {
        w.wheel = w
            .wheel
            .saturating_add(i32::from(buttons.usButtonData as i16));
    }
}

/// The mouse buttons down now.
fn buttons() -> u8 {
    [
        (VK_LBUTTON, MOUSE_LEFT),
        (VK_RBUTTON, MOUSE_RIGHT),
        (VK_MBUTTON, MOUSE_MIDDLE),
        (VK_XBUTTON1, MOUSE_BACK),
        (VK_XBUTTON2, MOUSE_FORWARD),
    ]
    .iter()
    // SAFETY: plain queries.
    .filter(|(key, _)| unsafe { GetAsyncKeyState(i32::from(*key)) } < 0)
    .fold(0, |all, (_, button)| all | button)
}

/// What the mouse did since the last call (the poll's).
pub(super) fn take() -> MouseInput {
    let mut w = window();
    let front = foreground();
    // Out of the front without a message (another desktop, a secure
    // prompt): the cursor is free.
    if w.capture.held() && front != w.hwnd {
        w.capture.on(Event::Active(false));
        hold(std::ptr::null_mut(), false);
    }
    // Made active behind another program, now in front.
    if w.waiting != 0 && w.waiting == front {
        w.hwnd = std::mem::take(&mut w.waiting);
        if w.capture.on(Event::Active(true)) {
            (w.dx, w.dy, w.wheel) = (0, 0, 0);
            // The cursor hides at its next move (WM_SETCURSOR).
            hold(front as HWND, true);
        }
    }
    let down = buttons();
    MouseInput {
        captured: w.capture.held(),
        buttons: w.capture.buttons(down),
        dx: std::mem::take(&mut w.dx),
        dy: std::mem::take(&mut w.dy),
        wheel: std::mem::take(&mut w.wheel),
    }
}

/// The process ends: a held cursor would stay held.
pub(crate) fn release() {
    if window().capture.held() {
        hold(std::ptr::null_mut(), false);
    }
}
