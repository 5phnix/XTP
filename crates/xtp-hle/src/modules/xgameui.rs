//! System dialogs a title opens (XGameUi). A profile card has nothing to
//! show and closes at once. Text entry, message dialogs and error dialogs
//! open a small host dialog on a thread of their own and complete the async
//! call with the user's answer, or as cancelled (`E_ABORT`) when the dialog
//! cannot open. When `XTP_DIALOGS=auto` (or [`set_auto_dismiss`] in
//! unattended/autopilot runs) is enabled, message dialogs complete immediately
//! with `cancel_button` (`S_OK`) and error dialogs complete immediately with
//! `S_OK` without opening a window.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use super::xasync::{self, AsyncBlock, E_ABORT};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};

pub const PROVIDER: Guid = Guid::new(
    0xdfcd_4649,
    0x4ff8,
    0x4043,
    [0xba, 0x07, 0x35, 0xd6, 0x07, 0xdf, 0x98, 0xb0],
);
/// As BTP 6 (2026) asks for it; the slots it calls (0x38,
/// 0x40, 0x88-0x98) take the public API's arguments in the 2020 order.
pub const IID_2026: Guid = Guid::new(
    0xeaf6_69df,
    0x5542,
    0x4590,
    [0x99, 0xa3, 0x8d, 0xc0, 0x61, 0xf8, 0x37, 0xcc],
);

pub static OBJECT: Object = Object::new("game ui", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID_2026, &OBJECT)];

/// Environment variable that auto-dismisses modal dialogs when set to `auto`.
pub const DIALOGS_ENV: &str = "XTP_DIALOGS";

/// `E_INVALIDARG` (`0x80070057`).
const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;

/// `XGameUiMessageDialogButton` values in `XGameUI.h`.
pub const BUTTON_FIRST: u32 = 0;
pub const BUTTON_SECOND: u32 = 1;
pub const BUTTON_THIRD: u32 = 2;

static AUTO_DISMISS: AtomicBool = AtomicBool::new(false);

/// Enable or disable unattended auto-dismissal of message and error dialogs
/// (used by `--autopilot` runs so modal dialogs never stall the session).
pub fn set_auto_dismiss(enabled: bool) {
    AUTO_DISMISS.store(enabled, Ordering::Relaxed);
}

fn auto_dismiss_enabled() -> bool {
    AUTO_DISMISS.load(Ordering::Relaxed)
        || std::env::var(DIALOGS_ENV).is_ok_and(|v| v.trim().eq_ignore_ascii_case("auto"))
}

type This = *const Object;

static MESSAGE: u8 = 0;
static PROFILE: u8 = 0;
static ERROR_DIALOG: u8 = 0;
static TEXT: u8 = 0;

/// Request passed to the host message-dialog backend.
///
/// Each entry in `buttons` carries its original `XGameUiMessageDialogButton`
/// enum value (`0`, `1`, or `2`) paired with its label so gaps (e.g. a null
/// second button) never shift the reported button index.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageDialogRequest {
    title: String,
    content: String,
    buttons: Vec<(u32, String)>,
    default_button: u32,
    cancel_button: u32,
}

fn escape_mnemonic(label: &str) -> String {
    label.replace('&', "&&")
}

