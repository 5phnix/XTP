//! Repository maintenance. Run with `cargo xtask <command>`.

mod conventions;
mod frames;
mod play;
mod policy;
mod pr_check;
mod replay;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Check commit messages, a pull request title or description against
    /// the conventions of CONTRIBUTING.md. Checks each thing it is given.
    Conventions {
        /// With --head: check every commit of base..head (merges excepted).
        #[arg(long, requires = "head")]
        base: Option<String>,
        #[arg(long, requires = "base")]
        head: Option<String>,
        /// A pull request title.
        #[arg(long)]
        title: Option<String>,
        /// A commit message file, as the commit-msg hook gets it.
        #[arg(long)]
        message_file: Option<PathBuf>,
        /// A pull request description: it must state its provenance.
        #[arg(long)]
        body_file: Option<PathBuf>,
    },
    /// Run locally what CI runs on a pull request (fmt, both clippy runs,
    /// tests, policy, commit conventions) and print a summary.
    PrCheck {
        /// Compare the branch with this commit (default: where it left
        /// upstream/main, origin/main or main).
        #[arg(long)]
        base: Option<String>,
        /// Also check the title you will give the pull request.
        #[arg(long)]
        title: Option<String>,
        /// Leave out a step (fmt, clippy, clippy-diagnostics, test, policy,
        /// conventions); repeatable.
        #[arg(long, value_name = "STEP")]
        skip: Vec<String>,
        /// Stop running steps after the first failure.
        #[arg(long)]
        fail_fast: bool,
    },
    /// Summarize a run log's frame times, stage costs and drop counters.
    Frames {
        /// A log captured from a run (no game data is copied into the repository).
        #[arg(value_name = "RUN.LOG")]
        log: PathBuf,
        /// Print the same summary as JSON for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Point git at .githooks/: commits run the policy and message checks.
    InstallHooks,
    /// Run the title from a copy of the golden data folder to gameplay with
    /// the autopilot, then leave the controls to you (docs/dev/autopilot.md).
    Play {
        /// End the title after this many seconds of gameplay and print a
        /// summary of its frame-rate lines.
        #[arg(long, value_name = "SECONDS")]
        measure: Option<u64>,
        /// The autopilot script (default: autopilot/gameplay.txt).
        #[arg(long)]
        script: Option<PathBuf>,
        /// The title executable (default: game_scarlett_master.exe in the
        /// game folder).
        #[arg(long)]
        exe: Option<PathBuf>,
        /// Run the diagnostics build as it is, without cargo build.
        #[arg(long)]
        no_build: bool,
        /// Start the title without `xtp precook` first.
        #[arg(long)]
        no_precook: bool,
    },
    /// Build the diagnostics xtp, replay a recording of the title's GPU
    /// work (`xtp run --record`) and print each frame's numbers beside the
    /// previous replay's (docs/analysis/frame-replay.md).
    Replay {
        /// The recording's folder.
        dir: PathBuf,
        /// Run the diagnostics build as it is, without cargo build.
        #[arg(long)]
        no_build: bool,
        /// Passed on to `xtp replay`: --frames, --loop, --poke, --pokes-file,
        /// --stat, --session-gpu-pages.
        #[arg(last = true)]
        pass: Vec<String>,
    },
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::Conventions {
            base,
            head,
            title,
            message_file,
            body_file,
        } => check_conventions(&root, base.zip(head), title, message_file, body_file),
        Cmd::PrCheck {
            base,
            title,
            skip,
            fail_fast,
        } => pr_check::run(
            &root,
            &pr_check::Options {
                base,
                title,
                skip,
                fail_fast,
            },
        ),
        Cmd::Frames { log, json } => frames::run(&log, json),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            println!(
                "hooks installed: commits now run `cargo xtask policy --staged` \
                 and check their message (`cargo xtask conventions`)"
            );
            Ok(())
        }
        Cmd::Play {
            measure,
            script,
            exe,
            no_build,
            no_precook,
        } => play::run(
            &root,
            &play::Options {
                measure,
                script,
                exe,
                no_build,
                no_precook,
            },
        ),
        Cmd::Replay {
            dir,
            no_build,
            pass,
        } => replay::run(
            &root,
            &replay::Options {
                dir,
                no_build,
                pass,
            },
        ),
    }
}

fn check_conventions(
    root: &Path,
    range: Option<(String, String)>,
    title: Option<String>,
    message_file: Option<PathBuf>,
    body_file: Option<PathBuf>,
) -> Result<()> {
    anyhow::ensure!(
        range.is_some() || title.is_some() || message_file.is_some() || body_file.is_some(),
        "nothing to check: give --base and --head, --title, --message-file or --body-file"
    );
    let mut problems = Vec::new();
    if let Some((base, head)) = &range {
        let (count, found) = conventions::check_range(root, base, head)?;
        println!("conventions: {count} commit(s) in {base}..{head}");
        problems.extend(found);
    }
    if let Some(title) = &title {
        problems.extend(
            conventions::check_header(title)
                .into_iter()
                .map(|p| format!("title `{title}`: {p}")),
        );
    }
    if let Some(path) = &message_file {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let message = conventions::strip_comments(&raw);
        // git's own merge messages: merges are not checked (see `commits`).
        if !message.starts_with("Merge ") {
            problems.extend(
                conventions::check_message(&message)
                    .into_iter()
                    .map(|p| format!("commit message: {p}")),
            );
        }
    }
    let mut body_problems = Vec::new();
    if let Some(path) = &body_file {
        let body =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        body_problems.extend(
            conventions::check_body(&body)
                .into_iter()
                .map(|p| format!("description: {p}")),
        );
    }
    if problems.is_empty() && body_problems.is_empty() {
        println!("conventions: ok");
        return Ok(());
    }
    for p in problems.iter().chain(&body_problems) {
        eprintln!("CONVENTION: {p}");
    }
    if !problems.is_empty() {
        eprintln!("\n{}", conventions::help());
    }
    let section = if problems.is_empty() {
        "Provenance"
    } else {
        "Commits and pull request titles"
    };
    anyhow::bail!(
        "{} convention problem(s); see CONTRIBUTING.md, \"{section}\"",
        problems.len() + body_problems.len()
    )
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
