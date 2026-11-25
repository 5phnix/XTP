//! Child-process test for `XLaunchRestartOnCrash` (issue #11 / R10):
//! a probe registers restart-on-crash arguments, triggers an unhandled fault,
//! is automatically relaunched with the recorded arguments after waiting for
//! the crashing process to exit, and does not relaunch a third time when it
//! faults again.
#![cfg(windows)]

use std::ffi::{CString, c_char};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use xtp_hle::modules::{lifecycle, xgame, xgameruntime};

const PROBE_LOG_ENV: &str = "XTP_RESTART_PROBE_LOG";

fn main() {
    if let Some(log_path) = std::env::var_os(PROBE_LOG_ENV) {
        run_child(PathBuf::from(log_path));
    } else {
        run_parent();
    }
}

fn run_parent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let log_path = dir.path().join("runs.log");

    let exe = std::env::current_exe().expect("test binary path");
    let status = Command::new(&exe)
        .arg("--probe-mode")
        .env(PROBE_LOG_ENV, &log_path)
        .env_remove("XTP_CRASH_RESTARTED")
        .env_remove("XTP_RELAUNCHED_FROM")
        .status()
        .expect("spawn initial child");
    assert!(
        !status.success(),
        "initial child should terminate with a fault"
    );

    // Wait for the restarted grandchild to start and log its PID.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut grandchild_pid = None;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&log_path) {
            let lines: Vec<&str> = text.lines().collect();
            if lines.len() >= 2 {
                grandchild_pid = lines[1]
                    .strip_prefix("pid=")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|s| s.parse::<u32>().ok());
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Deterministically wait for the restarted grandchild to fault and exit so
    // it is never left running and we know no third launch occurred.
    if let Some(pid) = grandchild_pid {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        // SAFETY: opening the grandchild PID for SYNCHRONIZE only.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if !handle.is_null() {
            // SAFETY: valid process handle returned by `OpenProcess`.
            unsafe {
                WaitForSingleObject(handle, 10_000);
                CloseHandle(handle);
            }
        }
    }

    let final_text = std::fs::read_to_string(&log_path).expect("read probe log");
    let final_lines: Vec<&str> = final_text.lines().collect();

    assert_eq!(
        final_lines.len(),
        2,
        "expected exactly 2 runs (initial + 1 restart), got {final_lines:?}"
    );
    assert!(
        final_lines[0].contains("--probe-mode") && !final_lines[0].contains("-recover"),
        "unexpected run 1 args: {}",
        final_lines[0]
    );
    assert!(
        final_lines[1].contains("--probe-mode") && final_lines[1].contains("-- -recover -slot=3"),
        "run 2 did not receive recorded restart arguments: {}",
        final_lines[1]
    );
    println!("test restart_on_crash ... ok");
}

fn run_child(log_path: PathBuf) {
    lifecycle::wait_for_previous();
    let args: Vec<String> = std::env::args().skip(1).collect();
    {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .expect("open probe log");
        writeln!(f, "pid={} args={}", std::process::id(), args.join(" ")).expect("write probe log");
        let _ = f.flush();
    }

    xtp_loader::report_crashes();
    xtp_loader::on_crash(lifecycle::relaunch_on_crash);

    // Call XLaunchRestartOnCrash through TITLE_OBJECT vtable slot 0x28.
    let obj = &xgame::TITLE_OBJECT;
    type RestartOnCrash = extern "system" fn(
        *const xgameruntime::Object,
        *const c_char,
        u32,
    ) -> xgameruntime::HRESULT;
    // SAFETY: `TITLE_OBJECT`'s first word is its vtable; slot 5 (offset 0x28)
    // is `launch_restart_on_crash`.
    let restart: RestartOnCrash = unsafe {
        let vtable = *(obj as *const xgameruntime::Object as *const *const *const ());
        std::mem::transmute(*vtable.add(5))
    };

    let bad_hr = restart(obj, std::ptr::null(), 1);
    assert!(bad_hr < 0, "non-zero reserved must fail");

    let recorded = CString::new("-recover -slot=3").unwrap();
    let hr = restart(obj, recorded.as_ptr(), 0);
    assert_eq!(hr, xgameruntime::S_OK);

    // Deliberately trigger an unhandled access violation so `report_unhandled`
    // invokes the registered crash-restart hook.
    // SAFETY: intentional null write in an isolated child probe process.
    unsafe {
        std::ptr::write_volatile(std::ptr::null_mut::<u32>(), 0xdead_beef);
    }
}
