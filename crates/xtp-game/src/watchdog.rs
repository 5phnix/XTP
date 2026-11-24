//! An opt-in way past the title's own watchdog, off by default: set
//! `XTP_WATCHDOG_HOOK` (e.g. `=1`) and the hook is applied when the title
//! is mapped, before any of its code runs.
//!
//! Ported from Marvisak/VIctory#454 (head d2c93fac49c2138402742128b953b45e2299a7b5)
//! by ruvenesh. XTP's version lives in the game layer instead of the
//! loader, wants the build's whole BLAKE3 instead of its first 16 digits,
//! checks every site before it writes any, also patches the second copy of
//! the watchdog loop, which the pull request leaves armed, and counts the
//! firings it stops so that they show in the log.
//!
//! What the title does (1.0.2.1, by RVA). A thread it creates runs the
//! function at `0x2215a60`: once a second it checks whether the title has
//! advanced. The compiler made two copies of that loop; a global set before
//! the thread starts picks one. Each copy fires after 30 ticks without
//! progress (the global at `0x71b6338` unchanged). The first copy fires on
//! a second trigger too: when the flag at `0x71b6334` is set, it walks a
//! table of per-entry heartbeats (64 groups at `0x6844a20`) and fires as
//! soon as one is older than the threshold at `0x6159e20` (`0x2215bdc`
//! jumps to the firing at `0x2215c0c`, which keeps the count the loop had).
//! The second copy walks the same table but never fires from it. A firing
//! loads a handler pointer (`0x2215c27`, `0x2215d9b`) and calls it. The
//! handler (`0x23cf620`) ends in a write to address 0 (`0x23cf683`), where
//! XTP's slow world frames made the title die. If no handler is set or it
//! returns, the loop writes to address 0 itself and breaks (`0x2215c35`,
//! `0x2215da9`).
//!
//! The hook: in each copy, the handler load becomes an increment of a
//! counter and a short jump to the loop's continuation (`0x2215c41`,
//! `0x2215db7`), with the tick count the firing left (0 after the
//! countdown). The loop goes on. After a countdown firing it fires again
//! only once the title has advanced and then stalled for 30 ticks more; a
//! heartbeat firing repeats every tick while the heartbeat stays stale. A
//! firing still reads a clock before the jump, as before. The handler is
//! left as it is: nothing else calls it directly.
//!
//! The counter is a `u32` in the slack past the end of `.data` (the rest of
//! its last page, which the title does not use). A thread of ours reads it
//! and logs a warning when it moves, at most every 30 s.
//!
//! The trade-off: the hook hides a stall; it does not fix what is slow. A
//! real deadlock then hangs for ever instead of ending the process, and
//! leaves no entry in `crashes.log`: only those warnings say that the
//! watchdog would have ended the title.

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
    /// Where the patched code jumps: the title's own code after the firing.
    pub resumes_at: usize,
}

/// The hook for one build: its sites, written in this order, and the
/// counter they increment.
#[derive(Debug, Clone, Copy)]
pub struct Hook {
    pub build: &'static KnownBuild,
    pub patches: &'static [Patch],
    /// RVA of the `u32` each stopped firing increments: zero and unused by
    /// the title, in writable memory.
    pub counter: usize,
}

/// Builds the hook knows. Any other build is refused: its offsets would be
/// someone else's code.
pub const HOOKS: &[Hook] = &[Hook {
    build: &BUILD_1_0_2_1,
    patches: PATCHES_1_0_2_1,
    counter: COUNTER_1_0_2_1,
}];

/// The last eight bytes of `.data`'s last page: `.data` ends at
/// `0xabce398`, `.pdata` starts at `0xabcf000`.
const COUNTER_1_0_2_1: usize = 0xabceff8;

/// `inc dword ptr [rip + disp32]` at `at`, on the `u32` at `counter`.
const fn count_into(at: usize, counter: usize) -> [u8; 6] {
    let d = ((counter as isize) - (at as isize + 6)) as i32;
    let d = d.to_le_bytes();
    [0xFF, 0x05, d[0], d[1], d[2], d[3]]
}

/// First copy, at `0x2215c27`: count; `jmp 0x2215c41` (the count is
/// already in ebx); a two-byte NOP.
const COPY_1: [u8; 10] = {
    let c = count_into(0x2215c27, COUNTER_1_0_2_1);
    [c[0], c[1], c[2], c[3], c[4], c[5], 0xEB, 0x12, 0x66, 0x90]
};

