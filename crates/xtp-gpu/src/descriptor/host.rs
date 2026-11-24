//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;
use xtp_shader::emit::{RawWindow, StrideEntry};

use super::{VIEW_TAG, View, view_at};
use crate::host::host;
use crate::shadow::Shadow;

pub(crate) struct HostHeap {
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Dynamic slots past `count` (none in sampler heaps): [`DYNAMIC`]
    /// where the host takes them on top of every title slot or caps the
    /// heap ([`capped_split`]), else [`MIN_DYNAMIC`].
    dynamic: u32,
    sampler: bool,
    /// The record (view index and generation) each slot was last made from,
    /// and for a texture the title placed, the host texture.
    made: Mutex<Made>,
    /// The host resource each slot views, held while the slot does: shaders
    /// index heaps directly, so a slot the title has not rewritten since it
    /// released the resource may still be read, and a freed host resource
    /// read there is a GPU page fault (the console reads stale memory).
    kept: Mutex<Vec<Option<ID3D12Resource>>>,
    /// The dynamic slots: which are taken (by lists not yet reset, or by
    /// `shared`), where the search starts, and how many are in use.
    slots: Mutex<Slots>,
    /// Raw views of host copies that every list naming the same range
    /// shares (root SRVs), in chunks of dynamic slots. Locked before
    /// `slots` when both are.
    shared: Mutex<Shared>,
    /// A raw view that reads zeros, in a dynamic slot of its own (made on
    /// first use, kept for the heap's life).
    null_raw: OnceLock<Option<u32>>,
    /// The runtime stride table and the raw views it names, in a view heap
    /// of title slots with `XTP_STRIDE_TABLE=1`
    /// ([`crate::device::stride_table_on`]).
    strides: Option<Strides>,
    /// The last [`HostHeap::picked_spans`]: its frame, and what it found.
    picked: Mutex<Option<(u64, Arc<Picked>)>>,
}

/// Slots past the dynamic ones in a view heap with a runtime stride table,
/// for the raw views its entries name ([`Strides`]): one per structured or
/// typed buffer view the heap holds, and one per such view replaced while
/// a list that may read it has not run.
/// shortcut: a guess (VIctory #704's), not a measured need; past it a view
/// gets no raw view, so its entry holds none and reads through the table
/// give zeros (logged once).
const ALIASES: u32 = 1 << 16;

/// A view heap's runtime stride table: an entry per slot
/// ([`StrideEntry`]), written with the slot's descriptor, and the raw
/// views the entries name. Shaders that read a buffer by element without
/// a stride known at translation read the entry of the slot they compute,
/// then access the raw view it names (VIctory #193 and #704).
struct Strides {
    table: StrideTable,
    /// The first of the [`ALIASES`] slots, past the dynamic ones.
    alias_base: u32,
    /// Shared with the holds of the recordings that read the table.
    aliases: Arc<Mutex<Aliases>>,
}

/// An upload buffer of one 8-byte entry per heap slot, mapped for good.
/// shortcut: shaders read it from system memory over PCIe (VIctory #704's
/// testers measured frame times 7 to 11 % higher with the table); copy it
/// to the card per submission if a session shows the reads cost.
struct StrideTable {
    resource: ID3D12Resource,
    /// The mapping's address.
    ptr: usize,
    gpu: u64,
    /// Its entries: every slot of the heap.
    len: u32,
}

impl StrideTable {
    /// A table of `len` entries of zeros (no view in any slot).
    fn new(device: &ID3D12Device, len: u32) -> Option<StrideTable> {
        let bytes = u64::from(len.max(1)) * u64::from(StrideEntry::BYTES);
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut resource: Option<ID3D12Resource> = None;
        let mut p = std::ptr::null_mut();
        // SAFETY: a plain upload buffer, mapped for good; the zeros are
        // written inside the mapping.
        let made = unsafe {
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_UPLOAD,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut resource,
                )
                .and_then(|()| {
                    let r = resource.as_ref().ok_or_else(windows::core::Error::empty)?;
                    r.Map(0, None, Some(&mut p))?;
                    std::ptr::write_bytes(p.cast::<u8>(), 0, bytes as usize);
                    Ok(r.GetGPUVirtualAddress())
                })
        };
        match (made, resource) {
            (Ok(gpu), Some(resource)) => Some(StrideTable {
                resource,
                ptr: p as usize,
                gpu,
                len,
            }),
            (made, _) => {
                tracing::warn!(error = ?made.err(), len, "no runtime stride table: shaders that read one are dropped");
                None
            }
        }
    }
}

/// The raw views of a heap's stride table, in the pool of [`ALIASES`]
/// slots from [`Strides::alias_base`].
///
/// A raw view the entries no longer name is not made again for another
/// slot while a list that may still read through it has not run: each
/// recording that reads the table holds the epoch it started at
/// ([`StrideHold`]), and a raw view given back at epoch `e` comes back to
/// the pool once no hold of an epoch up to `e` is left. A slot's own view
/// and entry are rewritten in place, as the title's descriptor is.
#[derive(Default)]
struct Aliases {
    /// The raw view of each slot that has one.
    of: HashMap<u32, Alias>,
    /// Pool slots free now.
    free: Vec<u32>,
    /// Pool slots given back, by epoch: the epoch, the pool slot, the host
    /// buffer it views (held while a list may read through it).
    retiring: VecDeque<(u64, u32, ID3D12Resource)>,
    /// Pool slots from here on were never taken.
    next: u32,
    /// Raw views given back so far.
    epoch: u64,
    /// Recordings that read the table, by the epoch they started at.
    holds: BTreeMap<u64, u32>,
    /// Pool slots taken (named or retiring), and the most at once.
    used: u32,
    peak: u32,
}

/// A slot's raw view: its pool slot, what it views (host buffer, first
/// dword, dwords, unordered access) and that buffer.
struct Alias {
    pool: u32,
    key: (usize, u64, u32, bool),
    buffer: ID3D12Resource,
}

impl Aliases {
    /// A free pool slot (taking back the retiring ones no hold may read),
    /// and the host buffers of those, to drop after the lock.
    fn take(&mut self) -> (Option<u32>, Vec<ID3D12Resource>) {
        let oldest = self.holds.keys().next().copied();
        let mut gone = Vec::new();
        while let Some(&(epoch, pool, _)) = self.retiring.front() {
            if oldest.is_some_and(|held| held <= epoch) {
                break;
            }
            if let Some((_, _, buffer)) = self.retiring.pop_front() {
                gone.push(buffer);
            }
            self.free.push(pool);
        }
        self.used -= gone.len() as u32;
        let pool = self.free.pop().or_else(|| {
            (self.next < ALIASES).then(|| {
                self.next += 1;
                self.next - 1
            })
        });
        if pool.is_some() {
            self.used += 1;
            if self.used > self.peak {
                // Logged at each new eighth of the pool.
                let eighth = |v: u32| v * 8 / ALIASES;
                if eighth(self.used) > eighth(self.peak) {
                    tracing::info!(
                        peak = self.used,
                        of = ALIASES,
                        retiring = self.retiring.len(),
                        "runtime stride table raw views: a new peak in use"
                    );
                }
                self.peak = self.used;
            }
        }
        (pool, gone)
    }

    /// Gives `alias` back: retiring until the holds of now have gone.
    fn retire(&mut self, alias: Alias) {
        self.retiring
            .push_back((self.epoch, alias.pool, alias.buffer));
        self.epoch += 1;
    }
}

/// A recording's hold on the raw views of its view heap's stride table: a
/// raw view an entry named when the recording started to read the table,
/// or later, is not made again for another slot until the hold goes, with
/// the recording (after its submissions have run).
pub(crate) struct StrideHold {
    aliases: Arc<Mutex<Aliases>>,
    epoch: u64,
}

// SAFETY: the host buffers the pool holds are D3D12 resources, which are
// free-threaded; the rest is behind the mutex.
unsafe impl Send for StrideHold {}

impl Drop for StrideHold {
    fn drop(&mut self) {
        let mut a = self.aliases.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = a.holds.get_mut(&self.epoch) {
            *n -= 1;
            if *n == 0 {
                a.holds.remove(&self.epoch);
            }
        }
    }
}

/// The bytes of its host copy a buffer view the host made reads, for the
/// raw view of the runtime stride table ([`make_with`]).
struct Viewed {
    buffer: ID3D12Resource,
    /// The view's first byte in the copy, and its bytes.
    start: u64,
    len: u64,
    /// Bytes per element (4 for a raw view).
    element: u64,
    /// The view is raw (read in its own slot).
    raw: bool,
    unordered: bool,
}

/// What each title slot of a host heap was made from.
struct Made {
    /// The record (view index and generation; 0 when not made).
    keys: Vec<u64>,
    /// For a texture the title placed itself, the host texture the slot
    /// views (see [`HostHeap::mirror`]).
    placed: HashMap<u32, usize>,
    /// Title slots whose buffer view waits for a draw (made null when the
    /// title wrote it), each once: [`HostHeap::picked_spans`] makes them.
    waiting: Vec<u32>,
    queued: Vec<bool>,
    /// The buffer views made in title slots: the title memory each reads
    /// (address, bytes) and whether it is for unordered access.
    buffers: HashMap<u32, (u64, u64, bool)>,
}

/// What the buffer views of a heap's title slots read, for shaders that
/// pick slots when they run ([`HostHeap::picked_spans`]).
#[derive(Default)]
pub(crate) struct Picked {
    /// Per host copy, the span of title memory its views read.
    pub(crate) reads: Vec<(Arc<Shadow>, u64, u64)>,
}

/// How long a [`HostHeap::picked_spans`] pass makes waiting views for.
const PICKED_BUDGET: std::time::Duration = std::time::Duration::from_millis(8);

/// Dynamic slot bookkeeping.
struct Slots {
    next: u32,
    taken: Vec<bool>,
    /// Slots taken now, and the most ever taken at once.
    used: u32,
    peak: u32,
}

/// Slots per chunk the shared views take at once.
pub(crate) const SHARED_CHUNK: u32 = 256;

/// Shared views no list holds that are kept for lists to come; past it the
/// least recently held half goes.
const IDLE_SHARED: usize = 4096;

/// A range of a host copy: the copy, the byte offset, the length.
type SharedKey = (usize, u64, u64);

/// Shared raw views of host copies (see [`HostHeap::shared_raw`]).
#[derive(Default)]
struct Shared {
    index: HashMap<SharedKey, u32>,
    entries: HashMap<u32, SharedView>,
    /// Chunks of dynamic slots: first slot, the free slots in it.
    chunks: Vec<(u32, Vec<u32>)>,
    /// Entries no list holds.
    idle: usize,
    /// Counts lists giving views back, to tell how long ago an entry was
    /// last held.
    tick: u64,
}

