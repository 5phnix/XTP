//! Shared test helpers for XTP (dev-dependency only).
//!
//! Host- and sample-dependent tests call [`skip`] when a prerequisite is absent
//! instead of silently returning or panicking. Every skip emits a uniform line
//! (`SKIPPED <test>: <reason>`) so test runners can count skips by reason, and
//! machines that must execute a category can set `XTP_REQUIRE=<reason,...>` to
//! turn matching skips into panics.

/// Environment variable listing comma-separated skip reasons that must panic
/// instead of skipping (or `all` / `*` to forbid every skip).
pub const REQUIRE_ENV: &str = "XTP_REQUIRE";

/// Record that the current test is skipped for `reason`, or panic if
/// [`REQUIRE_ENV`] lists `reason`.
///
/// Prints `SKIPPED <test>: <reason>` to stderr and returns when skipping is
/// allowed.
pub fn skip(reason: &str) {
    let test = current_test_name();
    let require = std::env::var(REQUIRE_ENV).ok();
    if is_required(reason, require.as_deref()) {
        panic!(
            "test `{test}` cannot skip for `{reason}` ({REQUIRE_ENV}={})",
            require.as_deref().unwrap_or_default()
        );
    }
    eprintln!("{}", format_skip(&test, reason));
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
        let line = format_skip(&name, "no-gpu");
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
        assert!(!is_required("no-gpu", None));
        assert!(!is_required("no-gpu", Some("")));
        assert!(!is_required("no-gpu", Some("no-dxc, no-sample")));
        assert!(is_required("no-gpu", Some("no-gpu")));
        assert!(is_required("no-gpu", Some("no-dxc, NO-GPU , no-sample")));
        assert!(is_required("no-gpu", Some("all")));
        assert!(is_required("no-gpu", Some("*")));
    }

    #[test]
    fn child_skip_no_gpu() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        skip("no-gpu");
    }

    #[test]
    fn child_skip_other_reason() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        skip("no-dxc");
    }

    #[test]
    fn require_env_panics_only_for_listed_reason() {
        let exe = std::env::current_exe().expect("test binary path");

        let panic_run = Command::new(&exe)
            .args(["--exact", "tests::child_skip_no_gpu", "--nocapture"])
            .env(CHILD_ENV, "1")
            .env(REQUIRE_ENV, "no-gpu")
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
            combined.contains("no-gpu"),
            "panic output should name reason: {combined}"
        );

        let skip_run = Command::new(&exe)
            .args(["--exact", "tests::child_skip_other_reason", "--nocapture"])
            .env(CHILD_ENV, "1")
            .env(REQUIRE_ENV, "no-gpu")
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
            "stderr should contain canonical skip marker: {stderr}"
        );
    }
}
