//! The title's Bink movies, seen through its import table (EXPERIMENT).
//!
//! `XTP_INTRO_TRACE` (any value) logs every movie the title opens, closes,
//! seeks or pauses: its path, length, and the title code that asked.
//! `XTP_INTRO_EXPERIMENT_SKIP=<substring>,...` makes the opens of the
//! movies whose path holds one of the substrings fail (mode `fail`, the
//! default) or report a one-frame movie to the title's probe (mode
//! `frames1`, `XTP_INTRO_EXPERIMENT_MODE`).
//!
//! The title (1.0.2.1) opens a movie twice: a probe (`0x442ea70`) opens it,
//! copies its frame count and rate into the movie object and closes it;
//! the playback open (`0x442efe0`, from `0x199ef20`) keeps the handle. Its
//! player (`0x2ef0e0`) decodes while the handle's last decoded frame is
//! below the frame count the probe copied. A failed playback open puts the
//! movie object in state 3 (`0x19463d0`).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::{BUILD_1_0_2_1, KnownBuild};

/// Set (any value) to log the title's movies.
pub const TRACE_ENV: &str = "XTP_INTRO_TRACE";
/// Comma-separated path substrings of the movies to skip (experiment).
pub const SKIP_ENV: &str = "XTP_INTRO_EXPERIMENT_SKIP";
/// `fail` (default) or `frames1` (experiment).
pub const MODE_ENV: &str = "XTP_INTRO_EXPERIMENT_MODE";

/// Where one build keeps what the hook needs, by RVA.
#[derive(Debug, Clone, Copy)]
pub struct Sites {
    pub build: &'static KnownBuild,
    /// Import slots of `Bink2Scarlett.dll` functions.
    pub open_slot: usize,
    pub close_slot: usize,
    pub goto_slot: usize,
    pub pause_slot: usize,
    /// Return addresses of the two `BinkOpenWithOptions` calls.
    pub probe_return: usize,
    pub playback_return: usize,
}

pub const SITES: &[Sites] = &[Sites {
    build: &BUILD_1_0_2_1,
    open_slot: 0x6794210,
    close_slot: 0x67941c0,
    goto_slot: 0x6794200,
    pause_slot: 0x6794218,
    probe_return: 0x442ebec,
    playback_return: 0x442f05c,
}];

/// The open flag that says the "name" is a movie in memory.
const FROM_MEMORY: u32 = 0x0400_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Fail,
    Frames1,
}

struct State {
    base: usize,
    size: usize,
    sites: Sites,
    trace: bool,
    skip: Vec<String>,
    mode: Mode,
    /// Open handles: path, opened when, and the frame count to put back.
    open: Mutex<HashMap<usize, OpenMovie>>,
}

/// An open handle's path, when it was opened, and the frame count to put
/// back.
type OpenMovie = (String, Instant, Option<u32>);

static STATE: OnceLock<State> = OnceLock::new();
static REAL_OPEN: AtomicUsize = AtomicUsize::new(0);
static REAL_CLOSE: AtomicUsize = AtomicUsize::new(0);
static REAL_GOTO: AtomicUsize = AtomicUsize::new(0);
static REAL_PAUSE: AtomicUsize = AtomicUsize::new(0);

type OpenFn = unsafe extern "system" fn(*const c_void, *const c_void, u32) -> *mut c_void;
type CloseFn = unsafe extern "system" fn(*mut c_void);
type GotoFn = unsafe extern "system" fn(*mut c_void, u32, i32);
type PauseFn = unsafe extern "system" fn(*mut c_void, i32) -> i32;

/// The public head of a Bink 2 handle (`bink.h`).
#[repr(C)]
struct BinkHead {
    width: u32,
    height: u32,
    frames: u32,
    frame_num: u32,
    last_frame_num: u32,
    frame_rate: u32,
    frame_rate_div: u32,
}

