use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, ErrorKind};
use std::ops::Bound;
use std::path::{Path, PathBuf};
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

#[derive(Debug, Default)]
struct Index {
    files: HashMap<CacheKey, Record>,
    oldest: BTreeSet<(SystemTime, [u8; 32])>,
    bytes: u64,
    hits: u64,
    active: HashMap<CacheKey, (usize, Arc<Flight>)>,
}

#[derive(Debug)]
struct Store {
    root: PathBuf,
    limit: u64,
    index: Mutex<Index>,
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
        if limit != 0 {
            index.scan(&root);
            index.trim(&root, limit);
        }
        let store = Arc::new(Store {
            root: root.clone(),
            limit,
            index: Mutex::new(index),
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
            tracing::warn!(path = %path.display(), error = %e, "could not store converted output");
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

impl Index {
    fn scan(&mut self, root: &Path) {
        fn hex(name: &std::ffi::OsStr, len: usize) -> bool {
            let s = name.as_encoded_bytes();
            s.len() == len
                && s.iter()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        }
        fn children(path: &Path) -> impl Iterator<Item = fs::DirEntry> {
            fs::read_dir(path).inspect_err(|e| tracing::warn!(path = %path.display(), error = %e, "could not scan cache directory"))
                .into_iter().flatten().filter_map(Result::ok)
        }
        for shard in children(root) {
            if !hex(&shard.file_name(), 2) || !shard.file_type().is_ok_and(|ty| ty.is_dir()) {
                continue;
            }
            for file in children(&shard.path()) {
                if !file.file_type().is_ok_and(|ty| ty.is_file()) {
                    continue;
                }
                let Ok(meta) = file.metadata() else { continue };
                let Ok(modified) = meta.modified() else {
                    continue;
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
                if let Ok(size) = disk::indexed_size(&file.path(), meta.len()) {
                    self.insert(key, size, modified);
                }
            }
        }
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
    }

    fn forget(&mut self, key: CacheKey) {
        if let Some(old) = self.files.remove(&key) {
            self.oldest.remove(&(old.modified, *key.0.as_bytes()));
            self.bytes -= old.bytes;
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
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(&*cache.get_or_convert(&conv, b"a").unwrap(), b"A");
        assert_eq!(cache.stats().entries, 0);
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
        let cache = Cache::open_with_limit(tmp.path(), 6).unwrap();
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
}
