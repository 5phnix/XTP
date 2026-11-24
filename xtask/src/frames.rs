//! `cargo xtask frames`: summarize the GPU meter's frame and drop lines.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};

const TARGET: &str = "xtp_gpu::host";
const ORDER: &[&str] = &[
    "fps",
    "ms_per_frame",
    "preparing_ms",
    "copies_ms",
    "textures_ms",
    "gpu_ms",
    "copy_back_ms",
    "waiting_ms",
    "outside_submissions_ms",
    "submissions_per_frame",
    "dropped_without_pipeline_per_frame",
    "indirect_dropped_per_frame",
];

#[derive(Default)]
struct Report {
    frame_lines: usize,
    other_lines: usize,
    partial_lines: usize,
    metrics: BTreeMap<&'static str, Vec<f64>>,
    drops: BTreeMap<String, Vec<f64>>,
    details: BTreeMap<String, Vec<f64>>,
}

impl Report {
    fn see(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        if let Some(values) = frame(line) {
            self.frame_lines += 1;
            for (name, value) in values {
                self.metrics.entry(name).or_default().push(value);
            }
        } else if let Some(drop) = drop_line(line) {
            let (kind, reason, value) = drop;
            let into = match kind {
                DropKind::Cause => &mut self.drops,
                DropKind::Detail => &mut self.details,
            };
            into.entry(reason).or_default().push(value);
        } else if is_partial(line) {
            self.partial_lines += 1;
        } else {
            self.other_lines += 1;
        }
    }

    fn skipped_lines(&self) -> usize {
        self.other_lines + self.partial_lines
    }

    fn text(&self) -> String {
        let mut out = format!(
            "{} frame line(s), {} other line(s), {} partial frame/drop line(s)\n",
            self.frame_lines, self.other_lines, self.partial_lines
        );
        if !self.metrics.is_empty() {
            out.push_str("\nper frame (times in ms)\n");
            headings(&mut out, "metric");
            for name in ORDER {
                if let Some(values) = self.metrics.get(name) {
                    row(&mut out, name, stats(values, *name == "fps"));
                }
            }
        }
        if !self.drops.is_empty() {
            out.push_str("\ndrop totals by cause (per frame)\n");
            drop_headings(&mut out, "cause");
            for (reason, values) in &self.drops {
                drop_row(&mut out, reason, stats(values, false));
            }
        }
        if !self.details.is_empty() {
            out.push_str("\ndrop details included in those totals (per frame)\n");
            drop_headings(&mut out, "detail");
            for (reason, values) in &self.details {
                drop_row(&mut out, reason, stats(values, false));
            }
        }
        out
    }

    fn json(&self) -> String {
        let mut out = format!(
            "{{\"frame_lines\":{},\"other_lines\":{},\"partial_lines\":{},\"skipped_lines\":{},\"metrics\":{{",
            self.frame_lines,
            self.other_lines,
            self.partial_lines,
            self.skipped_lines()
        );
        let mut first = true;
        for name in ORDER {
            if let Some(values) = self.metrics.get(name) {
                if !first {
                    out.push(',');
                }
                first = false;
                write!(
                    out,
                    "{}:{}",
                    json_string(name),
                    stats_json(values, *name == "fps", false)
                )
                .unwrap();
            }
        }
        out.push_str("},\"drops_per_frame\":{");
        write_drops_json(&mut out, &self.drops);
        out.push_str("},\"drop_details_per_frame\":{");
        write_drops_json(&mut out, &self.details);
        out.push_str("}}");
        out
    }
}

fn write_drops_json(out: &mut String, drops: &BTreeMap<String, Vec<f64>>) {
    for (index, (reason, values)) in drops.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        write!(
            out,
            "{}:{}",
            json_string(reason),
            stats_json(values, false, true)
        )
        .unwrap();
    }
}

fn headings(out: &mut String, first: &str) {
    out.push_str(&format!(
        "{first:<40} {:>10} {:>10} {:>10} {:>10}\n",
        "median", "p10", "p90", "worst"
    ));
}

