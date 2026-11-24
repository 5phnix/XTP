//! The commit-message and pull-request conventions of CONTRIBUTING.md
//! ("Commits and pull request titles", "Provenance").
//!
//! A header is `<area>[/<topic>][, <area>[/<topic>]...]: <summary>`, at most
//! [`MAX_HEADER`] characters, with no trailing period. The areas are those of
//! [`AREAS`], the same names as the `area/...` labels of .github/labeler.yml.
//! The same rules apply to commit messages and to pull request titles (a pull
//! request is squashed into one commit whose message is its title).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const MAX_HEADER: usize = 72;

/// Every area a header may name, with what it covers (shown in errors).
pub const AREAS: &[(&str, &str)] = &[
    ("core", "crates/xtp-core"),
    ("pe", "crates/xtp-pe"),
    ("loader", "crates/xtp-loader"),
    ("hle", "crates/xtp-hle"),
    ("gpu", "crates/xtp-gpu"),
    ("shader", "crates/xtp-shader"),
    ("formats", "crates/xtp-formats"),
    ("pipeline", "crates/xtp-pipeline"),
    ("game", "crates/xtp-game"),
    ("app", "apps/xtp"),
    ("xtask", "xtask"),
    ("tests", "tests/"),
    ("ci", ".github/, .githooks/"),
    ("docs", "docs/ and Markdown files"),
    ("deps", "dependency updates (Cargo.lock)"),
    (
        "build",
        "workspace manifest, toolchain, .cargo/, repository settings",
    ),
];

/// Prefixes git itself writes, followed by a header that must be valid.
const AUTOSQUASH: &[&str] = &["fixup! ", "squash! ", "amend! "];

/// Problems with one header (the first line of a message, or a PR title).
pub fn check_header(header: &str) -> Vec<String> {
    let mut header = header.trim_end_matches('\r');
    while let Some(rest) = AUTOSQUASH.iter().find_map(|p| header.strip_prefix(p)) {
        header = rest;
    }
    if let Some(inner) = header
        .strip_prefix("Revert \"")
        .and_then(|h| h.strip_suffix('"'))
    {
        return check_header(inner);
    }

    let mut problems = Vec::new();
    let Some((areas, summary)) = header.split_once(':') else {
        problems.push(format!("`{header}` has no `<area>: ` prefix"));
        return problems;
    };
    for area in areas.split(',') {
        let area = area.trim_start_matches(' ');
        let (name, topic) = area.split_once('/').unwrap_or((area, ""));
        if !AREAS.iter().any(|(a, _)| *a == name) {
            problems.push(format!("unknown area `{name}`"));
        }
        if area.split_once('/').is_some() && !is_topic(topic) {
            problems.push(format!(
                "topic `{topic}` after `{name}/` must be lowercase letters, digits, `-` or `_`"
            ));
        }
    }
    match summary.strip_prefix(' ') {
        None => problems.push("one space must follow the colon".to_owned()),
        Some(text) => {
            if text.trim().is_empty() {
                problems.push("the summary after the colon is empty".to_owned());
            } else if text.starts_with(' ') || text.ends_with(' ') {
                problems.push("the summary has extra spaces around it".to_owned());
            }
            if text.ends_with('.') {
                problems.push("the summary ends with a period".to_owned());
            }
            let first_word = text.split_whitespace().next().unwrap_or("");
            if first_word
                .trim_matches(|c: char| !c.is_alphanumeric())
                .eq_ignore_ascii_case("wip")
            {
                problems.push(
                    "work in progress goes in a draft pull request, not in a header".to_owned(),
                );
            }
        }
    }
    let len = header.chars().count();
    if len > MAX_HEADER {
        problems.push(format!("{len} characters, more than {MAX_HEADER}"));
    }
    problems
}

/// Problems with a whole commit message: its header, and the blank line
/// that separates it from a body.
pub fn check_message(message: &str) -> Vec<String> {
    let mut lines = message.lines();
    let header = lines.next().unwrap_or("");
    if header.trim().is_empty() {
        return vec!["the message is empty or starts with a blank line".to_owned()];
    }
    let mut problems = check_header(header);
    if let Some(second) = lines.next()
        && !second.trim().is_empty()
    {
        problems.push("a blank line must separate the header from the body".to_owned());
    }
    problems
}

