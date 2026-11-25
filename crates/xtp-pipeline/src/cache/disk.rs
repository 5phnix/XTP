use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

// Versioned magic, little-endian payload length, then BLAKE3 of the payload.
const MAGIC: &[u8; 4] = b"XTP\x01";
const HEADER: usize = 4 + 8 + blake3::OUT_LEN;

fn header(file: &mut File, file_size: u64) -> io::Result<(u64, blake3::Hash)> {
    let mut buf = [0; HEADER];
    file.read_exact(&mut buf)?;
    let size = u64::from_le_bytes(buf[4..12].try_into().unwrap());
    if &buf[..4] != MAGIC || file_size.checked_sub(HEADER as u64) != Some(size) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid cache header or length",
        ));
    }
    Ok((
        size,
        blake3::Hash::from_bytes(buf[12..].try_into().unwrap()),
    ))
}

/// The payload an entry file of `size` bytes holds, by its size alone (no
/// file opened): a malformed file is counted 44 bytes short, which only
/// the byte budget sees.
pub(super) fn payload_size(size: u64) -> u64 {
    size.checked_sub(HEADER as u64).unwrap_or(size)
}

/// Legacy and malformed files occupy their full size until rebuilt or
/// evicted. Opens the file: what opening a cache did for each entry before
/// the index (kept for the probe that measures it).
#[cfg(test)]
pub(super) fn indexed_size(path: &Path, size: u64) -> io::Result<u64> {
    if size < HEADER as u64 {
        return Ok(size);
    }
    let mut file = File::open(path)?;
    match header(&mut file, size) {
        Ok((payload, _)) => Ok(payload),
        Err(e) if matches!(e.kind(), ErrorKind::InvalidData | ErrorKind::UnexpectedEof) => Ok(size),
        Err(e) => Err(e),
    }
}

fn shard(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid cache shard",
        ))
    }
}

pub(super) fn unlink(path: &Path) -> io::Result<()> {
    shard(path.parent().expect("cache file has a shard"))?;
    fs::remove_file(path)
}

pub(super) fn read(path: &Path) -> io::Result<(Vec<u8>, SystemTime)> {
    shard(path.parent().expect("hashed path has a parent"))?;
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(ErrorKind::InvalidData, "invalid cache file"));
    }
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let (size, digest) = header(&mut file, meta.len())?;
    let mut bytes = Vec::new();
    // Bound reads even if another writer changes the file after the length check.
    file.take(size + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != size || blake3::hash(&bytes) != digest {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid cache payload",
        ));
    }
    Ok((bytes, meta.modified()?))
}

pub(super) struct Staged {
    path: PathBuf,
    published: bool,
}

impl Staged {
    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<Self> {
        let dir = path.parent().expect("hashed path has a parent");
        fs::create_dir_all(dir)?;
        shard(dir)?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (staged, mut file) = loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(".tmp-{}-{id}", std::process::id()));
            match File::create_new(&path) {
                Ok(file) => {
                    break (
                        Self {
                            path,
                            published: false,
                        },
                        file,
                    );
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        file.write_all(MAGIC)?;
        file.write_all(&(bytes.len() as u64).to_le_bytes())?;
        file.write_all(blake3::hash(bytes).as_bytes())?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(staged)
    }

    pub(super) fn publish(mut self, path: &Path) -> io::Result<SystemTime> {
        let modified = fs::metadata(&self.path)?.modified()?;
        fs::rename(&self.path, path)?;
        self.published = true;
        Ok(modified)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.published {
            super::remove_file(&self.path);
        }
    }
}

/// The index file at a cache's root: [`INDEX_MAGIC`], then records of
/// [`RECORD`] bytes: a snapshot (its own record, [`Change::Snapshot`], with
/// how many entries follow, then one published record per entry), then the
/// changes appended as they are made (one published, one removed).
///
/// The snapshot is written whole (a file renamed over the index), so its
/// count makes a cut anywhere in it damage, even one between two records:
/// the file then holds fewer entries than its snapshot says, or not even
/// the snapshot's own record. The changes appended after it are written
/// one record at a time, by any process: a cut inside a record is damage,
/// a cut between two of them (the last appends of a process stopped with
/// the machine) leaves a file that reads as sound without them. That is an
/// entry the index misses, or one it still lists, which only the byte
/// budget sees until [`super::Cache::reindex`] lists the folder again.
pub(super) const INDEX_FILE: &str = "index";
/// Version 2: the snapshot's own record and count.
const INDEX_MAGIC: &[u8; 16] = b"XTP cache index\x02";
/// A record: its kind (1 published, 2 removed, 3 a snapshot), three zero
/// bytes, the modification time's nanoseconds (u32), the key (32 bytes),
/// the payload's size (a snapshot's entry count) and the modification
/// time's seconds since 1970 (u64 each), then the first 8 bytes of the
/// BLAKE3 of the 56 before, so that a torn or changed record is damage.
const RECORD: usize = 64;

/// One record of the index file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Change {
    Published {
        key: [u8; 32],
        bytes: u64,
        modified: SystemTime,
    },
    Removed {
        key: [u8; 32],
    },
    /// The file's first record: the snapshot's entry count. Not a change:
    /// [`read_index`] checks it and leaves it out.
    Snapshot {
        entries: u64,
    },
}

