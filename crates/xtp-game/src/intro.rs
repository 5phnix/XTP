//! An opt-in cut of the intro movie, off by default: set `XTP_SKIP_INTRO`
//! (e.g. `=1`) and the movie the title plays in its start-up front end is
//! sent to its last frame as soon as it starts.
//!
//! Why (docs/analysis/intro-skip.md): the title starts loading the world
//! only once its intro movie has ended and the profile is accepted. The
//! movie (4K, 450 frames at 30 per second: 15 s) starts when the start-up
//! work is done, so in a test session it holds the world back by about 13 s.
//!
//! How the title plays it (1.0.2.1, by RVA). Its movies are Bink 2 files
//! played through `Bink2Scarlett.dll`, whose functions it calls through its
//! import slots. A movie is probed first (`0x442ea70`: opened, its size and
//! rate copied, closed), then opened for playback (`0x442efe0`). The player
//! (`0x2ef0e0`) calls `BinkNextFrame` before each frame it decodes, and
//! waits for that decode before the next one; its own seek is a `BinkGoto`
//! followed by a decode. The front end goes on once the movie has shown its
//! last frame.
//!
//! The cut. Once the imports are bound, three slots are wrapped:
//! - `BinkOpenWithOptions` (`0x6794210`): a movie opened while the start-up
//!   front end is up (`0x74e2c1c` = 1) and before any world loading
//!   (`0x74e007d` = 0) is marked with its frame count;
//! - `BinkNextFrame` (`0x6794208`): the first one on a marked movie becomes
//!   a `BinkGoto` (`0x6794200`, read, not wrapped) to its last frame, which
//!   the player then decodes as after its own seeks: the movie has ended;
//! - `BinkClose` (`0x67941c0`): forgets the mark (a probe is closed without
//!   ever being advanced, and Bink may hand the same handle out again).
//!
//! Two other ways were tried and refused: failing the playback open (the
//! front end then waits for ever) and showing the probe a one-frame movie
//! (the player still plays all 450 frames).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{BUILD_1_0_2_1, KnownBuild};

/// Set (any value, e.g. `XTP_SKIP_INTRO=1`) to cut the intro movie.
pub const ENV: &str = "XTP_SKIP_INTRO";

/// What one build needs, by RVA.
#[derive(Debug, Clone, Copy)]
pub struct Sites {
    pub build: &'static KnownBuild,
    /// Import slots of `Bink2Scarlett.dll`'s `BinkOpenWithOptions`,
    /// `BinkNextFrame`, `BinkGoto` and `BinkClose`.
    pub open_slot: usize,
    pub next_slot: usize,
    pub goto_slot: usize,
    pub close_slot: usize,
    /// `u32`, 1 while the front end is up.
    pub front_end: usize,
    /// `u8`, 1 once the title has started loading the world.
    pub world_loading: usize,
}

/// Builds the cut knows. Any other build is refused.
pub const SITES: &[Sites] = &[Sites {
    build: &BUILD_1_0_2_1,
    open_slot: 0x6794210,
    next_slot: 0x6794208,
    goto_slot: 0x6794200,
    close_slot: 0x67941c0,
    front_end: 0x74e2c1c,
    world_loading: 0x74e007d,
}];

impl Sites {
    /// The four import slots, as (RVA, `Bink2Scarlett.dll` export).
    pub fn imports(&self) -> [(usize, &'static str); 4] {
        [
            (self.open_slot, "BinkOpenWithOptions"),
            (self.next_slot, "BinkNextFrame"),
            (self.goto_slot, "BinkGoto"),
            (self.close_slot, "BinkClose"),
        ]
    }
}

/// The sites for the executable whose BLAKE3 is `blake3_hex` (any case).
pub fn sites_for(blake3_hex: &str) -> Option<&'static Sites> {
    SITES
        .iter()
        .find(|s| s.build.blake3.eq_ignore_ascii_case(blake3_hex))
}

/// A movie opened now is the intro: the start-up front end is up and no
/// world has started loading.
pub fn in_start_up_front_end(front_end: u32, world_loading: u8) -> bool {
    front_end == 1 && world_loading == 0
}

/// Movies to send to their last frame, by Bink handle.
#[derive(Debug, Default)]
pub struct Marks(Mutex<HashMap<usize, u32>>);

