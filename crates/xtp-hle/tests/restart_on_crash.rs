//! Isolated child-process probes for crash restart, the title's unhandled
//! exception filter, and unattended runs.
#![cfg(windows)]

use std::ffi::{c_char, c_void};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, RaiseException,
    SEM_NOGPFAULTERRORBOX, SetErrorMode,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, GetExitCodeProcess, OpenEventW, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    SYNCHRONIZATION_SYNCHRONIZE, SetEvent, TerminateProcess, WaitForSingleObject,
};
use xtp_hle::modules::{kernel, lifecycle, xgame, xgameruntime};

const PROBE_DIR_ENV: &str = "XTP_RESTART_PROBE_DIR";
const RECOVERABLE_EXCEPTION: u32 = 0xE042_4700;
const STATUS_ACCESS_VIOLATION: u32 = EXCEPTION_ACCESS_VIOLATION as u32;
const TIMEOUT_MS: u32 = 10_000;
static LOG: OnceLock<PathBuf> = OnceLock::new();

fn main() {
    if let Some(dir) = std::env::var_os(PROBE_DIR_ENV) {
        run_child(Path::new(&dir));
    } else {
        run_parent("default", &[]);
        run_parent(
            "title-filter",
            &["filter resumed", "execution resumed", "filter searched"],
        );
        run_parent("autopilot", &[]);
    }
}

fn run_parent(mode: &str, filter_log: &[&str]) {
    let dir = tempfile::tempdir().expect("probe directory");
    let dir = dir.path();
    let ready = event(dir, "ready", true);
    let proceed = event(dir, "proceed", true);
    let child = Command::new(std::env::current_exe().expect("test binary path"))
        .arg(mode)
        .env(PROBE_DIR_ENV, dir)
        .env_remove("XTP_CRASH_RESTARTED")
        .env_remove("XTP_RELAUNCHED_FROM")
        .spawn()
        .expect("spawn initial probe");

    let status = wait_for_exit(child.as_raw_handle(), "initial probe");
    if mode == "autopilot" {
        assert_eq!(status, STATUS_ACCESS_VIOLATION, "autopilot's original exit");
        assert!(!dir.join("replacement.pid").exists());
    } else {
        assert_eq!(status, 1, "crash-restart exit for {mode}");
        // The replacement publishes its PID and waits before it can fault.
        // Hold its process handle before allowing it to proceed.
        wait_for_event(&ready, "replacement PID");
        let pid = std::fs::read_to_string(dir.join("replacement.pid"))
            .expect("read replacement PID")
            .parse()
            .expect("parse replacement PID");
        // SAFETY: the waiting replacement published this PID.
        let process = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        let process = own_handle(process, "open replacement process");
        signal(&proceed);
        assert_eq!(
            wait_for_exit(process.as_raw_handle(), "replacement probe"),
            STATUS_ACCESS_VIOLATION,
            "the replacement must exit with the exception code instead of spawning again"
        );
    }

    let log = std::fs::read_to_string(dir.join("runs.log")).expect("read probe log");
    let mut expected = vec![mode];
    expected.extend_from_slice(filter_log);
    let replacement_args = format!("{mode} -- -recover -slot=3");
    if mode != "autopilot" {
        expected.push(&replacement_args);
    }
    assert_eq!(log.lines().collect::<Vec<_>>(), expected);
}