struct SharedView {
    key: SharedKey,
    /// The lists holding it (not yet reset).
    holders: u32,
    /// When it was last given back.
    last: u64,
    /// The host copy it views: an entry whose copy is gone (the title
    /// released the buffer) goes first.
    copy: Weak<Shadow>,
}

// SAFETY: D3D12 descriptor heaps and resources are free-threaded; the rest
// of the state is behind mutexes. Needed for XTP's own view heap, a static.
unsafe impl Send for HostHeap {}
// SAFETY: as above.
unsafe impl Sync for HostHeap {}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!(
                "{:#x} past its end (slot {})",
                va - start - bytes,
                (va - start) / step
            )
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more. An RTX 4080 SUPER (tier
/// 3) takes exactly this many, not one more (2026-11-24).
const PORTABLE_VIEWS: u32 = 1_000_000;

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw). A list holds the slots it
/// took until it is reset (its submissions have run) or released.
/// 2^16 ran out seconds into the world (every world run on an RX 9070 XT,
/// each a device removal shortly after; #59), and on capped heaps too:
/// NVIDIA worlds stalled (VIctory #747), and XTP's Series S world on the
/// RTX 4080 reached 57,364 of 65,536 and dropped draws for want of slots.
const DYNAMIC: u32 = 1 << 18;

/// The dynamic slots of a heap that the host would not make with
/// [`DYNAMIC`] more but would with these, which leave every title slot
/// in.
const MIN_DYNAMIC: u32 = 1 << 16;

/// A capped view heap of `limit` host slots split into (title slots
/// mirrored, dynamic slots): [`DYNAMIC`] dynamic ones, at least one title
/// slot when `limit` > 1. The title's slots past the first part read
/// nothing; BTP 6's view heap has 1,245,184, and none of its runs wrote a
/// slot past 934,464 (the part 2^16 dynamic slots leave).
fn capped_split(limit: u32) -> (u32, u32) {
    let dynamic = DYNAMIC.min(limit.saturating_sub(1));
    (limit - dynamic, dynamic)
}

