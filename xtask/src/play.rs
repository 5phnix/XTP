//! `cargo xtask play`: the title from a copy of the golden data folder to
//! gameplay in one command (docs/dev/autopilot.md). Builds the diagnostics
//! build, copies `[paths].golden_data` (`XTP_GOLDEN_DATA`) into a run folder
//! of the cache, runs the title with the autopilot script, and leaves the
//! controls to the player once it hands them back. With `--measure`, ends
//! the title after that many seconds of gameplay and prints a summary of
//! its frame-rate lines.
//!
//! Before the title, under the session lock it holds, it runs `xtp
//! precook` on the title's shader pack ([`precook`]), unless
//! `--no-precook`: a session after a translator change then finds its
//! translations and compute pipelines made, and one after none pays a few
//! seconds for the check.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};

/// The title's executable in its folder.
const TITLE: &str = "game_scarlett_master.exe";
/// Where the autopilot logs from.
const AUTOPILOT: &str = "xtp_hle::modules::autopilot";
/// What it logs once the player has control, and when its script gave up
/// (`HANDED_BACK` and `STOPS` in crates/xtp-hle/src/modules/autopilot/engine.rs).
const HANDED_BACK: &str = "control handed back to the player";
const STOPS: &str = "the script stops here";
/// The frame-rate line, every 5 s (crates/xtp-gpu/src/host.rs).
const FRAME_RATE: &str = "xtp_gpu::host";

pub struct Options {
    /// Seconds of gameplay before the title is ended.
    pub measure: Option<u64>,
    pub script: Option<PathBuf>,
    pub exe: Option<PathBuf>,
    pub no_build: bool,
    /// Start the title without `xtp precook` first.
    pub no_precook: bool,
}

pub fn run(root: &Path, options: &Options) -> Result<()> {
    let cfg = xtp_core::Config::load()?;
    let golden = cfg.golden_data.clone().context(
        "no golden data folder: set XTP_GOLDEN_DATA or [paths].golden_data in xtp.toml (see \
         xtp.example.toml)",
    )?;
    ensure!(
        golden.is_dir(),
        "the golden data folder {} does not exist",
        golden.display()
    );
    let game = match &options.exe {
        Some(exe) => exe.clone(),
        None => cfg.require_game_dir()?.join(TITLE),
    };
    ensure!(
        game.is_file(),
        "no title executable at {} (give --exe)",
        game.display()
    );
    let script = options
        .script
        .clone()
        .unwrap_or_else(|| root.join("autopilot").join("gameplay.txt"));
    let text = std::fs::read_to_string(&script)
        .with_context(|| format!("reading {}", script.display()))?;

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

    let console = console(std::env::var("XTP_CONSOLE").ok().as_deref())?;
    let _session = Session::take()?;
    refuse_a_second_title()?;
    if options.no_precook {
        println!("play: no precook (--no-precook)");
    } else {
        precook(&xtp, &game)?;
    }
    let run = cfg.cache_subdir("play")?.join(stamp(SystemTime::now()));
    let data = run.join("data");
    copy_dir(&golden, &data)?;
    let autopilot = run.join("autopilot.txt");
    std::fs::write(&autopilot, with_exit(&text, options.measure))
        .with_context(|| format!("writing {}", autopilot.display()))?;
    let log = run.join("run.log");
    println!("play: run folder {}", run.display());
    println!("play: console {console} (XTP_CONSOLE=series-x for Series X)");
    println!(
        "play: {} drives the title to gameplay{}",
        script.display(),
        match options.measure {
            Some(s) => format!(", which then ends after {s} s"),
            None => ", then the controls are yours".into(),
        }
    );
    let mut child = Command::new(&xtp)
        .arg("run")
        .arg(&game)
        .arg("--execute")
        .arg("--autopilot")
        .arg(&autopilot)
        .args(["--console", console])
        // Over the config file's: the title writes only the copy.
        .env("XTP_DATA_DIR", &data)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", xtp.display()))?;
    let outcome = watch(&mut child, &log, options.measure.is_some())?;

    println!();
    println!("play: log {}", log.display());
    for line in outcome.report() {
        println!("play: {line}");
    }
    if options.measure.is_some() {
        if let Some(stop) = &outcome.stopped {
            bail!("the autopilot did not reach gameplay: {stop}");
        }
        ensure!(
            outcome.gameplay.is_some(),
            "the title ended before the autopilot reached gameplay"
        );
    }
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
        .args(["build", "--profile", "diag", "-p", "xtp", "--features"])
        .arg("diagnostics")
        .current_dir(root)
        .status()
        .context("running cargo build")?;
    ensure!(status.success(), "cargo build failed ({status})");
    Ok(())
}

