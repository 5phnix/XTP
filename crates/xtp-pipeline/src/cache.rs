use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, ErrorKind};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::Converter;
mod disk;

/// Identifies one conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(blake3::Hash);

impl CacheKey {
    pub fn new(converter: &dyn Converter, input: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(converter.id().as_bytes());
        h.update(&[0]);
        h.update(&converter.version().to_le_bytes());
        h.update(input);
        CacheKey(h.finalize())
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

/// Committed files and payload bytes currently indexed, and successful lookups
/// that reused a disk entry or an in-flight result since this cache was opened.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: u64,
    pub hits: u64,
}

type Bytes = Arc<[u8]>;
type Flight = Mutex<Option<Result<Bytes, String>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
struct Record {
    bytes: u64,
    modified: SystemTime,
}

#[derive(Default)]
struct Index {
    files: HashMap<CacheKey, Record>,
    oldest: BTreeSet<(SystemTime, [u8; 32])>,
    bytes: u64,
    hits: u64,
    active: HashMap<CacheKey, (usize, Arc<Flight>)>,
    /// Where each change to `files` is appended once the cache is open
    /// ([`disk::INDEX_FILE`]).
    journal: Option<disk::Journal>,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("entries", &self.files.len())
            .field("bytes", &self.bytes)
            .field("hits", &self.hits)
            .finish_non_exhaustive()
    }
}

/// How a cache's entries were known when it was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// From its index file.
    Index,
    /// From its folder's entries: no index file, or a damaged one.
    Scan,
    /// Disk storage is off.
    Off,
}

#[derive(Debug)]
struct Store {
    root: PathBuf,
    limit: u64,
    index: Mutex<Index>,
    warned_write_failure: AtomicBool,
    opened: Opened,
}

// Opening the same directory twice in this process must also share its pins.
static STORES: LazyLock<Mutex<HashMap<PathBuf, Weak<Store>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Content-addressed disk store. Clones and opens of one root in this process
/// share their byte budget, statistics and in-flight conversions.
#[derive(Debug, Clone)]
pub struct Cache {
    store: Arc<Store>,
}

