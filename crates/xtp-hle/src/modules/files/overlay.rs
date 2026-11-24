//! The package drive's copy-on-write overlay.
//!
//! The package folder, the lower layer, is never written: it is the
//! person's game dump, and every measurement on it must stay valid.
//! Changing one of its files copies that one file into the upper layer
//! first, and the change goes to the copy; reads take the upper layer
//! first; deleting a package entry records a whiteout that hides it.
//! Entries that exist only in the upper layer behave as in any folder.
//! Pure logic over `std::fs`: the title's file API reaches it through
//! `files`.
//!
//! Paths here are relative to the package root: components joined by `\`,
//! without empty ones, `.` or `..`; the root is the empty path. Names
//! compare ignoring case, as the host's do.
//!
//! The overlay folder holds `files\` (the upper layer), `whiteouts.txt`
//! (the hidden paths, lower case, one per line) and `staging\` (copies in
//! progress, moved into the upper layer once complete).

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY, ERROR_DIRECTORY,
    ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
};

/// What a change needs of the entry beforehand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Must {
    Exist,
    NotExist,
    Either,
}

/// An entry made ready for a change.
#[derive(Debug)]
pub struct Prepared {
    /// Its upper path, where the change goes.
    pub path: PathBuf,
    /// It was there before.
    pub existed: bool,
    /// The upper entry is there (it was, or the package's was copied up);
    /// if not, the change creates it.
    pub present: bool,
}

pub struct Overlay {
    lower: PathBuf,
    upper: PathBuf,
    staging: PathBuf,
    whiteout_file: PathBuf,
    whiteouts: Mutex<BTreeSet<String>>,
    /// Copies run one at a time, so a file opened twice is copied once.
    copying: Mutex<()>,
    staged: AtomicU64,
}

fn error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

/// How whiteouts name a path.
fn key(rel: &str) -> String {
    rel.to_lowercase()
}

/// The folder holding `rel`; `None` for the root.
pub fn parent(rel: &str) -> Option<&str> {
    if rel.is_empty() {
        return None;
    }
    Some(rel.rsplit_once('\\').map_or("", |(folder, _)| folder))
}

/// `name` in the folder `rel`.
pub fn child(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_owned()
    } else {
        format!("{rel}\\{name}")
    }
}

/// `rel` below `root`. Not `Path::join`, which lets a component such as
/// `C:` replace the root.
pub fn under(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.as_os_str().to_owned();
    if !rel.is_empty() {
        path.push("\\");
        path.push(rel);
    }
    path.into()
}

/// What is at `path`, links followed (a broken one still counts).
fn stat(path: &Path) -> Option<Metadata> {
    fs::metadata(path)
        .or_else(|_| fs::symlink_metadata(path))
        .ok()
}

/// Whether a whiteout in `set` hides `rel`: one on it or on a folder
/// above it.
fn hides(set: &BTreeSet<String>, rel: &str) -> bool {
    if set.is_empty() {
        return false;
    }
    let key = key(rel);
    let mut at = key.as_str();
    loop {
        if set.contains(at) {
            return true;
        }
        match at.rsplit_once('\\') {
            Some((folder, _)) => at = folder,
            None => return false,
        }
    }
}

