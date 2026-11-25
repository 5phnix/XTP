//! The memory a recording keeps: where title memory is, its snapshot, and
//! the pages that change between captures.
//!
//! Title memory is what the runtime commits for the title below 2^40
//! (`xtp-hle` xmem keeps title memory there, decision 0001; the host's own
//! heaps and mappings lie above): committed, readable, neither code nor a
//! stack. Objects of ours that own memory elsewhere (descriptor heaps,
//! heaps, some committed resources, list uploads) add their ranges.
//!
//! A capture hashes every page (4 KiB) of that memory and keeps the pages
//! whose hash changed since the capture before: the title's CPU writes
//! before a submission, our write-backs of GPU results after one. Pages are
//! read with `ReadProcessMemory`, so memory another thread frees meanwhile
//! reads as missing instead of faulting.

use std::collections::HashMap;
use std::sync::Mutex;

use super::wire::{PAGE, Page};

/// Title memory lies below this address (see the module).
pub(crate) const TITLE_LIMIT: u64 = 1 << 40;

/// Page-aligned `(address, bytes)` ranges, sorted and merged.
pub(crate) fn merge(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    let page = PAGE as u64;
    for r in &mut ranges {
        let start = r.0 / page * page;
        let end = (r.0 + r.1).div_ceil(page) * page;
        *r = (start, end - start);
    }
    ranges.retain(|r| r.1 > 0);
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (at, len) in ranges {
        match out.last_mut() {
            Some(last) if at <= last.0 + last.1 => {
                last.1 = last.1.max(at + len - last.0);
            }
            _ => out.push((at, len)),
        }
    }
    out
}

/// The committed title memory of this process (see the module).
#[cfg(windows)]
pub(crate) fn title_ranges() -> Vec<(u64, u64)> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_IMAGE, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, PAGE_NOCACHE,
        PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOMBINE, PAGE_WRITECOPY, VirtualQuery,
    };
    let mut regions = Vec::new();
    let mut stacks = std::collections::HashSet::new();
    let mut at = 0x1_0000usize;
    while (at as u64) < TITLE_LIMIT {
        // SAFETY: plain data the call fills in.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a query of our own address space.
        let got = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if got == 0 || info.RegionSize == 0 {
            break;
        }
        let base = info.BaseAddress as usize;
        let next = base + info.RegionSize;
        if info.Protect & PAGE_GUARD != 0 {
            stacks.insert(info.AllocationBase as usize);
        }
        let readable = matches!(
            info.Protect & 0xff,
            PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY
        ) && info.Protect
            & (PAGE_GUARD | PAGE_NOCACHE | PAGE_WRITECOMBINE | PAGE_NOACCESS)
            == 0;
        if info.State == MEM_COMMIT && info.Type != MEM_IMAGE && readable {
            let end = next.min(TITLE_LIMIT as usize);
            regions.push((
                info.AllocationBase as usize,
                base as u64,
                (end - base) as u64,
            ));
        }
        at = next;
    }
    merge(
        regions
            .into_iter()
            .filter(|(allocation, _, _)| !stacks.contains(allocation))
            .map(|(_, at, len)| (at, len))
            .collect(),
    )
}

#[cfg(not(windows))]
pub(crate) fn title_ranges() -> Vec<(u64, u64)> {
    Vec::new()
}

/// Reads one page of this process's memory; false if it cannot be read.
pub(crate) fn read_page(at: u64, out: &mut [u8]) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut read = 0;
        // SAFETY: reads into our buffer; the system checks the source.
        let ok = unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                at as *const _,
                out.as_mut_ptr().cast(),
                out.len(),
                &mut read,
            )
        };
        ok != 0 && read == out.len()
    }
    #[cfg(not(windows))]
    {
        let _ = (at, out);
        false
    }
}

/// A page's hash (its BLAKE3's first 8 bytes); 0 is kept for "not read".
pub(crate) fn hash(page: &[u8]) -> u64 {
    let h = u64::from_le_bytes(
        blake3::hash(page).as_bytes()[..8]
            .try_into()
            .expect("8 bytes"),
    );
    h.max(1)
}

/// Threads for the memory passes: the machine's, at most 16.
pub(crate) fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16)
}

/// Each page's hash at the last capture, sharded for the passes' threads.
pub(crate) struct Hashes {
    shards: Vec<Mutex<HashMap<u64, u64>>>,
}

const SHARDS: usize = 64;

fn shard(at: u64) -> usize {
    ((at / PAGE as u64) as usize) % SHARDS
}

impl Default for Hashes {
    fn default() -> Hashes {
        Hashes {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
        }
    }
}

impl Hashes {
    /// Notes what a page held (0: unreadable).
    pub(crate) fn set(&self, at: u64, h: u64) {
        let mut s = self.shards[shard(at)]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        s.insert(at, h);
    }

    fn get(&self, at: u64) -> Option<u64> {
        let s = self.shards[shard(at)]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        s.get(&at).copied()
    }