impl Marks {
    /// `handle`, a movie of `frames` frames, is to be cut. A movie of one
    /// frame has nothing to cut.
    pub fn mark(&self, handle: usize, frames: u32) {
        if frames > 1 {
            self.0.lock().unwrap().insert(handle, frames);
        }
    }

    /// The frame to go to instead of the next one: once per mark.
    pub fn take(&self, handle: usize) -> Option<u32> {
        self.0.lock().unwrap().remove(&handle)
    }

    /// `handle` is closed.
    pub fn forget(&self, handle: usize) {
        self.0.lock().unwrap().remove(&handle);
    }
}

/// Why the cut was not installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotInstalled {
    /// No sites are known for this build.
    UnknownBuild,
    /// A site lies outside the image, or an import slot is empty: nothing
    /// written.
    Site { rva: usize },
    /// Already installed in this process: nothing written.
    Twice,
    /// Writing an import slot failed after `written` slots.
    Write {
        rva: usize,
        written: usize,
        os_error: i32,
    },
}

impl std::fmt::Display for NotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::UnknownBuild => write!(f, "no intro sites are known for this build"),
            Self::Site { rva } => write!(
                f,
                "the site at {rva:#x} is outside the image or an empty import slot; nothing written"
            ),
            Self::Twice => write!(f, "already installed; nothing written"),
            Self::Write {
                rva,
                written,
                os_error,
            } => write!(
                f,
                "writing the import slot {rva:#x} failed after {written} slot(s): {}",
                std::io::Error::from_raw_os_error(os_error)
            ),
        }
    }
}

impl std::error::Error for NotInstalled {}

#[cfg(windows)]
pub use hook::apply_if_asked;

#[cfg(windows)]
mod hook {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    use super::{ENV, Marks, NotInstalled, Sites, in_start_up_front_end, sites_for};

    pub(super) type OpenFn =
        unsafe extern "system" fn(*const c_void, *const c_void, u32) -> *mut c_void;
    pub(super) type NextFn = unsafe extern "system" fn(*mut c_void);
    pub(super) type GotoFn = unsafe extern "system" fn(*mut c_void, u32, i32);
    pub(super) type CloseFn = unsafe extern "system" fn(*mut c_void);

    /// The public head of a Bink 2 handle (`bink.h`).
    #[repr(C)]
    pub(super) struct BinkHead {
        pub width: u32,
        pub height: u32,
        pub frames: u32,
    }

    /// The bound functions, and where the title keeps its state words.
    struct Installed {
        open: OpenFn,
        next: NextFn,
        goto: GotoFn,
        close: CloseFn,
        front_end: usize,
        world_loading: usize,
        marks: Marks,
    }

    static INSTALLED: OnceLock<Installed> = OnceLock::new();

    /// Wraps the title's Bink imports when [`ENV`] is set, and logs the
    /// outcome. Does nothing otherwise.
    ///
    /// # Safety
    /// `base..base + size` is the mapped image of the executable whose
    /// original BLAKE3 is `blake3_hex`, its imports are bound, and no title
    /// code runs yet.
    pub unsafe fn apply_if_asked(blake3_hex: &str, base: usize, size: u32) {
        if std::env::var_os(ENV).is_none() {
            return;
        }
        let Some(sites) = sites_for(blake3_hex) else {
            let e = NotInstalled::UnknownBuild;
            tracing::warn!("intro movie not cut ({ENV}): {e}");
            return;
        };
        // SAFETY: forwarded from this function's contract.
        match unsafe { install(sites, base, size as usize) } {
            Ok(()) => tracing::info!(
                build = sites.build.label,
                "the intro movie will be cut ({ENV})"
            ),
            Err(e) => tracing::warn!("intro movie not cut ({ENV}): {e}"),
        }
    }

