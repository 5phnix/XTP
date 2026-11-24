//! An opt-in way past an assertion that the title's master build kept,
//! off by default: set `XTP_TITLE_ASSERTS=continue` and it is patched when
//! the title is mapped, before any of its code runs.
//!
//! What the title does (1.0.2.1, by RVA). A request worker thread
//! (`0x2592c50`) moves each of its requests through states (`0x2593560`; a
//! request's state is 5 bits that `0x2591de0` writes). From state `0xc` to
//! `0xd` it asks an "Archive" object (vtable `0x61cf360`, built at
//! `0x5b55850`) for the memory of one of its entries: slot 3 of that
//! vtable, `0x23d8e70`, finds the entry by handle (the low 22 bits; entries
//! of `0x30` bytes) and calls the entry's allocator. When the allocator
//! gives a null pointer, `0x23d8e70` breaks (`int3` at `0x23d8f03`), and
//! only then empties the entry's slot (releasing the object it held, as
//! `0x23d8ef0` does) and returns. Its caller does not look at the result:
//! for an Archive, the next two calls it makes (vtable `0x20` and `0xd0`)
//! are stubs that give null, and the request goes on through states `0xe`
//! and `0xf` to done. Nothing catches the break, so the process ends
//! there: the title's crash reporter runs, and then it is gone. Session
//! mesh64-a stopped there (docs/analysis/archive-null.md).
//!
//! The hook. The `int3` is reached only from the null test at `0x23d8ed5`,
//! so `rax` is zero there. The 22 bytes from the `int3` to the function's
//! end become: an increment of a counter; an exchange of `rax` with the
//! slot, which empties it and puts the object it held in `rax`; if there
//! was none, a jump to the function's return (`0x23d8efa`); otherwise
//! `rcx` = that object and a jump to the title's own release
//! (`0x23d8ef0`). The title then takes its own null path. Two differences
//! from the code it replaces: the slot is written even when it is already
//! empty (zero over zero), and the exchange is atomic (`xchg` with memory
//! always is).
//!
//! The counter is a `u32` in the slack past the end of `.data`, next to the
//! watchdog hook's ([`crate::watchdog`], whose patch machinery this
//! uses). A thread of ours reads it and logs when it moves: the first time
//! at once, then at most every 10 s, with the total.
//!
//! The trade-off: the hook hides an allocation that failed. The request
//! completes with the entry's slot empty, as the title's own code after the
//! `int3` leaves it; what the title does later with that empty entry is not
//! known (a null it tests, or a fault elsewhere). It is a way for test
//! sessions to go on, not a fix: the cause is in archive-null.md.

use std::ffi::OsStr;

use crate::BUILD_1_0_2_1;
use crate::watchdog::{Hook, NotApplied, Patch, count_into};

/// `XTP_TITLE_ASSERTS=continue` applies the hook.
pub const ENV: &str = "XTP_TITLE_ASSERTS";

/// The one value [`ENV`] knows.
pub const CONTINUE: &str = "continue";

/// What [`ENV`] asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asked {
    /// Not set: the title's assertions stay as they are.
    Nothing,
    /// `continue` (any case): patch them.
    Continue,
    /// Something else: nothing patched, and a warning.
    Unknown(String),
}

/// Reads [`ENV`]'s `value`.
pub fn asked(value: Option<&OsStr>) -> Asked {
    match value {
        None => Asked::Nothing,
        Some(v) if v.eq_ignore_ascii_case(CONTINUE) => Asked::Continue,
        Some(v) => Asked::Unknown(v.to_string_lossy().into_owned()),
    }
}

/// Builds the hook knows. Any other build is refused: its offsets would be
/// someone else's code.
pub const HOOKS: &[Hook] = &[Hook {
    build: &BUILD_1_0_2_1,
    patches: PATCHES_1_0_2_1,
    counter: COUNTER_1_0_2_1,
}];

/// In the slack past `.data` (it ends at `0xabce398`, `.pdata` starts at
/// `0xabcf000`), clear of the watchdog hook's counter at `0xabceff8`.
const COUNTER_1_0_2_1: usize = 0xabceff0;

/// Where the null pointer breaks.
const ARCHIVE_NULL_AT: usize = 0x23d8f03;