/// Installs the trace or the experiment when asked. Call once the title's
/// imports are bound, before any of its code runs.
///
/// # Safety
/// `base..base + size` is the mapped, bound image of the executable whose
/// original BLAKE3 is `blake3_hex`, and no title code runs yet.
#[cfg(windows)]
pub unsafe fn apply_if_asked(blake3_hex: &str, base: usize, size: u32) {
    let trace = std::env::var_os(TRACE_ENV).is_some();
    let skip: Vec<String> = std::env::var(SKIP_ENV)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if !trace && skip.is_empty() {
        return;
    }
    let Some(sites) = SITES
        .iter()
        .find(|s| s.build.blake3.eq_ignore_ascii_case(blake3_hex))
    else {
        tracing::warn!("intro: no movie sites are known for this build; nothing installed");
        return;
    };
    let mode = match std::env::var(MODE_ENV).as_deref() {
        Ok("frames1") => Mode::Frames1,
        _ => Mode::Fail,
    };
    let size = size as usize;
    let slots = [
        (sites.open_slot, open as *const () as usize, &REAL_OPEN),
        (sites.close_slot, close as *const () as usize, &REAL_CLOSE),
        (sites.goto_slot, goto as *const () as usize, &REAL_GOTO),
        (sites.pause_slot, pause as *const () as usize, &REAL_PAUSE),
    ];
    if slots.iter().any(|&(rva, _, _)| rva + 8 > size) {
        tracing::warn!("intro: an import slot lies outside the image; nothing installed");
        return;
    }
    let _ = STATE.set(State {
        base,
        size,
        sites: *sites,
        trace,
        skip,
        mode,
        open: Mutex::new(HashMap::new()),
    });
    for (rva, ours, real) in slots {
        let slot = (base + rva) as *mut usize;
        // SAFETY: inside the image (checked above), bound, no title code runs.
        let bound = unsafe { slot.read_volatile() };
        if bound == 0 {
            tracing::warn!(
                rva = format_args!("{rva:#x}"),
                "intro: an import slot is empty"
            );
            return;
        }
        real.store(bound, Ordering::SeqCst);
        // SAFETY: as above.
        if let Err(e) = unsafe { write_slot(slot, ours) } {
            tracing::warn!("intro: writing the import slot {rva:#x} failed: {e}");
            return;
        }
    }
    let state = STATE.get().unwrap();
    tracing::info!(
        trace,
        skip = ?state.skip,
        mode = ?state.mode,
        "intro: movie imports wrapped (experiment)"
    );
    if trace {
        watch_states(base, size);
    }
}

