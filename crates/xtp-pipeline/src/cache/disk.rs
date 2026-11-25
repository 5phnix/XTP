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

/// Legacy and malformed files occupy their full size until rebuilt or evicted.
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