/// `0x23d8f03` to `0x23d8f19`: count; `xchg [rbx + rdi + 0x20], rax`;
/// `test rax, rax`; `je 0x23d8efa`; `mov rcx, rax`; `jmp 0x23d8ef0`; `nop`.
const ARCHIVE_NULL: [u8; 22] = {
    let c = count_into(ARCHIVE_NULL_AT, COUNTER_1_0_2_1);
    [
        c[0], c[1], c[2], c[3], c[4], c[5], // inc dword ptr [rip + disp32]
        0x48, 0x87, 0x44, 0x3B, 0x20, // xchg [rbx + rdi + 0x20], rax
        0x48, 0x85, 0xC0, // test rax, rax
        0x74, 0xE7, // je 0x23d8efa
        0x48, 0x89, 0xC1, // mov rcx, rax
        0xEB, 0xD8, // jmp 0x23d8ef0
        0x90, // nop
    ]
};

const PATCHES_1_0_2_1: &[Patch] = &[Patch {
    what: "Archive entry allocation: count a null pointer, then the title's own null path instead of its int3",
    rva: ARCHIVE_NULL_AT,
    // int3; mov rcx, [rbx + rdi + 0x20] (the slot); test rcx, rcx;
    // je 0x23d8efa; mov qword ptr [rbx + rdi + 0x20], 0; jmp 0x23d8ef0
    expect: &[
        0xCC, 0x48, 0x8B, 0x4C, 0x3B, 0x20, 0x48, 0x85, 0xC9, 0x74, 0xEC, 0x48, 0xC7, 0x44, 0x3B,
        0x20, 0x00, 0x00, 0x00, 0x00, 0xEB, 0xD7,
    ],
    new: &ARCHIVE_NULL,
    resumes_at: 0x23d8efa,
}];

/// The hook for the executable whose BLAKE3 is `blake3_hex` (any case).
pub fn hook_for(blake3_hex: &str) -> Result<&'static Hook, NotApplied> {
    HOOKS
        .iter()
        .find(|h| h.build.blake3.eq_ignore_ascii_case(blake3_hex))
        .ok_or(NotApplied::UnknownBuild)
}

/// [`Hook::apply_to`] for the build whose BLAKE3 is `blake3_hex`.
pub fn apply_to(blake3_hex: &str, image: &mut [u8]) -> Result<usize, NotApplied> {
    hook_for(blake3_hex)?.apply_to(image)
}