impl Cache {
    /// Opens a cache outside the repo with a 1 GiB committed payload limit.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with_limit(root, 1024 * 1024 * 1024)
    }

    /// Indexes existing entries and prunes oldest publications first. Concurrent
    /// opens of the same root must agree on its limit. Zero disables disk storage.
    ///
    /// The entries are read from the index file at the root (`index`):
    /// one file, where looking at each entry took
    /// minutes for a cache of 157,805 (2026-11-25: sessions killed at 300 s
    /// before the title drew). Without one, or with a damaged one (a torn
    /// record, a snapshot cut short: `disk::INDEX_FILE`), the folder's
    /// entries are listed (their sizes and times, no file opened) and the
    /// index written, unless a folder could not be listed. Every
    /// publication and removal is appended to it after. The index only
    /// sizes the cache: a read looks for its entry's file whatever the
    /// index says, and checks it, so an index that misses entries (written
    /// by a build without one, or appends lost with the machine) or lists
    /// gone ones costs disk room or a stale count, never a wrong answer;
    /// [`Cache::reindex`] sets it right (`xtp precook` before each pass).
    pub fn open_with_limit(root: impl Into<PathBuf>, limit: u64) -> Result<Self> {
        let root = root.into();
        xtp_core::ensure_outside_repo(&root)?;
        if limit != 0 {
            fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        }
        let root = match fs::canonicalize(&root) {
            Err(e) if limit == 0 && e.kind() == ErrorKind::NotFound => std::path::absolute(&root)?,
            path => path?,
        };
        xtp_core::ensure_outside_repo(&root)?;
        let mut stores = lock(&STORES);
        if let Some(store) = stores.get(&root).and_then(Weak::upgrade) {
            anyhow::ensure!(
                store.limit == limit,
                "cache already open with a different size limit"
            );
            return Ok(Self { store });
        }
        let mut index = Index::default();
        let mut opened = Opened::Off;
        if limit != 0 {
            let started = std::time::Instant::now();
            let mut trim = true;
            opened = match disk::read_index(&root) {
                Some(changes) => {
                    let records = changes.len();
                    for change in changes {
                        index.apply(change);
                    }
                    if index.bytes > limit {
                        // Past the limit by the index alone, which may list
                        // entries gone since (removed by another process or
                        // by hand, a record of their removal lost): the
                        // folder is listed before anything goes, so that no
                        // live entry is evicted for one that is not there
                        // (Codex review of 12556d6, point 2). A listing that
                        // missed a folder evicts nothing.
                        trim = index.relist(&root) != Relisted::Unlisted;
                    } else if records > 2 * index.files.len() + 4096 {
                        // Mostly changes since its snapshot: a new snapshot.
                        index.snapshot(&root);
                    }
                    Opened::Index
                }
                None => {
                    // A listing that missed a folder it could not read is
                    // not written: the next open lists again, rather than
                    // trust an index without that folder's entries.
                    if index.scan(&root) {
                        index.snapshot(&root);
                    }
                    Opened::Scan
                }
            };
            index.journal = Some(disk::Journal::new(&root));
            if trim {
                index.trim(&root, limit);
            }
            tracing::info!(
                root = %root.display(),
                entries = index.files.len(),
                bytes = index.bytes,
                from = ?opened,
                ms = started.elapsed().as_millis() as u64,
                "cache opened"
            );
        }
        let store = Arc::new(Store {
            root: root.clone(),
            limit,
            index: Mutex::new(index),
            warned_write_failure: AtomicBool::new(false),
            opened,
        });
        stores.retain(|_, store| store.strong_count() > 0);
        stores.insert(root, Arc::downgrade(&store));
        Ok(Self { store })
    }

    /// On-disk location for diagnostics. Use the lookup API for stable reads.
    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        path_for(&self.store.root, key)
    }

    pub fn stats(&self) -> CacheStats {
        let index = lock(&self.store.index);
        CacheStats {
            entries: index.files.len(),
            bytes: index.bytes,
            hits: index.hits,
        }
    }

    /// How the entries were known when this cache was first opened in this
    /// process.
    pub fn opened(&self) -> Opened {
        self.store.opened
    }

    /// Lists the folder's entries again (their sizes and times: no file is
    /// opened) and writes the index anew from them, then prunes to the
    /// limit: for entries the index misses (written by a build that kept
    /// none, appended to an index another process replaced, or appended by
    /// a process stopped with the machine) and gone ones it still lists.
    /// Old staging files of writes that were cut short go too. The cache
    /// waits meanwhile: for a process that can (`xtp precook`), not one a
    /// title waits on.
    ///
    /// False, with the index left as it was, if a folder of the cache could
    /// not be listed: an index without its entries would be trusted after.
    /// False too if the listing could not be written as the index file
    /// (Codex review of 12556d6, point 5): the counts are the listing's, but
    /// the next open reads the file as it was.
    pub fn reindex(&self) -> bool {
        if self.store.limit == 0 {
            return true;
        }
        let mut index = lock(&self.store.index);
        let relisted = index.relist(&self.store.root);
        if relisted != Relisted::Unlisted {
            index.trim(&self.store.root, self.store.limit);
        }
        relisted == Relisted::Written
    }

    /// Returns an owned snapshot which remains readable after eviction.
    pub fn get_or_convert(&self, converter: &dyn Converter, input: &[u8]) -> Result<Arc<[u8]>> {
        self.get_or_convert_with(converter, input, Some)
    }

    /// Reads and decodes under the key's pin. A rejected disk entry is rebuilt
    /// once. Oversized outputs or outputs that cannot fit beside pinned readers
    /// are delivered to all participating callers without being stored on disk.
    pub fn get_or_convert_with<T>(
        &self,
        converter: &dyn Converter,
        input: &[u8],
        decode: impl Fn(Arc<[u8]>) -> Option<T>,
    ) -> Result<T> {
        let key = CacheKey::new(converter, input);
        let pin = Pin::acquire(&self.store, key);
        // Only this key's callers wait during conversion or validation.
        // An unwinding leader poisons just this flight. Its waiters share the
        // failure; the final pin removes it so a later call can start afresh.
        let mut flight = pin
            .flight
            .lock()
            .map_err(|_| anyhow::anyhow!("in-flight conversion panicked"))?;
        if let Some(result) = &*flight {
            let bytes = result.clone().map_err(anyhow::Error::msg)?;
            drop(flight);
            let value = decode(bytes).context("invalid in-flight conversion result")?;
            lock(&self.store.index).hits += 1;
            return Ok(value);
        }
        let result = self.load(key, converter, input, decode);
        *flight = Some(match &result {
            Ok((bytes, _)) => Ok(bytes.clone()),
            Err(error) => Err(format!("{error:#}")),
        });
        result.map(|(_, value)| value)
    }

    fn load<T>(
        &self,
        key: CacheKey,
        converter: &dyn Converter,
        input: &[u8],
        decode: impl Fn(Arc<[u8]>) -> Option<T>,
    ) -> Result<(Bytes, T)> {
        let path = self.path_for(&key);
        if self.store.limit != 0 {
            // Probe even on an index miss: another process may have published
            // this key since we opened the directory.
            match disk::read(&path) {
                Ok((bytes, modified)) => {
                    let bytes = Bytes::from(bytes);
                    lock(&self.store.index).insert(key, bytes.len() as u64, modified);
                    if let Some(value) = decode(bytes.clone()) {
                        lock(&self.store.index).hits += 1;
                        tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
                        return Ok((bytes, value));
                    }
                    tracing::warn!(path = %path.display(), "decoder rejected cache entry");
                }
                Err(e) if e.kind() == ErrorKind::NotFound => lock(&self.store.index).forget(key),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "cache entry could not be read; converting")
                }
            }
        }
        let bytes =
            Bytes::from(converter.convert(input).with_context(|| {
                format!("converter {} v{}", converter.id(), converter.version())
            })?);
        let value = decode(bytes.clone()).context("converter produced an invalid cache entry")?;
        let size = bytes.len() as u64;
        if self.store.limit != 0
            && size <= self.store.limit
            && let Err(e) = self.publish(key, &path, &bytes)
        {
            let already_warned = self
                .store
                .warned_write_failure
                .swap(true, Ordering::Relaxed);
            if already_warned {
                tracing::debug!(path = %path.display(), error = %e, "could not store converted output");
            } else {
                tracing::warn!(
                    path = %path.display(), error = %e,
                    "could not store converted output (further write failures logged at debug level)"
                );
            }
        }
        Ok((bytes, value))
    }

    fn publish(&self, key: CacheKey, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let size = bytes.len() as u64;
        let staged = disk::Staged::write(path, bytes)?;
        let mut index = lock(&self.store.index);
        // A damaged or legacy entry at this key is replaced atomically.
        let replaced = index.files.get(&key).map_or(0, |r| r.bytes);
        if index.trim(
            &self.store.root,
            (self.store.limit - size).saturating_add(replaced),
        ) {
            let modified = staged.publish(path)?;
            index.insert(key, size, modified);
        }
        Ok(())
    }
}

fn path_for(root: &Path, key: &CacheKey) -> PathBuf {
    let hex = key.to_hex();
    root.join(&hex[..2]).join(&hex[2..])
}

/// The index lock serializes acquiring a pin with choosing/removing a victim.
struct Pin<'a> {
    store: &'a Store,
    key: CacheKey,
    flight: Arc<Flight>,
}

impl<'a> Pin<'a> {
    fn acquire(store: &'a Store, key: CacheKey) -> Self {
        let mut index = lock(&store.index);
        let (users, flight) = index.active.entry(key).or_default();
        *users += 1;
        Self {
            store,
            key,
            flight: flight.clone(),
        }
    }
}

impl Drop for Pin<'_> {
    fn drop(&mut self) {
        let mut index = lock(&self.store.index);
        let (users, _) = index.active.get_mut(&self.key).expect("pin is registered");
        *users -= 1;
        if *users == 0 {
            index.active.remove(&self.key);
            // An entry discovered on disk may have exceeded the budget while pinned.
            if index.bytes > self.store.limit {
                index.trim(&self.store.root, self.store.limit);
            }
        }
    }
}

/// What [`Index::relist`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relisted {
    /// The listing is the index, and the index file.
    Written,
    /// The listing is the index; the index file could not be written.
    NotWritten,
    /// A folder could not be listed: the index is as it was.
    Unlisted,
}

impl Index {
    /// Lists the folder at `root` ([`Index::scan`]) and, if the listing is
    /// whole, takes its entries in place of these and writes them as the
    /// index file.
    fn relist(&mut self, root: &Path) -> Relisted {
        let mut listed = Index::default();
        if !listed.scan(root) {
            return Relisted::Unlisted;
        }
        self.files = listed.files;
        self.oldest = listed.oldest;
        self.bytes = listed.bytes;
        if self.snapshot(root) {
            Relisted::Written
        } else {
            Relisted::NotWritten
        }
    }

