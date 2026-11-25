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
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, GetExitCodeProcess, OpenEventW, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE, SetEvent,
    WaitForSingleObject,
};
use xtp_hle::modules::{kernel, lifecycle, xasync, xgame, xgameruntime};

const PROBE_DIR_ENV: &str = "XTP_RESTART_PROBE_DIR";
const RECOVERABLE_EXCEPTION: u32 = 0xE042_4700;
const STATUS_ACCESS_VIOLATION: u32 = EXCEPTION_ACCESS_VIOLATION as u32;
const TIMEOUT_MS: u32 = 10_000;
static LOG: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Copy)]
enum Scenario {
    Default,
    NullArgs,
    TitleFilter,
    Autopilot,
    Shutdown,
}

#[test]
fn the_replacement_exits_without_another_restart() {
    isolated(
        "the_replacement_exits_without_another_restart",
        Scenario::Default,
    );
}

#[test]
fn null_args_restart_without_title_arguments() {
    isolated(
        "null_args_restart_without_title_arguments",
        Scenario::NullArgs,
    );
}

#[test]
fn the_title_filter_runs_before_restart() {
    isolated(
        "the_title_filter_runs_before_restart",
        Scenario::TitleFilter,
    );
}

#[test]
fn autopilot_keeps_the_original_crash_exit() {
    isolated(
        "autopilot_keeps_the_original_crash_exit",
        Scenario::Autopilot,
    );
}

#[test]
fn a_fault_in_a_suspend_routine_does_not_restart() {
    isolated(
        "a_fault_in_a_suspend_routine_does_not_restart",
        Scenario::Shutdown,
    );
}

fn isolated(name: &str, scenario: Scenario) {
    if let Some(dir) = std::env::var_os(PROBE_DIR_ENV) {
        run_child(Path::new(&dir), scenario);
    }

    let dir = tempfile::tempdir().expect("probe directory");
    let dir = dir.path();
    let ready = event(dir, "ready", true);
    let proceed = event(dir, "proceed", true);
    let job = probe_job();
    let mut child = Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", name, "--nocapture"])
        .env(PROBE_DIR_ENV, dir)
        .env_remove("XTP_CRASH_RESTARTED")
        .env_remove("XTP_RELAUNCHED_FROM")
        .spawn()
        .expect("spawn initial probe");
    // SAFETY: both handles are live. The child waits for `proceed` before
    // running anything that could spawn a replacement.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
        let error = std::io::Error::last_os_error();
        child.kill().expect("stop the unassigned probe");
        child.wait().expect("reap the unassigned probe");
        panic!("assign initial probe to the job: {error}");
    }
    signal(&proceed);

    let status = wait_for_exit(child.as_raw_handle(), "initial probe");
    let restarted = !matches!(scenario, Scenario::Autopilot | Scenario::Shutdown);
    if !restarted {
        assert_eq!(status, STATUS_ACCESS_VIOLATION, "original exception exit");
    } else {
        assert_eq!(status, 1, "crash-restart exit for {name}");
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
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
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

    assert_eq!(process_count(&job), if restarted { 2 } else { 1 });
    let log = std::fs::read_to_string(dir.join("runs.log")).expect("read probe log");
    let expected: &[&str] = match scenario {
        Scenario::Default => &["initial", "replacement", "--", "-recover", "-slot=3"],
        Scenario::NullArgs => &["initial", "replacement"],
        Scenario::TitleFilter => &[
            "initial",
            "filter resumed",
            "execution resumed",
            "filter searched",
            "replacement",
            "--",
            "-recover",
            "-slot=3",
        ],
        Scenario::Autopilot => &["initial"],
        Scenario::Shutdown => &["initial", "suspend routine"],
    };
    assert_eq!(log.lines().collect::<Vec<_>>(), expected);
}

/// All probe descendants inherit job membership. Closing the parent's sole
/// job handle kills any that are still running, including on a failed check.
fn probe_job() -> OwnedHandle {
    // SAFETY: null attributes and name create an unnamed, non-inheritable job handle.
    let job = own_handle(
        unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) },
        "create probe job",
    );
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the live job handle and the buffer match the information class.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        )
    };
    assert_ne!(
        ok,
        0,
        "set probe job limits: {}",
        std::io::Error::last_os_error()
    );
    job
}

