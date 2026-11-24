//! The autopilot (`xtp run --autopilot <script>`, docs/dev/autopilot.md):
//! presses pad buttons when the title is in the right state, read from its
//! memory and its presented frames, then hands control back to the player.
//! Scripts live in `autopilot/`; their RVAs belong to one build of the title
//! and are refused on another.
//!
//! [`script`] reads a script, [`engine`] runs it a tick at a time, this
//! module runs the engine on a thread of its own against the live title:
//! its memory read as `XTP_WATCH` reads it, its buttons on the pad that
//! plays (`gameinput::hold_buttons`).

mod engine;
mod script;

use std::path::Path;
use std::time::{Duration, Instant};

use self::engine::{Engine, Status, Title};
use self::script::{Memory, Width};
use super::gameinput;

/// How often the autopilot looks at the title (as `XTP_WATCH` does).
const TICK: Duration = Duration::from_millis(5);

/// Reads the script at `path`, checks that it belongs to this build of the
/// title (`build`, the BLAKE3 of its executable, hex) and that what it reads
/// lies in the title's image (mapped at `base`, `size` bytes), then runs it
/// from now on, on a thread of its own.
pub fn start(path: &Path, build: &str, base: usize, size: usize) -> Result<(), String> {
    let name = path.display();
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {name}: {e}"))?;
    let script = script::parse(&text, gameinput::button).map_err(|e| format!("{name}: {e}"))?;
    if let Some(theirs) = &script.build
        && !build.to_ascii_lowercase().starts_with(theirs.as_str())
    {
        return Err(format!(
            "{name} is for the title build {theirs}, not this one ({build}): its RVAs would \
             read the wrong memory. Find them again for this build (docs/dev/autopilot.md)"
        ));
    }
    for (line, memory) in script.memory() {
        if memory.rva + memory.width.bytes() > size as u64 {
            return Err(format!(
                "{name}: line {line}: {memory} is past the end of the title's image ({size:#x} \
                 bytes)"
            ));
        }
    }
    tracing::info!(
        "{name}: {} steps, title build {}",
        script.steps.len(),
        script.build.as_deref().unwrap_or("any")
    );
    let engine = Engine::new(script);
    std::thread::Builder::new()
        .name("xtp-autopilot".into())
        .spawn(move || run(engine, &Live { base, size }))
        .map(drop)
        .map_err(|e| format!("starting the autopilot: {e}"))
}

/// The title running in this process.
struct Live {
    base: usize,
    size: usize,
}

impl Title for Live {
    fn read(&self, memory: Memory) -> Option<u64> {
        let end = memory.rva.checked_add(memory.width.bytes())?;
        if end > self.size as u64 {
            return None;
        }
        let at = self.base + memory.rva as usize;
        // SAFETY: inside the mapped image (checked above), whose sections
        // stay mapped while the title runs; aligned to the width (the
        // script refuses other RVAs, and the image base is page aligned).
        // Volatile: the title's threads write it.
        let value = unsafe {
            match memory.width {
                Width::U8 => u64::from((at as *const u8).read_volatile()),
                Width::U16 => u64::from((at as *const u16).read_volatile()),
                Width::U32 => u64::from((at as *const u32).read_volatile()),
                Width::U64 => (at as *const u64).read_volatile(),
            }
        };
        Some(value)
    }

    fn frames(&self) -> u64 {
        xtp_gpu::queue::presented()
    }
}

fn run(mut engine: Engine, title: &Live) {
    let start = Instant::now();
    let mut held = 0;
    loop {
        let tick = engine.tick(start.elapsed(), title);
        for log in &tick.logs {
            if log.error {
                tracing::error!("{}", log.text);
            } else {
                tracing::info!("{}", log.text);
            }
        }
        if tick.buttons != held {
            held = tick.buttons;
            gameinput::hold_buttons(held);
        }
        match tick.status {
            Status::Exit => end(),
            Status::Running => {}
            Status::Done | Status::Stopped if engine.exit_pending() => {}
            Status::Done | Status::Stopped => return,
        }
        std::thread::sleep(TICK);
    }
}

/// Ends the process, as the title's own end does (`lifecycle`): the cursor
/// let go, compiled pipelines kept for the next run.
fn end() -> ! {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        gameinput::release_mouse();
        xtp_gpu::pipeline::save_pipeline_cache();
        // SAFETY: ends this process; the title's threads go with it.
        unsafe { TerminateProcess(GetCurrentProcess(), 0) };
    }
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &tempfile::TempDir, text: &str) -> std::path::PathBuf {
        let path = dir.path().join("script.txt");
        std::fs::write(&path, text).unwrap();
        path
    }

    const BUILD: &str = "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93";

    #[test]
    fn autopilot_refuses_another_build_and_memory_past_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(&dir, "build b99f876b74d4a77f\nwait u32 0xfc == 0\n");
        let e = start(&path, &BUILD.replace('b', "c"), 0, 0x100).unwrap_err();
        assert!(
            e.contains("is for the title build b99f876b74d4a77f, not this one (c99f"),
            "{e}"
        );
        let path = script(&dir, "build b99f876b74d4a77f\nwait u32 0x100 == 0\n");
        let e = start(&path, BUILD, 0, 0x100).unwrap_err();
        assert!(
            e.ends_with("line 2: u32 0x100 is past the end of the title's image (0x100 bytes)"),
            "{e}"
        );
        let e = start(&dir.path().join("none.txt"), BUILD, 0, 0x100).unwrap_err();
        assert!(e.starts_with("reading "), "{e}");
    }

    #[test]
    fn autopilot_reads_the_live_image_by_width() {
        let image: Vec<u64> = vec![0x1122_3344_5566_7788, 0];
        let live = Live {
            base: image.as_ptr() as usize,
            size: 16,
        };
        let at = |rva, width| live.read(Memory { rva, width });
        assert_eq!(at(0, Width::U8), Some(0x88));
        assert_eq!(at(2, Width::U16), Some(0x5566));
        assert_eq!(at(4, Width::U32), Some(0x1122_3344));
        assert_eq!(at(0, Width::U64), Some(0x1122_3344_5566_7788));
        assert_eq!(at(8, Width::U64), Some(0));
        assert_eq!(at(12, Width::U64), None);
        assert_eq!(at(u64::MAX, Width::U8), None);
    }
}
