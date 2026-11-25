//! `xtp replay`: a recording of the title's GPU work (`xtp run --record`,
//! `XTP_RECORD`) replayed without the title, one JSON line per frame
//! (docs/analysis/frame-replay.md).
//!
//! The replay sets itself up as the session was (`info.txt`: the console
//! model and the session's `XTP_*` switches, unless this process sets them
//! itself), and captures each replayed frame: `XTP_CAPTURE` (the `--out`
//! folder by default) gets `frame-<n>.png` for the session's frame numbers,
//! and `XTP_PASS_DUMP`, `XTP_FRAME_PROBE` and `XTP_PRESENT_TARGET` work as
//! in a session.

#![cfg_attr(
    not(all(windows, feature = "diagnostics")),
    allow(dead_code, unused_imports)
)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// What `xtp replay` was asked.
pub(crate) struct Args {
    pub(crate) dir: PathBuf,
    pub(crate) frames: Option<String>,
    pub(crate) loops: u32,
    pub(crate) out: Option<PathBuf>,
    pub(crate) pokes: Vec<String>,
    pub(crate) poke_file: Option<PathBuf>,
    pub(crate) stats: Vec<String>,
    pub(crate) session_gpu_pages: bool,
}

/// What `info.txt` says of the session.
#[derive(Debug, Default, PartialEq)]
struct Info {
    console: Option<String>,
    env: Vec<(String, String)>,
    window: Option<u64>,
    last: Option<u64>,
}

fn info(text: &str) -> Info {
    let mut info = Info::default();
    for line in text.lines() {
        if let Some(var) = line.strip_prefix("env ") {
            if let Some((k, v)) = var.split_once('=') {
                info.env.push((k.to_owned(), v.to_owned()));
            }
        } else if let Some((k, v)) = line.split_once('=') {
            match k {
                "console" => info.console = Some(v.to_owned()),
                "window" => info.window = v.parse().ok(),
                "last" => info.last = v.parse().ok(),
                _ => {}
            }
        }
    }
    info
}

/// Switches the replay chooses itself, never taken from the session:
/// recording, captures and what they write.
fn replay_owns(var: &str) -> bool {
    [
        "XTP_RECORD",
        "XTP_CAPTURE",
        "XTP_FRAME_PROBE",
        "XTP_PASS_",
        "XTP_PRESENT_TARGET",
        "XTP_DUMP_TARGETS",
        "XTP_PLAY",
    ]
    .iter()
    .any(|p| var.starts_with(p))
}

/// `a..b`, `a..=b` or `a`: frames by session number.
fn frame_range(text: &str) -> Result<(u64, u64)> {
    let number = |s: &str| {
        s.trim()
            .parse::<u64>()
            .with_context(|| format!("--frames {text}"))
    };
    Ok(match text.split_once("..") {
        Some((a, b)) => (number(a)?, number(b.trim_start_matches('='))?),
        None => {
            let n = number(text)?;
            (n, n)
        }
    })
}

/// A `--stat <name>=<what>`: `ds` (the largest depth target's coverage),
/// `<program hash>` (the first output of the first pass running it), or
/// `<width>x<height>:f<format>` (the last output of that size and format).
#[derive(Debug, Clone, PartialEq)]
struct Stat {
    name: String,
    what: String,
}

fn stat(text: &str) -> Result<Stat> {
    let Some((name, what)) = text.split_once('=') else {
        bail!("--stat {text}: expected <name>=<ds|program|WxH:fN>");
    };
    Ok(Stat {
        name: name.trim().to_owned(),
        what: what.trim().to_owned(),
    })
}

/// The share of texels not zero ("lit") of an output line of a pass dump.
fn lit(line: &str) -> Option<f64> {
    let at = line.find(": lit ")? + ": lit ".len();
    line[at..].split('%').next()?.trim().parse().ok()
}

/// A pass dump's numbers for `stats`, and its ops and drops.
fn pass_stats(dump: &str, stats: &[Stat]) -> Vec<(String, Option<f64>)> {
    // The first section lists passes with their outputs; the second (`==
    // Ops`) repeats them op by op.
    let passes = dump.split("\n== ").nth(1).unwrap_or("");
    let mut out = Vec::new();
    if let Some(head) = dump.lines().next() {
        let field = |word: &str| -> Option<f64> {
            let at = head.find(word)?;
            head[..at]
                .split_whitespace()
                .last()?
                .trim_start_matches('(')
                .replace(',', "")
                .parse()
                .ok()
        };
        out.push(("ops".to_owned(), field(" ops")));
        out.push(("dropped".to_owned(), field(" dropped")));
    }
    for s in stats {
        let value = if s.what == "ds" {
            // The largest depth target, as the frame left it.
            let mut best: Option<(u64, f64)> = None;
            for line in passes
                .lines()
                .filter(|l| l.trim_start().starts_with("-> ds "))
            {
                let size = line
                    .split_whitespace()
                    .nth(3)
                    .and_then(|s| s.split_once('x'))
                    .and_then(|(w, h)| {
                        Some(w.parse::<u64>().ok()? * h.trim_end_matches(':').parse::<u64>().ok()?)
                    });
                if let (Some(size), Some(lit)) = (size, lit(line))
                    && best.is_none_or(|(b, _)| size >= b)
                {
                    best = Some((size, lit));
                }
            }
            best.map(|b| b.1)
        } else if s.what.contains('x') {
            let want = s.what.replace(":f", " f");
            passes
                .lines()
                .filter(|l| l.trim_start().starts_with("-> ") && l.contains(&format!(" {want}:")))
                .rev()
                .find_map(lit)
        } else {
            let mut lines = passes
                .lines()
                .skip_while(|l| !(l.starts_with('P') && l.contains(&s.what)));
            lines.next();
            lines.take_while(|l| l.starts_with("    ")).find_map(lit)
        };
        out.push((s.name.clone(), value));
    }
    out
}