/// The title's state words (docs/analysis/run-keys.md), logged with a
/// timestamp when they change, polled every 5 ms (experiment).
#[cfg(windows)]
fn watch_states(base: usize, size: usize) {
    const WORDS: [(usize, &str); 4] = [
        (0x74e2c1c, "front end"),
        (0xab43664, "loading screen"),
        (0x74e007c, "start-up flags"),
        (0x9e58138, "not in game"),
    ];
    if WORDS.iter().any(|&(rva, _)| rva + 4 > size) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("xtp-intro-states".into())
        .spawn(move || {
            let mut seen = [None; WORDS.len()];
            loop {
                for (&(rva, what), seen) in WORDS.iter().zip(&mut seen) {
                    // SAFETY: four bytes of the mapped image's .data.
                    let value = unsafe { ((base + rva) as *const u32).read_volatile() };
                    if *seen != Some(value) {
                        tracing::info!(
                            rva = format_args!("{rva:#x}"),
                            what,
                            value = format_args!("{value:#x}"),
                            "intro: title state"
                        );
                        *seen = Some(value);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
}

#[cfg(windows)]
unsafe fn write_slot(slot: *mut usize, value: usize) -> std::io::Result<()> {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let mut old = 0;
    // SAFETY: the caller's slot is mapped; only its protection changes.
    if unsafe { VirtualProtect(slot as _, 8, PAGE_READWRITE, &mut old) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: writable now; restored right after.
    unsafe {
        slot.write_volatile(value);
        VirtualProtect(slot as _, 8, old, &mut old);
    }
    Ok(())
}

/// The first return address in the title's image, as an RVA, and the next
/// few.
#[cfg(windows)]
fn callers(state: &State) -> Vec<usize> {
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    let mut frames = [std::ptr::null_mut::<c_void>(); 24];
    // SAFETY: writes at most 24 entries into `frames`.
    let n = unsafe { RtlCaptureStackBackTrace(0, 24, frames.as_mut_ptr(), std::ptr::null_mut()) };
    frames[..n as usize]
        .iter()
        .map(|&f| f as usize)
        .filter(|&f| f >= state.base && f < state.base + state.size)
        .map(|f| f - state.base)
        .take(8)
        .collect()
}

/// `len` bytes at `addr` are committed and readable.
#[cfg(windows)]
fn readable(addr: usize, len: usize) -> usize {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    if addr < 0x10000 {
        return 0;
    }
    // SAFETY: a plain query; `info` is written by the call.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { VirtualQuery(addr as _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
        return 0;
    }
    let ok = info.State == MEM_COMMIT
        && info.Protect & 0x100 == 0
        && info.Protect & (0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80) != 0;
    if !ok {
        return 0;
    }
    let end = info.BaseAddress as usize + info.RegionSize;
    end.saturating_sub(addr).min(len)
}

/// The path the title passes, when the "name" is its file descriptor.
#[cfg(windows)]
fn movie_path(name: *const c_void, flags: u32) -> Option<String> {
    if flags & FROM_MEMORY != 0 || readable(name as usize, 8) < 8 {
        return None;
    }
    // SAFETY: eight readable bytes, checked just above.
    let text = unsafe { (name as *const usize).read_unaligned() };
    let len = readable(text, 260);
    if len == 0 {
        return None;
    }
    // SAFETY: `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(text as *const u8, len) };
    let end = bytes.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

#[cfg(windows)]
fn head(handle: *mut c_void) -> Option<&'static BinkHead> {
    if readable(handle as usize, size_of::<BinkHead>()) < size_of::<BinkHead>() {
        return None;
    }
    // SAFETY: readable, and a live handle Bink returned.
    Some(unsafe { &*(handle as *const BinkHead) })
}

#[cfg(windows)]
unsafe extern "system" fn open(
    name: *const c_void,
    options: *const c_void,
    flags: u32,
) -> *mut c_void {
    // SAFETY: installed only after REAL_OPEN was stored.
    let real: OpenFn = unsafe { std::mem::transmute(REAL_OPEN.load(Ordering::SeqCst)) };
    let Some(state) = STATE.get() else {
        // SAFETY: the title's own call, forwarded.
        return unsafe { real(name, options, flags) };
    };
    let path = movie_path(name, flags);
    let callers = callers(state);
    let site = match callers.first() {
        Some(&r) if r == state.sites.probe_return => "probe",
        Some(&r) if r == state.sites.playback_return => "playback",
        _ => "other",
    };
    let callers_hex: Vec<String> = callers.iter().map(|r| format!("{r:#x}")).collect();
    let lower = path.as_deref().unwrap_or("").to_ascii_lowercase();
    // `frontend`: the start-up front end is up (0x74e2c1c = 1) and no world
    // loading has started yet (byte 0x74e007d = 0).
    let frontend = {
        // SAFETY: a u32 and a byte of the mapped image's .data.
        let (fe, loading) = unsafe {
            (
                ((state.base + 0x74e2c1c) as *const u32).read_volatile(),
                ((state.base + 0x74e007d) as *const u8).read_volatile(),
            )
        };
        fe == 1 && loading == 0
    };
    let by_path = !lower.is_empty()
        && state
            .skip
            .iter()
            .filter(|s| *s != "frontend" && !s.starts_with("size="))
            .any(|s| lower.contains(s.as_str()));
    let by_frontend = frontend && state.skip.iter().any(|s| s == "frontend");
    if (by_path || by_frontend) && state.mode == Mode::Fail && site == "playback" {
        tracing::info!(path = path.as_deref().unwrap_or("?"), site, by_frontend, callers = ?callers_hex, "intro: movie open made to fail (experiment)");
        return std::ptr::null_mut();
    }
    // SAFETY: the title's own call, forwarded.
    let mut handle = unsafe { real(name, options, flags) };
    let mut restore = None;
    if let Some(h) = head(handle) {
        let seconds = if h.frame_rate != 0 {
            h.frames as f64 * h.frame_rate_div as f64 / h.frame_rate as f64
        } else {
            0.0
        };
        if state.trace {
            tracing::info!(
                path = path.as_deref().unwrap_or("?"),
                flags = format_args!("{flags:#x}"),
                site,
                frontend,
                handle = format_args!("{:#x}", handle as usize),
                width = h.width,
                height = h.height,
                frames = h.frames,
                fps = format_args!("{}/{}", h.frame_rate, h.frame_rate_div),
                seconds = format_args!("{seconds:.2}"),
                callers = ?callers_hex,
                "intro: movie opened"
            );
        }
        let size = format!("size={}x{}x{}", h.width, h.height, h.frames);
        let by_size = state.skip.iter().any(|s| *s == size);
        if (by_path || by_size) && state.mode == Mode::Frames1 && site == "probe" {
            restore = Some(h.frames);
            // SAFETY: a live handle; put back before it is closed.
            unsafe { (*(handle as *mut BinkHead)).frames = 1 };
            tracing::info!(%size, "intro: probe shown one frame (experiment)");
        } else if by_size && state.mode == Mode::Fail && site == "playback" {
            // SAFETY: the handle just opened, closed before anyone saw it.
            unsafe {
                let close: CloseFn = std::mem::transmute(REAL_CLOSE.load(Ordering::SeqCst));
                close(handle);
            }
            handle = std::ptr::null_mut();
            tracing::info!(%size, site, "intro: movie open made to fail (experiment)");
        }
    } else if state.trace {
        tracing::info!(path = path.as_deref().unwrap_or("?"), site, callers = ?callers_hex, "intro: movie open failed");
    }
    if !handle.is_null() {
        state.open.lock().unwrap().insert(
            handle as usize,
            (path.unwrap_or_default(), Instant::now(), restore),
        );
    }
    handle
}

#[cfg(windows)]
unsafe extern "system" fn close(handle: *mut c_void) {
    // SAFETY: installed only after REAL_CLOSE was stored.
    let real: CloseFn = unsafe { std::mem::transmute(REAL_CLOSE.load(Ordering::SeqCst)) };
    if let Some(state) = STATE.get() {
        let entry = state.open.lock().unwrap().remove(&(handle as usize));
        if let Some((path, since, restore)) = entry {
            if let Some(frames) = restore {
                // SAFETY: still live; the frame count Bink wrote.
                unsafe { (*(handle as *mut BinkHead)).frames = frames };
            }
            if state.trace {
                let (frame, last) =
                    head(handle).map_or((0, 0), |h| (h.frame_num, h.last_frame_num));
                tracing::info!(
                    path,
                    handle = format_args!("{:#x}", handle as usize),
                    frame,
                    last,
                    open_for = format_args!("{:.2}", since.elapsed().as_secs_f64()),
                    "intro: movie closed"
                );
            }
        }
    }
    // SAFETY: the title's own call, forwarded.
    unsafe { real(handle) }
}

#[cfg(windows)]
unsafe extern "system" fn goto(handle: *mut c_void, frame: u32, flags: i32) {
    // SAFETY: installed only after REAL_GOTO was stored.
    let real: GotoFn = unsafe { std::mem::transmute(REAL_GOTO.load(Ordering::SeqCst)) };
    if let Some(state) = STATE.get().filter(|s| s.trace) {
        let path = state
            .open
            .lock()
            .unwrap()
            .get(&(handle as usize))
            .map(|e| e.0.clone())
            .unwrap_or_default();
        tracing::info!(path, frame, flags, "intro: movie seek");
    }
    // SAFETY: the title's own call, forwarded.
    unsafe { real(handle, frame, flags) }
}

#[cfg(windows)]
unsafe extern "system" fn pause(handle: *mut c_void, on: i32) -> i32 {
    // SAFETY: installed only after REAL_PAUSE was stored.
    let real: PauseFn = unsafe { std::mem::transmute(REAL_PAUSE.load(Ordering::SeqCst)) };
    if let Some(state) = STATE.get().filter(|s| s.trace) {
        let path = state
            .open
            .lock()
            .unwrap()
            .get(&(handle as usize))
            .map(|e| e.0.clone())
            .unwrap_or_default();
        tracing::info!(path, on, "intro: movie pause");
    }
    // SAFETY: the title's own call, forwarded.
    unsafe { real(handle, on) }
}