impl Overlay {
    /// The overlay in the folder `root` over the package folder `lower`.
    /// Nothing is written until something changes; copies a crash cut
    /// short are dropped.
    pub fn open(lower: &Path, root: &Path) -> io::Result<Overlay> {
        let whiteout_file = root.join("whiteouts.txt");
        let whiteouts = match fs::read_to_string(&whiteout_file) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
            Err(e) => return Err(e),
        };
        let staging = root.join("staging");
        if let Ok(entries) = fs::read_dir(&staging) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(Overlay {
            lower: lower.to_owned(),
            upper: root.join("files"),
            staging,
            whiteout_file,
            whiteouts: Mutex::new(whiteouts),
            copying: Mutex::new(()),
            staged: AtomicU64::new(0),
        })
    }

    pub fn upper_root(&self) -> &Path {
        &self.upper
    }

    pub fn lower_path(&self, rel: &str) -> PathBuf {
        under(&self.lower, rel)
    }

    pub fn upper_path(&self, rel: &str) -> PathBuf {
        under(&self.upper, rel)
    }

    fn whiteouts(&self) -> MutexGuard<'_, BTreeSet<String>> {
        self.whiteouts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether a whiteout hides the package entry `rel`.
    pub fn hidden(&self, rel: &str) -> bool {
        hides(&self.whiteouts(), rel)
    }

    /// The upper entry at `rel`.
    pub fn in_upper(&self, rel: &str) -> Option<Metadata> {
        stat(&self.upper_path(rel))
    }

    /// The package entry at `rel`, unless a whiteout hides it.
    pub fn in_lower(&self, rel: &str) -> Option<Metadata> {
        if self.hidden(rel) {
            return None;
        }
        stat(&self.lower_path(rel))
    }

    /// What the title sees at `rel`: the upper entry, else the package's.
    pub fn entry(&self, rel: &str) -> Option<Metadata> {
        self.in_upper(rel).or_else(|| self.in_lower(rel))
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.entry(rel).is_some()
    }

    pub fn is_dir(&self, rel: &str) -> bool {
        self.entry(rel).is_some_and(|m| m.is_dir())
    }

    /// The error for `rel` not being there, as the host words it: its
    /// folder is there (`ERROR_FILE_NOT_FOUND`) or not
    /// (`ERROR_PATH_NOT_FOUND`).
    pub fn missing(&self, rel: &str) -> io::Error {
        let folder = parent(rel).is_none_or(|folder| self.is_dir(folder));
        error(if folder {
            ERROR_FILE_NOT_FOUND
        } else {
            ERROR_PATH_NOT_FOUND
        })
    }

    /// Where to read `rel`: the upper layer first.
    pub fn read_path(&self, rel: &str) -> io::Result<PathBuf> {
        if self.in_upper(rel).is_some() {
            return Ok(self.upper_path(rel));
        }
        if self.in_lower(rel).is_some() {
            return Ok(self.lower_path(rel));
        }
        Err(self.missing(rel))
    }

    /// Makes `rel` ready for a change, which then goes to the upper path.
    /// With `keep`, a package file is copied up first; a package folder
    /// always is (its upper twin is made).
    pub fn prepare(&self, rel: &str, must: Must, keep: bool) -> io::Result<Prepared> {
        let upper = self.in_upper(rel).is_some();
        let lower = if upper { None } else { self.in_lower(rel) };
        let existed = upper || lower.is_some();
        match must {
            Must::NotExist if existed => return Err(error(ERROR_FILE_EXISTS)),
            Must::Exist if !existed => return Err(self.missing(rel)),
            _ => {}
        }
        let present = match lower {
            _ if upper => true,
            Some(meta) if keep || meta.is_dir() => {
                self.copy_up(rel)?;
                true
            }
            _ => {
                self.stage(rel)?;
                false
            }
        };
        Ok(Prepared {
            path: self.upper_path(rel),
            existed,
            present,
        })
    }

    /// Room for a new upper entry at `rel`: its folder must be there, and
    /// gets an upper twin.
    pub fn stage(&self, rel: &str) -> io::Result<PathBuf> {
        if let Some(folder) = parent(rel) {
            if !self.is_dir(folder) {
                return Err(error(ERROR_PATH_NOT_FOUND));
            }
            self.make_folders(folder)?;
        }
        Ok(self.upper_path(rel))
    }

    /// Makes the upper folder `rel` and those above it; returns the ones it
    /// made, outermost first.
    fn make_folders(&self, rel: &str) -> io::Result<Vec<PathBuf>> {
        if let Some(root) = self.upper.parent() {
            fs::create_dir_all(root)?;
        }
        let mut made = Vec::new();
        let mut path = self.upper.as_os_str().to_owned();
        let parts = std::iter::once("").chain(rel.split('\\').filter(|p| !p.is_empty()));
        for part in parts {
            if !part.is_empty() {
                path.push("\\");
                path.push(part);
            }
            let at = PathBuf::from(&path);
            match fs::create_dir(&at) {
                Ok(()) => made.push(at),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && at.is_dir() => {}
                Err(e) => {
                    unmake(&made);
                    return Err(e);
                }
            }
        }
        Ok(made)
    }

    /// Copies the package entry `rel` into the upper layer unless it is
    /// there already (a folder: makes its upper twin). A copy that fails
    /// leaves both layers as they were.
    pub fn copy_up(&self, rel: &str) -> io::Result<PathBuf> {
        let _one = self.copying.lock().unwrap_or_else(|e| e.into_inner());
        let upper = self.upper_path(rel);
        if self.in_upper(rel).is_some() {
            return Ok(upper);
        }
        let Some(meta) = self.in_lower(rel) else {
            return Err(self.missing(rel));
        };
        if meta.is_dir() {
            self.make_folders(rel)?;
            return Ok(upper);
        }
        self.copy_to(&self.lower_path(rel), rel)?;
        tracing::info!(file = rel, "package file copied to the overlay");
        Ok(upper)
    }

    /// Copies the host file `from` to the upper entry `rel` through the
    /// staging folder, so a copy cut short publishes nothing.
    fn copy_to(&self, from: &Path, rel: &str) -> io::Result<()> {
        fs::create_dir_all(&self.staging)?;
        let n = self.staged.fetch_add(1, Ordering::Relaxed);
        let staged = self.staging.join(format!("{}-{n}", std::process::id()));
        if let Err(e) = fs::copy(from, &staged) {
            let _ = fs::remove_file(&staged);
            return Err(e);
        }
        let made = match parent(rel).map_or(Ok(Vec::new()), |folder| self.make_folders(folder)) {
            Ok(made) => made,
            Err(e) => {
                let _ = fs::remove_file(&staged);
                return Err(e);
            }
        };
        if let Err(e) = fs::rename(&staged, self.upper_path(rel)) {
            let _ = fs::remove_file(&staged);
            unmake(&made);
            return Err(e);
        }
        Ok(())
    }

    /// Records that `rel` was made in the upper layer: whiteouts on it and
    /// on the folders above it go. The package entries of a folder that
    /// comes back stay hidden.
    pub fn commit(&self, rel: &str) -> io::Result<()> {
        let mut set = self.whiteouts();
        if !hides(&set, rel) {
            return Ok(());
        }
        let before = set.clone();
        let key = key(rel);
        let ends = key.match_indices('\\').map(|(i, _)| i).chain([key.len()]);
        for end in ends {
            let at = &key[..end];
            if set.remove(at)
                && let Ok(entries) = fs::read_dir(self.lower_path(at))
            {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    set.insert(child(at, &name));
                }
            }
        }
        if let Err(e) = self.persist(&set) {
            *set = before;
            return Err(e);
        }
        Ok(())
    }

    /// Hides the package entry `rel`, if there is one.
    pub fn hide(&self, rel: &str) -> io::Result<()> {
        if stat(&self.lower_path(rel)).is_none() {
            return Ok(());
        }
        let mut set = self.whiteouts();
        let key = key(rel);
        if !set.insert(key.clone()) {
            return Ok(());
        }
        if let Err(e) = self.persist(&set) {
            set.remove(&key);
            return Err(e);
        }
        tracing::info!(file = rel, "package entry hidden");
        Ok(())
    }

    /// Writes the whiteouts, replacing the earlier list in one step.
    fn persist(&self, set: &BTreeSet<String>) -> io::Result<()> {
        if let Some(root) = self.whiteout_file.parent() {
            fs::create_dir_all(root)?;
        }
        let text: String = set.iter().flat_map(|k| [k.as_str(), "\n"]).collect();
        let next = self.whiteout_file.with_extension("txt.new");
        fs::write(&next, text)?;
        fs::rename(&next, &self.whiteout_file)
    }

    /// Deletes the file `rel`: its upper copy goes, and a whiteout hides
    /// the package's.
    pub fn delete_file(&self, rel: &str) -> io::Result<()> {
        if let Some(meta) = self.in_upper(rel) {
            if meta.is_dir() || meta.permissions().readonly() {
                return Err(error(ERROR_ACCESS_DENIED));
            }
            fs::remove_file(self.upper_path(rel))?;
            return self.hide(rel);
        }
        match self.in_lower(rel) {
            None => Err(self.missing(rel)),
            Some(meta) if meta.is_dir() || meta.permissions().readonly() => {
                Err(error(ERROR_ACCESS_DENIED))
            }
            Some(_) => self.hide(rel),
        }
    }

    pub fn create_dir(&self, rel: &str) -> io::Result<()> {
        if self.exists(rel) {
            return Err(error(ERROR_ALREADY_EXISTS));
        }
        fs::create_dir(self.stage(rel)?)?;
        self.commit(rel)
    }

    /// Removes the empty folder `rel`.
    pub fn remove_dir(&self, rel: &str) -> io::Result<()> {
        let Some(meta) = self.entry(rel) else {
            return Err(self.missing(rel));
        };
        if rel.is_empty() {
            return Err(error(ERROR_ACCESS_DENIED));
        }
        if !meta.is_dir() {
            return Err(error(ERROR_DIRECTORY));
        }
        if !self.read_dir(rel)?.is_empty() {
            return Err(error(ERROR_DIR_NOT_EMPTY));
        }
        if self.in_upper(rel).is_some() {
            fs::remove_dir(self.upper_path(rel))?;
        }
        self.hide(rel)
    }

    /// Whether the folder `rel` can move: only an upper folder that shows
    /// no package entries does, since those would have to be copied.
    pub fn movable(&self, rel: &str) -> io::Result<()> {
        if self.in_upper(rel).is_none() {
            return Err(error(ERROR_ACCESS_DENIED));
        }
        if self.in_lower(rel).is_some_and(|m| m.is_dir()) {
            for entry in fs::read_dir(self.lower_path(rel))? {
                let name = entry?.file_name();
                if !self.hidden(&child(rel, &name.to_string_lossy())) {
                    return Err(error(ERROR_ACCESS_DENIED));
                }
            }
        }
        Ok(())
    }

    /// Renames `from` to `to`, both in the package; `replace` lets a file
    /// take another's place. A package file is copied to its new name; a
    /// package folder does not move.
    pub fn rename(&self, from: &str, to: &str, replace: bool) -> io::Result<()> {
        let Some(meta) = self.entry(from) else {
            return Err(self.missing(from));
        };
        if from.is_empty() || to.is_empty() {
            return Err(error(ERROR_ACCESS_DENIED));
        }
        let upper = self.in_upper(from).is_some();
        if key(from) == key(to) {
            // The same entry: at most the case of its name changes, which
            // only the upper layer records.
            if from != to {
                if !upper {
                    if meta.is_dir() {
                        return Err(error(ERROR_ACCESS_DENIED));
                    }
                    self.copy_up(from)?;
                }
                fs::rename(self.upper_path(from), self.upper_path(to))?;
            }
            return Ok(());
        }
        if let Some(target) = self.entry(to) {
            if !replace {
                return Err(error(ERROR_ALREADY_EXISTS));
            }
            if target.is_dir() || meta.is_dir() {
                return Err(error(ERROR_ACCESS_DENIED));
            }
        }
        let folder = parent(to).unwrap_or_default();
        if !self.is_dir(folder) {
            return Err(error(ERROR_PATH_NOT_FOUND));
        }
        if meta.is_dir() {
            self.movable(from)?;
        }
        if upper {
            let made = self.make_folders(folder)?;
            if let Err(e) = fs::rename(self.upper_path(from), self.upper_path(to)) {
                unmake(&made);
                return Err(e);
            }
        } else {
            let _one = self.copying.lock().unwrap_or_else(|e| e.into_inner());
            self.copy_to(&self.lower_path(from), to)?;
        }
        self.hide(from)?;
        self.commit(to)
    }

    /// Whether listing the folder `dir` needs both layers: it has an upper
    /// twin, or whiteouts below it.
    pub fn needs_merge(&self, dir: &str) -> bool {
        if self.in_upper(dir).is_some_and(|m| m.is_dir()) {
            return true;
        }
        let set = self.whiteouts();
        let key = key(dir);
        if key.is_empty() {
            return !set.is_empty();
        }
        let below = format!("{key}\\");
        set.range::<str, _>((Bound::Included(below.as_str()), Bound::Unbounded))
            .next()
            .is_some_and(|w| w.starts_with(&below))
    }

    /// The folder `dir`'s entries from both layers as a listing shows
    /// them: the upper ones, and the package ones they do not shadow and no
    /// whiteout hides; `.` and `..` first, then by name ignoring case, as
    /// NTFS lists.
    pub fn merge<T>(
        &self,
        dir: &str,
        upper: Vec<T>,
        lower: Vec<T>,
        name: impl Fn(&T) -> String,
    ) -> Vec<T> {
        let shadowed: HashSet<String> = upper.iter().map(|e| name(e).to_lowercase()).collect();
        let mut all: Vec<(String, T)> = upper.into_iter().map(|e| (name(&e), e)).collect();
        let set = self.whiteouts();
        for entry in lower {
            let n = name(&entry);
            let dots = n == "." || n == "..";
            if shadowed.contains(&n.to_lowercase()) || (!dots && hides(&set, &child(dir, &n))) {
                continue;
            }
            all.push((n, entry));
        }
        drop(set);
        all.sort_by_cached_key(|(n, _)| {
            let rank = match n.as_str() {
                "." => 0,
                ".." => 1,
                _ => 2,
            };
            (rank, n.to_uppercase())
        });
        all.into_iter().map(|(_, e)| e).collect()
    }

    /// The names in the folder `dir`, both layers merged.
    pub fn read_dir(&self, dir: &str) -> io::Result<Vec<OsString>> {
        let Some(meta) = self.entry(dir) else {
            return Err(self.missing(dir));
        };
        if !meta.is_dir() {
            return Err(error(ERROR_DIRECTORY));
        }
        let names = |path: PathBuf| -> io::Result<Vec<OsString>> {
            fs::read_dir(path)?
                .map(|e| e.map(|e| e.file_name()))
                .collect()
        };
        let upper = match self.in_upper(dir) {
            Some(m) if m.is_dir() => names(self.upper_path(dir))?,
            _ => Vec::new(),
        };
        let lower = match self.in_lower(dir) {
            Some(m) if m.is_dir() => names(self.lower_path(dir))?,
            _ => Vec::new(),
        };
        Ok(self.merge(dir, upper, lower, |n| n.to_string_lossy().into_owned()))
    }
}