extern "system" fn message_dialog_async(
    _: This,
    block: *mut AsyncBlock,
    title: *const c_char,
    content: *const c_char,
    first_button: *const c_char,
    second_button: *const c_char,
    third_button: *const c_char,
    default_button: u32,
    cancel_button: u32,
) -> HRESULT {
    let title = text(title).unwrap_or_default();
    let content = text(content).unwrap_or_default();
    let mut buttons = Vec::new();
    for (idx, label) in [
        (BUTTON_FIRST, first_button),
        (BUTTON_SECOND, second_button),
        (BUTTON_THIRD, third_button),
    ] {
        if let Some(s) = text(label).filter(|s| !s.is_empty()) {
            buttons.push((idx, s));
        }
    }
    if buttons.is_empty() {
        buttons.push((BUTTON_FIRST, "OK".to_owned()));
    }
    if auto_dismiss_enabled() {
        tracing::warn!(
            title,
            content,
            cancel_button,
            "message dialog: auto-dismissing with cancel_button"
        );
        return xasync::pending_with(block, (&raw const MESSAGE).cast(), move || {
            Ok(cancel_button.to_le_bytes().to_vec())
        });
    }
    tracing::info!(
        title,
        content,
        buttons = buttons.len(),
        default_button,
        cancel_button,
        "message dialog"
    );
    let req = MessageDialogRequest {
        title,
        content,
        buttons,
        default_button,
        cancel_button,
    };
    xasync::pending_with(
        block,
        (&raw const MESSAGE).cast(),
        move || match show_message_dialog(&req) {
            Some(button) => {
                tracing::info!(button, "message dialog: button chosen");
                Ok(button.to_le_bytes().to_vec())
            }
            None => {
                tracing::info!("message dialog: could not open");
                Err(E_ABORT)
            }
        },
    )
}

extern "system" fn message_dialog_result(
    _: This,
    block: *mut AsyncBlock,
    result_button: *mut u32,
) -> HRESULT {
    if result_button.is_null() {
        return E_INVALIDARG;
    }
    let mut raw = [0u8; 4];
    let mut written = 0usize;
    let hr = xasync::XAsyncGetResult(
        block,
        (&raw const MESSAGE).cast(),
        raw.len(),
        raw.as_mut_ptr().cast(),
        &mut written,
    );
    if hr < 0 {
        return hr;
    }
    put(result_button, u32::from_le_bytes(raw))
}

extern "system" fn error_dialog_async(
    _: This,
    block: *mut AsyncBlock,
    error_code: HRESULT,
    context: *const c_char,
) -> HRESULT {
    let context = text(context).unwrap_or_default();
    if auto_dismiss_enabled() {
        tracing::warn!(
            error_code = format_args!("{error_code:#010x}"),
            context,
            "error dialog: auto-dismissing"
        );
        return xasync::finished(block, (&raw const ERROR_DIALOG).cast(), S_OK);
    }
    tracing::warn!(
        error_code = format_args!("{error_code:#010x}"),
        context,
        "error dialog"
    );
    xasync::pending_with(block, (&raw const ERROR_DIALOG).cast(), move || {
        if show_error_dialog(error_code, &context) {
            tracing::info!("error dialog: dismissed");
            Ok(Vec::new())
        } else {
            tracing::info!("error dialog: could not open");
            Err(E_ABORT)
        }
    })
}

extern "system" fn error_dialog_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(
        block,
        (&raw const ERROR_DIALOG).cast(),
        0,
        none,
        none.cast(),
    )
}

extern "system" fn profile_card_async(
    _: This,
    block: *mut AsyncBlock,
    _user: *const c_void,
    xuid: u64,
) -> HRESULT {
    tracing::info!(xuid, "profile card: nothing to show");
    xasync::finished(block, (&raw const PROFILE).cast(), S_OK)
}
extern "system" fn profile_card_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const PROFILE).cast(), 0, none, none.cast())
}

/// BTP 6 treats `E_ABORT` as cancelled and goes on; any
/// other failure as an error.
extern "system" fn text_entry_async(
    _: This,
    block: *mut AsyncBlock,
    title: *const c_char,
    description: *const c_char,
    default: *const c_char,
    _scope: u32,
    max_length: u32,
) -> HRESULT {
    let [title, description, default] =
        [title, description, default].map(|t| text(t).unwrap_or_default());
    tracing::info!(title, description, max_length, "text entry");
    xasync::pending_with(block, (&raw const TEXT).cast(), move || {
        match ask(&title, &description, &default, max_length) {
            Some(typed) => {
                tracing::info!(length = typed.chars().count(), "text entry: done");
                let mut bytes = limit(&typed, max_length).into_bytes();
                bytes.push(0);
                Ok(bytes)
            }
            None => {
                tracing::info!("text entry: cancelled");
                Err(E_ABORT)
            }
        }
    })
}
/// Bytes of the result, its NUL included.
extern "system" fn text_entry_size(_: This, block: *mut AsyncBlock, out: *mut u32) -> HRESULT {
    let mut size = 0;
    let hr = xasync::XAsyncGetResultSize(block, &mut size);
    if hr < 0 {
        return hr;
    }
    put(out, size as u32)
}
extern "system" fn text_entry_result(
    _: This,
    block: *mut AsyncBlock,
    size: u32,
    buffer: *mut c_char,
    used: *mut u32,
) -> HRESULT {
    let mut written = 0;
    let identity = (&raw const TEXT).cast();
    let hr = xasync::XAsyncGetResult(block, identity, size as usize, buffer.cast(), &mut written);
    if hr < 0 || used.is_null() {
        return hr;
    }
    put(used, written as u32)
}