    /// Checks every site and reads the bound functions, then writes the
    /// three wrapped slots. Once per process.
    ///
    /// # Safety
    /// `base..base + size` is an image laid out as `sites` says, its import
    /// slots bound to functions of Bink's signatures, and no code calls
    /// through them yet.
    pub(super) unsafe fn install(
        sites: &Sites,
        base: usize,
        size: usize,
    ) -> Result<(), NotInstalled> {
        let inside = |rva: usize, len: usize| rva.checked_add(len).is_some_and(|end| end <= size);
        let bound = |rva: usize| {
            // SAFETY: an eight-byte slot inside the image (checked first).
            inside(rva, 8).then(|| unsafe { ((base + rva) as *const usize).read_volatile() })
        };
        let mut functions = [0usize; 4];
        for (f, (rva, _)) in functions.iter_mut().zip(sites.imports()) {
            *f = bound(rva)
                .filter(|&a| a != 0)
                .ok_or(NotInstalled::Site { rva })?;
        }
        for (rva, len) in [(sites.front_end, 4), (sites.world_loading, 1)] {
            if !inside(rva, len) {
                return Err(NotInstalled::Site { rva });
            }
        }
        let [open_f, next_f, goto_f, close_f] = functions;
        // SAFETY: each slot holds a bound function of the signature Bink
        // documents for it (the caller's contract).
        let installed = unsafe {
            Installed {
                open: std::mem::transmute::<usize, OpenFn>(open_f),
                next: std::mem::transmute::<usize, NextFn>(next_f),
                goto: std::mem::transmute::<usize, GotoFn>(goto_f),
                close: std::mem::transmute::<usize, CloseFn>(close_f),
                front_end: base + sites.front_end,
                world_loading: base + sites.world_loading,
                marks: Marks::default(),
            }
        };
        if INSTALLED.set(installed).is_err() {
            // A second install would wrap our own wrappers.
            return Err(NotInstalled::Twice);
        }
        let wrapped = [
            (sites.open_slot, open as *const () as usize),
            (sites.next_slot, next as *const () as usize),
            (sites.close_slot, close as *const () as usize),
        ];
        for (written, (rva, ours)) in wrapped.into_iter().enumerate() {
            // SAFETY: a bound slot inside the image; nothing calls it yet.
            unsafe { write_slot(base + rva, ours) }.map_err(|e| NotInstalled::Write {
                rva,
                written,
                os_error: e.raw_os_error().unwrap_or(0),
            })?;
        }
        Ok(())
    }

    /// Writes `value` into the import slot at `at`.
    ///
    /// # Safety
    /// `at` is an eight-byte import slot of a mapped image.
    unsafe fn write_slot(at: usize, value: usize) -> std::io::Result<()> {
        use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
        let mut old = 0;
        // SAFETY: mapped, per the caller; only these bytes' protection
        // changes, and it is restored below.
        if unsafe { VirtualProtect(at as _, 8, PAGE_READWRITE, &mut old) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: writable now, eight-byte aligned (an import slot).
        unsafe {
            (at as *mut usize).write_volatile(value);
            VirtualProtect(at as _, 8, old, &mut old);
        }
        Ok(())
    }

    fn installed() -> &'static Installed {
        INSTALLED
            .get()
            .expect("the slots are wrapped only once installed")
    }

    unsafe extern "system" fn open(
        name: *const c_void,
        options: *const c_void,
        flags: u32,
    ) -> *mut c_void {
        let hook = installed();
        // SAFETY: the title's own call, forwarded.
        let handle = unsafe { (hook.open)(name, options, flags) };
        // SAFETY: the image's state words, mapped for the process' life.
        let (front_end, world_loading) = unsafe {
            (
                (hook.front_end as *const u32).read_volatile(),
                (hook.world_loading as *const u8).read_volatile(),
            )
        };
        if !handle.is_null() && in_start_up_front_end(front_end, world_loading) {
            // SAFETY: a handle Bink just returned starts with its public head.
            let head = unsafe { &*(handle as *const BinkHead) };
            hook.marks.mark(handle as usize, head.frames);
            tracing::debug!(
                width = head.width,
                height = head.height,
                frames = head.frames,
                "a movie opened in the start-up front end"
            );
        }
        handle
    }

    unsafe extern "system" fn next(handle: *mut c_void) {
        let hook = installed();
        match hook.marks.take(handle as usize) {
            Some(frames) => {
                tracing::info!(frames, "intro movie cut: sent to its last frame ({ENV})");
                // SAFETY: the live handle the title advances, between two of
                // its decodes, as its own seek does.
                unsafe { (hook.goto)(handle, frames, 0) }
            }
            // SAFETY: the title's own call, forwarded.
            None => unsafe { (hook.next)(handle) },
        }
    }