/// Removes folders `make_folders` made, innermost first.
fn unmake(made: &[PathBuf]) {
    for dir in made.iter().rev() {
        let _ = fs::remove_dir(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A package folder and an overlay folder over it.
    struct Layers {
        _dir: tempfile::TempDir,
        package: PathBuf,
        root: PathBuf,
    }

    impl Layers {
        fn new() -> Layers {
            let dir = tempfile::tempdir().unwrap();
            let package = dir.path().join("package");
            for folder in ["data/sub", "empty"] {
                fs::create_dir_all(package.join(folder)).unwrap();
            }
            for (file, bytes) in [
                ("a.txt", &b"package a"[..]),
                ("data/b.bin", &[0, 1, 2, 3, 255][..]),
                ("data/sub/c.txt", b"package c"),
            ] {
                fs::write(package.join(file), bytes).unwrap();
            }
            let root = dir.path().join("overlay");
            Layers {
                _dir: dir,
                package,
                root,
            }
        }

        fn open(&self) -> Overlay {
            Overlay::open(&self.package, &self.root).unwrap()
        }
    }

    /// Every entry below `dir`: its path and, for a file, its bytes.
    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut todo = vec![dir.to_owned()];
        while let Some(at) = todo.pop() {
            let Ok(entries) = fs::read_dir(&at) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                let rel = path.strip_prefix(dir).unwrap().to_owned();
                if path.is_dir() {
                    todo.push(path);
                    out.insert(rel, None);
                } else {
                    out.insert(rel, Some(fs::read(&path).unwrap()));
                }
            }
        }
        out
    }

    fn code(result: io::Result<impl std::fmt::Debug>) -> u32 {
        result.unwrap_err().raw_os_error().unwrap() as u32
    }

    fn names(list: Vec<OsString>) -> Vec<String> {
        list.into_iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_write_copies_that_one_file_and_reads_take_the_copy() {
        let layers = Layers::new();
        let package = snapshot(&layers.package);
        let overlay = layers.open();
        assert_eq!(
            overlay.read_path("data\\b.bin").unwrap(),
            layers.package.join("data\\b.bin"),
            "an untouched file reads from the package"
        );

        let prepared = overlay.prepare("data\\b.bin", Must::Exist, true).unwrap();
        assert!(prepared.existed && prepared.present);
        assert_eq!(prepared.path, layers.root.join("files\\data\\b.bin"));
        let mut bytes = fs::read(&prepared.path).unwrap();
        assert_eq!(bytes, [0, 1, 2, 3, 255], "the copy holds the package bytes");
        bytes.extend_from_slice(b"new");
        fs::write(&prepared.path, &bytes).unwrap();

        assert_eq!(
            snapshot(&layers.package),
            package,
            "the package is unchanged"
        );
        let upper: Vec<_> = snapshot(&layers.root.join("files")).into_keys().collect();
        assert_eq!(
            upper,
            [PathBuf::from("data"), PathBuf::from("data\\b.bin")],
            "only that file was copied"
        );
        let read = overlay.read_path("data\\b.bin").unwrap();
        assert_eq!(read, prepared.path);
        assert_eq!(fs::read(read).unwrap(), bytes);
        assert!(overlay.lower_path("C:\\x").starts_with(&layers.package));
    }

    #[test]
    fn a_delete_hides_the_file_until_it_is_made_again() {
        let layers = Layers::new();
        let package = snapshot(&layers.package);
        let overlay = layers.open();
        overlay.delete_file("A.TXT").unwrap();
        assert_eq!(code(overlay.read_path("a.txt")), ERROR_FILE_NOT_FOUND);
        assert_eq!(code(overlay.delete_file("a.txt")), ERROR_FILE_NOT_FOUND);
        assert_eq!(code(overlay.read_path("gone\\a.txt")), ERROR_PATH_NOT_FOUND);
        assert_eq!(
            code(overlay.prepare("a.txt", Must::Exist, true)),
            ERROR_FILE_NOT_FOUND
        );
        assert_eq!(snapshot(&layers.package), package);
        drop(overlay);

        // Whiteouts outlive the overlay.
        let overlay = layers.open();
        assert!(!overlay.exists("a.txt"));
        let prepared = overlay.prepare("a.txt", Must::NotExist, false).unwrap();
        assert!(!prepared.existed && !prepared.present);
        fs::write(&prepared.path, b"made again").unwrap();
        overlay.commit("a.txt").unwrap();
        assert_eq!(
            fs::read(overlay.read_path("a.txt").unwrap()).unwrap(),
            b"made again"
        );
        drop(overlay);
        assert!(layers.open().exists("a.txt"));
        assert_eq!(snapshot(&layers.package), package);
    }

    #[test]
    fn renames_work_across_both_layers() {
        let layers = Layers::new();
        let package = snapshot(&layers.package);
        let overlay = layers.open();

        // A package file moves by copy, the original hidden.
        overlay.rename("a.txt", "data\\moved.txt", false).unwrap();
        assert!(!overlay.exists("a.txt"));
        let moved = overlay.read_path("data\\moved.txt").unwrap();
        assert_eq!(fs::read(moved).unwrap(), b"package a");

        // An upper file moves as itself.
        overlay
            .rename("data\\moved.txt", "moved.txt", false)
            .unwrap();
        assert!(!overlay.exists("data\\moved.txt"));
        assert!(overlay.in_upper("moved.txt").is_some());

        // Taking another file's place needs `replace`.
        assert_eq!(
            code(overlay.rename("moved.txt", "data\\b.bin", false)),
            ERROR_ALREADY_EXISTS
        );
        overlay.rename("moved.txt", "data\\b.bin", true).unwrap();
        let replaced = fs::read(overlay.read_path("data\\b.bin").unwrap()).unwrap();
        assert_eq!(replaced, b"package a");

        // A change of case keeps the entry, under its new name.
        overlay
            .rename("data\\sub\\c.txt", "data\\sub\\C.TXT", false)
            .unwrap();
        assert_eq!(names(overlay.read_dir("data\\sub").unwrap()), ["C.TXT"]);
        assert_eq!(
            fs::read(overlay.read_path("data\\sub\\c.txt").unwrap()).unwrap(),
            b"package c"
        );

        // Package folders do not move; missing folders are reported.
        assert_eq!(
            code(overlay.rename("data", "elsewhere", false)),
            ERROR_ACCESS_DENIED
        );
        assert_eq!(
            code(overlay.rename("data\\b.bin", "nowhere\\b.bin", false)),
            ERROR_PATH_NOT_FOUND
        );
        assert_eq!(
            code(overlay.rename("a.txt", "b.txt", false)),
            ERROR_FILE_NOT_FOUND
        );
        assert_eq!(snapshot(&layers.package), package);
    }

    #[test]
    fn listings_merge_both_layers_without_whiteouts() {
        let layers = Layers::new();
        let overlay = layers.open();
        assert!(!overlay.needs_merge(""));
        let prepared = overlay.prepare("new.txt", Must::NotExist, false).unwrap();
        fs::write(prepared.path, b"new").unwrap();
        overlay.commit("new.txt").unwrap();
        overlay.prepare("data\\b.bin", Must::Exist, true).unwrap();
        overlay.delete_file("a.txt").unwrap();

        assert!(overlay.needs_merge("") && overlay.needs_merge("data"));
        assert!(!overlay.needs_merge("empty") && !overlay.needs_merge("data\\sub"));
        assert_eq!(
            names(overlay.read_dir("").unwrap()),
            ["data", "empty", "new.txt"]
        );
        assert_eq!(names(overlay.read_dir("data").unwrap()), ["b.bin", "sub"]);
        assert_eq!(code(overlay.read_dir("a.txt")), ERROR_FILE_NOT_FOUND);
        assert_eq!(code(overlay.read_dir("new.txt")), ERROR_DIRECTORY);

        // Host listings merge the same way, `.` and `..` once each.
        let upper = vec![".", "..", "B.BIN", "x"];
        let lower = vec![".", "..", "b.bin", "a.txt", "y"];
        let merged = overlay.merge("", upper, lower, |n| n.to_string());
        assert_eq!(merged, [".", "..", "B.BIN", "x", "y"]);
    }

    #[test]
    fn overlay_only_files_behave_normally() {
        let layers = Layers::new();
        let package = snapshot(&layers.package);
        let overlay = layers.open();
        let prepared = overlay
            .prepare("data\\own.txt", Must::Either, false)
            .unwrap();
        assert!(!prepared.existed && !prepared.present);
        fs::write(&prepared.path, b"own").unwrap();
        overlay.commit("data\\own.txt").unwrap();
        assert_eq!(
            code(overlay.prepare("data\\own.txt", Must::NotExist, false)),
            ERROR_FILE_EXISTS
        );
        overlay.rename("data\\own.txt", "own.txt", false).unwrap();
        assert_eq!(
            fs::read(overlay.read_path("own.txt").unwrap()).unwrap(),
            b"own"
        );
        overlay.delete_file("own.txt").unwrap();
        assert!(!overlay.exists("own.txt"));
        overlay.create_dir("folder").unwrap();
        assert_eq!(code(overlay.create_dir("folder")), ERROR_ALREADY_EXISTS);
        overlay.remove_dir("folder").unwrap();
        assert!(!overlay.exists("folder"));
        assert!(
            overlay.whiteouts().is_empty(),
            "nothing in the package was hidden"
        );
        assert_eq!(snapshot(&layers.package), package);
    }

    #[test]
    fn a_failed_copy_leaves_both_layers_unchanged() {
        use std::os::windows::fs::OpenOptionsExt;
        let layers = Layers::new();
        let package = snapshot(&layers.package);
        let overlay = layers.open();
        overlay.prepare("a.txt", Must::Exist, true).unwrap();
        let upper = snapshot(&layers.root.join("files"));

        // Nobody else may open the file while this handle lives.
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(layers.package.join("data\\sub\\c.txt"))
            .unwrap();
        assert!(
            overlay
                .prepare("data\\sub\\c.txt", Must::Exist, true)
                .is_err()
        );
        assert!(overlay.rename("data\\sub\\c.txt", "c.txt", false).is_err());
        drop(locked);
        assert_eq!(snapshot(&layers.root.join("files")), upper);
        assert!(snapshot(&layers.root.join("staging")).is_empty());
        assert!(overlay.whiteouts().is_empty());
        assert_eq!(
            overlay.read_path("data\\sub\\c.txt").unwrap(),
            layers.package.join("data\\sub\\c.txt")
        );
        assert_eq!(snapshot(&layers.package), package);
    }

    #[test]
    fn package_folders_are_removed_and_made_again_empty() {
        let layers = Layers::new();
        let package = snapshot(&layers.package);
        let overlay = layers.open();
        assert_eq!(code(overlay.remove_dir("data")), ERROR_DIR_NOT_EMPTY);
        assert_eq!(code(overlay.remove_dir("a.txt")), ERROR_DIRECTORY);
        assert_eq!(code(overlay.remove_dir("")), ERROR_ACCESS_DENIED);
        overlay.delete_file("data\\sub\\c.txt").unwrap();
        overlay.remove_dir("data\\sub").unwrap();
        overlay.delete_file("data\\b.bin").unwrap();
        overlay.remove_dir("data").unwrap();
        assert!(!overlay.exists("data") && !overlay.exists("data\\b.bin"));

        // A folder made again shows none of the package's old entries.
        overlay.create_dir("data").unwrap();
        assert!(overlay.read_dir("data").unwrap().is_empty());
        overlay.create_dir("data\\sub").unwrap();
        assert!(overlay.read_dir("data\\sub").unwrap().is_empty());
        assert!(!overlay.exists("data\\sub\\c.txt"));
        drop(overlay);
        let overlay = layers.open();
        assert!(overlay.is_dir("data") && !overlay.exists("data\\b.bin"));
        assert_eq!(snapshot(&layers.package), package);
    }
}