fn row(out: &mut String, name: &str, s: Stats) {
    writeln!(
        out,
        "{name:<40} {:>10.2} {:>10.2} {:>10.2} {:>10.2}",
        s.median, s.p10, s.p90, s.worst
    )
    .unwrap();
}

fn drop_headings(out: &mut String, first: &str) {
    out.push_str(&format!(
        "{first:<40} {:>6} {:>10} {:>10} {:>10} {:>10}\n",
        "lines", "median", "p10", "p90", "worst"
    ));
}

fn drop_row(out: &mut String, name: &str, s: Stats) {
    writeln!(
        out,
        "{name:<40} {:>6} {:>10.2} {:>10.2} {:>10.2} {:>10.2}",
        s.lines, s.median, s.p10, s.p90, s.worst
    )
    .unwrap();
}

#[derive(Clone, Copy)]
struct Stats {
    lines: usize,
    median: f64,
    p10: f64,
    p90: f64,
    worst: f64,
}

fn stats(values: &[f64], lower_is_worse: bool) -> Stats {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Stats {
        lines: sorted.len(),
        median: percentile(&sorted, 0.5),
        p10: percentile(&sorted, 0.1),
        p90: percentile(&sorted, 0.9),
        worst: if lower_is_worse {
            sorted[0]
        } else {
            sorted[sorted.len() - 1]
        },
    }
}

/// Linear interpolation between the closest ranks, including the endpoints.
fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    let rank = (sorted.len() - 1) as f64 * fraction;
    let low = rank.floor() as usize;
    let high = rank.ceil() as usize;
    let t = rank - low as f64;
    sorted[low] * (1.0 - t) + sorted[high] * t
}

fn stats_json(values: &[f64], lower_is_worse: bool, with_lines: bool) -> String {
    let s = stats(values, lower_is_worse);
    let lines = if with_lines {
        format!("\"lines\":{},", s.lines)
    } else {
        String::new()
    };
    format!(
        "{{{lines}\"median\":{:.6},\"p10\":{:.6},\"p90\":{:.6},\"worst\":{:.6}}}",
        s.median, s.p10, s.p90, s.worst
    )
}

/// The complete frame line. Optional counters are included when present.
fn frame(line: &str) -> Option<Vec<(&'static str, f64)>> {
    if !line.contains(TARGET) || !line.contains(" fps, ") {
        return None;
    }
    let mut values = vec![
        ("fps", before(line, " fps,")?),
        ("ms_per_frame", before(line, " ms/frame")?),
        ("preparing_ms", before(line, " preparing:")?),
        ("copies_ms", after(line, "copies ")?),
        ("textures_ms", after(line, "textures ")?),
        ("gpu_ms", before(line, " on the GPU")?),
        ("copy_back_ms", before(line, " copying back")?),
        ("waiting_ms", before(line, " waiting for them")?),
        (
            "outside_submissions_ms",
            before(line, " outside submissions")?,
        ),
        ("submissions_per_frame", before(line, " submissions/frame")?),
    ];
    if line.contains(" without a host pipeline ") {
        values.push((
            "dropped_without_pipeline_per_frame",
            after(line, "dropped ")?,
        ));
    }
    if let Some(indirect) = before(line, " indirect") {
        values.push(("indirect_dropped_per_frame", indirect));
    }
    Some(values)
}

enum DropKind {
    Cause,
    Detail,
}

/// A diagnostics-only drop line, with its already-normalized per-frame count.
fn drop_line(line: &str) -> Option<(DropKind, String, f64)> {
    if !line.contains(TARGET) {
        return None;
    }
    let (head, reason) = line.split_once("/frame without a host pipeline: ")?;
    let reason = reason.trim();
    if reason.is_empty() {
        return None;
    }
    let value = after(head, "dropped ")?;
    if let Some(cause) = reason.strip_prefix("by cause: ") {
        if cause.trim().is_empty() {
            return None;
        }
        Some((DropKind::Cause, cause.to_string(), value))
    } else {
        Some((DropKind::Detail, reason.to_string(), value))
    }
}