/// A message as git hands it to the commit-msg hook: comment lines and
/// everything below the scissors line are not part of the commit.
pub fn strip_comments(message: &str) -> String {
    let mut out = String::new();
    for line in message.lines() {
        if line.starts_with("# ------------------------ >8") {
            break;
        }
        if !line.starts_with('#') {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_start_matches('\n').to_owned()
}

/// Problems with a pull request description: it must keep the template's
/// Provenance section with at least one ticked box.
pub fn check_body(body: &str) -> Vec<String> {
    let mut in_section = false;
    let mut found = false;
    let mut ticked = false;
    for line in body.lines() {
        let line = line.trim();
        if let Some(heading) = line.strip_prefix('#') {
            in_section = heading
                .trim_start_matches('#')
                .trim()
                .eq_ignore_ascii_case("provenance");
            found |= in_section;
            continue;
        }
        if in_section {
            let lower = line.to_ascii_lowercase();
            ticked |= lower.starts_with("- [x]") || lower.starts_with("* [x]");
        }
    }
    if !found {
        vec![
            "the description has no `## Provenance` section (keep the pull request template)"
                .to_owned(),
        ]
    } else if !ticked {
        vec!["no box is ticked under `## Provenance`: state where the code comes from".to_owned()]
    } else {
        Vec::new()
    }
}

/// One commit of a range: its id and its message.
#[derive(Debug, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub message: String,
}

/// Parses `git log --format=%H%x00%B%x1e` output.
pub fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\x1e')
        .filter_map(|record| {
            let (id, message) = record.trim_start_matches('\n').split_once('\0')?;
            Some(Commit {
                id: id.to_owned(),
                message: message.to_owned(),
            })
        })
        .collect()
}

/// The commits in `base..head`, oldest first, merge commits left out (a
/// pull request is squashed, and merging `main` into a branch is allowed).
pub fn commits(root: &Path, base: &str, head: &str) -> Result<Vec<Commit>> {
    let out = Command::new("git")
        .args([
            "log",
            "--no-merges",
            "--reverse",
            "--format=%H%x00%B%x1e",
            &format!("{base}..{head}"),
        ])
        .current_dir(root)
        .output()
        .context("running git log")?;
    if !out.status.success() {
        bail!(
            "git log {base}..{head} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_log(&String::from_utf8_lossy(&out.stdout)))
}

/// Problems with every commit of `base..head`, each prefixed with the short
/// commit id and its header. Also returns how many commits were checked.
pub fn check_range(root: &Path, base: &str, head: &str) -> Result<(usize, Vec<String>)> {
    let commits = commits(root, base, head)?;
    let mut problems = Vec::new();
    for c in &commits {
        let header = c.message.lines().next().unwrap_or("");
        let short = &c.id[..c.id.len().min(10)];
        for p in check_message(&c.message) {
            problems.push(format!("commit {short} `{header}`: {p}"));
        }
    }
    Ok((commits.len(), problems))
}

/// The commit to compare a local branch against: where it left the first of
/// `upstream/main`, `origin/main` and `main` that exists.
pub fn default_base(root: &Path) -> Result<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    for candidate in ["upstream/main", "origin/main", "main"] {
        if git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{candidate}^{{commit}}"),
        ])
        .is_some()
        {
            return git(&["merge-base", "HEAD", candidate])
                .with_context(|| format!("no common ancestor of HEAD and {candidate}"));
        }
    }
    bail!("none of upstream/main, origin/main or main exists: pass --base <commit>")
}

