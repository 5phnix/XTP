//! The intro cut's sites against a real dump, as a repeatable check.
//!
//! Opt-in: `XTP_SAMPLE_GAME=<title folder> cargo test -p xtp-game --test
//! intro_sample -- --nocapture` hashes the folder's
//! `game_scarlett_master.exe`. For a build the cut knows, it checks that
//! each import slot is the one the executable's import table gives to that
//! `Bink2Scarlett.dll` function, and that the state words lie in writable
//! data. Without the variable it does nothing. Messages name offsets and
//! function names only.

use std::path::Path;

use xtp_game::intro;
use xtp_pe::{ImageInfo, ImportSymbol};

#[test]
fn intro_sites_in_the_sample_game() {
    let Some(dir) = std::env::var_os("XTP_SAMPLE_GAME") else {
        println!("SKIPPED intro_sites_in_the_sample_game: XTP_SAMPLE_GAME is not set");
        return;
    };
    let exe = Path::new(&dir).join("game_scarlett_master.exe");
    let Ok(file) = std::fs::read(&exe) else {
        println!(
            "SKIPPED intro_sites_in_the_sample_game: no {}",
            exe.display()
        );
        return;
    };
    let hash = blake3::hash(&file).to_hex();
    let Some(sites) = intro::sites_for(&hash) else {
        println!("build {hash} has no intro sites: nothing to check");
        return;
    };
    println!("build {} ({hash})", sites.build.label);
    let info = ImageInfo::parse(&file).expect("parsing the executable");

    let bink = info
        .imports
        .iter()
        .find(|m| m.dll.eq_ignore_ascii_case("Bink2Scarlett.dll"))
        .expect("the title imports Bink2Scarlett.dll");
    for (rva, function) in sites.imports() {
        let slot = bink
            .entries
            .iter()
            .find(|e| matches!(&e.symbol, ImportSymbol::Name { name, .. } if name == function));
        let slot = slot.unwrap_or_else(|| panic!("no import of {function}"));
        assert_eq!(slot.iat_slot_rva as usize, rva, "{function}'s import slot");
    }

    for (rva, len, what) in [
        (sites.front_end, 4, "front end word"),
        (sites.world_loading, 1, "world loading byte"),
    ] {
        let data = info.sections.iter().any(|s| {
            let start = s.rva as usize;
            (start..start + s.virtual_size as usize).contains(&rva)
                && rva + len <= start + s.virtual_size as usize
                && s.characteristics & 0x8000_0000 != 0 // IMAGE_SCN_MEM_WRITE
        });
        assert!(data, "the {what} at {rva:#x} is not in writable data");
    }
}