impl HostHeap {
    /// A host heap for a title heap of `count` slots; view heaps get a
    /// runtime stride table with `XTP_STRIDE_TABLE=1`.
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        Self::new_with(sampler, count, crate::device::stride_table_on())
    }

    /// [`HostHeap::new`], with a runtime stride table (view heaps only) or
    /// not: [`ALIASES`] more slots for its raw views, on top of the dynamic
    /// ones, and counted in the host's cap.
    pub(crate) fn new_with(sampler: bool, count: u32, stride_table: bool) -> Option<Self> {
        let aliases = if sampler || !stride_table { 0 } else { ALIASES };
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        // The larger dynamic range where the host takes it on top of every
        // title slot; else the smaller one, which keeps every title slot;
        // past the host's cap, the larger one again (VIctory #1349).
        let (heap, count, extra) = if sampler {
            match make(count) {
                Ok(heap) => (heap, count, 0),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else if let Ok(heap) = make(count + DYNAMIC + aliases) {
            (heap, count, DYNAMIC)
        } else {
            match make(count + MIN_DYNAMIC + aliases) {
                Ok(heap) => (heap, count, MIN_DYNAMIC),
                Err(e) if count + MIN_DYNAMIC + aliases > PORTABLE_VIEWS => {
                    // shortcut: descriptors past the host's limit are left
                    // out; a title that reaches them needs remapping ("title
                    // slot past the host heap" says so). The raw views of
                    // the stride table come out of the dynamic part (2^18
                    // less 2^16 left; the Series S world's peak in use was
                    // 98,312), not the title's.
                    let (mirrored, dynamic) = capped_split(PORTABLE_VIEWS);
                    let dynamic = dynamic.saturating_sub(aliases);
                    tracing::warn!(
                        error = %e,
                        count,
                        mirrored,
                        dynamic,
                        aliases,
                        "host view heap capped at {PORTABLE_VIEWS}"
                    );
                    (make(PORTABLE_VIEWS).ok()?, mirrored, dynamic)
                }
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        };
        Self::from_heap(heap, kind, sampler, count, extra, aliases)
    }

    /// XTP's own view heap of `dynamic` dynamic slots and no title slots,
    /// for lists that bind no view heap but read root SRVs.
    pub(crate) fn own(dynamic: u32) -> Option<Self> {
        let host = host()?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain heap creation.
        let made = unsafe {
            host.device
                .CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: dynamic,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
        };
        match made {
            Ok(heap) => Self::from_heap(heap, kind, false, 0, dynamic, 0),
            Err(e) => {
                tracing::warn!(error = %e, dynamic, "no view heap of XTP's own");
                None
            }
        }
    }

    /// A host heap around `heap`: `count` title slots, then `extra`
    /// dynamic ones, then `aliases` for the raw views of a runtime stride
    /// table (none: no table), every slot made null.
    fn from_heap(
        heap: ID3D12DescriptorHeap,
        kind: D3D12_DESCRIPTOR_HEAP_TYPE,
        sampler: bool,
        count: u32,
        extra: u32,
        aliases: u32,
    ) -> Option<Self> {
        let host = host()?;
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, extra, aliases, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        let total = count + extra + aliases;
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(total) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        // Every slot of the heap has an entry: a shader reads the one of
        // whatever slot it computes.
        let strides = (aliases > 0)
            .then(|| StrideTable::new(&host.device, total))
            .flatten()
            .map(|table| Strides {
                table,
                alias_base: count + extra,
                aliases: Arc::default(),
            });
        let h = HostHeap {
            heap,
            cpu,
            step,
            count,
            dynamic: extra,
            sampler,
            made: Mutex::new(Made {
                keys: vec![0; count as usize],
                placed: HashMap::new(),
                waiting: Vec::new(),
                queued: vec![false; if sampler { 0 } else { count as usize }],
                buffers: HashMap::new(),
            }),
            kept: Mutex::new(vec![None; (count + extra) as usize]),
            slots: Mutex::new(Slots {
                next: 0,
                taken: vec![false; extra as usize],
                used: 0,
                peak: 0,
            }),
            shared: Mutex::default(),
            null_raw: OnceLock::new(),
            strides,
            picked: Mutex::new(None),
        };
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, total) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(
            ms = started.elapsed().as_millis(),
            "host descriptor heap made null"
        );
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: BLOCK,
                        ..Default::default()
                    })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler(start);
            } else {
                null_view(start);
            }
            let mut filled = 1;
            while filled < BLOCK {
                let n = filled.min(BLOCK - filled);
                host.device
                    .CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = BLOCK.min(total - done);
                host.device
                    .CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler(self.cpu(index));
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index. When none are free, the
    /// shared views no list holds give theirs back first.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if let Some(first) = self.take(n) {
            return Some(first);
        }
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        {
            let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            let idle = shared.idle;
            if idle > 0 {
                self.evict(&mut shared, idle);
            }
        }
        let first = self.take(n);
        if first.is_none() {
            static FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let failed = FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if failed.is_power_of_two() {
                let (used, peak) = self.dynamic_use();
                tracing::warn!(
                    n,
                    used,
                    peak,
                    of = self.dynamic,
                    failed,
                    "no free dynamic descriptor slots: a table or view is not made"
                );
            }
        }
        first
    }

    /// [`HostHeap::dynamic`] without taking shared views' slots back.
    fn take(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let s = &mut *guard;
        let n = n as usize;
        // Search from `next` on, then from the start.
        let starts = (s.next as usize..self.dynamic as usize).chain(0..s.next as usize);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if s.taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let first = found?;
        s.taken[first..first + n].fill(true);
        s.next = ((first + n) % self.dynamic as usize) as u32;
        s.used += n as u32;
        if s.used > s.peak {
            // Logged at each new eighth of the range.
            let eighth = |v: u32| u64::from(v) * 8 / u64::from(self.dynamic);
            if eighth(s.used) > eighth(s.peak) {
                tracing::info!(
                    peak = s.used,
                    of = self.dynamic,
                    "dynamic descriptor slots: a new peak in use"
                );
            }
            s.peak = s.used;
        }
        Some(self.count + first as u32)
    }

    /// The dynamic slots taken now, and the most ever taken at once.
    pub(crate) fn dynamic_use(&self) -> (u32, u32) {
        let s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        (s.used, s.peak)
    }

    /// The title slots this heap has host slots for; fewer than the title's
    /// when the host caps its size.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned).
    ///
    /// [`dynamic`]: HostHeap::dynamic
    #[cfg(test)]
    pub(crate) fn release(&self, first: u32, n: u32) {
        self.release_all(&[(first, n)]);
    }

    /// Gives back each range `(first, n)` of dynamic slots (indices
    /// [`HostHeap::dynamic`] returned), under one lock.
    pub(crate) fn release_all(&self, ranges: &[(u32, u32)]) {
        if ranges.is_empty() {
            return;
        }
        // Their stride table entries and raw views go first: once free, a
        // slot may be taken by another list, which writes its own (VIctory
        // #704, 4e03086).
        self.forget_ranges(ranges);
        let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let s = &mut *guard;
        for &(first, n) in ranges {
            let Some(at) = first.checked_sub(self.count) else {
                continue;
            };
            let end = (at.saturating_add(n)).min(self.dynamic) as usize;
            if let Some(slots) = s.taken.get_mut(at as usize..end) {
                let freed = slots.iter().filter(|&&t| t).count() as u32;
                slots.fill(false);
                s.used -= freed;
            }
        }
    }

    /// A raw view of `len` bytes of `copy` from byte `offset` (a multiple
    /// of 16; see [`HostHeap::raw_at`]): its slot, shared by every list
    /// that asks for the same range and held once more for the caller
    /// until [`HostHeap::give_back`]. Shared views are made once, not once
    /// per list, which spares the dynamic slots: a capped heap (NVIDIA) has
    /// 2^16 of them for every list's fresh tables too. `None` if no slot is
    /// free.
    pub(crate) fn shared_raw(&self, copy: &Arc<Shadow>, offset: u64, len: u64) -> Option<u32> {
        let key = (copy.buffer.as_raw() as usize, offset, len);
        let mut guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let s = &mut *guard;
        if let Some(&slot) = s.index.get(&key)
            && let Some(entry) = s.entries.get_mut(&slot)
        {
            if entry.holders == 0 {
                s.idle -= 1;
            }
            entry.holders += 1;
            return Some(slot);
        }
        let slot = match self.shared_slot(s) {
            Some(slot) => slot,
            None => {
                let idle = s.idle;
                self.evict(s, idle);
                self.shared_slot(s)?
            }
        };
        self.raw_at(slot, &copy.buffer, offset, len);
        s.index.insert(key, slot);
        s.entries.insert(
            slot,
            SharedView {
                key,
                holders: 1,
                last: s.tick,
                copy: Arc::downgrade(copy),
            },
        );
        Some(slot)
    }

    /// A free slot of the shared views' chunks, taking a new chunk if
    /// they have none.
    fn shared_slot(&self, s: &mut Shared) -> Option<u32> {
        if let Some(slot) = s.chunks.iter_mut().find_map(|(_, free)| free.pop()) {
            return Some(slot);
        }
        let first = self.take(SHARED_CHUNK)?;
        // Handed out from the chunk's start.
        s.chunks
            .push((first, (first + 1..first + SHARED_CHUNK).rev().collect()));
        Some(first)
    }

    /// The lists giving back shared views they held (slots
    /// [`HostHeap::shared_raw`] returned), under one lock. Views no list
    /// holds stay for later lists, up to [`IDLE_SHARED`] of them.
    pub(crate) fn give_back(&self, slots: &[u32]) {
        if slots.is_empty() {
            return;
        }
        let mut guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let s = &mut *guard;
        s.tick += 1;
        for slot in slots {
            if let Some(entry) = s.entries.get_mut(slot)
                && entry.holders > 0
            {
                entry.holders -= 1;
                if entry.holders == 0 {
                    entry.last = s.tick;
                    s.idle += 1;
                }
            }
        }
        if s.idle > IDLE_SHARED {
            let n = s.idle - IDLE_SHARED / 2;
            self.evict(s, n);
        }
    }

    /// Drops `n` shared views no list holds: those whose host copy is gone
    /// first, then the least recently held. Chunks left empty go back to
    /// the dynamic slots.
    fn evict(&self, s: &mut Shared, n: usize) {
        let mut idle: Vec<(bool, u64, u32)> = s
            .entries
            .iter()
            .filter(|(_, e)| e.holders == 0)
            .map(|(&slot, e)| (e.copy.strong_count() > 0, e.last, slot))
            .collect();
        idle.sort_unstable();
        for &(_, _, slot) in idle.iter().take(n) {
            let Some(entry) = s.entries.remove(&slot) else {
                continue;
            };
            s.index.remove(&entry.key);
            s.idle -= 1;
            // Nothing reads it (every list that held it was reset): made
            // null, it no longer holds the host copy.
            self.null_raw_at(slot);
            if let Some((_, free)) = s
                .chunks
                .iter_mut()
                .find(|(first, _)| (*first..*first + SHARED_CHUNK).contains(&slot))
            {
                free.push(slot);
            }
        }
        let mut empty = Vec::new();
        s.chunks.retain(|(first, free)| {
            let whole = free.len() == SHARED_CHUNK as usize;
            if whole {
                empty.push((*first, SHARED_CHUNK));
            }
            !whole
        });
        self.release_all(&empty);
    }

    /// The shared views (all, and those no list holds).
    #[cfg(test)]
    pub(crate) fn shared_count(&self) -> (usize, usize) {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        (s.entries.len(), s.idle)
    }

    /// A slot holding a raw view that reads zeros, the same for every list
    /// (never given back); `None` if no slot is free.
    pub(crate) fn null_raw(&self) -> Option<u32> {
        *self.null_raw.get_or_init(|| {
            let slot = self.dynamic(1)?;
            self.null_raw_at(slot);
            Some(slot)
        })
    }

    /// Makes slot `index` (a dynamic one) from `view`; a null view if the
    /// host cannot. True if the host made the view.
    pub(crate) fn make_at(&self, index: u32, view: &View) -> bool {
        let mut viewed = None;
        let wanted = self.strides.is_some().then_some(&mut viewed);
        let kept = make_with(view, self.cpu(index), wanted);
        let made = kept.is_some();
        if !made {
            null_for(view, self.cpu(index));
        }
        self.note_view(index, viewed.filter(|_| made));
        self.keep(index, kept.flatten());
        made
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
        self.forget_view(index);
        self.keep(index, None);
    }

    /// Slot `index` now views `resource` (or nothing of the host's).
    fn keep(&self, index: u32, resource: Option<ID3D12Resource>) {
        // The one it held goes after the lock: its last reference may free
        // video memory, which can wait.
        drop(self.swap_kept(index, resource));
    }

    /// [`HostHeap::keep`], handing back the resource the slot held.
    fn swap_kept(&self, index: u32, resource: Option<ID3D12Resource>) -> Option<ID3D12Resource> {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        kept.get_mut(index as usize)
            .and_then(|slot| std::mem::replace(slot, resource))
    }

    /// Makes slot `index` (a dynamic one) a constant buffer view of host
    /// memory (the list's own, which it holds).
    pub(crate) fn constant_at(&self, index: u32, gpu: u64, size: u32) {
        self.forget_view(index);
        self.keep(index, None);
        let Some(host) = host() else { return };
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: size.next_multiple_of(256),
        };
        // SAFETY: a slot of our heap; the memory is the caller's.
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.cpu(index))
        };
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view of `len` bytes
    /// of `buffer` from byte `offset` (a multiple of 16): a root SRV with a
    /// size, whose reads past it return zeros instead of reaching whatever
    /// the host placed after the buffer.
    ///
    /// The last dword is whole even when `len` is not a multiple of 4 (host
    /// copies are rounded up to 64 KiB, so it is inside `buffer`), and the
    /// view stops at D3D12's 2^27 buffer view elements (512 MiB).
    pub(crate) fn raw_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        const MAX_ELEMENTS: u64 = 1 << D3D12_REQ_BUFFER_RESOURCE_TEXEL_COUNT_2_TO_EXP;
        let Some(host) = host() else { return };
        let elements = len.div_ceil(4).clamp(1, MAX_ELEMENTS) as u32;
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), self.cpu(index))
        };
        // A root SRV's view: no table entry names it (no title slot does).
        self.forget_view(index);
        self.keep(index, Some(buffer.clone()));
    }

    /// Makes slot `index` a raw buffer view that reads zeros.
    pub(crate) fn null_raw_at(&self, index: u32) {
        null_buffer_view(self.cpu(index));
        self.forget_view(index);
        self.keep(index, None);
    }

    /// The shader-visible handle of slot `index`.
    pub(crate) fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: a live shader-visible heap.
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart().ptr };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start + u64::from(index) * u64::from(self.step),
        }
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are made only for draws (`for_draw`), which also copy their
    /// data: a host copy of a buffer is made on first use.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        if index >= self.count {
            // Diagnostic (NVIDIA heap cap): the highest title slot past the
            // host heap seen so far.
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        // A texture the title placed itself is viewed through the resource
        // made for its memory, which another replaces when a texture the
        // GPU writes comes there (#643): the record stays the same, so the
        // slot is made again when the host texture changes. Resolved
        // before the lock, since making the resource can wait.
        let placed = match &view {
            Some(v @ View::Placed(false, resource, _)) if for_draw && resource[0] != 1 => {
                Some(placed_texture(v))
            }
            _ => None,
        };
        let texture = placed.as_ref().map(|p| match p {
            Some(View::Shader(r, _)) => crate::resource::host_resource(*r as crate::object::This)
                .map_or(0, |t| t.as_raw() as usize),
            _ => 0,
        });
        // Whether the slot is to be made again: another record, or another
        // host texture. The host texture held by the slot cannot be
        // another's (`kept` holds it), so the same address is the same one.
        let stale = |made: &Made| {
            made.keys[index as usize] != key
                || texture.is_some_and(|t| made.placed.get(&index).copied() != Some(t))
        };
        // A buffer view's host copy, a resource (which can wait in the
        // kernel, on video memory), is made before the lock too when the
        // slot is to be made: under the lock, threads recording other lists
        // only wait for descriptors to be written (VidMm waits there took
        // nearly half the title's recording time in the world).
        let resolved = placed.as_ref().and_then(Option::as_ref).or(view.as_ref());
        if for_draw
            && let Some((address, len)) = resolved.and_then(copy_range)
            && stale(&self.made.lock().unwrap_or_else(|e| e.into_inner()))
        {
            let _ = crate::shadow::find(address, len);
        }
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let Some(view) = view else {
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            let mut old = None;
            if made.keys[index as usize] != 0 {
                made.keys[index as usize] = 0;
                made.placed.remove(&index);
                if self.sampler {
                    self.null_sampler(index);
                } else {
                    null_view(self.cpu(index));
                }
                self.forget_view(index);
                made.buffers.remove(&index);
                old = self.swap_kept(index, None);
            }
            // A host resource the slot held goes after the lock.
            drop(made);
            drop(old);
            return None;
        };
        let mut old = None;
        if stale(&made) {
            let view = placed.as_ref().and_then(Option::as_ref).unwrap_or(&view);
            let later = !for_draw && view.is_buffer();
            made.keys[index as usize] = if later { 0 } else { key };
            // Made null until a draw names it, or a shader that picks slots
            // at run time is bound (`picked_spans`): buffer views only. A
            // view of a texture the title placed itself would make a host
            // texture over memory the title may be drawing a target in, and
            // constant buffer views are no buffer a raw read can take.
            let waits = later && buffer_range(view).is_some();
            if waits
                && let Some(queued) = made.queued.get_mut(index as usize)
                && !*queued
            {
                *queued = true;
                made.waiting.push(index);
            }
            match texture {
                Some(t) if !later => made.placed.insert(index, t),
                _ => made.placed.remove(&index),
            };
            let mut viewed = None;
            let kept = if later {
                None
            } else {
                let wanted = self.strides.is_some().then_some(&mut viewed);
                make_with(view, self.cpu(index), wanted)
            };
            if kept.is_none() && !self.sampler {
                null_for(view, self.cpu(index));
            }
            // The entry with the descriptor, under the same lock.
            self.note_view(index, viewed.filter(|_| kept.is_some()));
            match buffer_range(view).filter(|&(_, len)| kept.is_some() && len > 0) {
                Some((address, len)) => {
                    let unordered = matches!(view, View::Unordered(..) | View::Placed(true, ..));
                    made.buffers.insert(index, (address, len, unordered));
                }
                None => {
                    made.buffers.remove(&index);
                }
            }
            old = self.swap_kept(index, kept.flatten());
        }
        drop(made);
        drop(old);
        Some(placed.flatten().unwrap_or(view))
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = *self
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys
            .get(index as usize)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_range(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_range(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// [`buffer_range`] of an unordered access view of memory at `base`.
fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

impl HostHeap {
    /// The runtime stride table's GPU address, and its buffer (for lists
    /// to keep), if the heap has one.
    pub(crate) fn stride_table(&self) -> Option<(u64, &ID3D12Resource)> {
        self.strides
            .as_ref()
            .map(|s| (s.table.gpu, &s.table.resource))
    }

    /// A hold for a recording that reads the stride table ([`StrideHold`]);
    /// none if the heap has no table.
    pub(crate) fn stride_hold(&self) -> Option<StrideHold> {
        let s = self.strides.as_ref()?;
        let mut a = s.aliases.lock().unwrap_or_else(|e| e.into_inner());
        let epoch = a.epoch;
        *a.holds.entry(epoch).or_default() += 1;
        Some(StrideHold {
            aliases: s.aliases.clone(),
            epoch,
        })
    }

    /// Writes slot `index`'s table entry, in one 64-bit store: a shader
    /// reading it (one load of its two dwords) never sees half of an old
    /// entry and half of a new one.
    fn set_entry(&self, index: u32, entry: StrideEntry) {
        let Some(s) = &self.strides else { return };
        if index >= s.table.len {
            return;
        }
        let bits = entry.bits().unwrap_or_else(|| {
            once!(?entry, "a stride table entry the table cannot hold: none");
            0
        });
        // SAFETY: an 8-byte aligned entry inside the live mapping.
        let at = unsafe { AtomicU64::from_ptr((s.table.ptr as *mut u64).add(index as usize)) };
        at.store(bits, Ordering::Release);
    }

    /// Slot `index`'s table entry for the view just made there: `viewed`,
    /// the bytes of its host copy a buffer view reads (none for any other
    /// view, or one the host could not make). A raw view is read in its own
    /// slot; a structured or typed one through a raw view of the same bytes
    /// in the pool, made before the entry names it. A view that changed
    /// gets a raw view in another pool slot: the old one may still be read
    /// by lists in flight ([`Aliases`]).
    fn note_view(&self, index: u32, viewed: Option<Viewed>) {
        let Some(s) = &self.strides else { return };
        let Some(v) = viewed else {
            return self.forget_view(index);
        };
        if v.raw {
            self.set_entry(
                index,
                StrideEntry {
                    stride: 0,
                    raw_slot: Some(index),
                    bias: 0,
                    count: u32::try_from(v.len / 4).unwrap_or(u32::MAX),
                },
            );
            return self.retire_alias(s, index);
        }
        let element = u32::try_from(v.element).unwrap_or(0);
        let window = RawWindow::covering(v.start, v.len)
            .filter(|_| element.is_multiple_of(4) && element <= StrideEntry::MAX_STRIDE);
        let Some(window) = window else {
            // shortcut: a view starting off a dword of its host copy, or of
            // elements no entry holds, is read through the table as no view.
            once!(
                start = v.start,
                element,
                "a buffer view the stride table cannot name a raw view for: read as none"
            );
            return self.forget_view(index);
        };
        let key = (
            v.buffer.as_raw() as usize,
            window.first,
            window.dwords,
            v.unordered,
        );
        let count = (v.len / v.element).min(u64::from(window.elements(element))) as u32;
        let mut a = s.aliases.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = a.of.get(&index)
            && old.key == key
        {
            // The same bytes: the raw view it has.
            let pool = old.pool;
            drop(a);
            return self.set_entry(
                index,
                StrideEntry {
                    stride: element,
                    raw_slot: Some(s.alias_base + pool),
                    bias: window.bias,
                    count,
                },
            );
        }
        let (pool, gone) = a.take();
        let Some(pool) = pool else {
            drop(a);
            drop(gone);
            once!(
                of = ALIASES,
                "no free raw view slots for the stride table: a buffer read through it reads zeros"
            );
            return self.forget_view(index);
        };
        let slot = s.alias_base + pool;
        raw_view(&v.buffer, window, v.unordered, self.cpu(slot));
        let old = a.of.insert(
            index,
            Alias {
                pool,
                key,
                buffer: v.buffer.clone(),
            },
        );
        self.set_entry(
            index,
            StrideEntry {
                stride: element,
                raw_slot: Some(slot),
                bias: window.bias,
                count,
            },
        );
        if let Some(old) = old {
            a.retire(old);
        }
        drop(a);
        drop(gone);
    }

    /// Slot `index` holds no view shaders read through the stride table:
    /// its entry says none, then its raw view is given back.
    fn forget_view(&self, index: u32) {
        let Some(s) = &self.strides else { return };
        self.set_entry(index, StrideEntry::NONE);
        self.retire_alias(s, index);
    }

    /// Gives back slot `index`'s raw view, if it has one.
    fn retire_alias(&self, s: &Strides, index: u32) {
        let mut a = s.aliases.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = a.of.remove(&index) {
            a.retire(old);
        }
    }

    /// [`HostHeap::forget_view`] for each slot of `ranges` (dynamic slots
    /// given back), under one lock.
    fn forget_ranges(&self, ranges: &[(u32, u32)]) {
        let Some(s) = &self.strides else { return };
        for &(first, n) in ranges {
            for index in first..first.saturating_add(n).min(s.alias_base) {
                self.set_entry(index, StrideEntry::NONE);
            }
        }
        let mut a = s.aliases.lock().unwrap_or_else(|e| e.into_inner());
        if a.of.is_empty() {
            return;
        }
        for &(first, n) in ranges {
            for index in first..first.saturating_add(n).min(s.alias_base) {
                if let Some(old) = a.of.remove(&index) {
                    a.retire(old);
                }
            }
        }
    }

    /// What the buffer views of the title's slots read, with every buffer
    /// view the title wrote made first, for a shader that picks the slots
    /// it reads when it runs (no used-resource entry names them, so no
    /// draw's table made their views: they were null views, read as zeros;
    /// VIctory #71, watch.md item 170). `start` is the title heap's first
    /// descriptor. Made at most once a frame per heap, by the first list of
    /// the frame that asks (none for the others: `None`): views the title
    /// writes later in the frame, and what it writes in their memory, wait
    /// for the next one. Title memory that is not write-watched is copied
    /// whole by each refresh, which per list made the pass cost seconds a
    /// frame (fxceless's VIctory fork, 578d459: 18 s a frame per list,
    /// some 30 ms once a frame).
    ///
    /// The approach of Marvisak/VIctory#1544 (n-romanovich): views written
    /// by the title made before a draw that reads the heap directly, their
    /// title memory brought up to date. Here the slots that waited are
    /// made once (not every slot that ever held a view, at each change of
    /// the heap), the spans are merged per host copy, and the views go
    /// through the checks a host view must pass (`fit_view`), so that a bad
    /// record makes a null view rather than remove the device.
    pub(crate) fn picked_spans(&self, start: u64) -> Option<Arc<Picked>> {
        let frame = crate::frame::current();
        let mut picked = self.picked.lock().unwrap_or_else(|e| e.into_inner());
        if picked.as_ref().is_some_and(|(at, _)| *at == frame) {
            return None;
        }
        let started = std::time::Instant::now();
        let waiting = {
            let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            let waiting = std::mem::take(&mut made.waiting);
            for &index in &waiting {
                if let Some(queued) = made.queued.get_mut(index as usize) {
                    *queued = false;
                }
            }
            waiting
        };
        // Within a budget a pass: each view may need a host copy made (a
        // resource, which can wait in the kernel), and the world's loading
        // leaves tens of thousands waiting. Those past it wait for the next
        // frames' passes, in the order the title wrote them.
        let mut made_now = 0u32;
        let mut left = Vec::new();
        for (n, &index) in waiting.iter().enumerate() {
            if started.elapsed() > PICKED_BUDGET {
                left.extend_from_slice(&waiting[n..]);
                break;
            }
            if self
                .mirror(index, start + u64::from(index) * 32, true)
                .is_some()
            {
                made_now += 1;
            }
        }
        if !left.is_empty() {
            let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            for &index in &left {
                if let Some(queued) = made.queued.get_mut(index as usize)
                    && !*queued
                {
                    *queued = true;
                    made.waiting.push(index);
                }
            }
        }
        let ranges: Vec<(u64, u64, bool)> = self
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buffers
            .values()
            .copied()
            .collect();
        let mut spans: HashMap<usize, (Arc<Shadow>, u64, u64)> = HashMap::new();
        let mut unordered_views = 0usize;
        for (address, len, unordered) in ranges.iter().copied() {
            let Some(copy) = crate::shadow::existing(address, len) else {
                continue;
            };
            let span = spans
                .entry(Arc::as_ptr(&copy) as usize)
                .or_insert_with(|| (copy.clone(), address, address + len));
            span.1 = span.1.min(address);
            span.2 = span.2.max(address + len);
            unordered_views += usize::from(unordered);
        }
        let p = Arc::new(Picked {
            reads: spans.into_values().collect(),
        });
        static PASSES: AtomicU64 = AtomicU64::new(0);
        let passes = PASSES.fetch_add(1, Ordering::Relaxed) + 1;
        if passes.is_power_of_two() {
            let bytes: u64 = p.reads.iter().map(|(_, from, to)| to - from).sum();
            tracing::info!(
                passes,
                waited = waiting.len(),
                made = made_now,
                left = left.len(),
                views = ranges.len(),
                copies = p.reads.len(),
                mib = bytes >> 20,
                unordered = unordered_views,
                us = started.elapsed().as_micros() as u64,
                "buffer views of slots picked at run time: made and listed"
            );
        }
        *picked = Some((frame, p.clone()));
        Some(p)
    }

    /// Title slots whose buffer view waits for a pass (tests).
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting
            .len()
    }

    /// Slot `index`'s table entry (tests).
    #[cfg(test)]
    pub(crate) fn entry(&self, index: u32) -> StrideEntry {
        let s = self.strides.as_ref().expect("a stride table");
        assert!(index < s.table.len);
        // SAFETY: as in `set_entry`.
        let at = unsafe { AtomicU64::from_ptr((s.table.ptr as *mut u64).add(index as usize)) };
        StrideEntry::from_bits(at.load(Ordering::Acquire))
    }

    /// The first slot of the raw views' pool (tests).
    #[cfg(test)]
    pub(crate) fn alias_base(&self) -> u32 {
        self.strides.as_ref().expect("a stride table").alias_base
    }
}

/// Makes a raw view of `window` of `buffer` at `at`: unordered access, or
/// a shader resource view.
fn raw_view(
    buffer: &ID3D12Resource,
    window: RawWindow,
    unordered: bool,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let Some(host) = host() else { return };
    // SAFETY: a live buffer holding the window (the view just made reads
    // those bytes of it) and a slot of our heap.
    unsafe {
        if unordered {
            let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV {
                        FirstElement: window.first,
                        NumElements: window.dwords,
                        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                        ..Default::default()
                    },
                },
            };
            host.device
                .CreateUnorderedAccessView(buffer, None::<&ID3D12Resource>, Some(&desc), at);
        } else {
            let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: window.first,
                        NumElements: window.dwords,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), at);
        }
    }
}