    /// Pages of `ranges` whose content changed since they were last noted
    /// (pages not noted before count as zero before), noted anew; read with
    /// `read` on `threads` threads, in address order. Pages run from each
    /// range's start; a range's last page ends with it.
    pub(crate) fn changes(
        &self,
        ranges: &[(u64, u64)],
        threads: usize,
        read: &(dyn Fn(u64, &mut [u8]) -> bool + Sync),
    ) -> Vec<Page> {
        let zeros = [0u8; PAGE];
        let zero = hash(&zeros);
        // Work in 1 MiB pieces.
        let mut pieces = Vec::new();
        for &(at, len) in ranges {
            let mut o = 0;
            while o < len {
                let n = (len - o).min(1 << 20);
                pieces.push((at + o, n));
                o += n;
            }
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let found: Mutex<Vec<Page>> = Mutex::default();
        std::thread::scope(|s| {
            for _ in 0..threads.max(1).min(pieces.len().max(1)) {
                s.spawn(|| {
                    let mut buffer = vec![0u8; PAGE];
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(at, len)) = pieces.get(i) else {
                            break;
                        };
                        for p in (at..at + len).step_by(PAGE) {
                            // The last page of a range that ends inside one.
                            let n = (at + len - p).min(PAGE as u64) as usize;
                            let bytes = &mut buffer[..n];
                            let h = if read(p, bytes) { hash(bytes) } else { 0 };
                            let before = self.get(p).unwrap_or_else(|| {
                                if n == PAGE { zero } else { hash(&zeros[..n]) }
                            });
                            if h != before {
                                self.set(p, h);
                                if h != 0 {
                                    mine.push(Page {
                                        at: p,
                                        before,
                                        bytes: bytes.to_vec(),
                                    });
                                }
                            }
                        }
                    }
                    found
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .append(&mut mine);
                });
            }
        });
        let mut pages = found.into_inner().unwrap_or_else(|e| e.into_inner());
        pages.sort_unstable_by_key(|p| p.at);
        pages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_ranges_are_page_aligned_and_merged() {
        let p = PAGE as u64;
        assert_eq!(
            merge(vec![(3 * p + 5, 10), (p, p), (2 * p, 1), (9 * p, 0)]),
            vec![(p, 3 * p)]
        );
        assert_eq!(merge(vec![(5 * p, p), (p, p)]), vec![(p, p), (5 * p, p)]);
    }

    #[test]
    fn record_captures_find_the_pages_that_changed() {
        let memory = Mutex::new(vec![0u8; 4 * PAGE]);
        let base = 0x4000_0000u64;
        let read = |at: u64, out: &mut [u8]| {
            let o = (at - base) as usize;
            out.copy_from_slice(&memory.lock().unwrap()[o..o + PAGE]);
            true
        };
        let ranges = [(base, 4 * PAGE as u64)];
        let hashes = Hashes::default();
        // Zero pages never seen count as zero: nothing changed.
        assert!(hashes.changes(&ranges, 2, &read).is_empty());
        memory.lock().unwrap()[PAGE + 3] = 7;
        memory.lock().unwrap()[3 * PAGE] = 9;
        let pages = hashes.changes(&ranges, 3, &read);
        let at: Vec<u64> = pages.iter().map(|p| p.at).collect();
        assert_eq!(at, vec![base + PAGE as u64, base + 3 * PAGE as u64]);
        assert_eq!(pages[0].bytes[3], 7);
        assert_eq!(pages[0].before, hash(&[0u8; PAGE]));
        // Noted: the same content is no change.
        assert!(hashes.changes(&ranges, 1, &read).is_empty());
        // Written back to zero: a change.
        memory.lock().unwrap()[PAGE + 3] = 0;
        let pages = hashes.changes(&ranges, 1, &read);
        assert_eq!(pages.len(), 1);
        assert!(pages[0].bytes.iter().all(|&b| b == 0));
    }

    #[cfg(windows)]
    #[test]
    fn record_title_memory_holds_what_we_commit_below_its_limit() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        // Somewhere free below the limit.
        let mut at = 0x30_0000_0000usize;
        let block = loop {
            // SAFETY: a fresh reservation of ours.
            let p = unsafe {
                VirtualAlloc(
                    at as *const _,
                    1 << 20,
                    MEM_RESERVE | MEM_COMMIT,
                    PAGE_READWRITE,
                )
            };
            if !p.is_null() {
                break p;
            }
            at += 1 << 30;
            assert!(
                (at as u64) < TITLE_LIMIT,
                "no free address space below the limit"
            );
        };
        let ranges = title_ranges();
        let inside = ranges
            .iter()
            .any(|&(a, l)| a <= block as u64 && block as u64 + (1 << 20) <= a + l);
        // SAFETY: our own reservation.
        unsafe { VirtualFree(block, 0, MEM_RELEASE) };
        assert!(inside, "{block:p} not in {ranges:x?}");
        let mut page = vec![0u8; PAGE];
        assert!(!read_page(0, &mut page), "address 0 reads");
    }
}