/// At most `max` characters (0: no limit).
fn limit(typed: &str, max: u32) -> String {
    match max {
        0 => typed.to_owned(),
        max => typed.chars().take(max as usize).collect(),
    }
}

/// Asks the user in a modal host dialog; `None` if dismissed.
#[cfg(not(test))]
fn ask(title: &str, description: &str, default: &str, max: u32) -> Option<String> {
    dialog::ask(title, description, default, max)
}

/// Tests answer for the user: "typed" for a title starting with "Type",
/// cancelled otherwise.
#[cfg(test)]
fn ask(title: &str, _: &str, default: &str, _: u32) -> Option<String> {
    title.starts_with("Type").then(|| format!("{default}typed"))
}

/// Shows a modal host message dialog; returns the chosen button enum value or
/// `None` if no window can be shown.
#[cfg(not(test))]
fn show_message_dialog(req: &MessageDialogRequest) -> Option<u32> {
    dialog::message(req)
}

/// Shows a modal host error dialog; returns `true` when dismissed by the user,
/// or `false` if no window could be shown.
#[cfg(not(test))]
fn show_error_dialog(error_code: HRESULT, context: &str) -> bool {
    dialog::error(error_code, context)
}

#[cfg(test)]
type MessageDialogHook = Box<dyn Fn(&MessageDialogRequest) -> Option<u32> + Send + Sync>;

#[cfg(test)]
type ErrorDialogHook = Box<dyn Fn(HRESULT, &str) -> bool + Send + Sync>;

#[cfg(test)]
static MESSAGE_HOOK: std::sync::Mutex<Option<MessageDialogHook>> = std::sync::Mutex::new(None);

#[cfg(test)]
static ERROR_HOOK: std::sync::Mutex<Option<ErrorDialogHook>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn show_message_dialog(req: &MessageDialogRequest) -> Option<u32> {
    let guard = MESSAGE_HOOK.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().and_then(|f| f(req))
}

#[cfg(test)]
fn show_error_dialog(error_code: HRESULT, context: &str) -> bool {
    let guard = ERROR_HOOK.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().is_some_and(|f| f(error_code, context))
}

mod dialog {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows_sys::Win32::UI::WindowsAndMessaging::{self as wm, DLGTEMPLATE};

    const EDIT: i32 = 100;
    const BTN_BASE: i32 = 200;
    // Static control style: do not interpret '&' as a mnemonic prefix.
    const SS_NOPREFIX: u32 = 0x0080;
    // Edit control messages (the controls headers).
    const EM_SETSEL: u32 = 0xb1;
    const EM_LIMITTEXT: u32 = 0xc5;

    struct MessageState {
        chosen: Option<u32>,
        button_ids: Vec<u32>,
        default_button: u32,
        cancel_button: u32,
    }