/// The host resource a slot made by [`make`] views, if any.
type Kept = Option<ID3D12Resource>;

/// Counts a buffer view the host cannot make because its buffer's start in
/// the host copy (`offset`) is not on one of its elements (of `element`
/// bytes): the slot reads zeros. A console view starts at any address, and
/// the title places structured views at ring positions inside a big buffer
/// (VIctory's 86a5ed6: the meshlet culling's 456-byte constants, so every
/// meshlet was culled; watch.md item 180). Logged at the first and each
/// power of two, with the element size: how many a frame, and of what.
/// shortcut: such a view needs a copy of its own bytes (86a5ed6's
/// `shadow::for_view`), which is not made here.
fn off_element(unordered: bool, offset: u64, element: u64) {
    static COUNT: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
    let n = COUNT[usize::from(unordered)].fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        tracing::warn!(
            count = n,
            unordered,
            offset,
            element,
            "buffer view not on an element of its host copy: null"
        );
    }
}

/// Makes a buffer view's elements fit its host copy `copy` (`first`,
/// `elements` of `element` bytes, the first moved to the copy already):
/// none if no element does, or if the host refuses such a view (a stride
/// past D3D12's 2048, a typeless format without the raw flag), which a
/// view past its resource's end or of no elements is too. A view the host
/// refuses can remove the device; the console reads zeros past a view's
/// end, as the cut view does (VIctory #193, f99b792; the `buffer_view_ok`
/// of fxceless's VIctory fork, 578d459).
///
/// A view running past its copy is cut, not made null as VIctory's main
/// does since 4051149 (watch.md item 182): the device removal it guards
/// against comes from a view past its resource, and the cut view lies
/// wholly inside its copy; it reads the bytes the console reads, and zeros
/// past the copy (D3D12's bounds), where a null view would read zeros for
/// the valid part too, the very failure of the culling's zero reads.
fn fit_view(
    copy: &ID3D12Resource,
    first: u64,
    elements: &mut u32,
    element: u64,
    (stride, format, raw): (u32, u32, bool),
) -> Option<()> {
    const MAX_ELEMENTS: u32 = 1 << D3D12_REQ_BUFFER_RESOURCE_TEXEL_COUNT_2_TO_EXP;
    if stride > 2048 {
        once!(
            stride,
            "a structured buffer view of a stride the host refuses: null"
        );
        return None;
    }
    if raw && !first.is_multiple_of(4) {
        // shortcut: the host wants raw views on 16 bytes; such a view is
        // made as before, counted (fxceless's VIctory fork's buffer_view_ok
        // refuses it).
        static OFF: AtomicU64 = AtomicU64::new(0);
        let n = OFF.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() {
            tracing::warn!(
                count = n,
                first,
                "a raw buffer view off 16 bytes of its host copy"
            );
        }
    }
    let typeless = crate::resource::family_members(format).is_some();
    if stride == 0 && !raw && typeless {
        once!(format, "a typeless buffer view without the raw flag: null");
        return None;
    }
    // SAFETY: a live buffer.
    let width = unsafe { copy.GetDesc() }.Width;
    let left = width.saturating_sub(first.saturating_mul(element)) / element;
    let fit = u64::from(*elements)
        .min(left)
        .min(u64::from(if stride == 0 { MAX_ELEMENTS } else { u32::MAX }));
    if fit < u64::from(*elements) {
        once!(
            elements = *elements,
            fit,
            "a buffer view past its host copy's end: cut to it"
        );
    }
    *elements = fit as u32;
    if *elements == 0 {
        once!("a buffer view of no elements in its host copy: null");
        return None;
    }
    Some(())
}

fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    viewed: Option<&mut Option<Viewed>>,
) -> Option<Kept> {
    let shadow = crate::shadow::find(address, len)?;
    let element = element_size(&desc);
    let format = desc.Format.0 as u32;
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let offset = shadow.offset(address);
    if element == 0 || offset % element != 0 {
        off_element(false, offset, element);
        return None;
    }
    buffer.FirstElement += offset / element;
    let raw = buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
    let shape = (buffer.StructureByteStride, format, raw);
    fit_view(
        &shadow.buffer,
        buffer.FirstElement,
        &mut buffer.NumElements,
        element,
        shape,
    )?;
    if let Some(v) = viewed {
        *v = Some(Viewed {
            buffer: shadow.buffer.clone(),
            start: buffer.FirstElement * element,
            len: u64::from(buffer.NumElements) * element,
            element,
            raw,
            unordered: false,
        });
    }
    repair_format(&mut desc.Format, shape);
    if shape.0 > 0 && xtp_shader::emit::raw_structured() {
        // SAFETY: a buffer view's union member.
        let b = unsafe { &mut desc.Anonymous.Buffer };
        if let Some((first, dwords)) =
            raw_window_of(b.FirstElement * element, u64::from(b.NumElements) * element)
        {
            (b.FirstElement, b.NumElements) = (first, dwords);
            b.StructureByteStride = 0;
            b.Flags = D3D12_BUFFER_SRV_FLAG_RAW;
            desc.Format = DXGI_FORMAT_R32_TYPELESS;
        }
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    Some(Some(shadow.buffer.clone()))
}

/// With `XTP_RAW_STRUCTURED=1` ([`xtp_shader::emit::raw_structured`]), a
/// structured view's bytes (`bytes` from byte `start` of its host copy) as
/// a raw view of the same bytes: its first dword and its dwords, when they
/// start on 16 bytes (a raw view's alignment), are whole dwords and at most
/// 2^27. On NVIDIA a raw read through a structured view returns other
/// dwords or zeros (watch.md items 148, 160, 190: the lighting's
/// pre-exposure read 0), and the translator then reads every buffer by
/// element through raw handles too. A view that cannot be made raw stays
/// structured (its raw reads keep the mismatch). The stride table still
/// gives such a view its own raw view and stride.
/// Ported from fxceless's VIctory fork (commit 578d459) by fxceless; see
/// also VIctory#1762.
fn raw_window_of(start: u64, bytes: u64) -> Option<(u64, u32)> {
    if !start.is_multiple_of(16) || !bytes.is_multiple_of(4) {
        return None;
    }
    let dwords = u32::try_from(bytes / 4).ok().filter(|&d| d <= 1 << 27)?;
    Some((start / 4, dwords))
}

/// The format the host wants for a buffer view of `shape` (stride, format,
/// raw): R32 typeless for a raw view, none for a structured one; the
/// title's otherwise. The console reads the same bytes whatever format a
/// raw or structured view names; the host refuses another (the checks of
/// fxceless's VIctory fork's buffer_view_ok, 578d459).
fn repair_format(format: &mut DXGI_FORMAT, (stride, _, raw): (u32, u32, bool)) {
    if raw {
        *format = DXGI_FORMAT_R32_TYPELESS;
    } else if stride > 0 {
        *format = DXGI_FORMAT_UNKNOWN;
    }
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; false if the copy cannot be
/// written by shaders (upload memory).
/// shortcut: what shaders write stays in the host copy; title memory (and
/// so the CPU, and copies run on the CPU) does not see it.
fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    viewed: Option<&mut Option<Viewed>>,
) -> Option<Kept> {
    let shadow = crate::shadow::find(address, len)?;
    let format = desc.Format.0 as u32;
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    let offset = shadow.offset(address);
    if element == 0 || offset % element != 0 {
        off_element(true, offset, element);
        return None;
    }
    buffer.FirstElement += offset / element;
    buffer.CounterOffsetInBytes = 0;
    let raw = buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
    let shape = (buffer.StructureByteStride, format, raw);
    fit_view(
        &shadow.buffer,
        buffer.FirstElement,
        &mut buffer.NumElements,
        element,
        shape,
    )?;
    if let Some(v) = viewed {
        *v = Some(Viewed {
            buffer: shadow.buffer.clone(),
            start: buffer.FirstElement * element,
            len: u64::from(buffer.NumElements) * element,
            element,
            raw,
            unordered: true,
        });
    }
    repair_format(&mut desc.Format, shape);
    if shape.0 > 0 && xtp_shader::emit::raw_structured() {
        // SAFETY: a buffer view's union member.
        let b = unsafe { &mut desc.Anonymous.Buffer };
        if let Some((first, dwords)) =
            raw_window_of(b.FirstElement * element, u64::from(b.NumElements) * element)
        {
            (b.FirstElement, b.NumElements) = (first, dwords);
            b.StructureByteStride = 0;
            b.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
            desc.Format = DXGI_FORMAT_R32_TYPELESS;
        }
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device.CreateUnorderedAccessView(
            &shadow.buffer,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
    Some(Some(shadow.buffer.clone()))
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in another format than a
/// typed resource's. A view of a typed texture in another format of its
/// family takes the texture's format.
/// shortcut: such a view (sRGB of UNORM, say) reads without its
/// conversion; cast with castable formats if colors come out wrong.
fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    if family(view) != family(own) {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && !typeless
    {
        *format = d.Format;
    }
    true
}

/// Makes the host descriptor for `view` at `at`, a slot of a heap of the
/// caller's (a CPU-only one: the second handle `ClearUnorderedAccessView*`
/// takes, which a shader-visible heap cannot give): the host resource it
/// views, or `None` if the host cannot make it (or it views none).
pub(crate) fn make_view(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<ID3D12Resource> {
    make(view, at).flatten()
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot.
fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    make_with(view, at, None)
}

/// [`make`], saying in `viewed` (if asked) what a buffer view reads.
fn make_with(
    view: &View,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    viewed: Option<&mut Option<Viewed>>,
) -> Option<Kept> {
    let host = host()?;
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            let shadow = crate::shadow::find(address, u64::from(size))?;
            let gpu = shadow.gpu(address);
            if gpu % 256 != 0 {
                once!(
                    address = format_args!("{address:#x}"),
                    "constant buffer not 256-byte aligned on the host"
                );
                return None;
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: size.next_multiple_of(256),
            };
            // SAFETY: a slot of our heap; the range lies in the host copy.
            unsafe { device.CreateConstantBufferView(Some(&desc), at) };
            Some(Some(shadow.buffer.clone()))
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(texture) = crate::resource::host_resource(resource) {
                let mut desc = desc.as_ref().map(srv_desc);
                if !fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return None;
                }
                let desc = crate::resource::view_alpha(resource, desc);
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some(Some(texture.clone()))
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                buffer_srv(data as u64, len, srv_desc(desc), at, viewed)
            } else {
                None
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(texture) = crate::resource::host_resource(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(data as u64, len, uav_desc(desc), at, viewed)
                    }
                    _ => None,
                };
            };
            let mut desc = desc.as_ref().map(uav_desc);
            if !fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return None;
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some(Some(texture.clone()))
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(view) = placed_texture(view) {
                return make(&view, at);
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                // shortcut: unordered access to a texture the title placed
                // itself is not mirrored; give it a resource as shader
                // views of such textures get when a dispatch writes one.
                once!(
                    dimension = resource[0],
                    "placed unordered texture view not mirrored"
                );
                return None;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at, viewed)
            } else {
                buffer_srv(address, len, srv_desc(words), at, viewed)
            }
        }
    }
}