/// Counts every process assigned to the job, including those that already exited.
fn process_count(job: &OwnedHandle) -> u32 {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: the live job handle and the writable buffer match the information class.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&raw mut info).cast(),
            size_of_val(&info) as u32,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        ok,
        0,
        "query probe job accounting: {}",
        std::io::Error::last_os_error()
    );
    info.TotalProcesses
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
    // SAFETY: callers hold a live process handle with wait and query rights.
    unsafe {
        let wait = WaitForSingleObject(process, TIMEOUT_MS);
        assert_eq!(wait, WAIT_OBJECT_0, "waiting for {what} to exit");
        let mut code = 0;
        assert_ne!(
            GetExitCodeProcess(process, &mut code),
            0,
            "query {what} exit code: {}",
            std::io::Error::last_os_error()
        );
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

fn run_child(dir: &Path, scenario: Scenario) -> ! {
    // SAFETY: this probe process can suppress dialogs for its own fault.
    unsafe { SetErrorMode(SEM_NOGPFAULTERRORBOX) };
    let replacement = std::env::var_os("XTP_CRASH_RESTARTED").is_some();
    let proceed = event(dir, "proceed", false);
    if !replacement {
        wait_for_event(&proceed, "parent to assign the probe to the job");
    }
    lifecycle::wait_for_previous();
    LOG.set(dir.join("runs.log")).expect("set probe log path");

    if replacement {
        log("replacement");
        for arg in std::env::args().skip_while(|arg| arg != "--") {
            log(&arg);
        }
        let ready = event(dir, "ready", false);
        std::fs::write(dir.join("replacement.pid"), std::process::id().to_string())
            .expect("publish replacement PID");
        signal(&ready);
        wait_for_event(&proceed, "parent to open the replacement process");
    } else {
        log("initial");
    }

    xtp_loader::report_crashes();
    lifecycle::install_crash_restart(matches!(scenario, Scenario::Autopilot));

    let obj = &xgame::TITLE_OBJECT;
    type RestartOnCrash = extern "system" fn(
        *const xgameruntime::Object,
        *const c_char,
        u32,
    ) -> xgameruntime::HRESULT;
    // SAFETY: `TITLE_OBJECT` starts with its vtable; slot 5 implements
    // `XLaunchRestartOnCrash` with this signature.
    let vtable = unsafe { *std::ptr::from_ref(obj).cast::<*const *const ()>() };
    // SAFETY: slot 5 in this vtable is a `RestartOnCrash` function pointer.
    let restart: RestartOnCrash = unsafe { std::mem::transmute(*vtable.add(5)) };
    assert_eq!(restart(obj, std::ptr::null(), 1), xasync::E_INVALIDARG);
    let args = match scenario {
        Scenario::NullArgs => std::ptr::null(),
        _ => c"-recover -slot=3".as_ptr(),
    };
    assert_eq!(restart(obj, args, 0), xgameruntime::S_OK);

    if !replacement && matches!(scenario, Scenario::TitleFilter) {
        kernel::SetUnhandledExceptionFilter(title_filter as *const () as usize);
        // SAFETY: a continuable test exception with no arguments. The title
        // filter resumes it without editing the processor context.
        unsafe { RaiseException(RECOVERABLE_EXCEPTION, 0, 0, std::ptr::null()) };
        log("execution resumed");
    }
    if matches!(scenario, Scenario::Shutdown) {
        let mut registration = 0;
        assert_eq!(
            lifecycle::RegisterAppStateChangeNotification(
                Some(fault_on_suspend),
                std::ptr::null_mut(),
                &mut registration
            ),
            0
        );
        type LaunchNewGame = extern "system" fn(
            *const xgameruntime::Object,
            *const c_char,
            *const c_char,
            *const c_void,
        );
        // SAFETY: slot 4 in this vtable implements `XLaunchNewGame` with this signature.
        let launch: LaunchNewGame = unsafe { std::mem::transmute(*vtable.add(4)) };
        launch(obj, std::ptr::null(), std::ptr::null(), std::ptr::null());
        panic!("XLaunchNewGame returned");
    }

    fault()
}

extern "system" fn fault_on_suspend(_: u8, _: *mut c_void) {
    log("suspend routine");
    fault()
}

fn fault() -> ! {
    // SAFETY: a synthetic access violation in this isolated probe. Its two
    // arguments describe a write to address zero.
    unsafe { RaiseException(STATUS_ACCESS_VIOLATION, 0, 2, [1, 0].as_ptr()) };
    panic!("the access violation returned");
}
