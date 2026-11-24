//! The watchdog hook against a real dump, as a repeatable check.
//!
//! Opt-in: `XTP_SAMPLE_GAME=<title folder> cargo test -p xtp-game --test
//! watchdog_sample -- --nocapture` hashes the folder's
//! `game_scarlett_master.exe`. For a build the hook knows, it lays the
//! image out as mapped and checks that each site holds the bytes the hook
//! expects at its offset and nowhere else in the image, and that the hook
//! applies to that copy once. Without the variable it does nothing.
//! Messages name offsets only, never the title's bytes.

use std::path::Path;

use xtp_game::watchdog;

#[test]
fn watchdog_sites_in_the_sample_game() {
    let Some(dir) = std::env::var_os("XTP_SAMPLE_GAME") else {
        println!("SKIPPED watchdog_sites_in_the_sample_game: XTP_SAMPLE_GAME is not set");
        return;
    };
    let exe = Path::new(&dir).join("game_scarlett_master.exe");
    let Ok(file) = std::fs::read(&exe) else {
        println!(
            "SKIPPED watchdog_sites_in_the_sample_game: no {}",
            exe.display()
        );
        return;
    };
    let hash = blake3::hash(&file).to_hex();
    let Ok(hook) = watchdog::hook_for(&hash) else {
        println!("build {hash} has no watchdog hook: nothing to check");
        return;
    };
    println!("build {} ({hash})", hook.build.label);

    // The image as the loader maps it: each section's file data at its RVA.
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
    assert_eq!(hook.apply_to(&mut image), Ok(hook.patches.len()));
    for p in hook.patches {
        assert!(image[p.rva..p.rva + p.new.len()] == *p.new, "{}", p.what);
    }
    assert!(hook.apply_to(&mut image).is_err(), "applied twice");
}