fn own_handle(handle: RawHandle, operation: &str) -> OwnedHandle {
    assert!(
        !handle.is_null(),
        "{operation}: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the successful Win32 call returned a new handle to close.
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

fn event(dir: &Path, name: &str, create: bool) -> OwnedHandle {
    let name = format!(
        "Local\\xtp-restart-{}-{name}",
        dir.file_name().unwrap().to_string_lossy()
    );
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: the event name is NUL-terminated. Only the parent creates the
    // events, and each signal wakes one waiter.
    let handle = unsafe {
        if create {
            CreateEventW(std::ptr::null(), 0, 0, name.as_ptr())
        } else {
            OpenEventW(
                EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
                0,
                name.as_ptr(),
            )
        }
    };
    own_handle(handle, "open probe event")
}

fn signal(event: &OwnedHandle) {
    // SAFETY: the event handle is live and allows signalling.
    assert_ne!(unsafe { SetEvent(event.as_raw_handle()) }, 0);
}

fn wait_for_event(event: &OwnedHandle, what: &str) {
    // SAFETY: the event handle is live and allows waiting.
    let wait = unsafe { WaitForSingleObject(event.as_raw_handle(), TIMEOUT_MS) };
    assert_eq!(wait, WAIT_OBJECT_0, "waiting for {what}");
}

fn wait_for_exit(process: RawHandle, what: &str) -> u32 {
    // SAFETY: callers hold a live process handle with wait, query and
    // terminate rights throughout this call.
    unsafe {
        let wait = WaitForSingleObject(process, TIMEOUT_MS);
        if wait != WAIT_OBJECT_0 {
            TerminateProcess(process, 1);
            WaitForSingleObject(process, TIMEOUT_MS);
            panic!("waiting for {what} to exit: {wait:#x}");
        }
        let mut code = 0;
        assert_ne!(GetExitCodeProcess(process, &mut code), 0);
        code
    }
}

fn log(line: &str) {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG.get().expect("probe log path"))
        .expect("open probe log");
    writeln!(log, "{line}").expect("write probe log");
}

extern "system" fn title_filter(info: *const c_void) -> i32 {
    // SAFETY: Windows supplies the active exception pointers to the filter.
    let code = unsafe { (*(*info.cast::<EXCEPTION_POINTERS>()).ExceptionRecord).ExceptionCode };
    if code as u32 == RECOVERABLE_EXCEPTION {
        log("filter resumed");
        EXCEPTION_CONTINUE_EXECUTION
    } else {
        log("filter searched");
        EXCEPTION_CONTINUE_SEARCH
    }
}

fn run_child(dir: &Path) {
    lifecycle::wait_for_previous();
    // SAFETY: this probe process can suppress dialogs for its own fault.
    unsafe { SetErrorMode(SEM_NOGPFAULTERRORBOX) };
    let args: Vec<_> = std::env::args().skip(1).collect();
    LOG.set(dir.join("runs.log")).expect("set probe log path");
    log(&args.join(" "));

    let replacement = args.iter().any(|arg| arg == "--");
    if replacement {
        let ready = event(dir, "ready", false);
        let proceed = event(dir, "proceed", false);
        std::fs::write(dir.join("replacement.pid"), std::process::id().to_string())
            .expect("publish replacement PID");
        signal(&ready);
        wait_for_event(&proceed, "parent to open the replacement process");
    }

    xtp_loader::report_crashes();
    lifecycle::install_crash_restart(args[0] == "autopilot");

    let obj = &xgame::TITLE_OBJECT;
    type RestartOnCrash = extern "system" fn(
        *const xgameruntime::Object,
        *const c_char,
        u32,
    ) -> xgameruntime::HRESULT;
    // SAFETY: `TITLE_OBJECT` starts with its vtable; slot 5 implements
    // `XLaunchRestartOnCrash` with this signature.
    let restart: RestartOnCrash = unsafe {
        let vtable = *std::ptr::from_ref(obj).cast::<*const *const ()>();
        std::mem::transmute(*vtable.add(5))
    };
    assert!(restart(obj, std::ptr::null(), 1) < 0);
    assert_eq!(
        restart(obj, c"-recover -slot=3".as_ptr(), 0),
        xgameruntime::S_OK
    );

    if !replacement && args[0] == "title-filter" {
        kernel::SetUnhandledExceptionFilter(title_filter as *const () as usize);
        // SAFETY: a continuable test exception with no arguments. The title
        // filter resumes it without editing the processor context.
        unsafe { RaiseException(RECOVERABLE_EXCEPTION, 0, 0, std::ptr::null()) };
        log("execution resumed");
    }

    // SAFETY: a synthetic access violation in this isolated probe. Its two
    // arguments describe a write to address zero.
    unsafe { RaiseException(STATUS_ACCESS_VIOLATION, 0, 2, [1, 0].as_ptr()) };
    panic!("the access violation returned");
}