    /// Adds the entries the folder at `root` holds, by its listing (no
    /// entry opened), and removes staging files older than an hour. False
    /// if part of it could not be listed: a folder, or an entry's size or
    /// time (what was found is added all the same).
    fn scan(&mut self, root: &Path) -> bool {
        self.scan_with(root, fs::DirEntry::file_type)
    }

    /// [`Index::scan`], which reads an entry's type with `file_type` (the
    /// tests' way to make one fail).
    fn scan_with(
        &mut self,
        root: &Path,
        file_type: impl Fn(&fs::DirEntry) -> io::Result<fs::FileType>,
    ) -> bool {
        fn hex(name: &std::ffi::OsStr, len: usize) -> bool {
            let s = name.as_encoded_bytes();
            s.len() == len
                && s.iter()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        }
        let complete = std::cell::Cell::new(true);
        let incomplete = |path: &Path, e: &io::Error| {
            tracing::warn!(path = %path.display(), error = %e, "could not scan cache directory");
            complete.set(false);
        };
        let children = |path: &Path| -> Vec<fs::DirEntry> {
            match fs::read_dir(path) {
                Ok(entries) => entries
                    .filter_map(|e| e.inspect_err(|e| incomplete(path, e)).ok())
                    .collect(),
                Err(e) => {
                    incomplete(path, &e);
                    Vec::new()
                }
            }
        };
        // An entry whose type cannot be read may be a folder of the cache,
        // or an entry of one: the listing is incomplete (Codex review of
        // 12556d6, point 5), not one without it.
        let file_type = |entry: &fs::DirEntry| {
            file_type(entry)
                .inspect_err(|e| incomplete(&entry.path(), e))
                .ok()
        };
        for shard in children(root) {
            if !hex(&shard.file_name(), 2) || !file_type(&shard).is_some_and(|ty| ty.is_dir()) {
                continue;
            }
            for file in children(&shard.path()) {
                if !file_type(&file).is_some_and(|ty| ty.is_file()) {
                    continue;
                }
                let found = file
                    .metadata()
                    .and_then(|meta| Ok((meta.len(), meta.modified()?)));
                let (len, modified) = match found {
                    Ok(found) => found,
                    Err(e) => {
                        incomplete(&file.path(), &e);
                        continue;
                    }
                };
                let name = file.file_name();
                if name.as_encoded_bytes().starts_with(b".tmp")
                    && modified
                        .elapsed()
                        .is_ok_and(|age| age > Duration::from_secs(3600))
                {
                    let _ = remove_file(&file.path());
                }
                if !hex(&name, 62) {
                    continue;
                }
                let hash = format!(
                    "{}{}",
                    shard.file_name().to_string_lossy(),
                    name.to_string_lossy()
                );
                let key =
                    CacheKey(blake3::Hash::from_hex(hash).expect("validated hexadecimal key"));
                // The directory entry's size and time: no file opened (on
                // Windows the listing holds them), where reading each
                // header took minutes cold.
                self.insert(key, disk::payload_size(len), modified);
            }
        }
        complete.get()
    }

    fn insert(&mut self, key: CacheKey, bytes: u64, modified: SystemTime) {
        if self
            .files
            .get(&key)
            .is_some_and(|old| old.bytes == bytes && old.modified == modified)
        {
            return;
        }
        self.forget(key);
        self.oldest.insert((modified, *key.0.as_bytes()));
        self.files.insert(key, Record { bytes, modified });
        self.bytes += bytes;
        if let Some(journal) = &mut self.journal {
            journal.append(&disk::Change::Published {
                key: *key.0.as_bytes(),
                bytes,
                modified,
            });
        }
    }

    fn forget(&mut self, key: CacheKey) {
        if let Some(old) = self.files.remove(&key) {
            self.oldest.remove(&(old.modified, *key.0.as_bytes()));
            self.bytes -= old.bytes;
            if let Some(journal) = &mut self.journal {
                journal.append(&disk::Change::Removed {
                    key: *key.0.as_bytes(),
                });
            }
        }
    }

    /// A change read from the index file (before the journal is open).
    fn apply(&mut self, change: disk::Change) {
        match change {
            disk::Change::Published {
                key,
                bytes,
                modified,
            } => self.insert(CacheKey(key.into()), bytes, modified),
            disk::Change::Removed { key } => self.forget(CacheKey(key.into())),
            // The snapshot's count: checked when the file is read.
            disk::Change::Snapshot { .. } => {}
        }
    }

    /// Replaces the index file with these entries; appends go to the new
    /// one. One that cannot be written leaves the old one, and the cache
    /// works as well: the next open may look at every entry. Whether it was
    /// written.
    fn snapshot(&mut self, root: &Path) -> bool {
        let entries = self.files.iter().map(|(key, r)| disk::Change::Published {
            key: *key.0.as_bytes(),
            bytes: r.bytes,
            modified: r.modified,
        });
        match disk::write_index(root, entries) {
            Ok(()) => {
                if let Some(journal) = &mut self.journal {
                    journal.reopen();
                }
                true
            }
            Err(e) => {
                tracing::warn!(root = %root.display(), error = %e, "cache index not written");
                false
            }
        }
    }

    fn trim(&mut self, root: &Path, available: u64) -> bool {
        if self.bytes <= available {
            return true;
        }
        let pinned: u64 = self
            .active
            .keys()
            .filter_map(|key| self.files.get(key))
            .map(|record| record.bytes)
            .sum();
        if pinned > available {
            return false;
        }
        let mut start = Bound::Unbounded;
        while self.bytes > available {
            let Some(&entry) = self.oldest.range((start, Bound::Unbounded)).next() else {
                return false;
            };
            start = Bound::Excluded(entry);
            let key = CacheKey(entry.1.into());
            if !self.active.contains_key(&key) && remove_file(&path_for(root, &key)) {
                self.forget(key);
            }
        }
        true
    }
}