/// The name the precook runs under, beside `xtp.exe` ([`precook`]).
const PRECOOK_EXE: &str = "xtp-precook";

/// `xtp precook` on the title's shader pack (`shaders.rpf` beside `game`),
/// before the title and under the session lock this run holds (it passes
/// `--lock-holder`), with this run's environment: the switches that change
/// translation are read as the title will read them. It runs as a copy of
/// `xtp` named [`PRECOOK_EXE`]: the same bytes, so the same translator and
/// cache key, but no `xtp.exe` that session tools watching for the title
/// would take for it (session.ps1 times its hard stop from the title's
/// start). A precook that fails is reported, and the title starts with the
/// cache as it is.
fn precook(xtp: &Path, game: &Path) -> Result<()> {
    let pack = game.with_file_name("shaders.rpf");
    if !pack.is_file() {
        println!("play: no {} to precook", pack.display());
        return Ok(());
    }
    let exe = xtp.with_file_name(format!("{PRECOOK_EXE}{}", std::env::consts::EXE_SUFFIX));
    if let Err(e) = std::fs::copy(xtp, &exe) {
        println!(
            "play: no precook: copying {} to {}: {e}; the title starts with the cache as it is",
            xtp.display(),
            exe.display()
        );
        return Ok(());
    }
    println!(
        "play: precook of {} (--no-precook skips it)",
        pack.display()
    );
    let started = Instant::now();
    let status = Command::new(&exe)
        .arg("precook")
        .arg(&pack)
        .args(["--lock-holder", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .status();
    let seconds = started.elapsed().as_secs_f64();
    match status {
        Ok(status) if status.success() => println!("play: precook done in {seconds:.0} s"),
        Ok(status) => println!(
            "play: precook failed ({status}) after {seconds:.0} s; the title starts with the \
             cache as it is"
        ),
        Err(e) => {
            println!("play: precook did not start ({e}); the title starts with the cache as it is")
        }
    }
    Ok(())
}

/// The console `xtp run` gets: `XTP_CONSOLE` (`series-x` or `series-s`),
/// else Series S, as `xtp run` picks it.
fn console(env: Option<&str>) -> Result<&'static str> {
    match env.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("series-s") => Ok("series-s"),
        Some("series-x") => Ok("series-x"),
        Some(other) => bail!("XTP_CONSOLE={other}: expected series-x or series-s"),
    }
}

/// The script, with `exit after` the measured seconds at its end: they
/// count from when it hands control back.
fn with_exit(script: &str, measure: Option<u64>) -> String {
    let mut text = script.to_string();
    if let Some(seconds) = measure {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text += &format!("\n# cargo xtask play --measure {seconds}\nexit after {seconds}\n");
    }
    text
}

/// `YYYYMMDD-HHMMSS`, UTC.
fn stamp(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, time) = (secs / 86_400, secs % 86_400);
    // Days to a civil date (Howard Hinnant's `civil_from_days`).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry.with_context(|| format!("reading {}", from.display()))?;
        let target = to.join(entry.path().strip_prefix(from)?);
        let done = if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)
        } else {
            std::fs::copy(entry.path(), &target).map(drop)
        };
        done.with_context(|| format!("copying to {}", target.display()))?;
    }
    Ok(())
}

/// `XTP_SESSION_LOCK`, when set: a file that says a game session runs, our
/// process id on its first line, held for the run. Build tools that wait
/// for it keep the measurements clean; a file whose process is gone is
/// stale.
struct Session(Option<PathBuf>);