    /// Shows a modal message dialog with up to three buttons from an in-memory
    /// template. Esc and the close box report `req.cancel_button`. Returns
    /// `None` only if the host dialog could not be created.
    #[allow(dead_code)] // unused in test builds
    pub(super) fn message(req: &super::MessageDialogRequest) -> Option<u32> {
        let mut t: Vec<u16> = Vec::new();
        fn dword(t: &mut Vec<u16>, v: u32) {
            t.extend([v as u16, (v >> 16) as u16]);
        }
        fn text(t: &mut Vec<u16>, s: &str) {
            t.extend(s.encode_utf16().filter(|&c| c != 0));
            t.push(0);
        }
        let btn_count = req.buttons.len().min(3) as u16;
        let width: u16 = 280;
        let height: u16 = 104;
        let style = wm::WS_POPUP | wm::WS_CAPTION | wm::WS_SYSMENU;
        let flags = (wm::DS_MODALFRAME | wm::DS_CENTER | wm::DS_SETFOREGROUND) as u32;
        dword(&mut t, style | flags);
        dword(&mut t, wm::WS_EX_TOPMOST);
        t.extend([1 + btn_count, 0, 0, width, height, 0, 0]);
        text(&mut t, &req.title);
        let mut item = |style: u32, rect: [u16; 4], id: u16, class: u16, caption: &str| {
            if t.len() % 2 == 1 {
                t.push(0);
            }
            dword(&mut t, wm::WS_CHILD | wm::WS_VISIBLE | style);
            dword(&mut t, 0);
            t.extend(rect);
            t.extend([id, 0xffff, class]);
            text(&mut t, caption);
            t.push(0);
        };
        let (button_cls, label_cls) = (0x80, 0x82);
        item(
            SS_NOPREFIX,
            [8, 8, width - 16, 66],
            0xffff,
            label_cls,
            &req.content,
        );
        let btn_w: u16 = 72;
        let gap: u16 = 6;
        let total_w = btn_count * btn_w + btn_count.saturating_sub(1) * gap;
        let start_x = width.saturating_sub(8 + total_w);
        for (pos, &(btn_value, ref label)) in req.buttons.iter().take(3).enumerate() {
            let is_default = btn_value == req.default_button;
            let btn_style = wm::WS_TABSTOP
                | if is_default {
                    wm::BS_DEFPUSHBUTTON as u32
                } else {
                    wm::BS_PUSHBUTTON as u32
                };
            let x = start_x + (pos as u16) * (btn_w + gap);
            let caption = super::escape_mnemonic(label);
            item(
                btn_style,
                [x, height - 22, btn_w, 14],
                (BTN_BASE + btn_value as i32) as u16,
                button_cls,
                &caption,
            );
        }
        let template: Vec<u32> = t
            .chunks(2)
            .map(|c| u32::from(c[0]) | u32::from(c.get(1).copied().unwrap_or(0)) << 16)
            .collect();
        let mut state = MessageState {
            chosen: None,
            button_ids: req.buttons.iter().take(3).map(|&(id, _)| id).collect(),
            default_button: req.default_button,
            cancel_button: req.cancel_button,
        };
        // SAFETY: `template` is a valid in-memory dialog template and `state`
        // outlives the synchronous `DialogBoxIndirectParamW` call.
        let rc = unsafe {
            wm::DialogBoxIndirectParamW(
                GetModuleHandleW(std::ptr::null()),
                template.as_ptr().cast::<DLGTEMPLATE>(),
                std::ptr::null_mut(),
                Some(message_proc),
                (&raw mut state) as LPARAM,
            )
        };
        if rc <= 0 {
            return None;
        }
        state.chosen
    }

