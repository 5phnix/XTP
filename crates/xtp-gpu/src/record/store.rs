//! A recording's files: event streams cut into compressed blocks, each
//! holding whole events, and the memory snapshot in compressed units of up
//! to a megabyte of one region, with its zero pages left out.
//!
//! shortcut: deflate (`miniz_oxide`, already a dependency) at its fastest
//! level, not zstd: the workspace has no zstd yet (asked on the task).

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use super::wire::PAGE;

const STREAM_MAGIC: &[u8; 8] = b"XTPREC01";
const MEMORY_MAGIC: &[u8; 8] = b"XTPMEM01";

/// Raw bytes a stream block holds before it is compressed.
pub(crate) const BLOCK: usize = 4 << 20;

/// Pages a memory unit holds at most (1 MiB).
const UNIT_PAGES: usize = 256;

fn compress(raw: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(raw, 1)
}

fn decompress(packed: &[u8], len: usize) -> io::Result<Vec<u8>> {
    let raw = miniz_oxide::inflate::decompress_to_vec_with_limit(packed, len)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
    if raw.len() != len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a block's size does not match its header",
        ));
    }
    Ok(raw)
}

fn header(w: &mut impl Write, raw: usize, packed: usize) -> io::Result<()> {
    w.write_all(&(raw as u32).to_le_bytes())?;
    w.write_all(&(packed as u32).to_le_bytes())
}

/// Writes a stream's blocks, compressed by a thread of its own so that the
/// title's threads only encode.
pub(crate) struct StreamWriter {
    send: Option<mpsc::SyncSender<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<io::Result<u64>>>,
}

impl StreamWriter {
    pub(crate) fn create(path: &Path) -> io::Result<StreamWriter> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(STREAM_MAGIC)?;
        let (send, receive) = mpsc::sync_channel::<Vec<u8>>(8);
        let thread = std::thread::Builder::new()
            .name("xtp-record-writer".into())
            .spawn(move || {
                let mut written = 8u64;
                for raw in receive {
                    let packed = compress(&raw);
                    header(&mut file, raw.len(), packed.len())?;
                    file.write_all(&packed)?;
                    written += 8 + packed.len() as u64;
                }
                file.flush()?;
                Ok(written)
            })?;
        Ok(StreamWriter {
            send: Some(send),
            thread: Some(thread),
        })
    }

    /// Queues a block of whole events.
    pub(crate) fn push(&self, raw: Vec<u8>) {
        if raw.is_empty() {
            return;
        }
        if let Some(send) = &self.send {
            // The writer only stops on a write error, which `finish` reports.
            let _ = send.send(raw);
        }
    }

    /// Writes what is queued; the bytes the file holds.
    pub(crate) fn finish(mut self) -> io::Result<u64> {
        drop(self.send.take());
        match self.thread.take().map(|t| t.join()) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(io::Error::other("the recording writer panicked")),
            None => Ok(0),
        }
    }
}

impl Drop for StreamWriter {
    fn drop(&mut self) {
        drop(self.send.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The blocks of a stream, decompressed one at a time.
pub(crate) struct StreamReader {
    file: BufReader<File>,
    path: PathBuf,
}

impl StreamReader {
    pub(crate) fn open(path: &Path) -> io::Result<StreamReader> {
        let mut file = BufReader::new(File::open(path)?);
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != STREAM_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a recording's event stream", path.display()),
            ));
        }
        Ok(StreamReader {
            file,
            path: path.to_owned(),
        })
    }

    /// The next block, or `None` at the end.
    pub(crate) fn block(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut head = [0u8; 8];
        match self.file.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let raw = u32::from_le_bytes(head[..4].try_into().expect("4 bytes")) as usize;
        let packed = u32::from_le_bytes(head[4..].try_into().expect("4 bytes")) as usize;
        let mut data = vec![0; packed];
        self.file.read_exact(&mut data).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{}: a block is cut short", self.path.display()),
            )
        })?;
        decompress(&data, raw).map(Some)
    }
}

