//! `cargo xtask replay <dir>`: the fast loop of docs/analysis/frame-replay.md.
//! Builds the diagnostics `xtp`, replays the recording, and prints each
//! frame's numbers beside the previous run's (kept in the recording's
//! `replay/last.jsonl`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

pub struct Options {
    pub dir: PathBuf,
    pub no_build: bool,
    /// Arguments passed on to `xtp replay` (frames, loops, pokes, stats).
    pub pass: Vec<String>,
}

/// The numbers this title's lighting work looks at, when no `--stat` is
/// given: the main depth's coverage, the G-buffer albedo, and the scene
/// colour after the deferred lighting pixel (docs/analysis/image-status.md).
const STATS: [&str; 3] = ["depth=ds", "albedo=1280x720:f27", "light=1cd9bc634417a981"];

pub fn run(root: &Path, options: &Options) -> Result<()> {
    let xtp = target_dir(root)
        .join("diag")
        .join(format!("xtp{}", std::env::consts::EXE_SUFFIX));
    if !options.no_build {
        build(root)?;
    }
    ensure!(
        xtp.is_file(),
        "no {} (build it, or drop --no-build)",
        xtp.display()
    );
    let mut args: Vec<String> = vec!["replay".into(), options.dir.display().to_string()];
    args.extend(options.pass.iter().cloned());
    if !options.pass.iter().any(|a| a == "--stat") {
        for s in STATS {
            args.extend(["--stat".into(), s.into()]);
        }
    }
    let started = std::time::Instant::now();
    let output = Command::new(&xtp)
        .args(&args)
        .output()
        .with_context(|| format!("running {}", xtp.display()))?;
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    ensure!(
        output.status.success(),
        "xtp replay failed ({})",
        output.status
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let folder = options.dir.join("replay");
    std::fs::create_dir_all(&folder)?;
    let last = folder.join("last.jsonl");
    let before = std::fs::read_to_string(&last).unwrap_or_default();
    print!("{}", compare(&before, &text));
    println!("replay: {:.1} s in all", started.elapsed().as_secs_f64());
    if !before.is_empty() {
        std::fs::write(folder.join("previous.jsonl"), &before)?;
    }
    std::fs::write(&last, &text).with_context(|| format!("writing {}", last.display()))?;
    Ok(())
}

/// Where cargo builds: the folder this xtask was built in (`<target>/debug`).
fn target_dir(root: &Path) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.to_path_buf()))
        .unwrap_or_else(|| root.join("target"))
}

/// The diagnostics build; cargo does nothing when it is up to date.
fn build(root: &Path) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "build",
            "--profile",
            "diag",
            "-p",
            "xtp",
            "--features",
            "diagnostics",
        ])
        .current_dir(root)
        .status()
        .context("running cargo build")?;
    ensure!(status.success(), "cargo build failed ({status})");
    Ok(())
}

/// A flat JSON object's fields as text (numbers, strings, null).
fn fields(line: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let body = line.trim().trim_start_matches('{').trim_end_matches('}');
    let (mut parts, mut current, mut quoted, mut escaped) =
        (Vec::new(), String::new(), false, false);
    for c in body.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);
    for part in parts {
        if let Some((k, v)) = part.split_once("\":") {
            out.insert(
                k.trim().trim_start_matches('"').to_owned(),
                v.trim().trim_matches('"').to_owned(),
            );
        }
    }
    out
}

/// Each frame's numbers, with the previous run's beside those that changed.
fn compare(before: &str, now: &str) -> String {
    let key = |f: &BTreeMap<String, String>| (f.get("pass").cloned(), f.get("frame").cloned());
    let old: Vec<BTreeMap<String, String>> = before.lines().map(fields).collect();
    let mut out = String::new();
    for line in now.lines().filter(|l| l.starts_with('{')) {
        let f = fields(line);
        let previous = old.iter().find(|o| key(o) == key(&f));
        out.push_str(&format!(
            "frame {} (pass {}):",
            f.get("frame").map_or("?", String::as_str),
            f.get("pass").map_or("?", String::as_str)
        ));
        for (k, v) in &f {
            if matches!(k.as_str(), "frame" | "pass" | "image") {
                continue;
            }
            match previous.and_then(|p| p.get(k)) {
                Some(was) if was != v => out.push_str(&format!(" {k}={v} (was {was})")),
                _ => out.push_str(&format!(" {k}={v}")),
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_compared_field_by_field() {
        let before =
            r#"{"frame":618,"pass":0,"draws":10,"light":0.1,"image":"E:\\r\\frame-618.png"}"#;
        let now = r#"{"frame":618,"pass":0,"draws":10,"light":57.1,"image":"E:\\r\\frame-618.png"}
{"frame":619,"pass":0,"draws":3,"light":null}"#;
        let text = compare(before, now);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "frame 618 (pass 0): draws=10 light=57.1 (was 0.1)"
        );
        assert_eq!(lines[1], "frame 619 (pass 0): draws=3 light=null");
        let f = fields(r#"{"a":"x,y","b":2}"#);
        assert_eq!(f.get("a").map(String::as_str), Some("x,y"));
    }
}