impl Change {
    fn encode(&self) -> [u8; RECORD] {
        let mut r = [0u8; RECORD];
        let (kind, key, bytes, modified) = match *self {
            Change::Published {
                key,
                bytes,
                modified,
            } => (1, key, bytes, modified),
            Change::Removed { key } => (2, key, 0, SystemTime::UNIX_EPOCH),
            Change::Snapshot { entries } => (3, [0; 32], entries, SystemTime::UNIX_EPOCH),
        };
        // Before 1970 (never for a file written now): the oldest there is.
        let since = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        r[0] = kind;
        r[4..8].copy_from_slice(&since.subsec_nanos().to_le_bytes());
        r[8..40].copy_from_slice(&key);
        r[40..48].copy_from_slice(&bytes.to_le_bytes());
        r[48..56].copy_from_slice(&since.as_secs().to_le_bytes());
        let check = blake3::hash(&r[..56]);
        r[56..].copy_from_slice(&check.as_bytes()[..8]);
        r
    }

    fn decode(r: &[u8; RECORD]) -> Option<Change> {
        let check = blake3::hash(&r[..56]);
        if r[56..] != check.as_bytes()[..8] || r[1..4] != [0; 3] {
            return None;
        }
        let word = |at: usize| u64::from_le_bytes(r[at..at + 8].try_into().unwrap());
        let nanos = u32::from_le_bytes(r[4..8].try_into().unwrap());
        let key: [u8; 32] = r[8..40].try_into().unwrap();
        match r[0] {
            1 => Some(Change::Published {
                key,
                bytes: word(40),
                modified: SystemTime::UNIX_EPOCH
                    .checked_add(std::time::Duration::new(word(48), nanos))?,
            }),
            2 => Some(Change::Removed { key }),
            3 if key == [0; 32] => Some(Change::Snapshot { entries: word(40) }),
            _ => None,
        }
    }
}

/// The changes the index file at `root` holds, in order (the snapshot's
/// entries first); `None` without one, or for one that is damaged anywhere
/// we can tell ([`INDEX_FILE`]: a torn or changed record, a snapshot cut
/// short, another version): the cache then lists its folder instead. The
/// cache reads it with its mark ([`read_index_marked`]).
#[cfg(test)]
pub(super) fn read_index(root: &Path) -> Option<Vec<Change>> {
    read_index_marked(root).0
}

/// What the index file at a cache's root held when a listing or a read of
/// it started: its size and the BLAKE3 of those bytes. A snapshot written
/// after carries what other processes appended to that file since
/// ([`write_index`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Mark {
    len: usize,
    hash: blake3::Hash,
}

impl Mark {
    fn of(bytes: &[u8]) -> Mark {
        Mark {
            len: bytes.len(),
            hash: blake3::hash(bytes),
        }
    }
}