/// The title memory whose host copy [`make`] needs for `view` (address and
/// length, as it asks [`crate::shadow::find`]): constant buffers and
/// buffer views.
fn copy_range(view: &View) -> Option<(u64, u64)> {
    match view {
        &View::Constant(address, size) => Some((address, u64::from(size))),
        View::Shader(r, Some(_)) | View::Unordered(r, _, Some(_)) => {
            crate::resource::buffer_bytes(*r as crate::object::This)
                .map(|(data, len)| (data as u64, len))
        }
        View::Placed(_, resource, desc) if resource[0] == 1 => Some((
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            u64::from(resource[4]) | u64::from(resource[5]) << 32,
        )),
        _ => None,
    }
}

/// A shader view of a texture the title placed in memory itself, as a view
/// of the resource made for that memory (on first use).
pub(crate) fn placed_texture(view: &View) -> Option<View> {
    const BUFFER: u32 = 1;
    let View::Placed(false, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let texture = crate::resource::texture_at(address, resource)?;
    Some(View::Shader(texture as usize, desc[..10].try_into().ok()))
}

/// A null view of the kind `view` is, for a slot whose view the host
/// cannot make: unordered access (to a buffer or a 2D texture) or a shader
/// view. Writes to it are dropped, reads give zeros.
fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    // Unordered views: whether a buffer, and the title's view dimension.
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        // Shader views of buffers read nothing through a null buffer view:
        // a null 2D view where the shader expects a buffer is a mismatch
        // the hardware does not survive in general (GPU-based validation
        // reports it).
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        // Textures: a null view of the title's dimension (1D, arrays, 3D,
        // cubes), not always 2D.
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        View::Constant(..) => {
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
            // SAFETY: a slot of our heap; a zero location makes a null view.
            return unsafe { host.device.CreateConstantBufferView(Some(&desc), at) };
        }
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A null texture UAV of the title's view dimension (1D, 1D or 2D arrays,
/// 3D), 2D if it names none: a 2D null view where the shader writes a 3D
/// texture is a mismatch GPU-based validation reports (the world's wind
/// field, CS_CopyWindFieldBeforeUpdateWithSmooth, 2026-11-24).
fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

/// A buffer view that reads zeros (raw, as translated shaders mostly read
/// buffers).
fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A texture view that reads zeros with the dimension (and mip and array
/// counts) of the title's description `words`; 2D if it names none.
fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    // One mip, one slice or cube, plane 0: fields a null view still checks.
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A view that reads zeros, for slots whose view the host cannot make.
/// shortcut: slots with no view at all (kind unknown) get this null 2D
/// view; null buffer views there did not stop the world's low-address
/// page faults (2026-11-24, run w10), so they are not the cause.
fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A structured view is made raw only where its bytes start on 16
    /// bytes, are whole dwords and fit one view.
    #[test]
    fn structured_views_go_raw_on_16_bytes() {
        assert_eq!(raw_window_of(32, 64), Some((8, 16)));
        assert_eq!(raw_window_of(0, 16 * 456), Some((0, 4 * 456)));
        assert_eq!(raw_window_of(36, 64), None, "off 16 bytes");
        assert_eq!(raw_window_of(32, 62), None, "not whole dwords");
        assert_eq!(raw_window_of(0, 4 << 27), Some((0, 1 << 27)));
        assert_eq!(raw_window_of(0, (4 << 27) + 4), None, "too many dwords");
    }

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    /// A capped view heap keeps 2^18 of its slots dynamic and the rest for
    /// the title; small limits keep a title slot and never underflow
    /// (VIctory #1349).
    #[test]
    fn capped_view_heaps_split_their_slots() {
        assert_eq!(capped_split(PORTABLE_VIEWS), (737_856, 1 << 18));
        assert_eq!(capped_split(1024), (1, 1023));
        assert_eq!(capped_split(1), (1, 0));
        assert_eq!(capped_split(0), (0, 0));
    }

    /// A view heap of BTP 6's size, past the 1,000,000 slots many hosts
    /// cap at: capped, it has 2^18 dynamic slots, all free at first; a host
    /// that takes it whole keeps every title slot.
    #[test]
    fn a_heap_of_the_titles_size_keeps_its_dynamic_slots() {
        const TITLE: u32 = 1_245_184;
        let Some(heap) = HostHeap::new(false, TITLE) else {
            return; // no host GPU
        };
        if heap.count() < TITLE {
            assert_eq!((heap.count(), heap.dynamic), capped_split(PORTABLE_VIEWS));
        } else {
            assert!(heap.dynamic >= MIN_DYNAMIC);
        }
        let first = heap.dynamic(heap.dynamic).expect("every dynamic slot free");
        assert_eq!(first, heap.count());
        heap.release(first, heap.dynamic);
    }

    /// The peak of dynamic slots in use is kept, and ranges go back in one
    /// batch.
    #[test]
    fn dynamic_slots_count_their_peak_and_go_back_in_a_batch() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let a = heap.dynamic(10).unwrap();
        let b = heap.dynamic(1).unwrap();
        let c = heap.dynamic(5).unwrap();
        assert_eq!(heap.dynamic_use(), (16, 16));
        heap.release_all(&[(a, 10), (c, 5)]);
        assert_eq!(heap.dynamic_use(), (1, 16));
        // Given back twice, or past the end: counted once.
        heap.release_all(&[(b, 1), (b, 1), (4 + heap.dynamic - 1, 5)]);
        assert_eq!(heap.dynamic_use(), (0, 16));
        assert_eq!(
            heap.dynamic(16),
            Some(c + 5),
            "the search goes on from the last"
        );
    }

    /// A shared raw view is made once per range and host copy; views no
    /// list holds stay for later lists and give their slots back when a
    /// table needs room.
    #[test]
    fn shared_views_are_made_once_and_make_room_when_slots_run_out() {
        use crate::testgpu::TitleBuffer;
        let test =
            "descriptor::host::tests::shared_views_are_made_once_and_make_room_when_slots_run_out";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let heap = HostHeap::new(false, 4).expect("a host view heap");
        let buffer = TitleBuffer::new(1 << 16);
        let copy = buffer.shadow();
        let a = heap.shared_raw(&copy, 0, 64).unwrap();
        assert_eq!(heap.shared_raw(&copy, 0, 64), Some(a), "the same range");
        let b = heap.shared_raw(&copy, 16, 48).unwrap();
        assert_ne!(a, b);
        assert_eq!(heap.shared_count(), (2, 0));
        // One chunk taken for both.
        assert_eq!(heap.dynamic_use().0, SHARED_CHUNK);
        heap.give_back(&[a, b]);
        assert_eq!(heap.shared_count(), (2, 1), "a is held once more");
        heap.give_back(&[a]);
        assert_eq!(heap.shared_count(), (2, 2));
        assert_eq!(
            heap.shared_raw(&copy, 0, 64),
            Some(a),
            "kept for later lists"
        );
        heap.give_back(&[a]);
        // Every other slot taken: the next table takes the idle views' chunk.
        let rest = heap.dynamic(heap.dynamic - SHARED_CHUNK).unwrap();
        assert_eq!(heap.dynamic(SHARED_CHUNK), Some(4));
        assert_eq!(heap.shared_count(), (0, 0));
        assert_eq!(heap.dynamic_use().0, heap.dynamic);
        heap.release_all(&[(rest, heap.dynamic - SHARED_CHUNK), (4, SHARED_CHUNK)]);
        // A view a list holds is never taken back.
        let held = heap.shared_raw(&copy, 0, 64).unwrap();
        while heap.take(SHARED_CHUNK).is_some() {}
        assert_eq!(heap.dynamic(SHARED_CHUNK), None);
        assert_eq!(heap.shared_count(), (1, 0));
        heap.give_back(&[held]);
    }

    /// A buffer view's slot is made, for a draw, over its buffer's host
    /// copy, made first (outside the slot lock) when it is not yet; not
    /// for a mere view creation.
    #[test]
    fn buffer_slots_get_their_host_copy_made_first() {
        use crate::testgpu::TitleBuffer;
        let test = "descriptor::host::tests::buffer_slots_get_their_host_copy_made_first";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let heap = crate::descriptor::TestHeap::views(4);
        let host = crate::descriptor::heap(heap.0).host().expect("a host heap");
        let buffer = TitleBuffer::new(1 << 16);
        let address = buffer.address();
        assert!(
            crate::shadow::existing(address, 256).is_none(),
            "no copy yet"
        );
        let handle = crate::descriptor::heap(heap.0).start + 2 * 32;
        let words = [address as u32, (address >> 32) as u32, 256, 0];
        crate::descriptor::create_cbv(std::ptr::null(), &words, handle);
        assert!(
            crate::shadow::existing(address, 256).is_none(),
            "views wait for a draw"
        );
        assert_eq!(
            host.mirror(2, handle, true),
            Some(View::Constant(address, 256))
        );
        let copy = crate::shadow::existing(address, 256).expect("made for the draw");
        let kept = host.kept.lock().unwrap_or_else(|e| e.into_inner())[2]
            .as_ref()
            .map(|b| b.as_raw() as usize);
        assert_eq!(kept, Some(copy.buffer.as_raw() as usize));
    }

    /// A slot viewing a texture the title placed itself follows the
    /// resource made for that memory: when a texture the GPU writes comes
    /// there (#643), the slot is made again over its copy, although the
    /// title's record has not changed.
    #[test]
    fn placed_texture_slots_follow_their_replacement() {
        use crate::object::{S_OK, This, guid, slot};
        use crate::resource::tests_support::target;
        let test = "descriptor::host::tests::placed_texture_slots_follow_their_replacement";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        const KIB_64: u64 = 1 << 16;
        // Freed only once nothing is placed in it any more (a failing
        // assertion leaks it rather than leave textures over freed memory).
        let memory =
            std::mem::ManuallyDrop::new(crate::resource::Memory::zeroed(KIB_64, KIB_64).unwrap());
        let address = memory.address();
        // A 16x16 RGBA8 texture placed there, and a shader view of it.
        let drawn = target(16, 16, 28);
        // SAFETY: a description is 56 bytes, 14 words.
        let mut resource: [u32; 14] = unsafe { std::mem::transmute_copy(&drawn) };
        resource[12] = 0; // not a render target: the title fills it
        let mut view = [0u32; 16];
        // Format, 2D, the default mapping, then (from word 4, 8-byte
        // aligned) the first mip and the mip count.
        view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        (view[10], view[11]) = (address as u32, (address >> 32) as u32);
        let heap = crate::descriptor::TestHeap::views(4);
        let host = crate::descriptor::heap(heap.0).host().expect("a host heap");
        let handle = crate::descriptor::heap(heap.0).start + 32;
        crate::descriptor::create_placed_srv(std::ptr::null(), &resource, &view, handle);
        let kept = |h: &HostHeap| {
            h.kept.lock().unwrap_or_else(|e| e.into_inner())[1]
                .as_ref()
                .map(|t| t.as_raw() as usize)
        };
        let host_of = |r: This| crate::resource::host_resource(r).map(|t| t.as_raw() as usize);
        assert!(matches!(
            host.mirror(1, handle, true),
            Some(View::Shader(..))
        ));
        let before = crate::resource::texture_at(address, &resource).unwrap();
        // Taken now: once replaced, `before` may be released at any time
        // (and its address reused); its copy stays while this holds it.
        let old = crate::resource::host_copy(before).expect("a host copy");
        let old_texture = Some(old.texture.as_raw() as usize);
        assert_eq!(kept(host), old_texture);
        // A render target comes to the same memory: the view's texture is
        // replaced by one over the target's copy.
        const RESOURCE: crate::object::Iid = guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        let mut rt = std::ptr::null();
        let made = crate::resource::create_at_address(
            std::ptr::null(),
            address,
            &drawn,
            0,
            std::ptr::null(),
            &RESOURCE,
            &mut rt,
        );
        assert_eq!(made, S_OK);
        assert!(matches!(
            host.mirror(1, handle, true),
            Some(View::Shader(..))
        ));
        let after = crate::resource::texture_at(address, &resource).unwrap();
        assert_eq!(
            host_of(after),
            host_of(rt),
            "texture_at took the target's copy"
        );
        assert_ne!(host_of(after), old_texture);
        assert_eq!(kept(host), host_of(after), "the slot views the replacement");
        // Nothing more changes: the slot stays as it is.
        host.mirror(1, handle, true);
        assert_eq!(kept(host), host_of(after));
        assert_eq!(slot::<extern "system" fn(This) -> u32>(rt, 0x10)(rt), 0);
        drop(heap);
        crate::resource::memory_remapped(address as usize, KIB_64 as usize);
        drop(std::mem::ManuallyDrop::into_inner(memory));
    }

    /// A buffer view of title buffer `r` as the title describes one:
    /// `count` elements from element `first`, in `format` (0 with a
    /// `stride` for a structured view; typeless, 39, with the raw flag for
    /// a raw one); unordered access or a shader resource view.
    fn buffer_view(
        unordered: bool,
        r: &crate::testgpu::TitleBuffer,
        (format, stride, raw): (u32, u32, bool),
        (first, count): (u32, u32),
    ) -> View {
        let mut w = [0u32; 10];
        (w[0], w[1]) = (format, 1);
        let flags = u32::from(raw);
        if unordered {
            // First element (64 bits), count, stride, counter offset, flags.
            (w[2], w[4], w[5], w[8]) = (first, count, stride, flags);
            View::Unordered(r.resource() as usize, 0, Some(w))
        } else {
            // The default mapping, first element, count, stride, flags.
            w[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
            (w[4], w[6], w[7], w[8]) = (first, count, stride, flags);
            View::Shader(r.resource() as usize, Some(w))
        }
    }

    const RAW: (u32, u32, bool) = (39, 0, true);
    const fn structured(stride: u32) -> (u32, u32, bool) {
        (0, stride, false)
    }

    /// A view heap with a runtime stride table: an entry per slot, written
    /// with the slot's descriptor. A raw view is read in its own slot; a
    /// structured or typed one through a raw view of the same bytes in the
    /// pool, which starts up to 12 bytes early; a null or constant view,
    /// or a slot given back, holds none.
    #[test]
    fn stride_table_entries_follow_the_views() {
        use crate::testgpu::TitleBuffer;
        let test = "descriptor::host::tests::stride_table_entries_follow_the_views";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let heap = HostHeap::new_with(false, 8, true).expect("a host view heap");
        let none = HostHeap::new(false, 8).expect("a host view heap");
        assert!(heap.stride_table().is_some() && none.stride_table().is_none());
        let buffer = TitleBuffer::new(1 << 12);
        let pool = heap.alias_base();
        let start = buffer.shadow().offset(buffer.address());
        let bias = |byte: u64| ((start + byte) % 16) as u32;
        let entry = |stride, raw_slot, bias, count| StrideEntry {
            stride,
            raw_slot: Some(raw_slot),
            bias,
            count,
        };
        heap.make_at(0, &buffer_view(false, &buffer, RAW, (0, 64)));
        assert_eq!(heap.entry(0), entry(0, 0, 0, 64), "raw: its own slot");
        heap.make_at(1, &buffer_view(false, &buffer, structured(12), (3, 5)));
        assert_eq!(heap.entry(1), entry(12, pool, bias(36), 5));
        heap.make_at(2, &buffer_view(true, &buffer, structured(20), (1, 4)));
        assert_eq!(heap.entry(2), entry(20, pool + 1, bias(20), 4));
        // Typed RGBA32F: 16-byte elements.
        heap.make_at(3, &buffer_view(false, &buffer, (2, 0, false), (2, 8)));
        assert_eq!(heap.entry(3), entry(16, pool + 2, bias(32), 8));
        // The same view again keeps its raw view.
        heap.make_at(1, &buffer_view(false, &buffer, structured(12), (3, 5)));
        assert_eq!(heap.entry(1), entry(12, pool, bias(36), 5));
        // Nothing to read: null, a constant view.
        heap.make_null(2);
        assert_eq!(heap.entry(2), StrideEntry::NONE);
        heap.constant_at(3, buffer.shadow().gpu(buffer.address()), 256);
        assert_eq!(heap.entry(3), StrideEntry::NONE);
        // A dynamic slot given back loses its entry before it is free.
        let first = heap.dynamic(2).expect("dynamic slots");
        heap.make_at(
            first + 1,
            &buffer_view(false, &buffer, structured(16), (0, 4)),
        );
        assert!(heap.entry(first + 1).raw_slot.is_some());
        heap.release(first, 2);
        assert_eq!(heap.entry(first + 1), StrideEntry::NONE);
    }

    /// A slot the title rewrites while a list that read the table has not
    /// run: its entry names a new raw view at once (one 64-bit store), and
    /// the old raw view is not made again for another slot until that
    /// list's hold has gone; then it is.
    #[test]
    fn raw_views_a_list_in_flight_may_read_are_kept_until_it_has_run() {
        use crate::testgpu::TitleBuffer;
        let test = "descriptor::host::tests::raw_views_a_list_in_flight_may_read_are_kept_until_it_has_run";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let heap = HostHeap::new_with(false, 8, true).expect("a host view heap");
        let buffer = TitleBuffer::new(1 << 12);
        let pool = heap.alias_base();
        let raw_slot = |i| heap.entry(i).raw_slot.expect("a raw view");
        heap.make_at(0, &buffer_view(false, &buffer, structured(12), (0, 8)));
        assert_eq!(raw_slot(0), pool);
        // A list reads the table, and has not run.
        let in_flight = heap.stride_hold().expect("a table");
        heap.make_at(0, &buffer_view(false, &buffer, structured(16), (0, 8)));
        assert_eq!(raw_slot(0), pool + 1, "a new raw view for the new view");
        heap.make_null(0);
        heap.make_at(1, &buffer_view(false, &buffer, structured(20), (0, 8)));
        assert_eq!(raw_slot(1), pool + 2, "the old ones are not reused");
        // A list that started after: its hold does not keep them.
        let later = heap.stride_hold().expect("a table");
        drop(in_flight);
        heap.make_at(2, &buffer_view(false, &buffer, structured(20), (1, 6)));
        heap.make_at(3, &buffer_view(false, &buffer, structured(20), (2, 4)));
        let mut reused = [raw_slot(2), raw_slot(3)];
        reused.sort_unstable();
        assert_eq!(reused, [pool, pool + 1], "given back once the list has run");
        drop(later);
    }

    /// A view past its host copy's end is cut to it (the host refuses, and
    /// may remove the device for, a view past its resource), one of no
    /// elements there or of a stride the host has no structured buffers for
    /// is null; the entry counts what the cut view holds.
    #[test]
    fn buffer_views_are_cut_to_their_host_copy() {
        use crate::testgpu::TitleBuffer;
        let test = "descriptor::host::tests::buffer_views_are_cut_to_their_host_copy";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        let heap = HostHeap::new_with(false, 8, true).expect("a host view heap");
        let buffer = TitleBuffer::new(1 << 16);
        let copy = buffer.shadow();
        // SAFETY: a live buffer.
        let width = unsafe { copy.buffer.GetDesc() }.Width;
        let first = (width - copy.offset(buffer.address())) / 16 - 2;
        // Two elements of 16 bytes are left from `first`.
        assert!(heap.make_at(
            0,
            &buffer_view(false, &buffer, structured(16), (first as u32, 100))
        ));
        assert_eq!(heap.entry(0).count, 2);
        let past = (width / 16) as u32;
        assert!(!heap.make_at(1, &buffer_view(false, &buffer, structured(16), (past, 4))));
        assert_eq!(heap.entry(1), StrideEntry::NONE);
        assert!(!heap.make_at(2, &buffer_view(false, &buffer, structured(4100), (0, 1))));
        assert!(!heap.make_at(3, &buffer_view(false, &buffer, structured(16), (0, 0))));
        // A typeless view without the raw flag is no view the host makes.
        assert!(!heap.make_at(4, &buffer_view(false, &buffer, (39, 0, false), (0, 4))));
    }

    /// `xtp-shader`'s stride tests' program, translated for a target with
    /// the table: it loads `[2, 4]` of the view in table slot 0 (no stride
    /// known), stores it at `[1, 8]` of the unordered view in slot 1 and
    /// adds 10 twice at `[2, 0]` there.
    fn unknown_stride_program() -> xtp_shader::emit::HostShader {
        use xtp_shader::native::{ComputeIo, StageIo};
        use xtp_shader::{HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource};
        let words: &[u32] = &[
            0xBE85_0380, // s_mov_b32 s5, 0
            0xF408_0202,
            0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
            0xF408_0302,
            0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20
            0x7E00_0282, // v_mov_b32 v0, 2
            0x7E02_0284, // v_mov_b32 v1, 4
            0xE030_3000,
            0x8002_0200, // buffer_load_dword v2, v[0:1], s[8:11], 0 idxen offen
            0x7E00_0281, // v_mov_b32 v0, 1
            0x7E02_0288, // v_mov_b32 v1, 8
            0xE070_3000,
            0x8003_0200, // buffer_store_dword v2, v[0:1], s[12:15], 0 idxen offen
            0x7E00_0282, // v_mov_b32 v0, 2
            0x7E02_0280, // v_mov_b32 v1, 0
            0x7E06_028A, // v_mov_b32 v3, 10
            0xE0C8_3000,
            0x8003_0300, // buffer_atomic_add v3, v[0:1], s[12:15], 0 idxen offen
            0xE0C8_3000,
            0x8003_0300, // buffer_atomic_add v3, v[0:1], s[12:15], 0 idxen offen
            0xBF81_0000, // s_endpgm
        ];
        let used = |kind, table_offset_dwords| UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        };
        let param = |root, sgprs, user_data, slot| RootParam {
            root,
            table_offset_dwords: 0,
            sgprs,
            base_register: 0,
            user_data,
            slot,
        };
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: false,
            vgprs: 4,
            layout: vec![
                param(RootType::Constants, 4, 0, 0),
                param(RootType::Table, 1, 4, 1),
            ],
            used: vec![used(ResourceType::Srv, 0), used(ResourceType::Uav, 8)],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 5 << 1,
                threads: [1, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = xtp_shader::ir::build(&native.code, native.wave64, native.vgprs).unwrap();
        let st = xtp_shader::structure::structure(&p).unwrap();
        let res = xtp_shader::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = xtp_shader::io::raise(&p, &native).unwrap();
        // An empty container (no signature parts): the tag, a digest,
        // version 1, its size, no parts.
        let mut container = b"DXBC".to_vec();
        container.extend([0; 16]);
        container.extend([1u32, 32, 0].into_iter().flat_map(u32::to_le_bytes));
        let c = xtp_shader::Container::parse(&container).unwrap();
        let target = xtp_shader::emit::Target {
            stride_table: true,
            ..xtp_shader::emit::Target::ANY
        };
        let cs = xtp_shader::emit::emit_for(&c, &native, &p, &st, &io, &res, target).unwrap();
        assert!(cs.stride_table);
        cs
    }

    /// The root signature and pipeline of `cs` as `pipeline::host` makes
    /// them: user data as root constants (parameter 0), the stride table as
    /// a root SRV (parameter 1), the heaps indexed directly.
    fn table_pipeline(
        device: &ID3D12Device,
        cs: &xtp_shader::emit::HostShader,
    ) -> (ID3D12RootSignature, ID3D12PipelineState) {
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let params = [
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 0,
                        RegisterSpace: xtp_shader::emit::USER_DATA_SPACE,
                        Num32BitValues: cs.user_data_dwords(),
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                        ShaderRegister: 0,
                        RegisterSpace: xtp_shader::emit::STRIDE_SPACE,
                        Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
        ];
        let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                    NumParameters: params.len() as u32,
                    pParameters: params.as_ptr(),
                    NumStaticSamplers: 0,
                    pStaticSamplers: std::ptr::null(),
                    Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                        | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
                },
            },
        };
        // SAFETY: the descriptions point at live data; the blob is read
        // before it goes.
        unsafe {
            let mut blob: Option<ID3DBlob> = None;
            D3D12SerializeVersionedRootSignature(&desc, &mut blob, None).unwrap();
            let blob = blob.unwrap();
            let bytes = std::slice::from_raw_parts(
                blob.GetBufferPointer().cast::<u8>(),
                blob.GetBufferSize(),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).unwrap();
            let pso = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: cs.dxil.as_ptr().cast(),
                        BytecodeLength: cs.dxil.len(),
                    },
                    ..Default::default()
                })
                .unwrap();
            (root, pso)
        }
    }

    /// A translated program reads and writes title buffers through a real
    /// host heap's stride table, whatever view of 4, 12, 16 or 20 bytes an
    /// element (structured, typed, also from an element off 16 bytes of the
    /// copy) or raw the title put in the slots: the bytes the console
    /// reads and writes, the next element never (with 4-byte elements the
    /// load at 4 and the store at 8 are past the element: 0, dropped).
    #[test]
    fn a_translated_program_reads_and_writes_through_the_heaps_table() {
        use crate::testgpu::{TitleBuffer, TitleList};
        let test = "descriptor::host::tests::a_translated_program_reads_and_writes_through_the_heaps_table";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let heap = HostHeap::new_with(false, 8, true).expect("a host view heap");
        let cs = unknown_stride_program();
        let (root, pso) = table_pipeline(&host.device, &cs);
        let (table, _) = heap.stride_table().expect("a table");
        let (src, dst) = (TitleBuffer::new(1 << 12), TitleBuffer::new(1 << 12));
        src.set_words(|i| 100 + i as u32);
        let data = src.words();
        for (kind, shape, stride, first, count) in [
            ("structured 4", structured(4), 4, 0, 64),
            ("structured 12", structured(12), 12, 0, 21),
            ("structured 16", structured(16), 16, 0, 16),
            ("structured 20", structured(20), 20, 0, 12),
            ("typed RGBA32F", (2, 0, false), 16, 0, 16),
            ("structured 12 from element 3", structured(12), 12, 3, 10),
            ("structured 20 from element 3", structured(20), 20, 3, 6),
            ("raw", RAW, 0, 0, 64),
        ] {
            dst.set_words(|_| 0);
            heap.make_at(0, &buffer_view(false, &src, shape, (first, count)));
            heap.make_at(1, &buffer_view(true, &dst, shape, (first, count)));
            assert!(heap.entry(0).count > 0 && heap.entry(1).count > 0, "{kind}");
            let list = TitleList::new();
            let l = list.host();
            let (from, to) = (src.shadow(), dst.shadow());
            from.refresh(l, src.address(), 1 << 12);
            to.refresh(l, dst.address(), 1 << 12);
            to.gpu_writes(l, dst.address(), 1 << 12);
            l.keep(&root);
            l.keep(&pso);
            let zeros = vec![0u32; cs.user_data_dwords() as usize];
            // SAFETY: an open list with every root argument set; the heap
            // and its table outlive the run.
            unsafe {
                l.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                l.list.SetComputeRootSignature(&root);
                l.list.SetPipelineState(&pso);
                l.list.SetComputeRoot32BitConstants(
                    0,
                    zeros.len() as u32,
                    zeros.as_ptr().cast(),
                    0,
                );
                l.list.SetComputeRootShaderResourceView(1, table);
                l.list.Dispatch(1, 1, 1);
            }
            list.run();
            let out = dst.words();
            let at = |index: u32, off: u32| (((first + index) * stride + off) / 4) as usize;
            let inside = |off: u32| stride == 0 || off + 4 <= stride;
            let mut want = vec![0; 1 << 10];
            if inside(8) {
                want[at(1, 8)] = if inside(4) { data[at(2, 4)] } else { 0 };
            }
            want[at(2, 0)] = 20;
            assert_eq!(out[..64], want[..64], "{kind}");
            assert!(out[64..].iter().all(|&w| w == 0), "{kind}: nothing else");
        }
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// A root SRV's one-view table (VIctory #493): a shader reading 64
    /// words through a raw view of 30 bytes from byte 16 of a host copy
    /// gets words 4 to 11 (the partial last one whole) and zeros after
    /// them, although the copy holds data there.
    #[test]
    fn root_srv_views_read_zeros_past_their_end() {
        use crate::testgpu::{TitleBuffer, TitleList, pattern};
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        use windows::core::s;
        const SOURCE: &str = r#"
#define RS "DescriptorTable(SRV(t0)), UAV(u0)"
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    dst.Store(id.x * 4, src.Load(id.x * 4));
}
"#;
        let test = "descriptor::host::tests::root_srv_views_read_zeros_past_their_end";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let heap = HostHeap::new(false, 4).expect("a host view heap");
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call.
        unsafe {
            D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("root srv test"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )
        }
        .expect("the test kernel compiles");
        let code = code.expect("compiled code");
        // SAFETY: the blob's own bytes, which hold the root signature.
        let bytes = unsafe {
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
        };
        // SAFETY: plain object creation from live bytes.
        let (root, pso) = unsafe {
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes).unwrap();
            let pso: ID3D12PipelineState = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr().cast(),
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                })
                .unwrap();
            (root, pso)
        };

        let src = TitleBuffer::new(1 << 16);
        src.set_words(pattern);
        let dst = TitleBuffer::new(1 << 16);
        dst.set_words(|_| 0xdead_beef);
        let (from, to) = (src.shadow(), dst.shadow());
        let slot = heap.dynamic(1).expect("a dynamic slot");
        heap.raw_at(slot, &from.buffer, from.offset(src.address()) + 16, 30);

        let list = TitleList::new();
        let l = list.host();
        from.refresh(l, src.address(), 256);
        to.refresh(l, dst.address(), 256);
        to.gpu_writes(l, dst.address(), 256);
        l.keep(&root);
        l.keep(&pso);
        // SAFETY: an open list; live objects kept until it is reset; the
        // slot is in the heap set on it.
        unsafe {
            l.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
            l.list.SetComputeRootSignature(&root);
            l.list.SetPipelineState(&pso);
            l.list
                .SetComputeRootDescriptorTable(0, heap.gpu_handle(slot));
            l.list
                .SetComputeRootUnorderedAccessView(1, to.gpu(dst.address()));
            l.list.Dispatch(1, 1, 1);
        }
        list.run();
        let words = dst.words();
        let wanted: Vec<u32> = (0..64)
            .map(|i| if i < 8 { pattern(i + 4) } else { 0 })
            .collect();
        assert_eq!(words[..64], wanted[..]);
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }
}
