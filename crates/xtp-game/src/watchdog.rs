//! An opt-in way past the title's own watchdog, off by default: set
//! `XTP_WATCHDOG_HOOK` (e.g. `=1`) and the hook is applied when the title
//! is mapped, before any of its code runs.
//!
//! Ported from Marvisak/VIctory#454 (head d2c93fac49c2138402742128b953b45e2299a7b5)
//! by ruvenesh. XTP's version lives in the game layer instead of the
//! loader, wants the build's whole BLAKE3 instead of its first 16 digits,
//! writes all of its sites or none, and also patches the second copy of
//! the watchdog loop, which the pull request leaves armed.
//!
//! What the title does (1.0.2.1, by RVA). A thread it creates runs the
//! function at `0x2215a60`: once a second it checks whether the title has
//! advanced, and after 30 ticks without progress it ends the process on
//! purpose. The compiler made two copies of that loop; a global set before
//! the thread starts picks one. At zero, each copy loads a handler pointer
//! (`0x2215c27`, `0x2215d9b`) and calls it. The handler (`0x23cf620`) ends
//! in a write to address 0 (`0x23cf683`), where XTP's slow world frames
//! made the title die. If no handler is set or it returns, the loop writes
//! to address 0 itself and breaks (`0x2215c35`, `0x2215da9`).
//!
//! The hook:
//! - in each copy, the handler load becomes a short jump to the loop's
//!   continuation (`0x2215c41`, `0x2215db7`), with the tick count the
//!   firing left (0 after the countdown). The loop goes on, and fires
//!   again only after the title has advanced and then stalled for 30 ticks
//!   once more. A firing still reads a clock before the jump, as before;
//! - the handler's fault becomes an eleven-byte NOP, in case anything else
//!   calls it: it then returns.
//!
//! It hides a stall; it does not fix what is slow. A real deadlock then
//! hangs instead of exiting.

use std::fmt;

use crate::{BUILD_1_0_2_1, KnownBuild};

/// Set (any value, e.g. `XTP_WATCHDOG_HOOK=1`) to apply the hook.
pub const ENV: &str = "XTP_WATCHDOG_HOOK";

/// One code patch: the bytes at `rva` must be exactly `expect` before `new`
/// (of the same length) replaces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patch {
    /// What it does, for the log.
    pub what: &'static str,
    pub rva: usize,
    pub expect: &'static [u8],
    pub new: &'static [u8],
}

/// The hook for one build: its sites, written in this order.
#[derive(Debug, Clone, Copy)]
pub struct Hook {
    pub build: &'static KnownBuild,
    pub patches: &'static [Patch],
}

/// Builds the hook knows. Any other build is refused: its offsets would be
/// someone else's code.
pub const HOOKS: &[Hook] = &[Hook {
    build: &BUILD_1_0_2_1,
    patches: PATCHES_1_0_2_1,
}];

/// Both loop copies first, then the handler: a write that fails part way
/// leaves the title no worse than unpatched.
const PATCHES_1_0_2_1: &[Patch] = &[
    Patch {
        what: "watchdog loop, first copy: skip the handler call and the fallback fault",
        rva: 0x2215c27,
        // mov rax, [rip + disp32] (the handler pointer)
        expect: &[0x48, 0x8B, 0x05, 0x1A, 0x07, 0xFA, 0x04],
        // jmp 0x2215c41 (the count is already in ebx), then NOPs
        new: &[0xEB, 0x18, 0x90, 0x90, 0x90, 0x90, 0x90],
    },
    Patch {
        what: "watchdog loop, second copy: skip the handler call and the fallback fault",
        rva: 0x2215d9b,
        // mov rax, [rip + disp32] (the same handler pointer)
        expect: &[0x48, 0x8B, 0x05, 0xA6, 0x05, 0xFA, 0x04],
        // xor eax, eax (the count, 0 as the countdown left it);
        // jmp 0x2215db7; then NOPs
        new: &[0x31, 0xC0, 0xEB, 0x18, 0x90, 0x90, 0x90],
    },
    Patch {
        what: "watchdog handler: its deliberate fault",
        rva: 0x23cf683,
        // mov dword ptr [0], imm32
        expect: &[0xC7, 0x04, 0x25, 0, 0, 0, 0, 0x21, 0xD4, 0x8B, 0x4D],
        // an eleven-byte NOP
        new: &[0x66, 0x66, 0x2E, 0x0F, 0x1F, 0x84, 0, 0, 0, 0, 0],
    },
];

/// Why the hook was not applied, or not wholly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotApplied {
    /// No offsets are known for this build: nothing written.
    UnknownBuild,
    /// A site does not hold the expected bytes, or lies outside the image:
    /// nothing written.
    Mismatch { what: &'static str, rva: usize },
    /// Making a site writable failed after `applied` sites were written.
    Write {
        what: &'static str,
        rva: usize,
        applied: usize,
        os_error: i32,
    },
}

