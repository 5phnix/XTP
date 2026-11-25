//! First-touch copy-back for xmem's section views. A completed result owns its
//! bytes until every CPU view can see them. No fence callback or shadow lock is
//! needed in the exception handler.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::watch::PAGE;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
    pub size: u64,
    pub alignment: u32,
    pub writer: u64,
    pub offset: u64,
}

impl Key {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split_whitespace();
        let key = Self {
            size: fields.next()?.parse().ok()?,
            alignment: fields.next()?.parse().ok()?,
            writer: u64::from_str_radix(fields.next()?, 16).ok()?,
            offset: fields.next()?.parse().ok()?,
        };
        (fields.next().is_none() && key.offset < key.size && key.alignment <= 16).then_some(key)
    }
}

struct Map {
    path: Option<PathBuf>,
    seen: BTreeSet<Key>,
    pending: Vec<Key>,
}

static MAP: OnceLock<Mutex<Map>> = OnceLock::new();

/// Called with the original executable's digest and its local cache file.
/// Learning in memory also works if the cache is unavailable.
pub fn configure_copy_back(path: Option<PathBuf>) {
    if std::env::var("XTP_READMAP").as_deref() == Ok("0") {
        return;
    }
    // The census installs independent one-shot guards on the same pages.
    #[cfg(any(test, feature = "diagnostics"))]
    if crate::shadow::census::mode() != 0 {
        return;
    }
    MAP.get_or_init(|| {
        let mut seen = BTreeSet::new();
        if let Some(file) = path.as_ref().and_then(|p| std::fs::File::open(p).ok()) {
            seen.extend(
                BufReader::new(file)
                    .lines()
                    .map_while(Result::ok)
                    .filter_map(|line| Key::parse(&line)),
            );
        }
        tracing::info!(ranges = seen.len(), "copy-back map loaded");
        Mutex::new(Map {
            path,
            seen,
            pending: Vec::new(),
        })
    });
}

pub(crate) fn enabled() -> bool {
    MAP.get().is_some()
}

pub(super) fn eager(key: Key) -> bool {
    MAP.get().is_some_and(|map| {
        map.lock()
            .unwrap_or_else(|e| e.into_inner())
            .seen
            .contains(&key)
    })
}

fn learn(key: Key) {
    if let Some(map) = MAP.get() {
        let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
        if map.seen.insert(key) && map.path.is_some() {
            map.pending.push(key);
        }
    }
}

/// Outside the fault handler and guard lock. An interrupted final record can
/// only affect eager hints. The process never trusts disk for ownership.
pub fn save_copy_back_map() {
    let Some(map) = MAP.get() else { return };
    let (path, keys) = {
        let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
        if map.pending.is_empty() {
            return;
        }
        (map.path.clone(), std::mem::take(&mut map.pending))
    };
    let Some(path) = path else { return };
    // A leading newline terminates a record interrupted by a crash. Each
    // save appends a batch in one write, with no file I/O in the handler.
    use std::fmt::Write as _;
    let mut buffer = String::from("\n");
    for k in &keys {
        writeln!(
            buffer,
            "{} {} {:016x} {}",
            k.size, k.alignment, k.writer, k.offset
        )
        .unwrap();
    }
    let saved = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(buffer.as_bytes()));
    if saved.is_err() {
        map.lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .extend(keys);
    }
}

struct Page {
    // The completed submission's result. Neither GPU reuse nor destruction
    // of its shadow can change this memory.
    bytes: Box<[u8]>,
    views: Vec<usize>,
    keys: BTreeSet<Key>,
}

/// All addresses here are 64 KiB block numbers. Access only under GUARDS.
#[derive(Default)]
pub(super) struct Owners {
    pages: BTreeMap<usize, Page>,
    by_view: BTreeMap<usize, usize>,
    restored: BTreeSet<usize>,
    pins: Vec<Range<usize>>,
}

