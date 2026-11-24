//! The fingerprint of this crate's sources, made by `build.rs` (which
//! compiles this file as a module of its own) into [`crate::SOURCES`]: what
//! a translation is a
//! function of, besides its container and its settings. The shader cache
//! keys translations by it instead of by the whole executable, so a build
//! that differs only outside the translator keeps them.
//!
//! What it covers: every file under `src/` (tests too: a change to them
//! only costs translating again), the manifest, the build script, and the
//! build's settings `build.rs` passes (features, target, debug assertions).
//! The crate depends on no other crate of the workspace; its external
//! crates (`thiserror`, `miniz_oxide`) do not decide what it emits.
//!
//! The hash is FNV-1a on 128 bits: stable, dependency-free in a build
//! script, and amply wide to tell two versions of a few megabytes of
//! sources apart (it guards against accidents, not against an adversary).

use std::io;
use std::path::{Path, PathBuf};

const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

/// FNV-1a, 128 bits, fed in parts.
struct Fnv(u128);

impl Fnv {
    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u128::from(b)).wrapping_mul(PRIME);
        }
    }

    /// A part with its length first, so that parts do not run together.
    fn part(&mut self, bytes: &[u8]) {
        self.bytes(&(bytes.len() as u64).to_le_bytes());
        self.bytes(bytes);
    }
}

/// The fingerprint of the crate at `root` (its manifest, its build script
/// and every file under `src/`, by path relative to `root` and content)
/// and of `settings`, as 32 hexadecimal digits.
pub fn of(root: &Path, settings: &[String]) -> io::Result<String> {
    let mut files = vec![root.join("Cargo.toml"), root.join("build.rs")];
    walk(&root.join("src"), &mut files)?;
    let mut named: Vec<(String, PathBuf)> = files
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            (name, path)
        })
        .collect();
    named.sort();
    let mut h = Fnv(OFFSET);
    h.part(b"xtp-shader sources 1");
    for (name, path) in &named {
        h.part(name.as_bytes());
        h.part(&std::fs::read(path)?);
    }
    for setting in settings {
        h.part(setting.as_bytes());
    }
    Ok(format!("{:032x}", h.0))
}

/// Every file under `dir`, at any depth.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A crate tree in a folder of its own, removed when dropped: a
    /// manifest, a build script and two sources, one in a folder.
    struct Tree(PathBuf);

    impl Tree {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tree() -> Tree {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("xtp-fingerprint-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/emit")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        std::fs::write(root.join("build.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod emit;\n").unwrap();
        std::fs::write(root.join("src/emit/mod.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
        Tree(root)
    }

    /// The same sources give the same fingerprint (another folder, another
    /// time); a change to any file of the translator, a file added or
    /// renamed, or another build setting gives another.
    #[test]
    fn the_fingerprint_follows_the_sources_and_nothing_else() {
        let (a, b) = (tree(), tree());
        let none: &[String] = &[];
        let print = of(a.path(), none).unwrap();
        assert_eq!(print.len(), 32);
        assert_eq!(of(b.path(), none).unwrap(), print, "same sources elsewhere");
        // A file of the crate outside its sources is no input (nor is
        // anything outside the crate: the rest of the executable).
        std::fs::write(a.path().join("README.md"), "changed").unwrap();
        assert_eq!(of(a.path(), none).unwrap(), print);

        let changed = |edit: &dyn Fn(&Path)| {
            let t = tree();
            edit(t.path());
            of(t.path(), none).unwrap()
        };
        let emit = |root: &Path| root.join("src/emit/mod.rs");
        assert_ne!(
            changed(&|r| std::fs::write(emit(r), "pub fn f() -> u32 { 2 }\n").unwrap()),
            print,
            "a source changed"
        );
        assert_ne!(
            changed(&|r| std::fs::write(r.join("src/emit/new.rs"), "").unwrap()),
            print,
            "a source added"
        );
        assert_ne!(
            changed(&|r| std::fs::rename(emit(r), r.join("src/emit/moved.rs")).unwrap()),
            print,
            "a source renamed"
        );
        assert_ne!(
            changed(&|r| std::fs::write(r.join("Cargo.toml"), "[package]\n").unwrap()),
            print,
            "the manifest changed"
        );
        let feature = ["CARGO_FEATURE_DIAGNOSTICS".to_owned()];
        assert_ne!(of(a.path(), &feature).unwrap(), print, "a feature");
    }

    /// Parts are framed: moving bytes from one file's end to the next
    /// file's start is a change.
    #[test]
    fn parts_do_not_run_together() {
        let t = tree();
        let print = of(t.path(), &[]).unwrap();
        std::fs::write(t.path().join("src/emit/mod.rs"), "pub fn f() -> u32 { 1 }").unwrap();
        std::fs::write(t.path().join("src/lib.rs"), "pub mod emit;\n\n").unwrap();
        assert_ne!(of(t.path(), &[]).unwrap(), print);
    }

    /// This crate's own fingerprint is what the build script made.
    #[test]
    fn the_crates_fingerprint_is_set() {
        assert_eq!(crate::SOURCES.len(), 32);
        assert!(crate::SOURCES.bytes().all(|c| c.is_ascii_hexdigit()));
    }
}