/// The index file at `root` as it is now ([`Mark`]); `None` without one.
pub(super) fn mark(root: &Path) -> Option<Mark> {
    fs::read(root.join(INDEX_FILE)).ok().map(|b| Mark::of(&b))
}

/// [`read_index`], and the file it read ([`Mark`]: also for a damaged one,
/// whose appends a snapshot carries all the same).
pub(super) fn read_index_marked(root: &Path) -> (Option<Vec<Change>>, Option<Mark>) {
    match fs::read(root.join(INDEX_FILE)) {
        Ok(bytes) => (parse_index(&bytes), Some(Mark::of(&bytes))),
        Err(_) => (None, None),
    }
}

/// The records of an index file of this version or the one before (the
/// same records, without the snapshot's), none checked against the others:
/// for the probe that times reading a real cache's index, which may have
/// been written by a build of either.
#[cfg(test)]
pub(super) fn records_of_any_version(bytes: &[u8]) -> Option<Vec<Change>> {
    let (magic, rest) = bytes.split_at_checked(INDEX_MAGIC.len())?;
    (magic[..15] == INDEX_MAGIC[..15]).then_some(())?;
    let (records, torn) = rest.as_chunks::<RECORD>();
    torn.is_empty().then_some(())?;
    records
        .iter()
        .map(Change::decode)
        .filter(|c| !matches!(c, Some(Change::Snapshot { .. })))
        .collect()
}

/// [`read_index`] of the file's bytes.
pub(super) fn parse_index(bytes: &[u8]) -> Option<Vec<Change>> {
    parse_index_parts(bytes).map(|(_, changes)| changes)
}

/// [`parse_index`], and how many of the changes are the snapshot's.
fn parse_index_parts(bytes: &[u8]) -> Option<(usize, Vec<Change>)> {
    let (records, torn) = bytes
        .strip_prefix(INDEX_MAGIC.as_slice())?
        .as_chunks::<RECORD>();
    if !torn.is_empty() {
        return None;
    }
    let (first, records) = records.split_first()?;
    let Change::Snapshot { entries } = Change::decode(first)? else {
        return None;
    };
    let entries = usize::try_from(entries).ok()?;
    let changes: Vec<Change> = records.iter().map(Change::decode).collect::<Option<_>>()?;
    // The snapshot whole, and nothing but changes after it.
    let (snapshot, appended) = changes.split_at_checked(entries)?;
    let sound = snapshot
        .iter()
        .all(|c| matches!(c, Change::Published { .. }))
        && !appended
            .iter()
            .any(|c| matches!(c, Change::Snapshot { .. }));
    sound.then_some((entries, changes))
}

/// Replaces the index file at `root` with one holding `entries` (published
/// changes) after the snapshot's record, then the changes other processes
/// appended to the file since `since` (the file as the listing or read
/// that gave `entries` found it): written beside it, then renamed over it,
/// with the file held against appends meanwhile ([`hold`]). Those changes
/// are returned, for the caller's index. Before, an entry another process
/// published while the listing ran was appended to the file the snapshot
/// then replaced, and lost (Codex review of d286ffd, point 2).
pub(super) fn write_index(
    root: &Path,
    entries: impl ExactSizeIterator<Item = Change>,
    since: Option<&Mark>,
) -> io::Result<Vec<Change>> {
    let mut bytes = INDEX_MAGIC.to_vec();
    let count = Change::Snapshot {
        entries: entries.len() as u64,
    };
    for change in std::iter::once(count).chain(entries) {
        bytes.extend_from_slice(&change.encode());
    }
    let path = root.join(INDEX_FILE);
    let tmp = root.join(format!("{INDEX_FILE}.{}.tmp", std::process::id()));
    let held = hold(&path)?;
    let appended = held
        .as_ref()
        .map_or_else(Vec::new, |(_, now)| appended_since(now, since));
    for change in &appended {
        bytes.extend_from_slice(&change.encode());
    }
    fs::write(&tmp, &bytes)?;
    let renamed = retried(|| fs::rename(&tmp, &path)).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    });
    drop(held);
    renamed.map(|()| appended)
}