/// A unit of the memory snapshot: up to [`UNIT_PAGES`] pages of one
/// region from `at`, those not all zero in `pages`.
pub(crate) struct Unit {
    pub(crate) at: u64,
    pub(crate) count: usize,
    /// Bit `i` set: page `i` is in `data` (in order); clear: all zero.
    pub(crate) present: Vec<u8>,
    pub(crate) data: Vec<u8>,
}

impl Unit {
    /// The unit's pages as `(address, bytes)`, zero pages included.
    pub(crate) fn pages(&self) -> impl Iterator<Item = (u64, Option<&[u8]>)> + '_ {
        let mut next = 0;
        (0..self.count).map(move |i| {
            let at = self.at + (i * PAGE) as u64;
            if self.present[i / 8] & (1 << (i % 8)) != 0 {
                let page = &self.data[next * PAGE..(next + 1) * PAGE];
                next += 1;
                (at, Some(page))
            } else {
                (at, None)
            }
        })
    }
}

/// A packed unit: which pages are present, and their compressed bytes.
type Packed = (Vec<u8>, Vec<u8>);

/// Packs `count` pages read at `at` into a unit (`page` reads one page
/// into the buffer, false when it cannot be read: kept as zero).
fn pack(at: u64, count: usize, mut page: impl FnMut(u64, &mut [u8]) -> bool) -> Packed {
    let mut present = vec![0u8; count.div_ceil(8)];
    let mut data = Vec::new();
    let mut buffer = vec![0u8; PAGE];
    for i in 0..count {
        if page(at + (i * PAGE) as u64, &mut buffer) && buffer.iter().any(|&b| b != 0) {
            present[i / 8] |= 1 << (i % 8);
            data.extend_from_slice(&buffer);
        }
    }
    (present, compress(&data))
}

/// Writes the snapshot of `ranges` (page-aligned `(address, bytes)`),
/// reading pages with `page` on `threads` threads; returns the pages
/// written that were not zero, and the bytes of the file.
pub(crate) fn write_memory(
    path: &Path,
    ranges: &[(u64, u64)],
    threads: usize,
    page: &(dyn Fn(u64, &mut [u8]) -> bool + Sync),
) -> io::Result<(u64, u64)> {
    let mut units = Vec::new();
    for &(at, len) in ranges {
        let pages = (len as usize).div_ceil(PAGE);
        let mut i = 0;
        while i < pages {
            let count = UNIT_PAGES.min(pages - i);
            units.push((at + (i * PAGE) as u64, count));
            i += count;
        }
    }
    let mut file = BufWriter::new(File::create(path)?);
    file.write_all(MEMORY_MAGIC)?;
    let (mut kept, mut bytes) = (0u64, 8u64);
    // A batch at a time, so the packed units wait in memory a batch long.
    for batch in units.chunks(threads.max(1) * 16) {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let slots: Vec<std::sync::Mutex<Option<Packed>>> = (0..batch.len())
            .map(|_| std::sync::Mutex::new(None))
            .collect();
        std::thread::scope(|s| {
            for _ in 0..threads.max(1).min(batch.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(at, count)) = batch.get(i) else {
                            return;
                        };
                        let unit = pack(at, count, page);
                        *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(unit);
                    }
                });
            }
        });
        for (&(at, count), slot) in batch.iter().zip(slots) {
            let unit = slot.into_inner().unwrap_or_else(|e| e.into_inner());
            let (present, data) = unit.expect("every unit packed");
            let pages: u32 = present.iter().map(|b| b.count_ones()).sum();
            kept += u64::from(pages);
            file.write_all(&at.to_le_bytes())?;
            file.write_all(&(count as u32).to_le_bytes())?;
            file.write_all(&present)?;
            header(&mut file, pages as usize * PAGE, data.len())?;
            file.write_all(&data)?;
            bytes += 8 + 4 + present.len() as u64 + 8 + data.len() as u64;
        }
    }
    file.flush()?;
    Ok((kept, bytes))
}