    unsafe extern "system" fn close(handle: *mut c_void) {
        let hook = installed();
        hook.marks.forget(handle as usize);
        // SAFETY: the title's own call, forwarded.
        unsafe { (hook.close)(handle) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = BUILD_1_0_2_1.blake3;

    #[test]
    fn only_the_known_build_has_sites() {
        assert_eq!(
            sites_for(HASH).map(|s| s.build.label),
            Some(BUILD_1_0_2_1.label)
        );
        assert!(sites_for(&HASH.to_ascii_uppercase()).is_some());
        for other in [
            "0".repeat(64),
            HASH[..16].to_owned(),
            format!("{}0000", &HASH[..60]),
            String::new(),
        ] {
            assert!(sites_for(&other).is_none(), "{other}");
        }
        for s in SITES {
            assert_eq!(
                crate::identify(s.build.blake3).map(|b| b.blake3),
                Some(s.build.blake3)
            );
        }
    }

    #[test]
    fn sites_are_aligned_and_apart() {
        for s in SITES {
            let slots = s.imports().map(|(rva, _)| rva);
            for (i, a) in slots.iter().enumerate() {
                assert_eq!(a % 8, 0, "import slot {a:#x}");
                for b in &slots[i + 1..] {
                    assert!(a.abs_diff(*b) >= 8, "{a:#x} and {b:#x} overlap");
                }
            }
            assert_eq!(s.front_end % 4, 0);
            assert!(s.world_loading.abs_diff(s.front_end) >= 4);
        }
    }

    #[test]
    fn the_intro_is_what_opens_in_the_start_up_front_end() {
        assert!(in_start_up_front_end(1, 0));
        // The front end is gone (world loading), or came back after a world
        // was loaded: nothing is cut.
        assert!(!in_start_up_front_end(0, 0));
        assert!(!in_start_up_front_end(1, 1));
        assert!(!in_start_up_front_end(0, 1));
        assert!(!in_start_up_front_end(2, 0));
    }

    #[test]
    fn a_mark_is_taken_once_and_forgotten_on_close() {
        let marks = Marks::default();
        marks.mark(0x10, 450);
        assert_eq!(marks.take(0x10), Some(450));
        assert_eq!(marks.take(0x10), None);
        // A probe: opened, marked, closed without being advanced.
        marks.mark(0x20, 450);
        marks.forget(0x20);
        assert_eq!(marks.take(0x20), None);
        // One frame: nothing to cut.
        marks.mark(0x30, 1);
        assert_eq!(marks.take(0x30), None);
        // Another handle is not touched.
        marks.mark(0x40, 90);
        assert_eq!(marks.take(0x41), None);
        assert_eq!(marks.take(0x40), Some(90));
    }

    /// The wrapped slots on a fabricated image, called as the title calls
    /// them, with stand-ins for Bink's functions.
    #[cfg(windows)]
    #[test]
    fn install_wraps_the_slots_and_cuts_only_the_start_up_movie() {
        use std::ffi::c_void;
        use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};

        use hook::{BinkHead, CloseFn, GotoFn, NextFn, OpenFn};

        static HANDLE: AtomicUsize = AtomicUsize::new(0);
        static NEXTS: AtomicU32 = AtomicU32::new(0);
        static GOTO_TO: AtomicU32 = AtomicU32::new(0);
        static CLOSES: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn bink_open(
            _: *const c_void,
            _: *const c_void,
            _: u32,
        ) -> *mut c_void {
            HANDLE.load(SeqCst) as *mut c_void
        }
        unsafe extern "system" fn bink_next(_: *mut c_void) {
            NEXTS.fetch_add(1, SeqCst);
        }
        unsafe extern "system" fn bink_goto(_: *mut c_void, frame: u32, _: i32) {
            GOTO_TO.store(frame, SeqCst);
        }
        unsafe extern "system" fn bink_close(_: *mut c_void) {
            CLOSES.fetch_add(1, SeqCst);
        }

        const TEST_SITES: Sites = Sites {
            build: &BUILD_1_0_2_1,
            open_slot: 0x100,
            next_slot: 0x108,
            goto_slot: 0x110,
            close_slot: 0x118,
            front_end: 0x200,
            world_loading: 0x204,
        };
        let mut image = vec![0u64; 0x300 / 8];
        let base = image.as_mut_ptr() as usize;
        let size = image.len() * 8;
        let slot = |rva: usize| (base + rva) as *mut usize;
        let set_state = |front_end: u32, world_loading: u8| {
            // SAFETY: inside `image`.
            unsafe {
                ((base + 0x200) as *mut u32).write_volatile(front_end);
                ((base + 0x204) as *mut u8).write_volatile(world_loading);
            }
        };
        // SAFETY: the slots lie inside `image`.
        unsafe {
            slot(0x100).write(bink_open as *const () as usize);
            slot(0x108).write(bink_next as *const () as usize);
            slot(0x110).write(bink_goto as *const () as usize);
            slot(0x118).write(bink_close as *const () as usize);
        }

        // Refused before anything is written: an empty slot, a site past
        // the end.
        let before = image.clone();
        // SAFETY: as the contract asks, with a slot left empty.
        unsafe { slot(0x110).write(0) };
        // SAFETY: `image` is laid out as TEST_SITES says.
        let empty = unsafe { hook::install(&TEST_SITES, base, size) };
        assert_eq!(empty, Err(NotInstalled::Site { rva: 0x110 }));
        // SAFETY: inside `image`.
        unsafe { slot(0x110).write(bink_goto as *const () as usize) };
        // SAFETY: as above, cut short before the state words.
        let short = unsafe { hook::install(&TEST_SITES, base, 0x1ff) };
        assert_eq!(short, Err(NotInstalled::Site { rva: 0x200 }));
        assert_eq!(image, before, "a refused install writes nothing");

        // SAFETY: as above.
        assert_eq!(unsafe { hook::install(&TEST_SITES, base, size) }, Ok(()));
        // SAFETY: inside `image`.
        let (open, next, close) = unsafe {
            (
                std::mem::transmute::<usize, OpenFn>(slot(0x100).read()),
                std::mem::transmute::<usize, NextFn>(slot(0x108).read()),
                std::mem::transmute::<usize, CloseFn>(slot(0x118).read()),
            )
        };
        // SAFETY: inside `image`.
        let goto_slot = unsafe { slot(0x110).read() };
        assert_ne!(
            open as *const () as usize, bink_open as *const () as usize,
            "open wrapped"
        );
        assert_ne!(
            next as *const () as usize, bink_next as *const () as usize,
            "next wrapped"
        );
        assert_ne!(
            close as *const () as usize, bink_close as *const () as usize,
            "close wrapped"
        );
        assert_eq!(
            goto_slot, bink_goto as *const () as usize,
            "BinkGoto only read"
        );
        let _: GotoFn = bink_goto;

        let movie = |frames: u32| {
            let head = Box::leak(Box::new(BinkHead {
                width: 3840,
                height: 2160,
                frames,
            }));
            HANDLE.store(head as *mut BinkHead as usize, SeqCst);
            head as *mut BinkHead as *mut c_void
        };
        let null = std::ptr::null();

        // The intro: opened in the start-up front end, cut at its first
        // frame, then played on as usual.
        set_state(1, 0);
        let intro = movie(450);
        // SAFETY: the wrappers, called as the title calls Bink.
        unsafe {
            assert_eq!(open(null, null, 0), intro);
            next(intro);
            assert_eq!((GOTO_TO.load(SeqCst), NEXTS.load(SeqCst)), (450, 0));
            next(intro);
            assert_eq!(NEXTS.load(SeqCst), 1);
            close(intro);
            assert_eq!(CLOSES.load(SeqCst), 1);
        }

        // A probe: opened and closed; a later movie that gets the same
        // handle is not cut.
        GOTO_TO.store(0, SeqCst);
        let probe = movie(450);
        // SAFETY: as above.
        unsafe {
            assert_eq!(open(null, null, 0), probe);
            close(probe);
            set_state(0, 1);
            assert_eq!(open(null, null, 0), probe);
            next(probe);
        }
        assert_eq!((GOTO_TO.load(SeqCst), NEXTS.load(SeqCst)), (0, 2));

        // Movies of the loading screen and the world are left alone.
        for (front_end, world_loading) in [(0, 1), (1, 1), (0, 0)] {
            set_state(front_end, world_loading);
            let other = movie(217);
            // SAFETY: as above.
            unsafe {
                open(null, null, 0);
                next(other);
            }
            assert_eq!(GOTO_TO.load(SeqCst), 0, "{front_end} {world_loading}");
        }
        assert_eq!(NEXTS.load(SeqCst), 5);

        // Once per process.
        // SAFETY: as above.
        let twice = unsafe { hook::install(&TEST_SITES, base, size) };
        assert_eq!(twice, Err(NotInstalled::Twice));
        // The wrappers stay in `image`'s slots; keep it for the process.
        std::mem::forget(image);
    }
}