fn remove_file(path: &Path) -> bool {
    match disk::unlink(path) {
        Ok(()) => true,
        Err(e) if e.kind() == ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not remove cache file");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Upper {
        version: u32,
        runs: AtomicUsize,
    }

    impl Converter for Upper {
        fn id(&self) -> &'static str {
            "test.upper"
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(input.to_ascii_uppercase())
        }
    }

    #[test]
    fn converts_once_then_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let a = cache.get_or_convert(&conv, b"abc").unwrap();
        let b = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(&*a, b"ABC");
        assert_eq!(
            disk::read(&cache.path_for(&CacheKey::new(&conv, b"abc")))
                .unwrap()
                .0,
            b"ABC"
        );
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_publication_removes_the_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        std::fs::create_dir_all(&path).unwrap();

        assert_eq!(&*cache.get_or_convert(&conv, b"abc").unwrap(), b"ABC");
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        assert_eq!(cache.stats(), CacheStats::default());
        fs::remove_dir(&path).unwrap();
        assert_eq!(&*cache.get_or_convert(&conv, b"abc").unwrap(), b"ABC");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn version_bump_invalidates() {
        let v1 = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let v2 = Upper {
            version: 2,
            runs: AtomicUsize::new(0),
        };
        assert_ne!(CacheKey::new(&v1, b"x"), CacheKey::new(&v2, b"x"));
    }

    #[test]
    fn refuses_cache_inside_repo() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(Cache::open(inside).is_err());
    }

    #[test]
    fn raw_lookup_rebuilds_truncated_and_corrupt_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        cache.get_or_convert(&conv, b"raw").unwrap();
        let path = cache.path_for(&CacheKey::new(&conv, b"raw"));
        let good = fs::read(&path).unwrap();
        let mut corrupt = good.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        for damaged in [&good[..good.len() - 1], &corrupt, b"old unframed bytes"] {
            fs::write(&path, damaged).unwrap();
            assert_eq!(&*cache.get_or_convert(&conv, b"raw").unwrap(), b"RAW");
            assert_eq!(fs::read(&path).unwrap(), good);
        }
        assert_eq!(conv.runs.load(Ordering::Relaxed), 4);
        assert_eq!(
            cache.stats(),
            CacheStats {
                entries: 1,
                bytes: 3,
                hits: 0
            }
        );
    }

    #[test]
    fn lookup_adopts_a_later_publication_and_charges_it_once() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open_with_limit(tmp.path(), 3).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        disk::Staged::write(&path, b"ABC")
            .unwrap()
            .publish(&path)
            .unwrap();
        for _ in 0..2 {
            assert_eq!(&*cache.get_or_convert(&conv, b"abc").unwrap(), b"ABC");
        }
        assert_eq!(conv.runs.load(Ordering::Relaxed), 0);
        assert_eq!(
            cache.stats(),
            CacheStats {
                entries: 1,
                bytes: 3,
                hits: 2
            }
        );
        cache.get_or_convert(&conv, b"def").unwrap();
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn an_inaccessible_entry_does_not_disable_the_cache() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        cache.get_or_convert(&conv, b"a").unwrap();
        let path = cache.path_for(&CacheKey::new(&conv, b"a"));
        drop(cache);
        let held = fs::File::options()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        // The index lists it, though it cannot be read now.
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(&*cache.get_or_convert(&conv, b"a").unwrap(), b"A");
        assert_eq!(cache.stats().entries, 1);
        cache.get_or_convert(&conv, b"b").unwrap();
        drop(held);
        assert_eq!(&*cache.get_or_convert(&conv, b"a").unwrap(), b"A");
        assert_eq!(
            cache.stats(),
            CacheStats {
                entries: 2,
                bytes: 2,
                hits: 1
            }
        );
        assert_eq!(conv.runs.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn scan_removes_only_stale_staging_files_and_zero_never_cleans() {
        let tmp = tempfile::tempdir().unwrap();
        let shard = tmp.path().join("01");
        fs::create_dir(&shard).unwrap();
        let stale = shard.join(".tmp-123-0");
        let fresh = shard.join(".tmp-123-1");
        let other = shard.join("other");
        for path in [&stale, &fresh, &other] {
            fs::write(path, b"temp").unwrap();
        }
        fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(7200))
            .unwrap();
        drop(Cache::open_with_limit(tmp.path(), 0).unwrap());
        assert!(stale.exists());
        let cache = Cache::open(tmp.path()).unwrap();
        assert!(!stale.exists());
        assert!(fresh.exists() && other.exists());
        assert_eq!(cache.stats(), CacheStats::default());
        let absent = tmp.path().join("absent");
        drop(Cache::open_with_limit(&absent, 0).unwrap());
        assert!(!absent.exists());
    }

    #[test]
    fn eviction_skips_an_undeletable_victim_and_returns_unstored_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let cache = Cache::open_with_limit(tmp.path(), 2).unwrap();
        cache.get_or_convert(&conv, b"a").unwrap();
        let oldest = cache.path_for(&CacheKey::new(&conv, b"a"));
        fs::File::options()
            .write(true)
            .open(&oldest)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(86400))
            .unwrap();
        cache.get_or_convert(&conv, b"b").unwrap();
        let second = cache.path_for(&CacheKey::new(&conv, b"b"));
        drop(cache);
        let cache = Cache::open_with_limit(tmp.path(), 2).unwrap();
        #[cfg(windows)]
        let held = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::File::options()
                .read(true)
                .share_mode(1)
                .open(&oldest)
                .unwrap()
        };
        #[cfg(not(windows))]
        {
            fs::remove_file(&oldest).unwrap();
            fs::create_dir(&oldest).unwrap();
        }
        assert_eq!(&*cache.get_or_convert(&conv, b"c").unwrap(), b"C");
        assert!(oldest.exists() && !second.exists());
        assert_eq!(cache.stats().bytes, 2);
        let large = cache.path_for(&CacheKey::new(&conv, b"dd"));
        assert_eq!(&*cache.get_or_convert(&conv, b"dd").unwrap(), b"DD");
        assert!(!large.exists());
        assert_eq!(cache.stats().bytes, 1);
        #[cfg(windows)]
        drop(held);
        #[cfg(not(windows))]
        fs::remove_dir(&oldest).unwrap();
        cache.get_or_convert(&conv, b"dd").unwrap();
        assert!(large.is_file());
    }

    #[test]
    fn poisoned_index_still_releases_pins() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let pin = Pin::acquire(&cache.store, CacheKey::new(&conv, b"a"));
        assert!(
            std::panic::catch_unwind(|| {
                let _held = lock(&cache.store.index);
                panic!("test panic holding the index");
            })
            .is_err()
        );
        drop(pin);
        assert!(lock(&cache.store.index).active.is_empty());
        assert_eq!(&*cache.get_or_convert(&conv, b"a").unwrap(), b"A");
    }

    #[test]
    fn failed_and_panicking_flights_release_all_eight_callers_and_allow_retry() {
        use std::sync::Barrier;
        use std::time::Instant;
        struct FailOnce<'a> {
            cache: &'a Cache,
            runs: AtomicUsize,
            panic: bool,
        }
        impl Converter for FailOnce<'_> {
            fn id(&self) -> &'static str {
                "test.fail-once"
            }
            fn version(&self) -> u32 {
                1
            }
            fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
                if self.runs.fetch_add(1, Ordering::Relaxed) == 0 {
                    let key = CacheKey::new(self, input);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while lock(&self.cache.store.index).active[&key].0 < 8 {
                        assert!(Instant::now() < deadline, "callers did not overlap");
                        std::thread::yield_now();
                    }
                    assert!(!self.panic, "test converter panic");
                    anyhow::bail!("test converter failure");
                }
                Ok(input.to_vec())
            }
        }
        for panic in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let cache = Cache::open(tmp.path()).unwrap();
            let conv = FailOnce {
                cache: &cache,
                runs: AtomicUsize::new(0),
                panic,
            };
            let start = Barrier::new(8);
            std::thread::scope(|scope| {
                let threads: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            cache.get_or_convert(&conv, b"x")
                        })
                    })
                    .collect();
                let mut panics = 0;
                for thread in threads {
                    match thread.join() {
                        Ok(result) => {
                            let error = format!("{:#}", result.unwrap_err());
                            let expected = if panic {
                                "in-flight conversion panicked"
                            } else {
                                "test converter failure"
                            };
                            assert!(error.contains(expected), "{error}");
                        }
                        Err(_) => panics += 1,
                    }
                }
                assert_eq!(panics, usize::from(panic));
            });
            assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
            assert!(lock(&cache.store.index).active.is_empty());
            assert_eq!(cache.stats(), CacheStats::default());
            assert_eq!(&*cache.get_or_convert(&conv, b"x").unwrap(), b"x");
            assert_eq!(conv.runs.load(Ordering::Relaxed), 2);
        }
    }

    #[test]
    fn eight_threads_share_one_conversion() {
        for limit in [2, 3] {
            check_concurrent_lookup(limit);
        }
    }

    fn check_concurrent_lookup(limit: u64) {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open_with_limit(tmp.path(), limit).unwrap();
        let other = Cache::open_with_limit(tmp.path().join("."), limit).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let key = CacheKey::new(&conv, b"abc");
        let start = Barrier::new(8);
        let first = AtomicBool::new(true);
        std::thread::scope(|scope| {
            for n in 0..8 {
                let cache = if n % 2 == 0 { &cache } else { &other };
                let (start, conv, first) = (&start, &conv, &first);
                scope.spawn(move || {
                    start.wait();
                    let result = cache
                        .get_or_convert_with(conv, b"abc", |bytes| {
                            if first.swap(false, Ordering::Relaxed) {
                                let deadline = Instant::now() + Duration::from_secs(10);
                                while cache.store.index.lock().unwrap().active[&key].0 < 8 {
                                    assert!(Instant::now() < deadline, "callers did not overlap");
                                    std::thread::yield_now();
                                }
                            }
                            Some(bytes)
                        })
                        .unwrap();
                    assert_eq!(&*result, b"ABC");
                });
            }
        });
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
        assert_eq!(
            cache.stats(),
            CacheStats {
                entries: usize::from(limit >= 3),
                bytes: if limit >= 3 { 3 } else { 0 },
                hits: 7
            }
        );
        assert_eq!(cache.path_for(&key).is_file(), limit >= 3);
        assert!(cache.store.index.lock().unwrap().active.is_empty());
    }

    #[test]
    fn pruning_skips_a_reader_and_reclaims_its_entry_after_release() {
        use std::sync::mpsc;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open_with_limit(tmp.path(), 3).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        cache.get_or_convert(&conv, b"abc").unwrap();
        let old = cache.path_for(&CacheKey::new(&conv, b"abc"));
        let new = cache.path_for(&CacheKey::new(&conv, b"xyz"));
        let (reading, entered) = mpsc::channel();
        let (release, released) = mpsc::channel();
        std::thread::scope(|scope| {
            let (cache, conv) = (&cache, &conv);
            let reader = scope.spawn(move || {
                cache
                    .get_or_convert_with(conv, b"abc", |bytes| {
                        reading.send(()).unwrap();
                        released.recv_timeout(Duration::from_secs(10)).unwrap();
                        Some(bytes)
                    })
                    .unwrap()
            });
            entered.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(&*cache.get_or_convert(conv, b"xyz").unwrap(), b"XYZ");
            assert!(old.is_file());
            assert!(!new.exists());
            assert_eq!(cache.stats().bytes, 3);
            release.send(()).unwrap();
            let snapshot = reader.join().unwrap();
            cache.get_or_convert(conv, b"xyz").unwrap();
            assert!(!old.exists());
            assert!(new.is_file());
            assert_eq!(&*snapshot, b"ABC");
        });
        assert_eq!(
            cache.stats(),
            CacheStats {
                entries: 1,
                bytes: 3,
                hits: 1
            }
        );
    }

    #[test]
    fn reopen_prunes_oldest_legacy_files_and_preserves_unrelated_files() {
        use std::time::{Duration, UNIX_EPOCH};

        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let paths =
            [b"old", b"mid", b"new"].map(|input| cache.path_for(&CacheKey::new(&conv, input)));
        drop(cache);
        for (n, path) in paths.iter().enumerate() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"abc").unwrap();
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(86400 * (n as u64 + 1)))
                .unwrap();
        }
        let foreign = paths[0].with_file_name("user-file");
        fs::write(&foreign, b"keep").unwrap();
        // A folder of entries without an index (written before there was
        // one): listed at open.
        fs::remove_file(tmp.path().join(disk::INDEX_FILE)).unwrap();
        let cache = Cache::open_with_limit(tmp.path(), 6).unwrap();
        assert_eq!(cache.opened(), Opened::Scan);
        assert!(!paths[0].exists());
        assert!(paths[1].is_file() && paths[2].is_file());
        assert_eq!(
            cache.stats(),
            CacheStats {
                entries: 2,
                bytes: 6,
                hits: 0
            }
        );
        assert_eq!(&*cache.get_or_convert(&conv, b"mid").unwrap(), b"MID");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
        drop(cache);
        let disabled = Cache::open_with_limit(tmp.path(), 0).unwrap();
        assert_eq!(disabled.stats(), CacheStats::default());
        assert_eq!(&*disabled.get_or_convert(&conv, b"new").unwrap(), b"NEW");
        assert!(paths[1].is_file());
        assert_eq!(fs::read(&paths[2]).unwrap(), b"abc");
        assert_eq!(fs::read(&foreign).unwrap(), b"keep");
    }

    fn upper(version: u32) -> Upper {
        Upper {
            version,
            runs: AtomicUsize::new(0),
        }
    }

    /// An open reads the index file the first one wrote, with what was
    /// published and pruned since appended: the same entries and bytes a
    /// listing of the folder gives. An entry file written behind its back
    /// (by a build without an index) is not counted, but still read; a
    /// reindex counts it. A damaged index makes the next open list the
    /// folder, and write the index again.
    #[test]
    fn opening_reads_the_index_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let conv = upper(1);
        {
            let cache = Cache::open_with_limit(&root, 9).unwrap();
            assert_eq!(cache.opened(), Opened::Scan, "no index yet");
            for input in [b"one".as_slice(), b"two", b"three"] {
                cache.get_or_convert(&conv, input).unwrap();
            }
            // "one" went for "three": the index says so too.
            assert_eq!(cache.stats().entries, 2);
        }
        let stray = upper(2);
        let stray_path = {
            let cache = Cache::open_with_limit(&root, 9).unwrap();
            assert_eq!(cache.opened(), Opened::Index);
            assert_eq!(
                (cache.stats().entries, cache.stats().bytes),
                (2, 8),
                "two and three"
            );
            cache.path_for(&CacheKey::new(&stray, b"x"))
        };
        disk::Staged::write(&stray_path, b"X")
            .unwrap()
            .publish(&stray_path)
            .unwrap();
        {
            let cache = Cache::open_with_limit(&root, 9).unwrap();
            assert_eq!(cache.opened(), Opened::Index);
            assert_eq!(cache.stats().entries, 2, "the stray file is not listed");
            cache.reindex();
            assert_eq!(cache.stats().entries, 3, "until a reindex");
            assert_eq!(&*cache.get_or_convert(&stray, b"x").unwrap(), b"X");
            assert_eq!(stray.runs.load(Ordering::Relaxed), 0, "read, not made");
        }
        let index = root.join(disk::INDEX_FILE);
        let mut bytes = fs::read(&index).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&index, &bytes).unwrap();
        {
            let cache = Cache::open_with_limit(&root, 9).unwrap();
            assert_eq!(cache.opened(), Opened::Scan, "a damaged index");
            assert_eq!(cache.stats().entries, 3);
        }
        fs::write(&index, &bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(disk::read_index(&root), None, "a torn record");
        let cache = Cache::open_with_limit(&root, 9).unwrap();
        assert_eq!(cache.opened(), Opened::Scan);
        drop(cache);
        let cache = Cache::open_with_limit(&root, 9).unwrap();
        assert_eq!(cache.opened(), Opened::Index, "written again");
        assert_eq!((cache.stats().entries, cache.stats().bytes), (3, 9));
        assert_eq!(conv.runs.load(Ordering::Relaxed), 3);
    }

    /// The index's records round-trip, and a changed byte is damage.
    #[test]
    fn index_records_round_trip_and_reject_damage() {
        let tmp = tempfile::tempdir().unwrap();
        let modified = SystemTime::UNIX_EPOCH + Duration::new(1_795_600_000, 123_456_700);
        let published = |k: u8| disk::Change::Published {
            key: [k; 32],
            bytes: 42 + u64::from(k),
            modified,
        };
        let snapshot = [published(7), published(8)];
        disk::write_index(tmp.path(), snapshot.into_iter()).unwrap();
        assert_eq!(disk::read_index(tmp.path()), Some(snapshot.to_vec()));
        let path = tmp.path().join(disk::INDEX_FILE);
        let good = fs::read(&path).unwrap();
        for at in [0, 16, 20, 60, 80, good.len() - 1] {
            let mut bad = good.clone();
            bad[at] ^= 1;
            fs::write(&path, &bad).unwrap();
            assert_eq!(disk::read_index(tmp.path()), None, "byte {at}");
        }
        fs::remove_file(&path).unwrap();
        assert_eq!(disk::read_index(tmp.path()), None, "none");
    }

    /// A snapshot cut short is damage wherever the cut falls: inside a
    /// record, between two (Codex review of c3da2f9, finding 2), down to
    /// the magic alone. Changes appended after it read back with it; one cut
    /// inside is damage, the last ones cut whole are not seen (only a
    /// listing can tell: [`Cache::reindex`]). An index of the version
    /// before (no snapshot record) is no index.
    #[test]
    fn a_snapshot_cut_short_is_damage() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(disk::INDEX_FILE);
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_600_000);
        let published = |k: u8| disk::Change::Published {
            key: [k; 32],
            bytes: u64::from(k),
            modified,
        };
        let snapshot = [published(1), published(2), published(3)];
        disk::write_index(tmp.path(), snapshot.into_iter()).unwrap();
        let whole = fs::read(&path).unwrap();
        let (magic, record) = (16, 64);
        assert_eq!(whole.len(), magic + 4 * record);
        for cut in [1, record - 1, record, 2 * record, 3 * record, 4 * record] {
            assert_eq!(
                disk::parse_index(&whole[..whole.len() - cut]),
                None,
                "{cut} bytes cut"
            );
        }
        let mut journal = disk::Journal::new(tmp.path());
        journal.append(&published(4));
        journal.append(&disk::Change::Removed { key: [1; 32] });
        let appended = fs::read(&path).unwrap();
        let mut changes = snapshot.to_vec();
        changes.extend([published(4), disk::Change::Removed { key: [1; 32] }]);
        assert_eq!(disk::parse_index(&appended), Some(changes.clone()));
        assert_eq!(disk::parse_index(&appended[..appended.len() - 1]), None);
        assert_eq!(
            disk::parse_index(&appended[..appended.len() - record]),
            Some(changes[..4].to_vec()),
            "an append cut whole is not seen"
        );
        // A second snapshot's record among the changes is damage.
        let mut twice = appended.clone();
        twice.extend_from_slice(&whole[magic..magic + record]);
        assert_eq!(disk::parse_index(&twice), None);
        // The version before: the magic's last byte 1, records alone.
        let mut before = b"XTP cache index\x01".to_vec();
        before.extend_from_slice(&whole[magic + record..]);
        assert_eq!(disk::parse_index(&before), None);
    }

    /// A listing that cannot read one of the cache's folders (here: held
    /// open by another handle that shares nothing) counts what it found but
    /// writes no index, so the next open lists again rather than trust an
    /// index without that folder (Codex review of c3da2f9, finding 3); a
    /// reindex then leaves the index as it was. Once the folder reads, the
    /// listing is written.
    #[cfg(windows)]
    #[test]
    fn a_listing_that_misses_a_folder_is_not_written() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let tmp = tempfile::tempdir().unwrap();
        let conv = upper(1);
        let cache = Cache::open(tmp.path()).unwrap();
        cache.get_or_convert(&conv, b"a").unwrap();
        let shard = |input: &[u8]| {
            cache
                .path_for(&CacheKey::new(&conv, input))
                .parent()
                .unwrap()
                .to_owned()
        };
        // Another entry, in another folder.
        let other = (0u32..)
            .map(|n| format!("other {n}").into_bytes())
            .find(|input| shard(input.as_slice()) != shard(b"a"))
            .unwrap();
        cache.get_or_convert(&conv, &other).unwrap();
        let held_folder = shard(b"a");
        drop(cache);
        let index = tmp.path().join(disk::INDEX_FILE);
        fs::remove_file(&index).unwrap();
        let hold = || {
            fs::File::options()
                .read(true)
                .share_mode(0)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(&held_folder)
                .unwrap()
        };
        let held = hold();
        let cache = Cache::open(tmp.path()).unwrap();
        assert_eq!(cache.opened(), Opened::Scan);
        assert_eq!(cache.stats().entries, 1, "the folder it could read");
        assert!(!index.exists(), "no index written from it");
        assert!(!cache.reindex(), "nor by a reindex");
        assert!(!index.exists());
        drop(cache);
        drop(held);
        let cache = Cache::open(tmp.path()).unwrap();
        assert_eq!(cache.opened(), Opened::Scan, "listed again");
        assert_eq!(cache.stats().entries, 2);
        drop(cache);
        let cache = Cache::open(tmp.path()).unwrap();
        assert_eq!(cache.opened(), Opened::Index, "then written");
        assert_eq!(cache.stats().entries, 2);
        // A reindex with a folder held keeps the index it had.
        let held = hold();
        assert!(!cache.reindex());
        assert_eq!(cache.stats().entries, 2);
        drop(held);
        assert!(cache.reindex());
        assert_eq!(cache.stats().entries, 2);
    }

    /// The staleness the review found (Codex review of c3da2f9, findings 1
    /// to 3) is what a reindex sets right: appends cut whole at the end of
    /// the index (an entry it misses), an entry file removed behind its
    /// back (one it still lists, which would evict a live one to make room
    /// for it), and an old staging file of a write cut short. `xtp precook`
    /// reindexes before every pass.
    #[test]
    fn a_reindex_sets_a_stale_index_right() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let conv = upper(1);
        let cache = Cache::open_with_limit(&root, 100).unwrap();
        for input in [b"one".as_slice(), b"two"] {
            cache.get_or_convert(&conv, input).unwrap();
        }
        let index = root.join(disk::INDEX_FILE);
        let before_three = fs::metadata(&index).unwrap().len();
        cache.get_or_convert(&conv, b"three").unwrap();
        let gone = cache.path_for(&CacheKey::new(&conv, b"one"));
        drop(cache);
        // The machine stopped before "three"'s record reached the disk.
        fs::File::options()
            .write(true)
            .open(&index)
            .unwrap()
            .set_len(before_three)
            .unwrap();
        fs::remove_file(&gone).unwrap();
        let staging = gone.with_file_name(".tmp-1-0");
        fs::write(&staging, b"cut").unwrap();
        fs::File::options()
            .write(true)
            .open(&staging)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(7200))
            .unwrap();
        let cache = Cache::open_with_limit(&root, 100).unwrap();
        assert_eq!(cache.opened(), Opened::Index);
        assert_eq!(
            (cache.stats().entries, cache.stats().bytes),
            (2, 6),
            "one (gone) and two, not three"
        );
        assert!(staging.exists());
        assert!(cache.reindex());
        assert_eq!(
            (cache.stats().entries, cache.stats().bytes),
            (2, 8),
            "two and three"
        );
        assert!(!staging.exists(), "the old staging file went");
        drop(cache);
        let cache = Cache::open_with_limit(&root, 100).unwrap();
        assert_eq!(cache.opened(), Opened::Index);
        assert_eq!((cache.stats().entries, cache.stats().bytes), (2, 8));
        assert_eq!(conv.runs.load(Ordering::Relaxed), 3, "nothing made again");
    }

    /// Codex review of 12556d6, point 2: an index that lists an entry gone
    /// since (removed by another process, the record of its removal lost)
    /// is past the limit by that entry alone. The open evicted the oldest
    /// live entry for it before any listing; it now lists the folder first,
    /// and nothing that fits goes.
    #[test]
    fn an_open_past_the_limit_lists_before_it_evicts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let conv = upper(1);
        let (old, gone) = {
            let cache = Cache::open_with_limit(&root, 10).unwrap();
            cache.get_or_convert(&conv, b"old").unwrap();
            // Later than "old", whatever the clock's grain.
            std::thread::sleep(Duration::from_millis(50));
            cache.get_or_convert(&conv, b"younger").unwrap();
            assert_eq!(cache.stats().bytes, 10);
            (
                cache.path_for(&CacheKey::new(&conv, b"old")),
                cache.path_for(&CacheKey::new(&conv, b"younger")),
            )
        };
        fs::remove_file(&gone).unwrap();
        // 10 bytes by the index, past 9; 3 on disk.
        let cache = Cache::open_with_limit(&root, 9).unwrap();
        assert_eq!(cache.opened(), Opened::Index);
        assert!(old.exists(), "the oldest live entry stays");
        assert_eq!((cache.stats().entries, cache.stats().bytes), (1, 3));
        assert_eq!(&*cache.get_or_convert(&conv, b"old").unwrap(), b"OLD");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 2, "read, not made");
        drop(cache);
        let cache = Cache::open_with_limit(&root, 9).unwrap();
        assert_eq!(
            (cache.stats().entries, cache.stats().bytes),
            (1, 3),
            "the listing was written as the index"
        );
    }

    /// Codex review of 12556d6, point 2: another process's snapshot
    /// replaces the index file; this process's appends after it went to the
    /// file it replaced, and were lost. Each append now opens the file there.
    #[test]
    fn appends_go_to_the_index_another_process_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_600_000);
        let published = |k: u8| disk::Change::Published {
            key: [k; 32],
            bytes: u64::from(k),
            modified,
        };
        disk::write_index(tmp.path(), [published(1)].into_iter()).unwrap();
        let mut journal = disk::Journal::new(tmp.path());
        journal.append(&published(2));
        assert_eq!(
            disk::read_index(tmp.path()),
            Some(vec![published(1), published(2)])
        );
        // Another process's snapshot, then this one's next change.
        disk::write_index(tmp.path(), [published(3)].into_iter()).unwrap();
        journal.append(&published(4));
        assert_eq!(
            disk::read_index(tmp.path()),
            Some(vec![published(3), published(4)]),
            "appended to the index there is"
        );
    }

    /// Codex review of 12556d6, point 5: an entry whose type cannot be read
    /// (a folder of the cache, or an entry of one) makes the listing
    /// incomplete, as a folder that cannot be read does: it skipped the
    /// entry and called the listing whole, which a reindex then wrote.
    #[test]
    fn an_entry_whose_type_cannot_be_read_makes_the_listing_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let conv = upper(1);
        let cache = Cache::open(tmp.path()).unwrap();
        cache.get_or_convert(&conv, b"a").unwrap();
        let entry = cache.path_for(&CacheKey::new(&conv, b"a"));
        let file = entry.file_name().unwrap().to_owned();
        let shard = entry.parent().unwrap().file_name().unwrap().to_owned();
        drop(cache);
        /// The type of each entry, but `name`'s, which cannot be read.
        fn failing(
            name: &std::ffi::OsStr,
        ) -> impl Fn(&fs::DirEntry) -> io::Result<fs::FileType> + '_ {
            move |e| {
                if e.file_name() == name {
                    Err(io::Error::other("type not read"))
                } else {
                    e.file_type()
                }
            }
        }
        let mut listed = Index::default();
        assert!(listed.scan_with(tmp.path(), failing("".as_ref())));
        assert_eq!(listed.files.len(), 1, "a whole listing");
        for name in [&shard, &file] {
            let mut listed = Index::default();
            assert!(
                !listed.scan_with(tmp.path(), failing(name)),
                "{}",
                name.display()
            );
            assert!(listed.files.is_empty());
        }
    }

    /// Codex review of 12556d6, point 5: a reindex whose listing cannot be
    /// written as the index file (here: a folder in its place) said it had
    /// set the index right. It now says it did not; the counts are the
    /// listing's all the same.
    #[test]
    fn a_reindex_that_cannot_write_its_index_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let conv = upper(1);
        let cache = Cache::open(tmp.path()).unwrap();
        cache.get_or_convert(&conv, b"a").unwrap();
        assert!(cache.reindex(), "written");
        let index = tmp.path().join(disk::INDEX_FILE);
        fs::remove_file(&index).unwrap();
        fs::create_dir(&index).unwrap();
        assert!(!cache.reindex(), "not written");
        assert!(index.is_dir());
        assert_eq!((cache.stats().entries, cache.stats().bytes), (1, 1));
        fs::remove_dir(&index).unwrap();
        // The stray temporary file of the failed write is not left behind.
        let strays = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(strays, 0);
    }

    /// Opening a real cache folder (`XTP_CACHE_PROBE`), four ways: each
    /// entry's header read (what an open did before the index), the folder
    /// listed (the open without an index), the index file read (written to
    /// a folder of its own: the probed one is not changed), and, on Windows,
    /// the folder's own index file read cold (`cold`: unbuffered, so the
    /// file is read from the disk whatever the system cache holds; Codex
    /// review of c3da2f9, finding 4). Run with `--ignored --nocapture`
    /// (`XTP_CACHE_PROBE_WAY` picks one: headers, listing, index, cold),
    /// cold for the first two figures that matter.
    #[test]
    #[ignore = "a probe of a real cache folder (XTP_CACHE_PROBE)"]
    fn probe_opening_a_real_cache() {
        let Some(root) = std::env::var_os("XTP_CACHE_PROBE").map(PathBuf::from) else {
            eprintln!("XTP_CACHE_PROBE is not set");
            return;
        };
        let way = std::env::var("XTP_CACHE_PROBE_WAY").unwrap_or_default();
        let timed = |what: &str, f: &dyn Fn() -> (usize, u64)| {
            let started = std::time::Instant::now();
            let (entries, bytes) = f();
            println!(
                "{what}: {entries} entries, {bytes} bytes, {:.3} s",
                started.elapsed().as_secs_f64()
            );
        };
        #[cfg(windows)]
        if way.is_empty() || way == "cold" {
            timed("own index read cold", &|| {
                let started = std::time::Instant::now();
                let bytes = read_unbuffered(&root.join(disk::INDEX_FILE)).unwrap();
                println!(
                    "  its {} bytes from the disk in {:.3} s",
                    bytes.len(),
                    started.elapsed().as_secs_f64()
                );
                let mut read = Index::default();
                for change in disk::records_of_any_version(&bytes).expect("a sound index") {
                    read.apply(change);
                }
                (read.files.len(), read.bytes)
            });
            if way == "cold" {
                return;
            }
        }
        if way.is_empty() || way == "headers" {
            timed("headers read", &|| {
                let mut entries = (0, 0);
                for shard in fs::read_dir(&root).unwrap().flatten() {
                    if !shard.file_type().is_ok_and(|t| t.is_dir()) {
                        continue;
                    }
                    for file in fs::read_dir(shard.path()).unwrap().flatten() {
                        let Ok(meta) = file.metadata() else { continue };
                        if let Ok(size) = disk::indexed_size(&file.path(), meta.len()) {
                            entries.0 += 1;
                            entries.1 += size;
                        }
                    }
                }
                entries
            });
        }
        let mut index = Index::default();
        if way.is_empty() || way == "listing" {
            timed("folder listed", &|| {
                let mut index = Index::default();
                index.scan(&root);
                (index.files.len(), index.bytes)
            });
        }
        index.scan(&root);
        let copy = tempfile::tempdir().unwrap();
        index.snapshot(copy.path());
        timed("index read", &|| {
            let mut read = Index::default();
            for change in disk::read_index(copy.path()).unwrap() {
                read.apply(change);
            }
            (read.files.len(), read.bytes)
        });
    }

    /// The bytes of the file at `path`, read past the system's file cache
    /// (`FILE_FLAG_NO_BUFFERING`: from the disk, in whole sectors, into a
    /// buffer aligned to them).
    #[cfg(windows)]
    fn read_unbuffered(path: &Path) -> io::Result<Vec<u8>> {
        use std::io::Read;
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        #[repr(C, align(4096))]
        #[derive(Clone, Copy)]
        struct Page([u8; 4096]);
        const CHUNK: usize = 256;
        let size = fs::metadata(path)?.len() as usize;
        let mut file = fs::File::options()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(path)?;
        let mut pages = vec![Page([0; 4096]); size.div_ceil(4096).next_multiple_of(CHUNK)];
        let mut read = 0;
        for chunk in pages.chunks_mut(CHUNK) {
            // SAFETY: whole pages, aligned to 4096 bytes (any sector size up
            // to that), seen as bytes.
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(chunk.as_mut_ptr().cast::<u8>(), chunk.len() * 4096)
            };
            let n = file.read(bytes)?;
            read += n;
            if n < bytes.len() {
                break;
            }
        }
        // SAFETY: as above, the pages as bytes, of which `read` were read.
        let all = unsafe { std::slice::from_raw_parts(pages.as_ptr().cast::<u8>(), read) };
        Ok(all.to_vec())
    }
}