/// Reads a memory snapshot's units, decompressed on `threads` threads, and
/// hands each to `unit` in file order.
pub(crate) fn read_memory(
    path: &Path,
    threads: usize,
    mut unit: impl FnMut(Unit),
) -> io::Result<()> {
    let mut file = BufReader::new(File::open(path)?);
    let mut magic = [0; 8];
    file.read_exact(&mut magic)?;
    if &magic != MEMORY_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a recording's memory snapshot", path.display()),
        ));
    }
    let mut done = false;
    while !done {
        // A batch of packed units, then decompressed together.
        let mut batch = Vec::new();
        while batch.len() < threads.max(1) * 16 {
            let mut head = [0u8; 12];
            match file.read_exact(&mut head) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    done = true;
                    break;
                }
                Err(e) => return Err(e),
            }
            let at = u64::from_le_bytes(head[..8].try_into().expect("8 bytes"));
            let count = u32::from_le_bytes(head[8..].try_into().expect("4 bytes")) as usize;
            if count == 0 || count > UNIT_PAGES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a memory unit is corrupt",
                ));
            }
            let mut present = vec![0u8; count.div_ceil(8)];
            file.read_exact(&mut present)?;
            let mut sizes = [0u8; 8];
            file.read_exact(&mut sizes)?;
            let raw = u32::from_le_bytes(sizes[..4].try_into().expect("4 bytes")) as usize;
            let packed = u32::from_le_bytes(sizes[4..].try_into().expect("4 bytes")) as usize;
            let mut data = vec![0; packed];
            file.read_exact(&mut data)?;
            batch.push((at, count, present, raw, data));
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let slots: Vec<std::sync::Mutex<Option<io::Result<Vec<u8>>>>> = (0..batch.len())
            .map(|_| std::sync::Mutex::new(None))
            .collect();
        std::thread::scope(|s| {
            for _ in 0..threads.max(1).min(batch.len().max(1)) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((_, _, _, raw, data)) = batch.get(i) else {
                            return;
                        };
                        let out = decompress(data, *raw);
                        *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
                    }
                });
            }
        });
        for ((at, count, present, _, _), slot) in batch.into_iter().zip(slots) {
            let data = slot
                .into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every unit read")?;
            unit(Unit {
                at,
                count,
                present,
                data,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_stream_blocks_come_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.bin");
        let w = StreamWriter::create(&path).unwrap();
        w.push(vec![1, 2, 3]);
        w.push(vec![]);
        w.push((0..100_000).map(|i| (i % 7) as u8).collect());
        assert!(w.finish().unwrap() > 8);
        let mut r = StreamReader::open(&path).unwrap();
        assert_eq!(r.block().unwrap().unwrap(), vec![1, 2, 3]);
        assert_eq!(r.block().unwrap().unwrap().len(), 100_000);
        assert!(r.block().unwrap().is_none());
    }

    #[test]
    fn record_memory_keeps_pages_that_are_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.bin");
        // 300 pages from 0x10000 (two units), 2 more at 0x900000; pages
        // whose number is a multiple of 3 hold their number, the others
        // are zero, and page 7 cannot be read.
        let ranges = [(0x10000u64, 300 * PAGE as u64), (0x900000, 2 * PAGE as u64)];
        let page = |at: u64, out: &mut [u8]| {
            let n = at / PAGE as u64;
            if n == 0x10 + 7 {
                return false;
            }
            out.fill(0);
            if n.is_multiple_of(3) {
                out[..8].copy_from_slice(&n.to_le_bytes());
            }
            true
        };
        let (kept, _) = write_memory(&path, &ranges, 3, &page).unwrap();
        let mut seen = Vec::new();
        read_memory(&path, 2, |u| {
            for (at, bytes) in u.pages() {
                if let Some(b) = bytes {
                    seen.push((at, u64::from_le_bytes(b[..8].try_into().unwrap())));
                }
            }
        })
        .unwrap();
        assert_eq!(seen.len() as u64, kept);
        assert!(
            seen.iter()
                .all(|&(at, n)| at / PAGE as u64 == n && n % 3 == 0)
        );
        assert_eq!(seen.first().map(|s| s.0), Some(0x12000));
        assert_eq!(seen.last().map(|s| s.0), Some(0x900000));
    }
}