impl fmt::Display for NotApplied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::UnknownBuild => write!(f, "no watchdog offsets are known for this build"),
            Self::Mismatch { what, rva } => write!(
                f,
                "the bytes at {rva:#x} ({what}) are not the expected ones; nothing written"
            ),
            Self::Write {
                what,
                rva,
                applied,
                os_error,
            } => write!(
                f,
                "making {rva:#x} ({what}) writable failed after {applied} site(s): {}",
                std::io::Error::from_raw_os_error(os_error)
            ),
        }
    }
}

impl std::error::Error for NotApplied {}

/// The hook for the executable whose BLAKE3 is `blake3_hex` (any case).
pub fn hook_for(blake3_hex: &str) -> Result<&'static Hook, NotApplied> {
    HOOKS
        .iter()
        .find(|h| h.build.blake3.eq_ignore_ascii_case(blake3_hex))
        .ok_or(NotApplied::UnknownBuild)
}

impl Hook {
    /// Every site holds its expected bytes. `bytes_at` gives the image's
    /// `len` bytes at `rva`, `None` outside it.
    fn check<'a>(
        &self,
        mut bytes_at: impl FnMut(usize, usize) -> Option<&'a [u8]>,
    ) -> Result<(), NotApplied> {
        match self
            .patches
            .iter()
            .find(|p| bytes_at(p.rva, p.expect.len()) != Some(p.expect))
        {
            Some(p) => Err(NotApplied::Mismatch {
                what: p.what,
                rva: p.rva,
            }),
            None => Ok(()),
        }
    }

    /// Patches `image`, laid out as mapped (indexed by RVA): every site, or
    /// none if one differs. Returns how many were written.
    pub fn apply_to(&self, image: &mut [u8]) -> Result<usize, NotApplied> {
        self.check(|rva, len| image.get(rva..rva.checked_add(len)?))?;
        for p in self.patches {
            image[p.rva..p.rva + p.new.len()].copy_from_slice(p.new);
        }
        Ok(self.patches.len())
    }
}

/// [`Hook::apply_to`] for the build whose BLAKE3 is `blake3_hex`.
pub fn apply_to(blake3_hex: &str, image: &mut [u8]) -> Result<usize, NotApplied> {
    hook_for(blake3_hex)?.apply_to(image)
}

/// Applies the hook to the mapped title when [`ENV`] is set, and logs the
/// outcome. Does nothing otherwise.
///
/// # Safety
/// `base..base + size` is the mapped image of the executable whose original
/// BLAKE3 is `blake3_hex`, and no title code runs yet.
#[cfg(windows)]
pub unsafe fn apply_if_asked(blake3_hex: &str, base: usize, size: u32) {
    if std::env::var_os(ENV).is_none() {
        return;
    }
    // SAFETY: forwarded from this function's contract.
    match unsafe { apply_mapped(blake3_hex, base, size as usize) } {
        Ok((hook, sites)) => tracing::info!(
            build = hook.build.label,
            sites,
            "title watchdog hook applied ({ENV})"
        ),
        Err(e) => tracing::warn!("title watchdog hook not applied ({ENV}): {e}"),
    }
}

/// # Safety
/// See [`apply_if_asked`].
#[cfg(windows)]
unsafe fn apply_mapped(
    blake3_hex: &str,
    base: usize,
    size: usize,
) -> Result<(&'static Hook, usize), NotApplied> {
    let hook = hook_for(blake3_hex)?;
    // Only the sites are read: an image can have pages that are not.
    hook.check(|rva, len| {
        rva.checked_add(len).filter(|&end| end <= size)?;
        // SAFETY: inside the image, in code of this exact build (the hash
        // names it, and its sites are in `.text`), which is readable.
        Some(unsafe { std::slice::from_raw_parts((base + rva) as *const u8, len) })
    })?;
    for (applied, p) in hook.patches.iter().enumerate() {
        // SAFETY: checked inside the image just above; no title code runs.
        unsafe { write_code(base + p.rva, p.new) }.map_err(|e| NotApplied::Write {
            what: p.what,
            rva: p.rva,
            applied,
            os_error: e.raw_os_error().unwrap_or(0),
        })?;
    }
    Ok((hook, hook.patches.len()))
}