/// Second copy, at `0x2215d9b`: count; `xor eax, eax` (the count, 0 as the
/// countdown left it); `jmp 0x2215db7`.
const COPY_2: [u8; 10] = {
    let c = count_into(0x2215d9b, COUNTER_1_0_2_1);
    [c[0], c[1], c[2], c[3], c[4], c[5], 0x31, 0xC0, 0xEB, 0x12]
};

/// A write that fails part way leaves the other copy armed: no worse than
/// unpatched.
const PATCHES_1_0_2_1: &[Patch] = &[
    Patch {
        what: "watchdog loop, first copy: count the firing, skip the handler call and the fallback fault",
        rva: 0x2215c27,
        // mov rax, [rip + disp32] (the handler pointer); test rax, rax
        expect: &[0x48, 0x8B, 0x05, 0x1A, 0x07, 0xFA, 0x04, 0x48, 0x85, 0xC0],
        new: &COPY_1,
        resumes_at: 0x2215c41,
    },
    Patch {
        what: "watchdog loop, second copy: count the firing, skip the handler call and the fallback fault",
        rva: 0x2215d9b,
        // mov rax, [rip + disp32] (the same handler pointer); test rax, rax
        expect: &[0x48, 0x8B, 0x05, 0xA6, 0x05, 0xFA, 0x04, 0x48, 0x85, 0xC0],
        new: &COPY_2,
        resumes_at: 0x2215db7,
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
    /// The counter is outside the image, not zero, or not writable:
    /// nothing written.
    Counter { rva: usize },
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
            Self::Counter { rva } => write!(
                f,
                "the firing counter at {rva:#x} is not zeroed, writable memory of the image; nothing written"
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
    /// Every site holds its expected bytes and the counter is zero.
    /// `bytes_at` gives the image's `len` bytes at `rva`, `None` outside
    /// it. All or nothing: this is checked before anything is written.
    fn check<'a>(
        &self,
        mut bytes_at: impl FnMut(usize, usize) -> Option<&'a [u8]>,
    ) -> Result<(), NotApplied> {
        if let Some(p) = self
            .patches
            .iter()
            .find(|p| bytes_at(p.rva, p.expect.len()) != Some(p.expect))
        {
            return Err(NotApplied::Mismatch {
                what: p.what,
                rva: p.rva,
            });
        }
        if bytes_at(self.counter, 4) != Some(&[0; 4]) {
            return Err(NotApplied::Counter { rva: self.counter });
        }
        Ok(())
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
/// outcome; then logs the firings it stops. Does nothing otherwise.
///
/// # Safety
/// `base..base + size` is the mapped image of the executable whose original
/// BLAKE3 is `blake3_hex`, and no title code runs yet.
#[cfg(windows)]
pub unsafe fn apply_if_asked(blake3_hex: &str, base: usize, size: u32) {
    if std::env::var_os(ENV).is_none() {
        return;
    }
    let applied = hook_for(blake3_hex).and_then(|hook| {
        // SAFETY: forwarded from this function's contract.
        unsafe { apply_mapped(hook, base, size as usize) }.map(|sites| (hook, sites))
    });
    match applied {
        Ok((hook, sites)) => {
            tracing::info!(
                build = hook.build.label,
                sites,
                "title watchdog hook applied ({ENV})"
            );
            report_firings(base + hook.counter);
        }
        Err(e) => tracing::warn!("title watchdog hook not applied ({ENV}): {e}"),
    }
}

/// Writes `hook` into the image mapped at `base`: checks every site and the
/// counter first, then writes the sites in order. Returns how many.
///
/// # Safety
/// `base..base + size` is the mapped image `hook` was made for (or memory
/// laid out like it), and no thread runs its patched code.
#[cfg(windows)]
unsafe fn apply_mapped(hook: &Hook, base: usize, size: usize) -> Result<usize, NotApplied> {
    // Only the sites and the counter are read: an image can have pages
    // that are not.
    hook.check(|rva, len| {
        rva.checked_add(len).filter(|&end| end <= size)?;
        // SAFETY: inside the image, in code (or data) of the build the hash
        // names, which is readable.
        Some(unsafe { std::slice::from_raw_parts((base + rva) as *const u8, len) })
    })?;
    if !writable(base + hook.counter, 4) {
        return Err(NotApplied::Counter { rva: hook.counter });
    }
    for (applied, p) in hook.patches.iter().enumerate() {
        // SAFETY: checked inside the image just above; no thread runs it.
        unsafe { write_code(base + p.rva, p.new) }.map_err(|e| NotApplied::Write {
            what: p.what,
            rva: p.rva,
            applied,
            os_error: e.raw_os_error().unwrap_or(0),
        })?;
    }
    Ok(hook.patches.len())
}

/// `len` bytes at `at` lie in one committed region that may be written.
#[cfg(windows)]
fn writable(at: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY,
        PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY, VirtualQuery,
    };
    // SAFETY: a plain query into a local the call fills.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { VirtualQuery(at as _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
        return false;
    }
    let write = PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    info.State == MEM_COMMIT
        && info.Protect & PAGE_GUARD == 0
        && info.Protect & write != 0
        && at + len <= info.BaseAddress as usize + info.RegionSize
}

/// A thread that warns when the counter at `at` moves: at most one line
/// every 30 s, with the total so far.
#[cfg(windows)]
fn report_firings(at: usize) {
    let spawned = std::thread::Builder::new()
        .name("xtp-watchdog-log".into())
        .spawn(move || {
            let mut logged = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                // SAFETY: a u32 of the mapped image, checked writable when
                // the hook was applied; the title increments it.
                let fired = unsafe { (at as *const u32).read_volatile() };
                if fired != logged {
                    tracing::warn!(
                        times = fired,
                        "the title's watchdog would have ended it (no progress for 30 s, or a stale heartbeat); {ENV} kept it running"
                    );
                    logged = fired;
                    std::thread::sleep(std::time::Duration::from_secs(29));
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("watchdog firings will not be logged: {e}");
    }
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
        let end = hook.patches.iter().map(|p| p.rva + p.expect.len());
        let end = end.chain([hook.counter + 4]).max();
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
    fn a_used_counter_writes_none() {
        let hook = hook_for(HASH).unwrap();
        let mut img = image(hook, |p| p.expect);
        img[hook.counter + 3] = 1;
        let before = img.clone();
        assert_eq!(
            apply_to(HASH, &mut img),
            Err(NotApplied::Counter { rva: hook.counter })
        );
        assert_eq!(img, before);
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
        // The counter past the image's end.
        let mut short = image(hook, |p| p.expect);
        short.truncate(hook.counter + 3);
        assert_eq!(
            hook.apply_to(&mut short),
            Err(NotApplied::Counter { rva: hook.counter })
        );
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
    fn patches_keep_their_length_and_stay_apart() {
        for hook in HOOKS {
            for (i, p) in hook.patches.iter().enumerate() {
                assert_eq!(p.new.len(), p.expect.len(), "{}", p.what);
                assert_ne!(p.new, p.expect, "{}", p.what);
                // One page each: a site is never half written across a
                // protection change.
                assert_eq!(
                    p.rva / 0x1000,
                    (p.rva + p.new.len() - 1) / 0x1000,
                    "{} crosses a page boundary",
                    p.what
                );
                let clear_of =
                    |rva: usize, len: usize| p.rva + p.expect.len() <= rva || rva + len <= p.rva;
                assert!(clear_of(hook.counter, 4), "{} overlaps the counter", p.what);
                for q in &hook.patches[i + 1..] {
                    assert!(
                        clear_of(q.rva, q.expect.len()),
                        "{} overlaps {}",
                        p.what,
                        q.what
                    );
                }
            }
            assert_eq!(hook.counter % 4, 0, "the counter is aligned");
        }
    }

    /// What each patch does, decoded: it increments the counter, the second
    /// copy zeroes eax, and a short jump lands on the loop's continuation,
    /// right after each copy's fallback `int3`.
    #[test]
    fn patches_count_then_jump_to_the_loop_continuations() {
        let hook = hook_for(HASH).unwrap();
        for p in hook.patches {
            let n = p.new;
            // inc dword ptr [rip + disp32]
            assert_eq!(n[..2], [0xFF, 0x05], "{}", p.what);
            let disp = i32::from_le_bytes(n[2..6].try_into().unwrap()) as isize;
            assert_eq!(
                (p.rva + 6).wrapping_add_signed(disp),
                hook.counter,
                "{}",
                p.what
            );
            let mut at = 6;
            if n[at..at + 2] == [0x31, 0xC0] {
                at += 2;
            }
            assert_eq!(n[at], 0xEB, "{}: a short jump", p.what);
            let rel = n[at + 1] as i8 as isize;
            assert_eq!(
                (p.rva + at + 2).wrapping_add_signed(rel),
                p.resumes_at,
                "{}",
                p.what
            );
            // What follows the jump is never run: NOPs, or nothing.
            assert!(matches!(n[at + 2..], [] | [0x66, 0x90]), "{}", p.what);
        }
        assert_eq!(hook.patches[0].resumes_at, 0x2215c41);
        assert_eq!(hook.patches[1].resumes_at, 0x2215db7);
        // Only the second copy zeroes the count (eax) before it jumps.
        assert_eq!(hook.patches[1].new[6..8], [0x31, 0xC0]);
        assert_ne!(hook.patches[0].new[6..8], [0x31, 0xC0]);
    }

    /// `apply_mapped` on real pages: code made `PAGE_EXECUTE_READ` like an
    /// image's `.text`, a read-write page for the counter. The sites are
    /// written, and their page keeps its protection.
    #[cfg(windows)]
    #[test]
    fn apply_mapped_writes_read_only_code_and_restores_its_protection() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ,
            PAGE_READONLY, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
        };

        const PATCHES: &[Patch] = &[
            Patch {
                what: "test site one",
                rva: 0x100,
                expect: &[1, 2, 3, 4],
                new: &[5, 6, 7, 8],
                resumes_at: 0x200,
            },
            Patch {
                what: "test site two",
                rva: 0xff0,
                expect: &[9, 10],
                new: &[11, 12],
                resumes_at: 0x300,
            },
        ];
        const HOOK: Hook = Hook {
            build: &BUILD_1_0_2_1,
            patches: PATCHES,
            counter: 0x1ff8,
        };
        let protection = |at: usize| {
            // SAFETY: a plain query into a local the call fills.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            let n =
                unsafe { VirtualQuery(at as _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
            assert_ne!(n, 0);
            info.Protect
        };
        let set = |at: usize, len: usize, to: u32| {
            let mut old = 0;
            // SAFETY: our own allocation below.
            assert_ne!(unsafe { VirtualProtect(at as _, len, to, &mut old) }, 0);
        };

        // SAFETY: a fresh allocation of two pages, freed at the end.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                0x2000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(base, 0);
        for p in PATCHES {
            // SAFETY: inside the first page, writable for now.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    p.expect.as_ptr(),
                    (base + p.rva) as *mut u8,
                    p.expect.len(),
                )
            };
        }
        set(base, 0x1000, PAGE_EXECUTE_READ);

        // A counter that cannot be written: refused, nothing written.
        set(base + 0x1000, 0x1000, PAGE_READONLY);
        // SAFETY: two pages laid out like the hook's image; nothing runs them.
        let refused = unsafe { apply_mapped(&HOOK, base, 0x2000) };
        assert_eq!(refused, Err(NotApplied::Counter { rva: HOOK.counter }));
        for p in PATCHES {
            // SAFETY: readable.
            let now =
                unsafe { std::slice::from_raw_parts((base + p.rva) as *const u8, p.expect.len()) };
            assert_eq!(now, p.expect, "{}: written although refused", p.what);
        }

        set(base + 0x1000, 0x1000, PAGE_READWRITE);
        // A size that cuts the counter off: refused too.
        // SAFETY: as above.
        let cut = unsafe { apply_mapped(&HOOK, base, 0x1ffa) };
        assert_eq!(cut, Err(NotApplied::Counter { rva: HOOK.counter }));

        // SAFETY: as above.
        let applied = unsafe { apply_mapped(&HOOK, base, 0x2000) };
        assert_eq!(applied, Ok(PATCHES.len()));
        for p in PATCHES {
            // SAFETY: readable.
            let now =
                unsafe { std::slice::from_raw_parts((base + p.rva) as *const u8, p.new.len()) };
            assert_eq!(now, p.new, "{}", p.what);
            assert_eq!(protection(base + p.rva), PAGE_EXECUTE_READ, "{}", p.what);
        }
        assert_eq!(protection(base + HOOK.counter), PAGE_READWRITE);
        // Applied: a second time finds the new bytes and writes nothing.
        // SAFETY: as above.
        let again = unsafe { apply_mapped(&HOOK, base, 0x2000) };
        assert!(matches!(again, Err(NotApplied::Mismatch { .. })));

        // SAFETY: our allocation, no longer used.
        assert_ne!(unsafe { VirtualFree(base as _, 0, MEM_RELEASE) }, 0);
    }
}