impl Session {
    fn take() -> Result<Session> {
        let Some(path) = std::env::var_os("XTP_SESSION_LOCK").filter(|p| !p.is_empty()) else {
            return Ok(Session(None));
        };
        let path = PathBuf::from(path);
        let mut told = false;
        loop {
            match std::fs::File::create_new(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}\ncargo xtask play", std::process::id())?;
                    return Ok(Session(Some(path)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let holder = text.lines().next().and_then(|l| l.trim().parse().ok());
                    if let Some(pid) = holder
                        && !alive(pid)
                    {
                        println!(
                            "play: removing the stale {} (pid {pid} is gone)",
                            path.display()
                        );
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if !told {
                        println!(
                            "play: another game session holds {}; waiting",
                            path.display()
                        );
                        told = true;
                    }
                    std::thread::sleep(Duration::from_secs(5));
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Lines of `tasklist` for processes matching `filter`, empty elsewhere.
fn tasks(filter: &str) -> String {
    if !cfg!(windows) {
        return String::new();
    }
    Command::new("tasklist")
        .args(["/FI", filter, "/NH", "/FO", "CSV"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn alive(pid: u32) -> bool {
    !cfg!(windows) || tasks(&format!("PID eq {pid}")).contains(&format!("\"{pid}\""))
}

/// One title at a time on a machine.
fn refuse_a_second_title() -> Result<()> {
    // Only an xtp.exe with a window is a title: offline `xtp translate` or
    // `shader-info` runs by other workers have none and must not block a
    // session (2026-11-25: they did).
    let running = if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq xtp.exe", "/FI", "WINDOWTITLE ne N/A"])
            .args(["/NH", "/FO", "CSV"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    } else {
        String::new()
    };
    ensure!(
        !running.to_ascii_lowercase().contains("\"xtp.exe\""),
        "an xtp.exe is already running; close it first (one title at a time)"
    );
    Ok(())
}

/// What the run showed.
#[derive(Debug, Default, PartialEq)]
struct Outcome {
    /// When the autopilot handed control back, from the start.
    gameplay: Option<Duration>,
    /// The autopilot's line saying why its script stopped.
    stopped: Option<String>,
    /// Frame-rate lines from gameplay on.
    frame_rates: Vec<String>,
    /// The title's exit code; `None` when ended without one.
    code: Option<i32>,
}

impl Outcome {
    /// Takes one line of the title's log, `at` after its start.
    fn see(&mut self, line: &str, at: Duration) {
        if line.contains(AUTOPILOT) && line.contains(HANDED_BACK) && self.gameplay.is_none() {
            self.gameplay = Some(at);
        } else if line.contains(AUTOPILOT) && line.contains(STOPS) {
            self.stopped = Some(line.to_string());
        } else if self.gameplay.is_some() && line.contains(FRAME_RATE) && line.contains(" fps, ") {
            self.frame_rates.push(line.to_string());
        }
    }

    fn report(&self) -> Vec<String> {
        let mut lines = Vec::new();
        match (self.gameplay, &self.stopped) {
            (Some(at), _) => lines.push(format!("gameplay after {:.0} s", at.as_secs_f64())),
            (None, Some(stop)) => lines.push(format!("autopilot stopped: {stop}")),
            (None, None) => lines.push("gameplay not reached".into()),
        }
        lines.extend(summary(&self.frame_rates));
        // In an A/B run (`XTP_*=ab`), the same figures per arm.
        lines.extend(xtp_core::ab::summary(&self.frame_rates, &FIGURES));
        lines.push(match self.code {
            Some(code) => format!("title exit code {code}"),
            None => "title ended without an exit code".into(),
        });
        lines
    }
}

/// Runs until the title ends: its output goes on to ours and, without
/// colours, to `log`; with `measuring`, a stopped script ends the title.
fn watch(child: &mut Child, log: &Path, measuring: bool) -> Result<Outcome> {
    let mut file =
        std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().context("the title's output")?;
    let stderr = child.stderr.take().context("the title's errors")?;
    // Not joined: a process the title starts can keep its pipes open.
    drop(pump(stdout, tx.clone(), false));
    drop(pump(stderr, tx, true));
    let started = Instant::now();
    let mut outcome = Outcome::default();
    let mut ended: Option<Instant> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                writeln!(file, "{line}").with_context(|| format!("writing {}", log.display()))?;
                let was_stopped = outcome.stopped.is_some();
                outcome.see(&line, started.elapsed());
                if measuring && !was_stopped && outcome.stopped.is_some() {
                    println!("play: the autopilot stopped; ending the title");
                    let _ = child.kill();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if ended.is_none() && child.try_wait()?.is_some() {
            ended = Some(Instant::now());
        }
        // What is left of its output comes in this time.
        if ended.is_some_and(|at| at.elapsed() > Duration::from_secs(2)) {
            break;
        }
    }
    outcome.code = child.wait()?.code();
    Ok(outcome)
}

/// Copies lines from `from` to our output or errors, and sends each
/// without colours to `to`.
fn pump(
    from: impl Read + Send + 'static,
    to: mpsc::Sender<String>,
    errors: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(from);
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
            let _ = if errors {
                std::io::stderr().write_all(&line)
            } else {
                std::io::stdout().write_all(&line)
            };
            let text = strip_colours(&String::from_utf8_lossy(&line));
            if to.send(text.trim_end().to_string()).is_err() {
                return;
            }
            line.clear();
        }
    })
}

/// Without ANSI escape sequences (`ESC [ ... letter`).
fn strip_colours(text: &str) -> String {
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

/// The number written just before `label` (`30.0 fps`).
fn before(line: &str, label: &str) -> Option<f64> {
    let head = &line[..line.find(label)?];
    head.rsplit(|c: char| c.is_whitespace() || c == '(')
        .next()?
        .parse()
        .ok()
}

/// The number written just after `label` (`dropped 12`).
fn after(line: &str, label: &str) -> Option<f64> {
    let tail = &line[line.find(label)? + label.len()..];
    tail.split_whitespace().next()?.parse().ok()
}

/// Reads one figure of a frame-rate line.
type Figure = fn(&str) -> Option<f64>;

/// The figures of a frame-rate line the end table gives.
const FIGURES: [(&str, Figure); 7] = [
    ("fps", |l| before(l, " fps,")),
    ("ms/frame", |l| before(l, " ms/frame")),
    ("ms preparing", |l| before(l, " preparing")),
    ("ms on the GPU", |l| before(l, " on the GPU")),
    ("ms copying back", |l| before(l, " copying back")),
    ("draws/frame", |l| before(l, " draws/frame")),
    ("dropped/frame", |l| after(l, "dropped ")),
];

/// Median, lowest and highest of each figure of the frame-rate lines.
fn summary(lines: &[String]) -> Vec<String> {
    if lines.is_empty() {
        return vec!["no frame-rate line in gameplay (one comes every 5 s)".into()];
    }
    let mut out = vec![format!(
        "{} frame-rate lines in gameplay (median, lowest, highest):",
        lines.len()
    )];
    for (name, read) in FIGURES {
        let mut values: Vec<f64> = lines.iter().filter_map(|l| read(l)).collect();
        if values.is_empty() {
            continue;
        }
        values.sort_by(f64::total_cmp);
        out.push(format!(
            "  {name:<16} {:>8.1} {:>8.1} {:>8.1}",
            values[values.len() / 2],
            values[0],
            values[values.len() - 1]
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORLD: &str = "2026-11-24T13:52:27.256955Z  INFO xtp_gpu::host: 1.4 fps, 714.3 ms/frame \
        (365.2 preparing: copies 280.0 ms 212.3 MB, textures 0.0 ms 0.0 MB; 250.5 on the GPU; \
        98.0 copying back 40.0 MB), 2400 draws/frame, dropped 1250 without a host pipeline \
        (12 being made) and 230 indirect";

    #[test]
    fn play_runs_series_s_unless_told() {
        assert_eq!(console(None).unwrap(), "series-s");
        assert_eq!(console(Some("")).unwrap(), "series-s");
        assert_eq!(console(Some(" Series-X ")).unwrap(), "series-x");
        assert!(console(Some("one")).is_err());
    }

    #[test]
    fn play_reads_the_frame_rate_lines_figures() {
        assert_eq!(before(WORLD, " fps,"), Some(1.4));
        assert_eq!(before(WORLD, " ms/frame"), Some(714.3));
        assert_eq!(before(WORLD, " preparing"), Some(365.2));
        assert_eq!(before(WORLD, " on the GPU"), Some(250.5));
        assert_eq!(before(WORLD, " copying back"), Some(98.0));
        assert_eq!(before(WORLD, " draws/frame"), Some(2400.0));
        assert_eq!(after(WORLD, "dropped "), Some(1250.0));
        assert_eq!(before(WORLD, " absent"), None);

        let lines: Vec<String> = ["1.4", "2.0", "0.9"]
            .iter()
            .map(|fps| WORLD.replace("1.4 fps", &format!("{fps} fps")))
            .collect();
        let report = summary(&lines);
        assert_eq!(
            report[0],
            "3 frame-rate lines in gameplay (median, lowest, highest):"
        );
        assert_eq!(report[1], "  fps                   1.4      0.9      2.0");
        assert_eq!(report.len(), 8);
        assert_eq!(summary(&[]).len(), 1);
    }

    #[test]
    fn play_counts_frame_rates_from_gameplay_on() {
        let autopilot = |text: &str| format!("2026-11-24T13:52:27Z  INFO {AUTOPILOT}: {text}");
        let mut outcome = Outcome::default();
        outcome.see(WORLD, Duration::from_secs(10));
        outcome.see(
            &autopilot("line 9: control handed back to the player"),
            Duration::from_secs(61),
        );
        outcome.see(WORLD, Duration::from_secs(65));
        // Only the autopilot's own lines count.
        outcome.see(
            "INFO xtp_hle::modules::xuser: control handed back to the player",
            Duration::from_secs(70),
        );
        assert_eq!(outcome.gameplay, Some(Duration::from_secs(61)));
        assert_eq!(outcome.frame_rates, [WORLD]);

        let mut outcome = Outcome::default();
        let stop = autopilot(
            "line 5: timed out after 120.0 s (frame 900, u8 0x74e007f = 0x0); the script stops \
             here and presses nothing more: the title is yours",
        );
        outcome.see(&stop, Duration::from_secs(125));
        assert_eq!(outcome.stopped.as_deref(), Some(stop.as_str()));
        assert_eq!(outcome.report()[0], format!("autopilot stopped: {stop}"));
    }

    /// Frame-rate lines tagged with their A/B phase (`xtp_core::ab`): the
    /// end table adds the median per arm of those past settling.
    #[test]
    fn play_gives_an_ab_runs_medians_per_arm() {
        let tagged = |tag: &str, fps: &str| {
            WORLD
                .replace("xtp_gpu::host: ", &format!("xtp_gpu::host: {tag} "))
                .replace("1.4 fps", &format!("{fps} fps"))
        };
        let mut outcome = Outcome {
            gameplay: Some(Duration::from_secs(30)),
            ..Outcome::default()
        };
        for (tag, fps) in [
            ("[A/B A0 settling]", "9.0"),
            ("[A/B A0]", "1.4"),
            ("[A/B A0]", "1.2"),
            ("[A/B B1 settling]", "9.0"),
            ("[A/B B1]", "2.1"),
        ] {
            outcome.see(&tagged(tag, fps), Duration::from_secs(40));
        }
        assert_eq!(outcome.frame_rates.len(), 5);
        let report = outcome.report();
        let at = report
            .iter()
            .position(|l| l.starts_with("A/B, median per arm"))
            .expect("an A/B table");
        assert_eq!(
            report[at],
            "A/B, median per arm of the frame lines past settling (2 A, 1 B; 2 settling left out):"
        );
        assert_eq!(
            report[at + 2],
            "  fps                   1.4      2.1   +50.0%"
        );
        assert_eq!(
            report[at + 3],
            "  ms/frame            714.3    714.3    +0.0%"
        );
        // Seven figures, then the exit code.
        assert_eq!(report.len(), at + 2 + 7 + 1);
        // Without tags, no A/B table.
        let plain = Outcome {
            gameplay: Some(Duration::from_secs(30)),
            frame_rates: vec![WORLD.into()],
            ..Outcome::default()
        };
        assert!(!plain.report().iter().any(|l| l.starts_with("A/B")));
    }

    #[test]
    fn play_appends_the_measured_exit_to_the_script() {
        assert_eq!(with_exit("release", None), "release");
        assert_eq!(
            with_exit("release", Some(30)),
            "release\n\n# cargo xtask play --measure 30\nexit after 30\n"
        );
    }

    #[test]
    fn play_strips_colours_from_the_log() {
        assert_eq!(
            strip_colours("\u{1b}[2m2026\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m x"),
            "2026  INFO x"
        );
    }

    #[test]
    fn play_names_runs_by_utc_date_and_time() {
        let at = UNIX_EPOCH + Duration::from_secs(1_795_525_524);
        assert_eq!(stamp(at), "20261124-130524");
        assert_eq!(stamp(UNIX_EPOCH), "19700101-000000");
        // A leap day.
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(stamp(leap), "20000229-000000");
    }

    #[test]
    fn play_copies_the_golden_folder() {
        let dir = std::env::temp_dir().join(format!("xtask-play-{}", std::process::id()));
        let golden = dir.join("golden");
        std::fs::create_dir_all(golden.join("7952987A/storage")).unwrap();
        std::fs::write(golden.join("7952987A/storage/save"), b"save").unwrap();
        copy_dir(&golden, &dir.join("copy")).unwrap();
        let copied = std::fs::read(dir.join("copy/7952987A/storage/save")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(copied, b"save");
    }
}