/// The changes appended to the index file `now` since `since`: the records
/// after the bytes `since` marks if the file still begins with them; else
/// (another process's snapshot replaced it, or there was none) those
/// appended after its own snapshot. Up to the first that does not read.
fn appended_since(now: &[u8], since: Option<&Mark>) -> Vec<Change> {
    let after_mark = since.and_then(|m| {
        let prefix = now.get(..m.len)?;
        let aligned = m
            .len
            .checked_sub(INDEX_MAGIC.len())
            .is_some_and(|n| n % RECORD == 0);
        (aligned && blake3::hash(prefix) == m.hash).then(|| &now[m.len..])
    });
    let changes: Vec<Change> = match after_mark {
        Some(tail) => tail
            .as_chunks::<RECORD>()
            .0
            .iter()
            .map_while(Change::decode)
            .collect(),
        None => match parse_index_parts(now) {
            Some((entries, changes)) => changes[entries..].to_vec(),
            None => Vec::new(),
        },
    };
    changes
        .into_iter()
        .filter(|c| !matches!(c, Change::Snapshot { .. }))
        .collect()
}

/// The index file at `path` held against appends while a snapshot replaces
/// it (a handle that shares reading and deleting, not writing: an append
/// opens the file to write, and holds it shared with no deleting, so
/// neither starts while the other is under way), and its bytes; `None`
/// without one. Waits for an append under way.
fn hold(path: &Path) -> io::Result<Option<(File, Vec<u8>)>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    match retried(|| options.open(path)) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Some((file, bytes)))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
const FILE_SHARE_READ: u32 = 1;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 2;
#[cfg(windows)]
const FILE_SHARE_DELETE: u32 = 4;

/// `op`, tried again for up to two seconds while the index file is in
/// another process's hands (a sharing violation, or a rename over a file
/// open without deleting shared).
fn retried<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match op() {
            Err(e)
                if cfg!(windows)
                    && matches!(e.raw_os_error(), Some(5 | 32 | 33))
                    && std::time::Instant::now() < until =>
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            done => return done,
        }
    }
}

/// Appends changes to the index file at a cache's root, one record per
/// write. Another process may append beside it (a record is one write) or
/// replace the file with a snapshot of its own: each append opens the file
/// that is there now, so what this one appends goes to the index the next
/// open reads, not to the file the other replaced (Codex review of
/// 12556d6, point 2: those records were lost, entries the index missed
/// until the next [`super::Cache::reindex`]). One appended while the
/// other's listing ran is carried by its snapshot ([`write_index`]), and a
/// snapshot does not replace the file while a record goes in (each holds
/// it shared without the other's access: [`hold`]). A budget is still each process's
/// own (two may each find room for their entry): an open past the limit
/// lists the folder before it evicts. A failed write leaves a record torn
/// or missing: the next open sees damage, or misses an entry.
pub(super) struct Journal {
    path: PathBuf,
    failed: bool,
}

impl Journal {
    pub(super) fn new(root: &Path) -> Journal {
        Journal {
            path: root.join(INDEX_FILE),
            failed: false,
        }
    }

    pub(super) fn append(&mut self, change: &Change) {
        if self.failed {
            return;
        }
        // Only to the file there is: a file made here would hold these
        // changes without the entries before them, and the next open would
        // trust it. Without one, the next open looks at every entry.
        let mut options = fs::OpenOptions::new();
        options.append(true);
        // Shared with no deleting: a snapshot does not replace the file
        // while this record goes in ([`hold`]).
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
        let written = retried(|| options.open(&self.path))
            .and_then(|mut file| file.write_all(&change.encode()));
        if let Err(e) = written {
            tracing::warn!(path = %self.path.display(), error = %e, "cache index not appended to; the next open may look at every entry");
            self.failed = true;
        }
    }

    /// The file was replaced (a new snapshot) by this process: appends go
    /// to it again after a failure.
    pub(super) fn reopen(&mut self) {
        self.failed = false;
    }
}