    extern "system" fn message_proc(
        dialog: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> isize {
        // SAFETY: `dialog` belongs to `message` above; `GWLP_USERDATA` holds
        // the `&mut MessageState` stored during `WM_INITDIALOG`.
        unsafe {
            match message {
                wm::WM_INITDIALOG => {
                    wm::SetWindowLongPtrW(dialog, wm::GWLP_USERDATA, lparam);
                    let state = &*(lparam as *const MessageState);
                    let target = if state.button_ids.contains(&state.default_button) {
                        state.default_button
                    } else {
                        state.button_ids.first().copied().unwrap_or(0)
                    };
                    let btn = wm::GetDlgItem(dialog, BTN_BASE + target as i32);
                    if !btn.is_null() {
                        SetFocus(btn);
                        0 // focus explicitly set on the default button
                    } else {
                        1
                    }
                }
                wm::WM_COMMAND => {
                    let state = &mut *(wm::GetWindowLongPtrW(dialog, wm::GWLP_USERDATA)
                        as *mut MessageState);
                    let id = (wparam & 0xffff) as i32;
                    if id == wm::IDCANCEL {
                        state.chosen = Some(state.cancel_button);
                        wm::EndDialog(dialog, 1);
                        return 1;
                    }
                    if id >= BTN_BASE {
                        let value = (id - BTN_BASE) as u32;
                        if state.button_ids.contains(&value) {
                            state.chosen = Some(value);
                            wm::EndDialog(dialog, 1);
                            return 1;
                        }
                    }
                    0
                }
                wm::WM_CLOSE => {
                    let state = &mut *(wm::GetWindowLongPtrW(dialog, wm::GWLP_USERDATA)
                        as *mut MessageState);
                    state.chosen = Some(state.cancel_button);
                    wm::EndDialog(dialog, 1);
                    1
                }
                _ => 0,
            }
        }
    }

    /// Shows a modal host error dialog with `MessageBoxW`. Returns `false` if
    /// no window could be shown (`MessageBoxW` returns 0).
    #[allow(dead_code)] // unused in test builds
    pub(super) fn error(error_code: super::HRESULT, context: &str) -> bool {
        let title: Vec<u16> = "Error\0".encode_utf16().collect();
        let body = if context.is_empty() {
            format!("Error {error_code:#010x}")
        } else {
            format!("{context}\n\nError {error_code:#010x}")
        };
        let wide_body: Vec<u16> = body.encode_utf16().chain(Some(0)).collect();
        // SAFETY: both strings are NUL-terminated UTF-16 slices.
        let rc = unsafe {
            wm::MessageBoxW(
                std::ptr::null_mut(),
                wide_body.as_ptr(),
                title.as_ptr(),
                wm::MB_OK | wm::MB_ICONERROR | wm::MB_TOPMOST,
            )
        };
        rc != 0
    }

    /// The dialog's state, behind its user data while it runs.
    struct Entry {
        max: u32,
        typed: Option<String>,
    }

    /// The description, an edit box with the default text, OK and Cancel,
    /// from an in-memory template (dialog units, the system font); Enter
    /// accepts, Esc cancels.
    #[allow(dead_code)] // unused in test builds
    pub(super) fn ask(title: &str, description: &str, default: &str, max: u32) -> Option<String> {
        let mut t: Vec<u16> = Vec::new();
        fn dword(t: &mut Vec<u16>, v: u32) {
            t.extend([v as u16, (v >> 16) as u16]);
        }
        fn text(t: &mut Vec<u16>, s: &str) {
            t.extend(s.encode_utf16().filter(|&c| c != 0));
            t.push(0);
        }
        let style = wm::WS_POPUP | wm::WS_CAPTION | wm::WS_SYSMENU;
        let flags = (wm::DS_MODALFRAME | wm::DS_CENTER | wm::DS_SETFOREGROUND) as u32;
        dword(&mut t, style | flags);
        dword(&mut t, wm::WS_EX_TOPMOST);
        t.extend([4, 0, 0, 240, 62, 0, 0]); // items, x, y, cx, cy, no menu, default class
        text(&mut t, title);
        let mut item = |style: u32, rect: [u16; 4], id: u16, class: u16, caption: &str| {
            if t.len() % 2 == 1 {
                t.push(0); // items start on a DWORD
            }
            dword(&mut t, wm::WS_CHILD | wm::WS_VISIBLE | style);
            dword(&mut t, 0);
            t.extend(rect);
            t.extend([id, 0xffff, class]);
            text(&mut t, caption);
            t.push(0); // no creation data
        };
        let (button, edit, label) = (0x80, 0x81, 0x82);
        let tab = wm::WS_TABSTOP;
        item(0, [7, 7, 226, 10], 0xffff, label, description);
        let edit_style = wm::WS_BORDER | tab | wm::ES_AUTOHSCROLL as u32;
        item(edit_style, [7, 20, 226, 12], EDIT as u16, edit, default);
        let ok = wm::BS_DEFPUSHBUTTON as u32 | tab;
        item(ok, [129, 40, 50, 14], wm::IDOK as u16, button, "OK");
        let cancel = wm::BS_PUSHBUTTON as u32 | tab;
        item(
            cancel,
            [183, 40, 50, 14],
            wm::IDCANCEL as u16,
            button,
            "Cancel",
        );
        // The template must start on a DWORD.
        let template: Vec<u32> = t
            .chunks(2)
            .map(|c| u32::from(c[0]) | u32::from(c.get(1).copied().unwrap_or(0)) << 16)
            .collect();
        let mut entry = Entry { max, typed: None };
        // SAFETY: a complete template and our dialog procedure; `entry`
        // outlives the modal dialog.
        unsafe {
            wm::DialogBoxIndirectParamW(
                GetModuleHandleW(std::ptr::null()),
                template.as_ptr().cast::<DLGTEMPLATE>(),
                std::ptr::null_mut(),
                Some(procedure),
                (&raw mut entry) as LPARAM,
            );
        }
        entry.typed
    }

    extern "system" fn procedure(
        dialog: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> isize {
        // SAFETY: our dialog; its user data is the `Entry` from `ask`,
        // stored at WM_INITDIALOG.
        unsafe {
            let edit = wm::GetDlgItem(dialog, EDIT);
            match message {
                wm::WM_INITDIALOG => {
                    wm::SetWindowLongPtrW(dialog, wm::GWLP_USERDATA, lparam);
                    let entry = &*(lparam as *const Entry);
                    wm::SendMessageW(edit, EM_LIMITTEXT, entry.max as WPARAM, 0);
                    wm::SendMessageW(edit, EM_SETSEL, 0, -1);
                    SetFocus(edit);
                    0 // focus set here
                }
                wm::WM_COMMAND => {
                    let entry =
                        &mut *(wm::GetWindowLongPtrW(dialog, wm::GWLP_USERDATA) as *mut Entry);
                    match (wparam & 0xffff) as i32 {
                        wm::IDOK => {
                            let mut buffer =
                                vec![0u16; wm::GetWindowTextLengthW(edit) as usize + 1];
                            let n =
                                wm::GetWindowTextW(edit, buffer.as_mut_ptr(), buffer.len() as i32);
                            entry.typed =
                                Some(String::from_utf16_lossy(&buffer[..n.max(0) as usize]));
                            wm::EndDialog(dialog, 1);
                        }
                        wm::IDCANCEL => {
                            wm::EndDialog(dialog, 0);
                        }
                        _ => return 0,
                    }
                    1
                }
                _ => 0,
            }
        }
    }
}

static VTABLE: Methods<20> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    message_dialog_async as *const (),  // 0x18 message dialog
    message_dialog_result as *const (), // 0x20
    missing::<5> as *const (),          // 0x28 send invite
    missing::<6> as *const (),          // 0x30
    profile_card_async as *const (),    // 0x38
    profile_card_result as *const (),   // 0x40
    missing::<9> as *const (),          // 0x48 achievements
    missing::<10> as *const (),         // 0x50
    missing::<11> as *const (),         // 0x58 player picker
    missing::<12> as *const (),         // 0x60
    missing::<13> as *const (),         // 0x68
    error_dialog_async as *const (),    // 0x70 error dialog
    error_dialog_result as *const (),   // 0x78
    missing::<16> as *const (),         // 0x80 notification position
    text_entry_async as *const (),      // 0x88
    text_entry_size as *const (),       // 0x90
    text_entry_result as *const (),     // 0x98
]);

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::modules::xgameruntime::method;

    /// Serialize tests that mutate process-wide dialog hooks / auto-dismiss.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn block() -> AsyncBlock {
        AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    #[test]
    fn dialogs_end_with_what_was_typed() {
        let this: This = &OBJECT;
        let mut b = block();
        type Card = extern "system" fn(This, *mut AsyncBlock, *const c_void, u64) -> HRESULT;
        type Done = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
        assert_eq!(
            method::<Card>(&OBJECT, 0x38)(this, &mut b, std::ptr::null(), 1),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(method::<Done>(&OBJECT, 0x40)(this, &mut b), S_OK);

        type Entry = extern "system" fn(
            This,
            *mut AsyncBlock,
            *const c_char,
            *const c_char,
            *const c_char,
            u32,
            u32,
        ) -> HRESULT;
        type Size = extern "system" fn(This, *mut AsyncBlock, *mut u32) -> HRESULT;
        type Result_ =
            extern "system" fn(This, *mut AsyncBlock, u32, *mut c_char, *mut u32) -> HRESULT;
        let null = std::ptr::null();
        let entry = method::<Entry>(&OBJECT, 0x88);
        // Dismissed: cancelled.
        let mut b = block();
        assert_eq!(
            entry(this, &mut b, c"Name".as_ptr(), null, null, 0, 16),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), E_ABORT);
        let mut size = 0;
        assert_eq!(
            method::<Size>(&OBJECT, 0x90)(this, &mut b, &mut size),
            E_ABORT
        );
        // Typed: the text, cut to the limit, with its NUL.
        let mut b = block();
        let default = c"abc".as_ptr();
        assert_eq!(
            entry(this, &mut b, c"Type".as_ptr(), null, default, 0, 6),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(method::<Size>(&OBJECT, 0x90)(this, &mut b, &mut size), S_OK);
        assert_eq!(size, 7);
        let mut out = [0 as c_char; 7];
        let mut used = 0;
        let result = method::<Result_>(&OBJECT, 0x98);
        assert_eq!(result(this, &mut b, 7, out.as_mut_ptr(), &mut used), S_OK);
        assert_eq!((text(out.as_ptr()).as_deref(), used), (Some("abctyp"), 7));
        assert_eq!(limit("\u{e9}t\u{e9}s", 3), "\u{e9}t\u{e9}");
    }

    #[test]
    fn message_dialog_buttons_gaps_default_cancel_and_auto_dismiss() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_auto_dismiss(false);

        let this: This = &OBJECT;
        type ShowMsg = extern "system" fn(
            This,
            *mut AsyncBlock,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            u32,
            u32,
        ) -> HRESULT;
        type MsgResult = extern "system" fn(This, *mut AsyncBlock, *mut u32) -> HRESULT;
        let show = method::<ShowMsg>(&OBJECT, 0x18);
        let result = method::<MsgResult>(&OBJECT, 0x20);

        // 1. Fake dialog answers button 2 (BUTTON_SECOND) and records the request
        //    without asserting on the worker thread.
        let recorded = Arc::new(Mutex::new(None::<MessageDialogRequest>));
        let rec = recorded.clone();
        *MESSAGE_HOOK.lock().unwrap() = Some(Box::new(move |req| {
            *rec.lock().unwrap() = Some(req.clone());
            Some(BUTTON_SECOND)
        }));
        let mut b = block();
        assert_eq!(
            show(
                this,
                &mut b,
                c"Confirm".as_ptr(),
                c"Proceed?".as_ptr(),
                c"Yes".as_ptr(),
                c"No".as_ptr(),
                std::ptr::null(),
                BUTTON_SECOND,
                BUTTON_SECOND,
            ),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(result(this, &mut b, std::ptr::null_mut()), E_INVALIDARG);
        let mut chosen = u32::MAX;
        assert_eq!(result(this, &mut b, &mut chosen), S_OK);
        assert_eq!(chosen, BUTTON_SECOND);
        let req = recorded.lock().unwrap().take().expect("hook ran");
        assert_eq!(req.title, "Confirm");
        assert_eq!(req.default_button, BUTTON_SECOND);
        assert_eq!(req.cancel_button, BUTTON_SECOND);
        assert_eq!(
            req.buttons,
            vec![
                (BUTTON_FIRST, "Yes".to_owned()),
                (BUTTON_SECOND, "No".to_owned()),
            ]
        );

        // 2. Labels with a gap (null second button): third button keeps BUTTON_THIRD.
        let rec = recorded.clone();
        *MESSAGE_HOOK.lock().unwrap() = Some(Box::new(move |req| {
            *rec.lock().unwrap() = Some(req.clone());
            Some(BUTTON_THIRD)
        }));
        let mut b = block();
        assert_eq!(
            show(
                this,
                &mut b,
                c"Gap".as_ptr(),
                c"Save & Quit".as_ptr(),
                c"Save".as_ptr(),
                std::ptr::null(),
                c"Quit".as_ptr(),
                BUTTON_THIRD,
                BUTTON_FIRST,
            ),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(result(this, &mut b, &mut chosen), S_OK);
        assert_eq!(chosen, BUTTON_THIRD);
        let req = recorded.lock().unwrap().take().expect("hook ran");
        assert_eq!(
            req.buttons,
            vec![
                (BUTTON_FIRST, "Save".to_owned()),
                (BUTTON_THIRD, "Quit".to_owned()),
            ]
        );
        assert_eq!(req.default_button, BUTTON_THIRD);
        assert_eq!(req.cancel_button, BUTTON_FIRST);

        // 3. Esc / cancel returns req.cancel_button with S_OK.
        *MESSAGE_HOOK.lock().unwrap() = Some(Box::new(|req| Some(req.cancel_button)));
        let mut b = block();
        assert_eq!(
            show(
                this,
                &mut b,
                c"Esc".as_ptr(),
                c"Close?".as_ptr(),
                c"Stay".as_ptr(),
                c"Leave".as_ptr(),
                c"Cancel".as_ptr(),
                BUTTON_FIRST,
                BUTTON_THIRD,
            ),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(result(this, &mut b, &mut chosen), S_OK);
        assert_eq!(chosen, BUTTON_THIRD);

        // 4. Unattended auto-dismiss answers with cancel_button without calling the hook.
        *MESSAGE_HOOK.lock().unwrap() = None;
        set_auto_dismiss(true);
        let mut b = block();
        assert_eq!(
            show(
                this,
                &mut b,
                c"Auto".as_ptr(),
                c"Unattended".as_ptr(),
                c"OK".as_ptr(),
                c"Cancel".as_ptr(),
                std::ptr::null(),
                BUTTON_FIRST,
                BUTTON_SECOND,
            ),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(result(this, &mut b, &mut chosen), S_OK);
        assert_eq!(chosen, BUTTON_SECOND);
        set_auto_dismiss(false);

        // 5. No window / cannot open: completes with E_ABORT.
        let mut b = block();
        assert_eq!(
            show(
                this,
                &mut b,
                c"Headless".as_ptr(),
                c"No window".as_ptr(),
                c"OK".as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                BUTTON_FIRST,
                BUTTON_FIRST,
            ),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), E_ABORT);
        assert_eq!(result(this, &mut b, &mut chosen), E_ABORT);
        assert_eq!(escape_mnemonic("Save & Exit"), "Save && Exit");
    }

    #[test]
    fn error_dialog_completes_auto_dismisses_or_cancels_without_window() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_auto_dismiss(false);

        let this: This = &OBJECT;
        type ShowErr = extern "system" fn(This, *mut AsyncBlock, HRESULT, *const c_char) -> HRESULT;
        type ErrResult = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
        let show = method::<ShowErr>(&OBJECT, 0x70);
        let result = method::<ErrResult>(&OBJECT, 0x78);

        let recorded = Arc::new(Mutex::new(None::<(HRESULT, String)>));
        let rec = recorded.clone();
        *ERROR_HOOK.lock().unwrap() = Some(Box::new(move |code, ctx| {
            *rec.lock().unwrap() = Some((code, ctx.to_owned()));
            true
        }));
        let mut b = block();
        assert_eq!(
            show(this, &mut b, E_ABORT, c"network offline".as_ptr()),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(result(this, &mut b), S_OK);
        assert_eq!(
            recorded.lock().unwrap().take(),
            Some((E_ABORT, "network offline".to_owned()))
        );

        // Unattended auto-dismiss completes immediately with S_OK.
        *ERROR_HOOK.lock().unwrap() = None;
        set_auto_dismiss(true);
        let mut b = block();
        assert_eq!(show(this, &mut b, E_ABORT, std::ptr::null()), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(result(this, &mut b), S_OK);
        set_auto_dismiss(false);

        // No window / cannot open: completes as cancelled (E_ABORT).
        let mut b = block();
        assert_eq!(show(this, &mut b, E_ABORT, std::ptr::null()), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), E_ABORT);
        assert_eq!(result(this, &mut b), E_ABORT);
    }
}
