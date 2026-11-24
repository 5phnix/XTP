//! Shared test helpers for XTP (dev-dependency only).
//!
//! Host- and sample-dependent tests call [`skip`] when a prerequisite is absent
//! instead of silently returning or panicking. Every skip writes a uniform line
//! (`SKIPPED <test>: <reason>`) directly to stderr (bypassing `libtest`
//! capture) so test runners can count skips by reason, and machines that must
//! execute a category can set `XTP_REQUIRE=<reason,...>` to turn matching skips
//! into panics. (In backlog item B3, `XTP_REQUIRE_GPU` will fold into
//! `XTP_REQUIRE=no-gpu`.)
//!
//! # Example
//!
//! ```no_run
//! fn needs_gpu() {
//!     let has_gpu = false;
//!     if !has_gpu {
//!         xtp_testkit::skip(xtp_testkit::reason::NO_GPU);
//!         return;
//!     }
//! }
//! ```

use std::io::Write;

/// Environment variable listing comma-separated skip reasons that must panic
/// instead of skipping (or `all` / `*` to forbid every skip).
pub const REQUIRE_ENV: &str = "XTP_REQUIRE";

/// Standard skip reasons shared across crates so call sites and `XTP_REQUIRE`
/// values do not drift apart.
pub mod reason {
    /// No hardware D3D12 GPU adapter is available on this host.
    pub const NO_GPU: &str = "no-gpu";
    /// The host GPU does not support Shader Model 6.6.
    pub const NO_SM66: &str = "no-sm66";
    /// Host `dxcompiler.dll` / `dxc` is unavailable.
    pub const NO_DXC: &str = "no-dxc";
    /// Host `dxil.dll` validator is unavailable.
    pub const NO_DXIL: &str = "no-dxil";
    /// Console DXC compiler is unavailable.
    pub const NO_CONSOLE_DXC: &str = "no-console-dxc";
}

/// Record that the current test is skipped for `reason`, or panic if
/// [`REQUIRE_ENV`] lists `reason`.
///
/// Writes `SKIPPED <test>: <reason>` directly to stderr (so it stays visible
/// under default `libtest` output capture) and returns; callers must `return`
/// immediately after calling `skip`.
pub fn skip(reason: &str) {
    let test = current_test_name();
    let require = std::env::var(REQUIRE_ENV).ok();
    if is_required(reason, require.as_deref()) {
        panic!(
            "test `{test}` cannot skip for `{reason}` ({REQUIRE_ENV}={})",
            require.as_deref().unwrap_or_default()
        );
    }
    let _ = writeln!(std::io::stderr().lock(), "{}", format_skip(&test, reason));
}

/// Format the canonical skip marker for `test` and `reason`.
pub fn format_skip(test: &str, reason: &str) -> String {
    format!("SKIPPED {test}: {reason}")
}

/// Name of the running test from the current thread (set by `libtest`), or
/// `<unnamed>` outside a named thread.
pub fn current_test_name() -> String {
    std::thread::current()
        .name()
        .filter(|n| !n.is_empty())
        .unwrap_or("<unnamed>")
        .to_owned()
}

/// Return `true` if `require_list` (a comma-separated `XTP_REQUIRE` value)
/// forbids skipping for `reason`.
pub fn is_required(reason: &str, require_list: Option<&str>) -> bool {
    let Some(list) = require_list else {
        return false;
    };
    let reason = reason.trim();
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            entry == "*" || entry.eq_ignore_ascii_case("all") || entry.eq_ignore_ascii_case(reason)
        })
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    const CHILD_ENV: &str = "XTP_TESTKIT_CHILD";

    #[test]
    fn marker_names_the_current_test() {
        let name = current_test_name();
        let line = format_skip(&name, reason::NO_GPU);
        assert!(
            line.starts_with("SKIPPED ") && line.ends_with(": no-gpu"),
            "unexpected marker: {line}"
        );
        assert!(
            line.contains("marker_names_the_current_test"),
            "marker should include test name: {line}"
        );
    }

    #[test]
    fn require_list_matching() {
        assert!(!is_required(reason::NO_GPU, None));
        assert!(!is_required(reason::NO_GPU, Some("")));
        assert!(!is_required(reason::NO_GPU, Some("no-dxc, no-sample")));
        assert!(is_required(reason::NO_GPU, Some("no-gpu")));
        assert!(is_required(
            reason::NO_GPU,
            Some("no-dxc, NO-GPU , no-sample")
        ));
        assert!(is_required(reason::NO_GPU, Some("all")));
        assert!(is_required(reason::NO_GPU, Some("*")));
    }

    #[test]
    fn child_skip_no_gpu() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        skip(reason::NO_GPU);
    }

    #[test]
    fn child_skip_other_reason() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        skip(reason::NO_DXC);
    }

    #[test]
    fn require_env_panics_only_for_listed_reason() {
        let exe = std::env::current_exe().expect("test binary path");

        let panic_run = Command::new(&exe)
            .args(["--exact", "tests::child_skip_no_gpu"])
            .env(CHILD_ENV, "1")
            .env(REQUIRE_ENV, reason::NO_GPU)
            .output()
            .expect("spawn child test for required reason");
        assert!(
            !panic_run.status.success(),
            "expected child to panic when XTP_REQUIRE=no-gpu"
        );
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&panic_run.stdout),
            String::from_utf8_lossy(&panic_run.stderr)
        );
        assert!(
            combined.contains("cannot skip") && combined.contains(reason::NO_GPU),
            "panic output should report cannot skip and reason: {combined}"
        );

        // Intentionally run without `--nocapture` to prove the SKIPPED marker
        // reaches stderr under default `libtest` output capture.
        let skip_run = Command::new(&exe)
            .args(["--exact", "tests::child_skip_other_reason"])
            .env(CHILD_ENV, "1")
            .env(REQUIRE_ENV, reason::NO_GPU)
            .output()
            .expect("spawn child test for non-required reason");
        assert!(
            skip_run.status.success(),
            "unrelated reason should skip cleanly: stderr={}",
            String::from_utf8_lossy(&skip_run.stderr)
        );
        let stderr = String::from_utf8_lossy(&skip_run.stderr);
        assert!(
            stderr.contains("SKIPPED tests::child_skip_other_reason: no-dxc"),
            "stderr should contain canonical skip marker under default capture: {stderr}"
        );
    }
}
