//! `cargo xtask pr-check`: what CI checks on a pull request, run locally,
//! with one summary at the end. Each step bears the name of the CI job that
//! runs it (.github/workflows/ci.yml and pr-conventions.yml).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::conventions;

/// The cargo steps, exactly as CI runs them.
pub const CARGO_STEPS: &[(&str, &[&str])] = &[
    ("fmt", &["fmt", "--all", "--check"]),
    (
        "clippy",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    ),
    (
        "clippy-diagnostics",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    ),
    ("test", &["test", "--workspace", "--locked"]),
];

/// The steps run in-process, after the cargo steps.
pub const POLICY: &str = "policy";
pub const CONVENTIONS: &str = "conventions";

pub fn step_names() -> Vec<&'static str> {
    CARGO_STEPS
        .iter()
        .map(|(name, _)| *name)
        .chain([POLICY, CONVENTIONS])
        .collect()
}

pub struct Options {
    /// Commit the branch is compared against; by default where it left main.
    pub base: Option<String>,
    /// The pull request title to check with the commits.
    pub title: Option<String>,
    pub skip: Vec<String>,
    pub fail_fast: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Passed,
    Failed(String),
    Skipped(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub name: &'static str,
    pub status: Status,
    pub time: Duration,
    /// What the step checked, or a note on how it passed.
    pub detail: String,
}

pub fn run(root: &Path, opts: &Options) -> Result<()> {
    if let Some(unknown) = opts
        .skip
        .iter()
        .find(|s| !step_names().contains(&s.as_str()))
    {
        bail!(
            "unknown step `{unknown}` for --skip (steps: {})",
            step_names().join(", ")
        );
    }
    let dirty = git_output(root, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty());

    let mut outcomes = Vec::new();
    let mut failed = false;
    for name in step_names() {
        let skip_reason = if opts.skip.iter().any(|s| s == name) {
            Some("--skip")
        } else if failed && opts.fail_fast {
            Some("--fail-fast")
        } else {
            None
        };
        if let Some(reason) = skip_reason {
            outcomes.push(Outcome {
                name,
                status: Status::Skipped(reason),
                time: Duration::ZERO,
                detail: String::new(),
            });
            continue;
        }
        println!("\n==> pr-check: {name}");
        let start = Instant::now();
        let (status, detail) = run_step(root, name, opts);
        failed |= matches!(status, Status::Failed(_));
        outcomes.push(Outcome {
            name,
            status,
            time: start.elapsed(),
            detail,
        });
    }

    println!("\n{}", summary(&outcomes, dirty));
    let failures = outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Failed(_)))
        .count();
    if failures > 0 {
        bail!("pr-check: {failures} step(s) failed");
    }
    Ok(())
}

fn run_step(root: &Path, name: &str, opts: &Options) -> (Status, String) {
    if let Some((_, args)) = CARGO_STEPS.iter().find(|(n, _)| *n == name) {
        let line = format!("cargo {}", args.join(" "));
        let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(*args)
            .current_dir(root)
            .status();
        return match status {
            Ok(s) if s.success() => (Status::Passed, line),
            Ok(s) => (Status::Failed(format!("exited with {s}")), line),
            Err(e) => (Status::Failed(format!("could not start cargo: {e}")), line),
        };
    }
    let result = match name {
        POLICY => crate::policy::run(root, false).map(|()| "cargo xtask policy".to_owned()),
        CONVENTIONS => check_conventions(root, opts),
        _ => unreachable!("every step is handled"),
    };
    match result {
        Ok(detail) => (Status::Passed, detail),
        Err(e) => (Status::Failed(format!("{e:#}")), String::new()),
    }
}

fn check_conventions(root: &Path, opts: &Options) -> Result<String> {
    let base = match &opts.base {
        Some(b) => b.clone(),
        None => conventions::default_base(root)?,
    };
    let (count, mut problems) = conventions::check_range(root, &base, "HEAD")?;
    if let Some(title) = &opts.title {
        for p in conventions::check_header(title) {
            problems.push(format!("title `{title}`: {p}"));
        }
    }
    for p in &problems {
        eprintln!("CONVENTION: {p}");
    }
    let short = &base[..base.len().min(10)];
    if !problems.is_empty() {
        bail!(
            "{} problem(s) in {count} commit(s) since {short}; see CONTRIBUTING.md, \
             \"Commits and pull request titles\"",
            problems.len()
        );
    }
    let title = if opts.title.is_some() {
        " and the title"
    } else {
        ""
    };
    println!("conventions: {count} commit(s) since {short}{title} ok");
    Ok(format!("{count} commit(s) since {short}{title}"))
}