/// A JSON string.
fn quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn number(v: Option<f64>) -> String {
    v.map_or("null".to_owned(), |v| format!("{v}"))
}

pub(crate) fn run(args: Args) -> Result<()> {
    #[cfg(not(all(windows, feature = "diagnostics")))]
    {
        let _ = args;
        bail!("xtp replay needs a diagnostics build (--features diagnostics)")
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    replay(args)
}

#[cfg(all(windows, feature = "diagnostics"))]
fn replay(args: Args) -> Result<()> {
    let dir = &args.dir;
    let text = std::fs::read_to_string(dir.join("info.txt"))
        .with_context(|| format!("{}: not a recording (no info.txt)", dir.display()))?;
    let session = info(&text);
    let (Some(window), Some(last)) = (session.window, session.last) else {
        bail!("{}: the recording never reached its frames", dir.display());
    };
    let out = args.out.clone().unwrap_or_else(|| dir.join("replay"));
    std::fs::create_dir_all(&out).with_context(|| out.display().to_string())?;
    let frames = args.frames.as_deref().map(frame_range).transpose()?;
    let (first, end) = frames.unwrap_or((window + 1, last));
    let set = |k: &str, v: &str| {
        // SAFETY: no thread of ours runs yet (the replay has not started).
        unsafe { std::env::set_var(k, v) }
    };
    // The session's switches, unless set here.
    let mut taken = Vec::new();
    for (k, v) in &session.env {
        if !replay_owns(k) && std::env::var_os(k).is_none() {
            set(k, v);
            taken.push(format!("{k}={v}"));
        }
    }
    // SAFETY: as above.
    unsafe {
        std::env::remove_var("XTP_RECORD");
        std::env::remove_var("XTP_CAPTURE_WORLD");
    }
    if std::env::var_os("XTP_CAPTURE").is_none() {
        set("XTP_CAPTURE", &out.display().to_string());
    }
    if std::env::var_os("XTP_CAPTURE_FRAMES").is_none() {
        let list: Vec<String> = (first..=end).map(|f| f.to_string()).collect();
        set("XTP_CAPTURE_FRAMES", &list.join(","));
    }
    let capture = PathBuf::from(std::env::var_os("XTP_CAPTURE").unwrap_or_default());
    xtp_hle::console::set(match session.console.as_deref() {
        Some("series-x") => xtp_hle::console::Console::SeriesX,
        _ => xtp_hle::console::Console::SeriesS,
    });
    let cfg = xtp_core::Config::load()?;
    xtp_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let mut pokes = Vec::new();
    if let Some(file) = &args.poke_file {
        pokes.extend(xtp_gpu::record::read_pokes(file).map_err(anyhow::Error::msg)?);
    }
    for p in &args.pokes {
        pokes.push(xtp_gpu::record::parse_poke(p).map_err(anyhow::Error::msg)?);
    }
    let stats: Vec<Stat> = args.stats.iter().map(|s| stat(s)).collect::<Result<_>>()?;
    eprintln!(
        "replay: {} frames {}..={} (session), {} switch(es) from the session{}{}",
        dir.display(),
        window + 1,
        last,
        taken.len(),
        if taken.is_empty() {
            String::new()
        } else {
            format!(": {}", taken.join(" "))
        },
        if pokes.is_empty() {
            String::new()
        } else {
            format!(", {} poke(s)", pokes.len())
        },
    );
    let options = xtp_gpu::record::Options {
        dir: dir.clone(),
        frames,
        loops: args.loops.max(1),
        pokes,
        session_gpu_pages: args.session_gpu_pages,
        own_process: true,
    };
    let started = std::time::Instant::now();
    let summary = xtp_gpu::record::run(&options, |f| println!("{}", line(f, &capture, &stats)))
        .map_err(anyhow::Error::msg)?;
    eprintln!(
        "replay: {} frame(s) in {:.1} s (setup {:.1} s: {} calls made, {} left out; {} snapshot pages not placed)",
        summary.frames.len(),
        started.elapsed().as_secs_f64(),
        summary.setup_ms as f64 / 1000.0,
        summary.journal_calls,
        summary.journal_skipped,
        summary.unplaced_pages,
    );
    Ok(())
}

/// A frame's JSON line: the replay's counts, the presented image and the
/// pass dump's numbers when they were written.
#[cfg(all(windows, feature = "diagnostics"))]
fn line(f: &xtp_gpu::record::Frame, capture: &Path, stats: &[Stat]) -> String {
    let image = capture.join(format!("frame-{}.png", f.frame));
    let hash = std::fs::read(&image)
        .ok()
        .map(|b| blake3::hash(&b).to_hex()[..16].to_owned());
    let dump = std::fs::read_to_string(capture.join(format!("passes-{}.txt", f.frame))).ok();
    let mut fields = vec![
        format!("\"frame\":{}", f.frame),
        format!("\"pass\":{}", f.pass),
        format!("\"calls\":{}", f.calls),
        format!("\"draws\":{}", f.draws),
        format!("\"dispatches\":{}", f.dispatches),
        format!("\"drawn\":{}", f.drawn),
        format!("\"dispatched\":{}", f.dispatched),
        format!(
            "\"not_run\":{}",
            (f.draws + f.dispatches).saturating_sub(f.drawn + f.dispatched)
        ),
        format!("\"cpu_pages\":{}", f.pages),
        format!("\"gpu_pages_same\":{}", f.gpu_same),
        format!("\"gpu_pages_written\":{}", f.gpu_written),
        format!("\"gpu_pages_different\":{}", f.gpu_different),
        format!("\"missing_objects\":{}", f.missing),
        format!("\"ms\":{}", f.ms),
        format!(
            "\"image\":{}",
            if image.exists() {
                quoted(&image.display().to_string())
            } else {
                "null".into()
            }
        ),
        format!(
            "\"image_hash\":{}",
            hash.map_or("null".into(), |h| quoted(&h))
        ),
    ];
    if let Some(dump) = dump {
        for (name, value) in pass_stats(&dump, stats) {
            fields.push(format!("{}:{}", quoted(&name), number(value)));
        }
    }
    format!("{{{}}}", fields.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_is_read_from_its_info() {
        let i = info(
            "console=series-s\nenv XTP_STRIDE_TABLE=1\nenv XTP_CAPTURE=E:\\x\nwindow=617\nlast=620\n",
        );
        assert_eq!(i.console.as_deref(), Some("series-s"));
        assert_eq!((i.window, i.last), (Some(617), Some(620)));
        assert_eq!(i.env.len(), 2);
        assert!(!replay_owns("XTP_STRIDE_TABLE"));
        assert!(
            replay_owns("XTP_CAPTURE")
                && replay_owns("XTP_PASS_DUMP")
                && replay_owns("XTP_RECORD_FRAMES")
        );
    }

    #[test]
    fn frames_and_stats_are_read() {
        assert_eq!(frame_range("618..620").unwrap(), (618, 620));
        assert_eq!(frame_range("618..=620").unwrap(), (618, 620));
        assert_eq!(frame_range("619").unwrap(), (619, 619));
        assert!(frame_range("a..b").is_err());
        assert_eq!(
            stat("light=1cd9bc634417a981").unwrap().what,
            "1cd9bc634417a981"
        );
        assert!(stat("light").is_err());
    }

    #[test]
    fn pass_dumps_give_their_numbers() {
        let dump = "XTP pass dump of frame 621: 91 lists, 811 passes, 7516 ops (285 dropped).\n\
            A pass is ...\n\
            \n== Passes\n\
            P1 L1 2/2 ran, programs [aaaa]\n\
            \x20   -> ds 0x1 1280x720 f19: lit 8.3% max (1.0000)\n\
            P2 L1 1/1 ran, programs [bbbb, cccc]\n\
            \x20   -> rt 0x2 1280x720 f27: lit 40.5% max (0.5)\n\
            \x20   -> ds 0x1 1280x720 f19: lit 79.5% max (1.0000)\n\
            \x20   -> ds 0x3 32x8 f53: lit 100.0% max (1.0000)\n\
            P3 L2 1/1 ran, programs [dddd]\n\
            \x20   -> rt 0x4 1280x720 f10: lit 6.7% max (0.01)\n\
            \n== Ops\n\
            P3 dddd\n";
        let stats = [
            stat("depth=ds").unwrap(),
            stat("albedo=1280x720:f27").unwrap(),
            stat("light=dddd").unwrap(),
            stat("none=eeee").unwrap(),
        ];
        let got = pass_stats(dump, &stats);
        let get = |n: &str| got.iter().find(|(k, _)| k == n).unwrap().1;
        assert_eq!(get("ops"), Some(7516.0));
        assert_eq!(get("dropped"), Some(285.0));
        assert_eq!(get("depth"), Some(79.5));
        assert_eq!(get("albedo"), Some(40.5));
        assert_eq!(get("light"), Some(6.7));
        assert_eq!(get("none"), None);
        assert_eq!(quoted("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