/// Writes `bytes` over code at `at`.
///
/// # Safety
/// `at..at + bytes.len()` is mapped code that no thread runs.
#[cfg(windows)]
unsafe fn write_code(at: usize, bytes: &[u8]) -> std::io::Result<()> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut old = 0;
    // SAFETY: mapped, per the caller; only the protection of these bytes'
    // pages changes, and it is restored below.
    if unsafe { VirtualProtect(at as _, bytes.len(), PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: writable now, and nobody runs it, per the caller. Then the
    // old protection back, and the instruction cache flushed.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len());
        VirtualProtect(at as _, bytes.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as _, bytes.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = BUILD_1_0_2_1.blake3;

    /// A synthetic image for `hook`: zeros, plus `fill` at every site.
    fn image(hook: &Hook, fill: impl Fn(&Patch) -> &'static [u8]) -> Vec<u8> {
        let end = hook.patches.iter().map(|p| p.rva + p.expect.len()).max();
        let mut image = vec![0u8; end.unwrap() + 64];
        for p in hook.patches {
            image[p.rva..p.rva + p.expect.len()].copy_from_slice(fill(p));
        }
        image
    }

    #[test]
    fn applies_every_site_when_all_match() {
        let hook = hook_for(HASH).unwrap();
        let mut img = image(hook, |p| p.expect);
        assert_eq!(apply_to(HASH, &mut img), Ok(hook.patches.len()));
        assert_eq!(img, image(hook, |p| p.new));
        // Applied once: the sites no longer hold what the hook replaces.
        let before = img.clone();
        assert!(matches!(
            apply_to(HASH, &mut img),
            Err(NotApplied::Mismatch { .. })
        ));
        assert_eq!(img, before);
    }

    #[test]
    fn one_differing_site_writes_none() {
        let hook = hook_for(HASH).unwrap();
        for (i, odd) in hook.patches.iter().enumerate() {
            let mut img = image(hook, |p| p.expect);
            img[odd.rva + odd.expect.len() - 1] ^= 0xff;
            let before = img.clone();
            assert_eq!(
                apply_to(HASH, &mut img),
                Err(NotApplied::Mismatch {
                    what: odd.what,
                    rva: odd.rva
                }),
                "site {i}"
            );
            assert_eq!(img, before, "site {i}: something was written");
        }
    }

    #[test]
    fn sites_outside_the_image_are_refused() {
        let hook = hook_for(HASH).unwrap();
        let mut small = vec![0u8; 0x1000];
        assert!(matches!(
            hook.apply_to(&mut small),
            Err(NotApplied::Mismatch { .. })
        ));
        // A site cut short by the image's end.
        let first = hook.patches[0];
        let mut cut = image(hook, |p| p.expect);
        cut.truncate(first.rva + first.expect.len() - 1);
        assert!(hook.apply_to(&mut cut).is_err());
    }

    #[test]
    fn unknown_builds_are_refused() {
        let hook = hook_for(HASH).unwrap();
        let mut img = image(hook, |p| p.expect);
        let before = img.clone();
        for other in [
            "0".repeat(64),
            // What the pull request compared: the first 16 digits only.
            HASH[..16].to_owned(),
            format!("{}0000", &HASH[..60]),
            String::new(),
        ] {
            assert_eq!(apply_to(&other, &mut img), Err(NotApplied::UnknownBuild));
        }
        assert_eq!(img, before);
        // Case does not matter.
        assert!(hook_for(&HASH.to_ascii_uppercase()).is_ok());
    }

    #[test]
    fn every_hooked_build_is_a_known_one() {
        for hook in HOOKS {
            assert_eq!(
                crate::identify(hook.build.blake3).map(|b| b.blake3),
                Some(hook.build.blake3)
            );
        }
    }

    #[test]
    fn patches_keep_their_length_and_do_not_overlap() {
        for hook in HOOKS {
            for (i, p) in hook.patches.iter().enumerate() {
                assert_eq!(p.new.len(), p.expect.len(), "{}", p.what);
                assert_ne!(p.new, p.expect, "{}", p.what);
                for q in &hook.patches[i + 1..] {
                    let apart = p.rva + p.expect.len() <= q.rva || q.rva + q.expect.len() <= p.rva;
                    assert!(apart, "{} overlaps {}", p.what, q.what);
                }
            }
        }
    }

    /// The short jumps land on the instruction after each copy's fallback
    /// `int3`, where the loop goes on.
    #[test]
    fn jumps_land_on_the_loop_continuations() {
        let hook = hook_for(HASH).unwrap();
        let target = |p: &Patch| {
            let at = p.new.iter().position(|&b| b == 0xEB).unwrap();
            let rel = p.new[at + 1] as i8 as isize;
            (p.rva + at + 2).wrapping_add_signed(rel)
        };
        assert_eq!(target(&hook.patches[0]), 0x2215c41);
        assert_eq!(target(&hook.patches[1]), 0x2215db7);
        // The second copy zeroes the count (eax) before it jumps.
        assert_eq!(hook.patches[1].new[..2], [0x31, 0xC0]);
    }
}