/// Applies the hook to the mapped title when [`ENV`] asks for it, and logs
/// the outcome; then logs the assertions it skips. Does nothing otherwise.
///
/// # Safety
/// `base..base + size` is the mapped image of the executable whose original
/// BLAKE3 is `blake3_hex`, and no title code runs yet.
#[cfg(windows)]
pub unsafe fn apply_if_asked(blake3_hex: &str, base: usize, size: u32) {
    match asked(std::env::var_os(ENV).as_deref()) {
        Asked::Nothing => return,
        Asked::Continue => {}
        Asked::Unknown(v) => {
            tracing::warn!(
                "{ENV}={v} is not known (only `{CONTINUE}`): the title's assertions are left as they are"
            );
            return;
        }
    }
    let applied = hook_for(blake3_hex).and_then(|hook| {
        // SAFETY: forwarded from this function's contract.
        unsafe { crate::watchdog::apply_mapped(hook, base, size as usize) }
            .map(|sites| (hook, sites))
    });
    match applied {
        Ok((hook, sites)) => {
            tracing::info!(
                build = hook.build.label,
                sites,
                "title assertion hook applied ({ENV}={CONTINUE})"
            );
            let at = hook.patches[0].rva;
            crate::watchdog::watch_counter(base + hook.counter, "xtp-asserts-log", 10, move |n| {
                tracing::warn!(
                    times = n,
                    "title assertion at {at:#x} skipped {n} times (an Archive entry's allocation gave no memory; the title took its own null path)"
                );
            });
        }
        Err(e) => tracing::warn!("title assertion hook not applied ({ENV}): {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = BUILD_1_0_2_1.blake3;

    /// A synthetic image for `hooks`: zeros, plus `fill` at every site.
    fn image(hooks: &[&Hook], fill: impl Fn(&Patch) -> &'static [u8]) -> Vec<u8> {
        let ends = hooks.iter().flat_map(|h| {
            let sites = h.patches.iter().map(|p| p.rva + p.expect.len());
            sites.chain([h.counter + 4])
        });
        let mut image = vec![0u8; ends.max().unwrap() + 64];
        for p in hooks.iter().flat_map(|h| h.patches) {
            image[p.rva..p.rva + p.expect.len()].copy_from_slice(fill(p));
        }
        image
    }

    #[test]
    fn applies_once_when_the_site_matches() {
        let hook = hook_for(HASH).unwrap();
        let mut img = image(&[hook], |p| p.expect);
        assert_eq!(apply_to(HASH, &mut img), Ok(1));
        assert_eq!(img, image(&[hook], |p| p.new));
        let before = img.clone();
        assert!(matches!(
            apply_to(HASH, &mut img),
            Err(NotApplied::Mismatch { .. })
        ));
        assert_eq!(img, before);
    }

    #[test]
    fn any_differing_byte_writes_nothing() {
        let hook = hook_for(HASH).unwrap();
        let p = hook.patches[0];
        for i in 0..p.expect.len() {
            let mut img = image(&[hook], |p| p.expect);
            img[p.rva + i] ^= 0x01;
            let before = img.clone();
            assert_eq!(
                apply_to(HASH, &mut img),
                Err(NotApplied::Mismatch {
                    what: p.what,
                    rva: p.rva
                }),
                "byte {i}"
            );
            assert_eq!(img, before, "byte {i}: something was written");
        }
    }

    #[test]
    fn a_used_counter_or_a_short_image_writes_nothing() {
        let hook = hook_for(HASH).unwrap();
        let mut img = image(&[hook], |p| p.expect);
        img[hook.counter] = 1;
        let before = img.clone();
        assert_eq!(
            apply_to(HASH, &mut img),
            Err(NotApplied::Counter { rva: hook.counter })
        );
        assert_eq!(img, before);
        let mut short = image(&[hook], |p| p.expect);
        short.truncate(hook.counter + 2);
        assert_eq!(
            hook.apply_to(&mut short),
            Err(NotApplied::Counter { rva: hook.counter })
        );
        let mut cut = image(&[hook], |p| p.expect);
        cut.truncate(hook.patches[0].rva + 10);
        assert!(matches!(
            hook.apply_to(&mut cut),
            Err(NotApplied::Mismatch { .. })
        ));
    }

    #[test]
    fn unknown_builds_are_refused() {
        let hook = hook_for(HASH).unwrap();
        let mut img = image(&[hook], |p| p.expect);
        let before = img.clone();
        for other in ["0".repeat(64), HASH[..16].to_owned(), String::new()] {
            assert_eq!(apply_to(&other, &mut img), Err(NotApplied::UnknownBuild));
        }
        assert_eq!(img, before);
        assert!(hook_for(&HASH.to_ascii_uppercase()).is_ok());
        for hook in HOOKS {
            assert!(crate::identify(hook.build.blake3).is_some());
        }
    }

    #[test]
    fn only_continue_asks_for_it() {
        assert_eq!(asked(None), Asked::Nothing);
        assert_eq!(asked(Some(OsStr::new("continue"))), Asked::Continue);
        assert_eq!(asked(Some(OsStr::new("Continue"))), Asked::Continue);
        for other in ["1", "", "on", "continues"] {
            assert_eq!(
                asked(Some(OsStr::new(other))),
                Asked::Unknown(other.to_owned())
            );
        }
    }

    /// Short jumps in `code` (at `rva`): (where, target).
    fn short_jumps(code: &[u8], rva: usize, at: &[usize]) -> Vec<(u8, usize)> {
        at.iter()
            .map(|&i| {
                let rel = code[i + 1] as i8 as isize;
                (code[i], (rva + i + 2).wrapping_add_signed(rel))
            })
            .collect()
    }

    /// The patch, decoded: it counts into the counter, empties the slot by
    /// an exchange with `rax`, and goes where the code it replaces went,
    /// the function's return or the title's release, with `rcx` the object.
    #[test]
    fn the_patch_counts_then_takes_the_titles_null_path() {
        let hook = hook_for(HASH).unwrap();
        let p = hook.patches[0];
        let n = p.new;
        assert_eq!(n.len(), p.expect.len());
        assert_eq!(p.rva + n.len(), 0x23d8f19, "ends where the function does");
        // inc dword ptr [rip + disp32], on the counter
        assert_eq!(n[..2], [0xFF, 0x05]);
        let disp = i32::from_le_bytes(n[2..6].try_into().unwrap()) as isize;
        assert_eq!((p.rva + 6).wrapping_add_signed(disp), hook.counter);
        // xchg [rbx + rdi + 0x20], rax: the slot the replaced code loads
        // (mov rcx, [rbx + rdi + 0x20]) and clears; same addressing bytes.
        assert_eq!(n[6..11], [0x48, 0x87, 0x44, 0x3B, 0x20]);
        assert_eq!(p.expect[1..6], [0x48, 0x8B, 0x4C, 0x3B, 0x20]);
        assert_eq!(p.expect[13..16], [0x44, 0x3B, 0x20]);
        // test rax, rax; then mov rcx, rax before the release.
        assert_eq!(n[11..14], [0x48, 0x85, 0xC0]);
        assert_eq!(n[16..19], [0x48, 0x89, 0xC1]);
        // je and jmp land where the replaced code's do: the return, then
        // the release.
        let new_jumps = short_jumps(n, p.rva, &[14, 19]);
        let old_jumps = short_jumps(p.expect, p.rva, &[9, 20]);
        assert_eq!(new_jumps, old_jumps);
        assert_eq!(new_jumps, [(0x74, 0x23d8efa), (0xEB, 0x23d8ef0)]);
        assert_eq!(p.resumes_at, 0x23d8efa);
        // The last byte is never run.
        assert_eq!(n[21], 0x90);
        // The int3 it replaces is the first byte.
        assert_eq!(p.expect[0], 0xCC);
        assert_eq!(n.iter().filter(|&&b| b == 0xCC).count(), 0);
    }

    /// Both hooks can be applied to one image, in either order: their
    /// sites and counters do not overlap, and the counter is aligned in the
    /// page it shares with the watchdog's.
    #[test]
    fn stays_clear_of_the_watchdog_hook() {
        let ours = hook_for(HASH).unwrap();
        let watchdog = crate::watchdog::hook_for(HASH).unwrap();
        let ranges = |h: &Hook| {
            let sites = h.patches.iter().map(|p| (p.rva, p.expect.len()));
            sites.chain([(h.counter, 4)]).collect::<Vec<_>>()
        };
        for (a, alen) in ranges(ours) {
            for (b, blen) in ranges(watchdog) {
                assert!(a + alen <= b || b + blen <= a, "{a:#x} overlaps {b:#x}");
            }
        }
        assert_eq!(ours.counter % 4, 0);
        assert_eq!(ours.counter / 0x1000, watchdog.counter / 0x1000);
        for first_ours in [true, false] {
            let mut img = image(&[ours, watchdog], |p| p.expect);
            let (first, second) = if first_ours {
                (ours, watchdog)
            } else {
                (watchdog, ours)
            };
            assert_eq!(first.apply_to(&mut img), Ok(first.patches.len()));
            assert_eq!(second.apply_to(&mut img), Ok(second.patches.len()));
            assert_eq!(img, image(&[ours, watchdog], |p| p.new));
        }
    }

    /// Against a real dump, when `XTP_SAMPLE_GAME` names its folder: for a
    /// build the hook knows, the site's bytes are found once, at its RVA,
    /// in executable code; the counter is in the unused end of a writable
    /// section; the hook applies to that copy once. Messages name offsets
    /// only, never the title's bytes.
    #[test]
    fn site_in_the_sample_game() {
        let Some(dir) = std::env::var_os("XTP_SAMPLE_GAME") else {
            println!("SKIPPED site_in_the_sample_game: XTP_SAMPLE_GAME is not set");
            return;
        };
        let exe = std::path::Path::new(&dir).join("game_scarlett_master.exe");
        let Ok(file) = std::fs::read(&exe) else {
            println!("SKIPPED site_in_the_sample_game: no {}", exe.display());
            return;
        };
        let hash = blake3::hash(&file).to_hex();
        let Ok(hook) = hook_for(&hash) else {
            println!("build {hash} has no assertion hook: nothing to check");
            return;
        };
        let info = xtp_pe::ImageInfo::parse(&file).expect("parsing the executable");
        let mut image = vec![0u8; info.size_of_image as usize];
        for s in &info.sections {
            let len = s.file_size.min(s.virtual_size) as usize;
            let (from, to) = (s.file_offset as usize, s.rva as usize);
            image[to..to + len].copy_from_slice(&file[from..from + len]);
        }
        for p in hook.patches {
            let hits: Vec<usize> = memchr::memmem::find_iter(&image, p.expect).collect();
            assert_eq!(hits, [p.rva], "{}: expected once, at {:#x}", p.what, p.rva);
            let code = info.sections.iter().any(|s| {
                let start = s.rva as usize;
                (start..start + s.virtual_size as usize).contains(&p.rva)
                    && s.characteristics & 0x2000_0000 != 0 // IMAGE_SCN_MEM_EXECUTE
            });
            assert!(code, "{} at {:#x} is not in executable code", p.what, p.rva);
        }
        let slack = info.sections.iter().any(|s| {
            let end = s.rva as usize + s.virtual_size as usize;
            s.characteristics & 0x8000_0000 != 0 // IMAGE_SCN_MEM_WRITE
                && (end..end.next_multiple_of(0x1000)).contains(&hook.counter)
                && hook.counter + 4 <= end.next_multiple_of(0x1000)
        });
        assert!(slack, "the counter at {:#x} is not in slack", hook.counter);
        assert_eq!(hook.apply_to(&mut image), Ok(hook.patches.len()));
        assert!(hook.apply_to(&mut image).is_err(), "applied twice");
    }
}