impl Owners {
    pub(super) const fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
            by_view: BTreeMap::new(),
            restored: BTreeSet::new(),
            pins: Vec::new(),
        }
    }

    /// xmem pins its private view during mapping changes and DirectStorage
    /// writes. A kernel read pins the title view for the duration of the call.
    pub(super) fn pin(&mut self, range: Range<usize>, learn: bool) -> usize {
        let before = self.pages.len();
        let owners: Vec<_> = self
            .pages
            .range(range.clone())
            .map(|(&p, _)| p)
            .chain(self.by_view.range(range.clone()).map(|(_, &p)| p))
            .collect();
        for owner in owners {
            self.release(owner, learn);
        }
        self.pins.push(range);
        before - self.pages.len()
    }

    /// A racing reader may have restored our view after the failed system
    /// call. Retry it under the pin in that case too.
    pub(super) fn may_have_faulted(&self, views: Range<usize>) -> bool {
        self.by_view.range(views.clone()).next().is_some()
            || self.restored.range(views).next().is_some()
    }

    pub(super) fn unpin(&mut self, range: Range<usize>) {
        let i = self.pins.iter().position(|r| *r == range).unwrap();
        self.pins.swap_remove(i);
    }

    pub(super) fn forget(&mut self, range: Range<usize>) {
        let gone: Vec<_> = self.restored.range(range).copied().collect();
        for page in gone {
            self.restored.remove(&page);
        }
    }

    fn release(&mut self, physical: usize, learned: bool) {
        let Some(page) = self.pages.remove(&physical) else {
            return;
        };
        // SAFETY: xmem pins before removing or adding a mapping and before
        // writing through its permanent view. All title views are NOACCESS.
        unsafe {
            std::ptr::copy_nonoverlapping(page.bytes.as_ptr(), (physical * PAGE) as *mut u8, PAGE)
        };
        if learned {
            for key in page.keys {
                learn(key);
            }
        }
        for view in page.views {
            self.by_view.remove(&view);
            self.restored.insert(view);
            // A mapped page cannot disappear while xmem waits for GUARDS.
            assert!(crate::watch::set_protection(
                view..view + 1,
                windows_sys::Win32::System::Memory::PAGE_READONLY
            ));
        }
    }

    pub(super) fn touch(&mut self, view: usize, learned: bool) -> bool {
        if let Some(&owner) = self.by_view.get(&view) {
            self.release(owner, learned);
            true
        } else {
            false
        }
    }

    #[cfg(feature = "diagnostics")]
    pub(super) fn owns(&self, view: usize) -> bool {
        self.by_view.contains_key(&view)
    }

    pub(super) fn restored(&self, view: usize) -> bool {
        self.restored.contains(&view)
    }

    pub(super) fn pinned(&self, physical: usize, views: &[usize]) -> bool {
        self.pins
            .iter()
            .any(|pin| pin.contains(&physical) || views.iter().any(|view| pin.contains(view)))
    }

    /// All title views are already write-guarded. A writer (including one
    /// racing the completion) therefore waits until this merge finishes.
    pub(super) fn publish(
        &mut self,
        physical: usize,
        views: &[usize],
        key: Key,
        offset: usize,
        gpu: &mut [u8],
        before: Option<&[u8]>,
    ) -> Option<usize> {
        let eager = eager(key);
        if eager {
            self.release(physical, false);
        }
        // SAFETY: the callback returned xmem's committed permanent view of
        // this live page. Mapping changes and runtime writers take a pin.
        let backing = (physical * PAGE) as *mut u8;
        let page = if eager {
            // SAFETY: access through the permanent view while stores to the
            // title views are blocked. Readers may still read those views.
            unsafe { std::slice::from_raw_parts_mut(backing.add(offset), gpu.len()) }
        } else {
            if let std::collections::btree_map::Entry::Vacant(entry) = self.pages.entry(physical) {
                use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, PAGE_READONLY};
                for (i, &v) in views.iter().enumerate() {
                    if !crate::watch::set_protection(v..v + 1, PAGE_NOACCESS) {
                        for &previous in &views[..i] {
                            assert!(crate::watch::set_protection(
                                previous..previous + 1,
                                PAGE_READONLY
                            ));
                        }
                        return None;
                    }
                }
                // SAFETY: views are closed and the backing stays mapped.
                let bytes = unsafe { std::slice::from_raw_parts(backing, PAGE) }.into();
                for &v in views {
                    self.by_view.insert(v, physical);
                }
                entry.insert(Page {
                    bytes,
                    views: views.to_vec(),
                    keys: BTreeSet::new(),
                });
            }
            let page = self.pages.get_mut(&physical).unwrap();
            page.keys.insert(key);
            &mut page.bytes[offset..offset + gpu.len()]
        };
        // A title write made before the guards went on wins. A common
        // uncontended piece needs only a wide comparison and one copy.
        if let Some(before) = before
            && page != before
        {
            for ((gpu, title), old) in gpu.iter_mut().zip(page.iter()).zip(before) {
                if title != old {
                    *gpu = *title;
                }
            }
        }
        page.copy_from_slice(gpu);
        Some(if eager { gpu.len() } else { 0 })
    }
}
