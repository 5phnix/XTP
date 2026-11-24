use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::Converter;

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
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        let root = fs::canonicalize(root)?;
        let mut stores = STORES.lock().unwrap();
        if let Some(store) = stores.get(&root).and_then(Weak::upgrade) {
            anyhow::ensure!(
                store.limit == limit,
                "cache already open with a different size limit"
            );
            return Ok(Self { store });
        }
        let mut index = Index::scan(&root)?;
        index.make_room(&root, limit, 0)?;
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
        let index = self.store.index.lock().unwrap();
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
        let mut flight = pin.flight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = &*flight {
            let bytes = result.clone().map_err(anyhow::Error::msg)?;
            drop(flight);
            let value = decode(bytes).context("invalid in-flight conversion result")?;
            self.store.index.lock().unwrap().hits += 1;
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
        let indexed = self.store.index.lock().unwrap().files.contains_key(&key);
        if indexed {
            if let Ok(bytes) = fs::read(&path) {
                let bytes = Bytes::from(bytes);
                if let Some(value) = decode(bytes.clone()) {
                    self.store.index.lock().unwrap().hits += 1;
                    tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
                    return Ok((bytes, value));
                }
            }
            self.store
                .index
                .lock()
                .unwrap()
                .remove(&self.store.root, key)?;
        }
        let bytes =
            Bytes::from(converter.convert(input).with_context(|| {
                format!("converter {} v{}", converter.id(), converter.version())
            })?);
        let value = decode(bytes.clone()).context("converter produced an invalid cache entry")?;
        let size = bytes.len() as u64;
        if self.store.limit == 0 || size > self.store.limit {
            return Ok((bytes, value));
        }
        let dir = path.parent().expect("hashed path has a parent");
        fs::create_dir_all(dir)?;
        anyhow::ensure!(fs::symlink_metadata(dir)?.is_dir(), "invalid cache shard");
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&bytes)?;
        let modified = tmp.as_file().metadata()?.modified()?;
        let mut index = self.store.index.lock().unwrap();
        if index.make_room(&self.store.root, self.store.limit, size)? {
            tmp.persist(&path)?;
            index.insert(key, size, modified);
        }
        Ok((bytes, value))
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
        let mut index = store.index.lock().unwrap();
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
        let mut index = self.store.index.lock().unwrap();
        let (users, _) = index.active.get_mut(&self.key).expect("pin is registered");
        *users -= 1;
        if *users == 0 {
            index.active.remove(&self.key);
        }
    }
}

impl Index {
    fn scan(root: &Path) -> Result<Self> {
        fn hex(name: &std::ffi::OsStr, len: usize) -> bool {
            let s = name.as_encoded_bytes();
            s.len() == len
                && s.iter()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        }
        let mut index = Self::default();
        for shard in fs::read_dir(root)? {
            let shard = shard?;
            if !hex(&shard.file_name(), 2) || !shard.file_type()?.is_dir() {
                continue;
            }
            for file in fs::read_dir(shard.path())? {
                let file = file?;
                if !hex(&file.file_name(), 62) || !file.file_type()?.is_file() {
                    continue;
                }
                let name = format!(
                    "{}{}",
                    shard.file_name().to_string_lossy(),
                    file.file_name().to_string_lossy()
                );
                let key = CacheKey(blake3::Hash::from_hex(name)?);
                let meta = file.metadata()?;
                index.insert(key, meta.len(), meta.modified()?);
            }
        }
        Ok(index)
    }

    fn insert(&mut self, key: CacheKey, bytes: u64, modified: SystemTime) {
        self.oldest.insert((modified, *key.0.as_bytes()));
        self.files.insert(key, Record { bytes, modified });
        self.bytes += bytes;
    }

    fn remove(&mut self, root: &Path, key: CacheKey) -> Result<()> {
        match fs::remove_file(path_for(root, &key)) {
            Err(err) if err.kind() != ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        if let Some(old) = self.files.remove(&key) {
            self.oldest.remove(&(old.modified, *key.0.as_bytes()));
            self.bytes -= old.bytes;
        }
        Ok(())
    }

    fn make_room(&mut self, root: &Path, limit: u64, incoming: u64) -> Result<bool> {
        let available = limit - incoming;
        if self.bytes <= available && (limit != 0 || self.files.is_empty()) {
            return Ok(true);
        }
        let pinned: u64 = self
            .active
            .keys()
            .filter_map(|key| self.files.get(key))
            .map(|record| record.bytes)
            .sum();
        if pinned > available {
            return Ok(false);
        }
        while self.bytes > available || (limit == 0 && !self.files.is_empty()) {
            let key = self
                .oldest
                .iter()
                .map(|(_, hash)| CacheKey((*hash).into()))
                .find(|key| !self.active.contains_key(key))
                .context("no evictable entry")?;
            self.remove(root, key)?;
        }
        Ok(true)
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
            fs::read(cache.path_for(&CacheKey::new(&conv, b"abc"))).unwrap(),
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

        assert!(cache.get_or_convert(&conv, b"abc").is_err());
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
        assert_eq!(&*cache.get_or_convert(&conv, b"mid").unwrap(), b"abc");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 0);
        drop(cache);
        let disabled = Cache::open_with_limit(tmp.path(), 0).unwrap();
        assert_eq!(disabled.stats(), CacheStats::default());
        assert_eq!(fs::read(&foreign).unwrap(), b"keep");
    }
}