/// The table printed at the end. Pure, for testing.
pub fn summary(outcomes: &[Outcome], dirty: bool) -> String {
    let mut out = String::from("pr-check summary\n");
    for o in outcomes {
        let (mark, note) = match &o.status {
            Status::Passed => ("ok  ", o.detail.clone()),
            Status::Failed(why) => ("FAIL", why.clone()),
            Status::Skipped(why) => ("skip", format!("({why}; CI still runs it)")),
        };
        let time = if o.time.is_zero() {
            String::new()
        } else {
            format!("{:.1} s", o.time.as_secs_f64())
        };
        out.push_str(&format!("  {mark}  {:<20} {time:>8}  {note}\n", o.name));
    }
    let failed = outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Failed(_)))
        .count();
    let skipped = outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Skipped(_)))
        .count();
    if dirty {
        out.push_str(
            "note: uncommitted changes are checked by the cargo steps and the policy, \
             not by the commit check\n",
        );
    }
    out.push_str(&if failed > 0 {
        format!(
            "{failed} of {} step(s) failed: fix them before you open or update the pull request\n",
            outcomes.len()
        )
    } else if skipped > 0 {
        format!("all steps run passed; {skipped} skipped\n")
    } else {
        "all steps passed: the branch is ready for a pull request\n".to_owned()
    });
    out
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &str) -> String {
        std::fs::read_to_string(crate::workspace_root().join(path)).unwrap()
    }

    /// pr-check runs what CI runs: every cargo step is a `run:` line of the CI
    /// job that bears its name, and every step name is a CI job.
    #[test]
    fn steps_are_the_ci_jobs() {
        let ci = read(".github/workflows/ci.yml");
        let pr = read(".github/workflows/pr-conventions.yml");
        for (name, args) in CARGO_STEPS {
            let job = job_body(&ci, name).unwrap_or_else(|| panic!("ci.yml has no job `{name}`"));
            let line = format!("run: cargo {}", args.join(" "));
            assert!(job.contains(&line), "job `{name}` does not run `{line}`");
        }
        let policy = job_body(&ci, POLICY).expect("ci.yml has a policy job");
        assert!(policy.contains("run: cargo xtask policy"));
        let conv = job_body(&pr, CONVENTIONS).expect("pr-conventions.yml has a conventions job");
        assert!(conv.contains("cargo xtask conventions --base"));
    }

    /// The lines of job `name` in a workflow: those indented below `  name:`.
    fn job_body(workflow: &str, name: &str) -> Option<String> {
        let header = format!("  {name}:");
        let mut lines = workflow.lines().skip_while(|l| *l != header);
        lines.next()?;
        let body: Vec<&str> = lines
            .take_while(|l| l.is_empty() || l.starts_with("   "))
            .collect();
        Some(body.join("\n"))
    }

    #[test]
    fn step_names_are_unique() {
        let mut names = step_names();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), step_names().len());
    }

    #[test]
    fn rejects_unknown_skips() {
        let opts = Options {
            base: None,
            title: None,
            skip: vec!["lint".into()],
            fail_fast: false,
        };
        let err = run(&crate::workspace_root(), &opts).unwrap_err();
        assert!(err.to_string().contains("unknown step `lint`"), "{err}");
    }

    #[test]
    fn summarizes_outcomes() {
        let outcome = |name, status, secs: u64| Outcome {
            name,
            status,
            time: Duration::from_millis(secs * 1000),
            detail: "cargo fmt --all --check".into(),
        };
        let all_ok = summary(&[outcome("fmt", Status::Passed, 2)], false);
        assert!(all_ok.contains("ok    fmt"), "{all_ok}");
        assert!(all_ok.contains("2.0 s"), "{all_ok}");
        assert!(all_ok.contains("ready for a pull request"), "{all_ok}");

        let mixed = summary(
            &[
                outcome("fmt", Status::Passed, 1),
                outcome("clippy", Status::Failed("exited with 101".into()), 30),
                outcome("test", Status::Skipped("--fail-fast"), 0),
            ],
            true,
        );
        assert!(mixed.contains("FAIL  clippy"), "{mixed}");
        assert!(mixed.contains("exited with 101"), "{mixed}");
        assert!(mixed.contains("skip  test"), "{mixed}");
        assert!(mixed.contains("CI still runs it"), "{mixed}");
        assert!(mixed.contains("uncommitted changes"), "{mixed}");
        assert!(mixed.contains("1 of 3 step(s) failed"), "{mixed}");

        let skipped = summary(
            &[
                outcome("fmt", Status::Passed, 1),
                outcome("test", Status::Skipped("--skip"), 0),
            ],
            false,
        );
        assert!(skipped.contains("1 skipped"), "{skipped}");
    }
}