/// A candidate frame or drop line that could not be parsed completely.
fn is_partial(line: &str) -> bool {
    line.contains(TARGET)
        && (line.contains(" fps, ") || line.contains("/frame without a host pipeline"))
}

/// The finite number immediately before `label`.
pub(crate) fn before(line: &str, label: &str) -> Option<f64> {
    let head = &line[..line.find(label)?];
    number(
        head.rsplit(|c: char| c.is_whitespace() || c == '(')
            .next()?,
    )
}

/// The finite number immediately after `label`.
pub(crate) fn after(line: &str, label: &str) -> Option<f64> {
    let tail = &line[line.find(label)? + label.len()..];
    number(tail.split_whitespace().next()?)
}

fn number(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Without ANSI escape sequences (`ESC [ ... letter`). Shared with `play`'s log writer.
pub(crate) fn strip_colours(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn read(path: &Path) -> Result<Report> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    let mut report = Report::default();
    loop {
        bytes.clear();
        let n = reader
            .read_until(b'\n', &mut bytes)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        report.see(&strip_colours(&String::from_utf8_lossy(&bytes)));
    }
    Ok(report)
}

pub fn run(path: &Path, json: bool) -> Result<()> {
    let report = read(path)?;
    if json {
        println!("{}", report.json());
    } else {
        print!("{}", report.text());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "2026-11-24T12:00:00Z INFO xtp_gpu::host: 30.0 fps, 33.3 ms/frame \
        (8.0 preparing: copies 2.0 ms 12.0 MB, textures 1.0 ms 4.0 MB; 12.0 on the GPU; \
        3.0 copying back 5.0 MB; 2.0 waiting for them, 1.0 overlapped; 11.3 outside \
        submissions), 4.0 submissions/frame of 2.0 lists, 100 draws/frame, dropped 6 without \
        a host pipeline (2 being made) and 3 indirect";
    const B: &str = "2026-11-24T12:00:05Z INFO xtp_gpu::host: 20.0 fps, 50.0 ms/frame \
        (14.0 preparing: copies 4.0 ms 20.0 MB, textures 3.0 ms 8.0 MB; 20.0 on the GPU; \
        7.0 copying back 9.0 MB; 4.0 waiting for them, 2.0 overlapped; 25.0 outside \
        submissions), 8.0 submissions/frame of 4.0 lists, 80 draws/frame, dropped 10 without \
        a host pipeline (3 being made) and 5 indirect";
    const CURRENT: &str = "\u{1b}[32m2026-11-25T01:00:00Z INFO xtp_gpu::host: 7.2 fps, 138.2 ms/frame \
        (23.7 preparing: copies 18.0 ms 42.0 MB, textures 2.0 ms 4.0 MB; 44.2 on the GPU; \
        0.0 copying back 0.0 MB; 8.0 waiting for them (3.0 at flips and presents), 2.0 overlapped; \
        60.0 outside submissions; frames in flight off, the longest frame 120 ms), 2.0 \
        submissions/frame of 3.0 lists, the longest 70.0 ms (40.0 on the GPU, 2 lists), buffer \
        copies 4 on the GPU 8.0 MB and 2 on the CPU 4.0 MB, 1 in-list writes on the GPU, 80 \
        draws/frame, dropped 6 without a host pipeline (2 being made) and 3 indirect\u{1b}[0m";

    #[test]
    fn parses_frame_metrics_and_separates_drop_totals_from_details() {
        let mut report = Report::default();
        report.see(A);
        report.see(B);
        report.see(
            "INFO xtp_gpu::host: dropped 1.5/frame without a host pipeline: by cause: test reason",
        );
        report.see(
            "INFO xtp_gpu::host: dropped 2.5/frame without a host pipeline: by cause: test reason",
        );
        report.see("INFO xtp_gpu::host: dropped 1.5/frame without a host pipeline: test reason: [synthetic]");
        assert_eq!(report.frame_lines, 2);
        assert_eq!(report.metrics["fps"], [30.0, 20.0]);
        assert_eq!(report.metrics["copies_ms"], [2.0, 4.0]);
        assert_eq!(report.metrics["outside_submissions_ms"], [11.3, 25.0]);
        assert_eq!(report.metrics["submissions_per_frame"], [4.0, 8.0]);
        assert_eq!(
            report.metrics["dropped_without_pipeline_per_frame"],
            [6.0, 10.0]
        );
        assert_eq!(report.metrics["indirect_dropped_per_frame"], [3.0, 5.0]);
        assert_eq!(report.drops["test reason"], [1.5, 2.5]);
        assert_eq!(report.details["test reason: [synthetic]"], [1.5]);
        assert_eq!(report.skipped_lines(), 0);
    }

    #[test]
    fn current_producer_format_and_ansi_are_parsed() {
        let mut report = Report::default();
        report.see(&strip_colours(CURRENT));
        assert_eq!(report.frame_lines, 1);
        assert_eq!(report.metrics["fps"], [7.2]);
        assert_eq!(report.metrics["copies_ms"], [18.0]);
        assert_eq!(report.metrics["textures_ms"], [2.0]);
        assert_eq!(report.metrics["gpu_ms"], [44.2]);
        assert_eq!(report.metrics["submissions_per_frame"], [2.0]);
        assert_eq!(report.other_lines, 0);
    }

    #[test]
    fn skips_unknown_partial_and_non_finite_lines() {
        let mut report = Report::default();
        report.see("ordinary log line");
        report.see("INFO xtp_gpu::host: 30.0 fps, incomplete");
        report.see(&A.replace("30.0 fps", "NaN fps"));
        report.see("INFO xtp_gpu::host: dropped 2.0/frame without a host pipeline: ");
        report.see("");
        assert_eq!(report.frame_lines, 0);
        assert_eq!(report.other_lines, 1);
        assert_eq!(report.partial_lines, 3);
        assert_eq!(report.skipped_lines(), 4);
        assert!(report.metrics.is_empty());
        assert!(report.drops.is_empty());
    }

    #[test]
    fn invalid_utf8_is_lossy_instead_of_an_error() {
        let path = std::env::temp_dir().join(format!("xtask-frames-{}.log", std::process::id()));
        let mut bytes = A.as_bytes().to_vec();
        bytes.extend_from_slice(b"\n");
        bytes.push(0xff);
        std::fs::write(&path, bytes).unwrap();
        let report = read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(report.frame_lines, 1);
        assert_eq!(report.other_lines, 1);
        assert_eq!(report.partial_lines, 0);
    }

    #[test]
    fn interpolates_percentiles_and_chooses_the_worst_direction() {
        let values = [1.0, 2.0, 3.0, 4.0];
        let time = stats(&values, false);
        assert_eq!(
            (time.median, time.p10, time.p90, time.worst),
            (2.5, 1.3, 3.7, 4.0)
        );
        assert_eq!(stats(&values, true).worst, 1.0);
        assert!(stats(&[f64::MAX, f64::MAX], false).median.is_finite());
        assert_eq!(stats(&[-f64::MAX, f64::MAX], false).median, 0.0);
    }

    #[test]
    fn json_is_stable_and_escapes_reason_strings() {
        let mut report = Report::default();
        report.see(A);
        report.see("INFO xtp_gpu::host: dropped 1.0/frame without a host pipeline: by cause: a \"test\"\\path");
        report.see("INFO xtp_gpu::host: dropped 2.0/frame without a host pipeline: a \"test\"\\path: [synthetic]");
        let json = report.json();
        assert!(json.starts_with(
            "{\"frame_lines\":1,\"other_lines\":0,\"partial_lines\":0,\"skipped_lines\":0,\"metrics\":{\"fps\":"
        ));
        assert!(json.contains("\"a \\\"test\\\"\\\\path\":{\"lines\":1"));
        assert!(json.contains("\"drop_details_per_frame\":{"));
        assert!(json.ends_with("}}"));
        assert_eq!(json_string("line\n\t\u{1}"), "\"line\\n\\t\\u0001\"");
    }
}
