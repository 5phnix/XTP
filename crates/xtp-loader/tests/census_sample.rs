//! The import census of a real title folder, as a repeatable check.
//!
//! Opt-in: `XTP_SAMPLE_GAME=<title folder> cargo test -p xtp-loader --test
//! census_sample -- --nocapture` classifies every image of the folder
//! against our runtime and the host, as `xtp run` binds them, and prints
//! the stubbed and missing imports. Without the variable it does nothing.
//!
//! If `census/expected.txt` describes the same build, the test fails when
//! an import that resolved (or was stubbed) is now missing, and says when
//! the file can list fewer. `XTP_CENSUS_BLESS=1` rewrites the file; it
//! holds public API names and counts only.
#![cfg(windows)]

use std::path::{Path, PathBuf};

use xtp_hle::{HleResolver, HostPolicy};
use xtp_loader::{Census, StubDb};

fn expected_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/census/expected.txt")
}

#[test]
fn census_of_the_sample_game() {
    let Some(dir) = std::env::var_os("XTP_SAMPLE_GAME") else {
        return;
    };
    let host = HostPolicy::default();
    let resolver = HleResolver::new(host.clone());
    // As `xtp run` takes title DLLs: never one the host or our runtime provides.
    let wanted =
        |dll: &str| !host.allows(dll) && !xtp_hle::registry().iter().any(|m| m.serves(dll));
    let census = Census::of_folder(Path::new(&dir), &resolver, &wanted, StubDb::builtin())
        .expect("classifying the sample folder");
    println!("{}", census.report());
    assert!(!census.images.is_empty(), "no image in {dir:?}");

    let file = expected_file();
    if std::env::var_os("XTP_CENSUS_BLESS").is_some() {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, census.expectation()).unwrap();
        println!("wrote {}", file.display());
        return;
    }
    let Ok(expected) = std::fs::read_to_string(&file) else {
        println!("no {}: nothing to compare with", file.display());
        return;
    };
    let drift = census.compare(&expected);
    if let Some(build) = &drift.other_build {
        println!("{} describes build {build}: not compared", file.display());
        return;
    }
    for key in &drift.improvements {
        println!("now covered: {key} (XTP_CENSUS_BLESS=1 updates the file)");
    }
    assert!(
        drift.regressions.is_empty(),
        "imports missing now that were not: {:?}",
        drift.regressions
    );
}