fn is_topic(topic: &str) -> bool {
    !topic.is_empty()
        && topic
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The rules and the areas, printed under header problems.
pub fn help() -> String {
    let mut out = format!(
        "A header is `<area>: <summary>` (or `<area>/<topic>: ...`, `<area>, <area>: ...`),\n\
         at most {MAX_HEADER} characters, without a final period. Areas:\n"
    );
    for (area, what) in AREAS {
        out.push_str(&format!("  {area:<9} {what}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(header: &str) {
        assert_eq!(check_header(header), Vec::<String>::new(), "{header}");
    }

    fn bad(header: &str, expected: &str) {
        let problems = check_header(header);
        assert!(
            problems.iter().any(|p| p.contains(expected)),
            "{header}: expected a problem containing `{expected}`, got {problems:?}"
        );
    }

    #[test]
    fn accepts_conforming_headers() {
        ok("gpu: clamp indirect draw arguments");
        ok("hle/audio: decode Opus streams in parallel");
        ok("gpu, shader: translate 64-bit image atomics");
        ok("deps: Bump windows-sys from 0.61.2 to 0.62.0");
        ok("ci: bump the actions group with 2 updates");
        ok("fixup! loader: map title DLLs on first use");
        ok("Revert \"formats: detile 3D textures\"");
        ok(&format!("docs: {}", "x".repeat(MAX_HEADER - 6)));
    }

    #[test]
    fn rejects_malformed_headers() {
        bad("Fix the loader", "no `<area>: ` prefix");
        bad("renderer: fix it", "unknown area `renderer`");
        bad("xtp-gpu: fix it", "unknown area `xtp-gpu`");
        bad("gpu:fix it", "one space must follow");
        bad("gpu: ", "summary after the colon is empty");
        bad("gpu: fix it.", "ends with a period");
        bad("gpu: WIP indirect draws", "draft pull request");
        bad("gpu/Draws: fix", "topic `Draws`");
        bad("gpu/: fix", "topic ``");
        bad(
            &format!("docs: {}", "x".repeat(MAX_HEADER - 5)),
            "more than 72",
        );
        bad("Revert \"no area here\"", "no `<area>: ` prefix");
        bad("fixup! bad", "no `<area>: ` prefix");
    }

    #[test]
    fn checks_the_blank_line_before_the_body() {
        assert!(check_message("core: add x\n\nWhy it is needed.\n").is_empty());
        assert!(check_message("core: add x\n").is_empty());
        assert!(!check_message("core: add x\nWhy it is needed.\n").is_empty());
        assert!(!check_message("\ncore: add x\n").is_empty());
        assert!(!check_message("").is_empty());
    }

    #[test]
    fn strips_what_git_does_not_commit() {
        let edited = "core: add x\n\nbody\n# Please enter the commit message\n\
                      # ------------------------ >8 ------------------------\ndiff --git a b\n";
        assert_eq!(strip_comments(edited), "core: add x\n\nbody\n");
    }

    #[test]
    fn requires_a_ticked_provenance_box() {
        let template = include_str!("../../.github/pull_request_template.md");
        assert!(
            !check_body(template).is_empty(),
            "the untouched template must not pass"
        );
        let first_box = template.replacen("- [ ]", "- [x]", 1);
        assert!(
            !check_body(&first_box).is_empty(),
            "a box ticked outside Provenance does not count"
        );
        let (before, after) = template
            .split_once("## Provenance")
            .expect("the template has a Provenance section");
        let filled = format!(
            "{before}## Provenance{}",
            after.replacen("- [ ]", "- [x]", 1)
        );
        assert_eq!(check_body(&filled), Vec::<String>::new());
        assert!(!check_body("Fixes the loader.").is_empty());
    }

    #[test]
    fn parses_git_log_records() {
        let out = "aaa\0core: one\n\nbody\n\x1e\nbbb\0gpu: two\n\x1e\n";
        assert_eq!(
            parse_log(out),
            vec![
                Commit {
                    id: "aaa".into(),
                    message: "core: one\n\nbody\n".into()
                },
                Commit {
                    id: "bbb".into(),
                    message: "gpu: two\n".into()
                },
            ]
        );
    }

    /// The areas are the `area/...` labels the labeler applies, and each
    /// crate of the workspace has its area.
    #[test]
    fn areas_match_the_labeler_and_the_crates() {
        let labeler = include_str!("../../.github/labeler.yml");
        let mut labels: Vec<&str> = labeler
            .lines()
            .filter_map(|l| l.strip_prefix("area/"))
            .filter_map(|l| l.strip_suffix(':'))
            .collect();
        let mut areas: Vec<&str> = AREAS.iter().map(|(a, _)| *a).collect();
        labels.sort_unstable();
        areas.sort_unstable();
        assert_eq!(labels, areas);

        let crates = crate::workspace_root().join("crates");
        for entry in std::fs::read_dir(crates).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let area = name.strip_prefix("xtp-").expect("crates are named xtp-*");
            assert!(areas.contains(&area), "crate {name} has no area");
            assert!(
                labeler.contains(&format!("'crates/{name}/**'")),
                "crate {name} has no labeler glob"
            );
        }
    }

    /// A directory under the system's temporary folder, removed on drop.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("xtask-{name}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Checks a real range of commits in a scratch repository.
    #[test]
    fn checks_a_range_of_commits() {
        let dir = ScratchDir::new("conventions-range");
        let hooks = dir.path().join("no-hooks");
        std::fs::create_dir(&hooks).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-c", "user.name=xtask", "-c", "user.email=xtask@invalid"])
                .args(["-c", "commit.gpgsign=false", "-c"])
                .arg(format!("core.hooksPath={}", hooks.display()))
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(status.status.success(), "git {args:?}: {status:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "Initial commit"]);
        git(&["checkout", "-q", "-b", "topic"]);
        git(&["commit", "-q", "--allow-empty", "-m", "core: add a thing"]);
        git(&["commit", "-q", "--allow-empty", "-m", "Fix stuff."]);

        let (count, problems) = check_range(dir.path(), "main", "topic").unwrap();
        assert_eq!(count, 2, "the commit on main is not in the range");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("`Fix stuff.`"), "{problems:?}");
        assert!(check_range(dir.path(), "main", "no-such-branch").is_err());
    }
}
