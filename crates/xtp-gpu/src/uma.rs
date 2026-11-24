//! Title memory as GPU heaps, in place (backlog G15b;
//! `docs/analysis/uma-feasibility.md` section 8).
//!
//! The host opens committed process memory as a D3D12 heap:
//! `OpenExistingHeapFromAddress1` over any committed range, or
//! `OpenExistingHeapFromAddress` over a whole, fully committed allocation
//! on a device without `ID3D12Device13`. A buffer placed over all of a heap
//! then shows the GPU the title's bytes where the title keeps them: the CPU
//! and the GPU see each other's writes with no copy and no shadow.
//!
//! This is the registry of those heaps, kept in step with xmem
//! (`xtp-hle`), which calls in:
//! - [`reserved`]: a reservation of the title's (`XMemVirtualAlloc`). Its
//!   heaps sit at the title's own addresses.
//! - [`mapped`]: the runtime's permanent view of a physical-page section
//!   (`docs/analysis/formats.md` F3). Heaps over physical pages sit there,
//!   never on the title's views: an open heap blocks unmapping the view it
//!   is on, and the title remaps its views all the time. The heap is on the
//!   section's pages, so every view of them sees what the GPU writes.
//! - [`committed`]: memory of those was committed. Heaps follow, opened on
//!   a thread of their own: 0.05–0.55 s a GiB is no cost for the title's
//!   call to bear.
//! - [`changing`], before xmem decommits, releases or unmaps memory: the
//!   heaps over it are released once the GPU is done with them, since the
//!   host refuses those calls under an open heap (error 87). The call goes
//!   through [`Change::call`], which gives the host the moment it takes to
//!   let go of the released pages. Then [`Change::done`]: what is still
//!   committed is opened again.
//!
//! [`lookup`] turns a title address into a heap's buffer and the offset in
//! it; [`used`] says which GPU work uses a heap, so that releasing it waits
//! for that work. Nothing renders from these heaps yet (G16 on).
//!
//! Imported memory is locked RAM: the heaps' total stays under a cap
//! (`XTP_UMA_CAP_MIB`, by default a quarter of the machine's memory), and
//! what does not fit stays on the shadow path. All of this is off unless
//! `XTP_UMA=1`; `XTP_UMA_WHOLE=1` opens heaps as on a device without
//! `ID3D12Device13`.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, GetLastError};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
};

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;
const HOST_PAGE: usize = 4 * KIB;

/// The most a heap over existing memory holds: the host refuses 4 GiB,
/// with every call (uma-feasibility section 4).
pub const MAX: usize = (4 << 30) - 64 * KIB;

/// What a region of title memory is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The title's virtual memory, at the title's addresses.
    Virtual,
    /// A physical-page section, at the runtime's permanent view of it.
    Physical,
}

impl Kind {
    /// Heaps start and end on multiples of this from their region's start,
    /// or at its end.
    fn grain(self) -> usize {
        match self {
            // The host's allocation granularity; the spike opened heaps at
            // such interior addresses.
            Kind::Virtual => 64 * KIB,
            // Sections are committed 64 KiB at a time, as DirectStorage
            // first writes their pages: blocks of 64 MiB, opened once all
            // committed, keep their heaps few (98 for Grand Theft Auto VI's
            // 6.1 GiB) where following each commit would make thousands.
            Kind::Physical => 64 * MIB,
        }
    }
}

/// How a registry opens heaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Bytes under open heaps at most; the rest stays on the shadow path.
    pub cap: usize,
    /// Whole, fully committed allocations only, opened with
    /// `OpenExistingHeapFromAddress`: as on a device without
    /// `ID3D12Device13`.
    pub whole_allocations: bool,
}

impl Options {
    /// As the environment says: `XTP_UMA_CAP_MIB` (default: a quarter of
    /// the machine's memory), `XTP_UMA_WHOLE=1`.
    pub fn from_env() -> Options {
        let cap = std::env::var("XTP_UMA_CAP_MIB")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .map_or_else(default_cap, |mib| mib.saturating_mul(MIB));
        Options {
            cap,
            whole_allocations: std::env::var("XTP_UMA_WHOLE").is_ok_and(|v| v == "1"),
        }
    }
}

/// A quarter of the machine's memory: importing 10–13 GB is fine with
/// 64 GB of RAM, not with 16 (uma-feasibility section 9).
fn default_cap() -> usize {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: a sized structure of ours.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return 4 << 30;
    }
    usize::try_from(status.ullTotalPhys / 4).unwrap_or(usize::MAX)
}

/// Where title bytes are for the GPU: the buffer over the heap that holds
/// them, and their offset in it.
///
/// For recording only: the buffer holds its heap open, so while a `Placed`
/// lives the title cannot decommit or unmap that memory (error 87), the
/// registry's release or not. Drop it once the work is recorded and named
/// with [`Registry::used`]: the registry keeps the heap until that work is
/// done.
#[derive(Clone, Debug)]
pub struct Placed {
    pub buffer: ID3D12Resource,
    pub offset: u64,
    /// The bytes' GPU address: the buffer's plus `offset`.
    pub gpu: u64,
}

/// A heap over title memory and the buffer placed over all of it.
struct Heap {
    len: usize,
    _heap: ID3D12Heap,
    buffer: ID3D12Resource,
    gpu: u64,
    /// GPU work that used it: each fence, and the value it reaches once
    /// that work is done.
    uses: Vec<(ID3D12Fence, u64)>,
}

impl Heap {
    fn note(&mut self, fence: &ID3D12Fence, value: u64) {
        match self
            .uses
            .iter_mut()
            .find(|(f, _)| f.as_raw() == fence.as_raw())
        {
            Some((_, last)) => *last = (*last).max(value),
            None => self.uses.push((fence.clone(), value)),
        }
    }
}

/// Title memory the registry follows: an allocation, or a permanent view.
struct Region {
    len: usize,
    kind: Kind,
    /// Open heaps by start address; they never overlap.
    heaps: BTreeMap<usize, Heap>,
    /// Committed memory may have no heap yet: the opener looks again.
    dirty: bool,
    /// The opener is opening a heap in it, out of the lock.
    opening: bool,
    /// Calls changing its memory under way ([`Registry::changing`]).
    changing: usize,
    /// Spans that failed to open, not tried again until the memory changes.
    failed: Vec<(usize, usize)>,
    /// A span did not fit under the cap; looked at again when heaps go.
    starved: bool,
}

impl Region {
    fn new(len: usize, kind: Kind) -> Region {
        Region {
            len,
            kind,
            heaps: BTreeMap::new(),
            dirty: true,
            opening: false,
            changing: 0,
            failed: Vec::new(),
            starved: false,
        }
    }

    /// What the opener does next in this region, which starts at `start`,
    /// with `room` bytes left under the cap.
    fn next(&self, start: usize, whole: bool, room: usize) -> Next {
        let runs = committed_runs(start, self.len);
        let candidates = if whole {
            // `OpenExistingHeapFromAddress`: the allocation from its base,
            // all of it committed.
            if self.heaps.is_empty() && self.len <= MAX && runs == [(start, self.len)] {
                vec![(start, self.len)]
            } else {
                Vec::new()
            }
        } else {
            let open: Vec<(usize, usize)> = self.heaps.iter().map(|(&at, h)| (at, h.len)).collect();
            spans(start, self.len, self.kind.grain(), &runs, &open)
        };
        let candidates: Vec<(usize, usize)> = candidates
            .into_iter()
            .filter(|span| !self.failed.contains(span))
            .collect();
        match candidates.iter().find(|&&(_, len)| len <= room) {
            Some(&(at, len)) => Next::Open(at, len),
            None if candidates.is_empty() => Next::Nothing,
            None => Next::Starved,
        }
    }
}

enum Next {
    Open(usize, usize),
    /// Spans are left that do not fit under the cap.
    Starved,
    Nothing,
}

#[derive(Default)]
struct State {
    /// By start address; they never overlap.
    regions: BTreeMap<usize, Region>,
    /// Bytes under open heaps and the one being opened.
    imported: usize,
    /// Why nothing opens here, for good (no device, or no existing heaps).
    dead: Option<String>,
    told_cap: bool,
    failures: usize,
}

/// The device calls a registry opens heaps with.
struct Devices {
    device: ID3D12Device,
    device3: ID3D12Device3,
    /// `OpenExistingHeapFromAddress1`; without it, whole allocations only.
    device13: Option<ID3D12Device13>,
}

impl Devices {
    fn new(device: ID3D12Device, whole: bool) -> Result<Devices, String> {
        let mut heaps = D3D12_FEATURE_DATA_EXISTING_HEAPS::default();
        // SAFETY: a sized structure of ours.
        let checked = unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_EXISTING_HEAPS,
                (&raw mut heaps).cast(),
                size_of_val(&heaps) as u32,
            )
        };
        if checked.is_err() || !heaps.Supported.as_bool() {
            return Err("the device cannot open existing heaps".into());
        }
        let device3 = device
            .cast::<ID3D12Device3>()
            .map_err(|e| format!("no ID3D12Device3: {e}"))?;
        let device13 = if whole {
            None
        } else {
            device.cast::<ID3D12Device13>().ok()
        };
        Ok(Devices {
            device,
            device3,
            device13,
        })
    }

    /// A heap over `[at, at + len)` and a buffer over all of it; `whole`:
    /// with `OpenExistingHeapFromAddress`, `at` being an allocation's base
    /// and `len` its size.
    fn open(&self, at: usize, len: usize, whole: bool) -> Result<Heap, String> {
        // SAFETY: committed memory of a region the registry follows, which
        // stays committed while the heap is open: xmem releases heaps before
        // it changes that memory (`changing`). The host checks the rest and
        // fails cleanly on anything else.
        let heap: windows::core::Result<ID3D12Heap> = unsafe {
            match (&self.device13, whole) {
                (Some(device13), false) => {
                    device13.OpenExistingHeapFromAddress1(at as *const c_void, len)
                }
                _ => self
                    .device3
                    .OpenExistingHeapFromAddress(at as *const c_void),
            }
        };
        let heap = heap.map_err(|e| format!("opening: {e}"))?;
        // SAFETY: a plain query.
        let size = unsafe { heap.GetDesc() }.SizeInBytes;
        if size != len as u64 {
            return Err(format!("the heap holds {size} bytes, not {len}"));
        }
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            // The host places nothing else over an existing heap.
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
                | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: a buffer over all of the heap just opened.
        unsafe {
            self.device.CreatePlacedResource(
                &heap,
                0,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        }
        .map_err(|e| format!("placing its buffer: {e}"))?;
        let buffer = buffer.ok_or("placing its buffer: none")?;
        // SAFETY: a plain query of a buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        Ok(Heap {
            len,
            _heap: heap,
            buffer,
            gpu,
            uses: Vec::new(),
        })
    }
}

/// The heaps over title memory, and the title memory they follow.
pub struct Registry {
    state: Mutex<State>,
    /// Signalled whenever a region or a heap changes.
    changed: Condvar,
    options: Options,
    /// The device to open heaps on; the host's when `None`.
    device: Option<ID3D12Device>,
    devices: OnceLock<Result<Devices, String>>,
    /// Heaps are opened by a thread of their own (the process's registry);
    /// else by [`Registry::settle`].
    background: bool,
}

impl Registry {
    /// A registry opening heaps on `device` (the host's when `None`, once
    /// it is first needed) when [`Registry::settle`] is called.
    pub fn new(device: Option<ID3D12Device>, options: Options) -> Registry {
        Registry {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            options,
            device,
            devices: OnceLock::new(),
            background: false,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed.wait(guard).unwrap_or_else(|e| e.into_inner())
    }

    fn devices(&self) -> Result<&Devices, &str> {
        self.devices
            .get_or_init(|| {
                let device = match &self.device {
                    Some(device) => device.clone(),
                    None => crate::host::host()
                        .ok_or("no host D3D12 device")?
                        .device
                        .clone(),
                };
                let devices = Devices::new(device, self.options.whole_allocations)?;
                tracing::info!(
                    cap_mib = self.options.cap / MIB,
                    any_committed_range = devices.device13.is_some(),
                    "uma: heaps over title memory"
                );
                Ok(devices)
            })
            .as_ref()
            .map_err(String::as_str)
    }

    /// Whether heaps open here, or why not; makes the device if need be.
    pub fn ready(&self) -> Result<(), String> {
        self.devices().map(|_| ()).map_err(str::to_owned)
    }

    /// The title reserved `[at, at + len)`, and may have committed some.
    pub fn reserved(&self, at: usize, len: usize) {
        self.add(at, len, Kind::Virtual);
    }

    /// The runtime mapped its permanent view `[at, at + len)` of a section
    /// of physical pages.
    pub fn mapped(&self, at: usize, len: usize) {
        self.add(at, len, Kind::Physical);
    }

    fn add(&self, at: usize, len: usize, kind: Kind) {
        let (at, len) = pages(at, len);
        let mut s = self.lock();
        if s.dead.is_some() || len == 0 {
            return;
        }
        // What was there before is gone (freed out of our sight): no heap
        // can have been open over it then, but drop any.
        while overlapping(&s.regions, at, len)
            .iter()
            .any(|start| s.regions[start].opening)
        {
            s = self.wait(s);
        }
        let mut stale = Vec::new();
        for start in overlapping(&s.regions, at, len) {
            if let Some(region) = s.regions.remove(&start) {
                stale.extend(region.heaps);
            }
        }
        s.imported -= stale.iter().map(|(_, h)| h.len).sum::<usize>();
        s.regions.insert(at, Region::new(len, kind));
        self.changed.notify_all();
        drop(s);
        release(stale);
    }

    /// The title or the runtime committed `[at, at + len)`.
    pub fn committed(&self, at: usize, len: usize) {
        let (at, len) = pages(at, len);
        let mut s = self.lock();
        if s.dead.is_some() {
            return;
        }
        let starts = overlapping(&s.regions, at, len);
        if starts.is_empty() {
            return;
        }
        for start in starts {
            if let Some(region) = s.regions.get_mut(&start) {
                region.dirty = true;
            }
        }
        self.changed.notify_all();
    }

    /// `[at, at + len)` is about to be decommitted, released or unmapped:
    /// the heaps over it are released, once the GPU is done with them, and
    /// none opens over its regions until [`Change::done`].
    pub fn changing(&self, at: usize, len: usize) -> Change<'_> {
        let (at, len) = pages(at, len);
        let mut s = self.lock();
        if s.dead.is_some() {
            return Change::none();
        }
        // A heap being opened there is open first.
        while overlapping(&s.regions, at, len)
            .iter()
            .any(|start| s.regions[start].opening)
        {
            s = self.wait(s);
        }
        let starts = overlapping(&s.regions, at, len);
        let end = at + len;
        let mut taken = Vec::new();
        for start in &starts {
            let Some(region) = s.regions.get_mut(start) else {
                continue;
            };
            region.changing += 1;
            let hit: Vec<usize> = region
                .heaps
                .range(..end)
                .filter(|&(&h, heap)| h + heap.len > at)
                .map(|(&h, _)| h)
                .collect();
            taken.extend(
                hit.into_iter()
                    .filter_map(|h| region.heaps.remove_entry(&h)),
            );
        }
        let freed: usize = taken.iter().map(|(_, h)| h.len).sum();
        s.imported -= freed;
        if freed > 0 {
            // Room under the cap again.
            for region in s.regions.values_mut().filter(|r| r.starved) {
                (region.starved, region.dirty) = (false, true);
            }
            self.changed.notify_all();
        }
        drop(s);
        release(taken);
        Change {
            registry: Some(self),
            at,
            len,
            regions: starts,
            released: freed > 0,
        }
    }

    fn finish(&self, at: usize, len: usize, starts: &[usize], gone: bool) {
        let mut s = self.lock();
        let mut left = Vec::new();
        for start in starts {
            let Some(region) = s.regions.get_mut(start) else {
                continue;
            };
            region.changing -= 1;
            region.dirty = true;
            region.failed.clear();
            let inside = *start >= at && start + region.len <= at + len;
            if gone && inside && region.changing == 0 {
                left.extend(s.regions.remove(start).into_iter().flat_map(|r| r.heaps));
            }
        }
        s.imported -= left.iter().map(|(_, h)| h.len).sum::<usize>();
        self.changed.notify_all();
        drop(s);
        release(left);
    }

    /// The heap holding all of `[at, at + len)`, as its buffer and the
    /// offset there; `None` if no open heap does (the shadow path's then).
    /// See [`Placed`] for how long to keep it.
    pub fn lookup(&self, at: usize, len: usize) -> Option<Placed> {
        let end = at.checked_add(len.max(1))?;
        let s = self.lock();
        let (&start, region) = s.regions.range(..=at).next_back()?;
        if end > start + region.len {
            return None;
        }
        let (&h, heap) = region.heaps.range(..=at).next_back()?;
        if end > h + heap.len {
            return None;
        }
        let offset = (at - h) as u64;
        Some(Placed {
            buffer: heap.buffer.clone(),
            offset,
            gpu: heap.gpu + offset,
        })
    }

    /// GPU work reading or writing `[at, at + len)` is done once `fence`
    /// reaches `value`: releasing the heaps there waits for it. The value
    /// must be one a submission already queued signals.
    pub fn used(&self, at: usize, len: usize, fence: &ID3D12Fence, value: u64) {
        let end = at.saturating_add(len.max(1));
        let mut s = self.lock();
        for start in overlapping(&s.regions, at, end - at) {
            let Some(region) = s.regions.get_mut(&start) else {
                continue;
            };
            for (&h, heap) in region.heaps.range_mut(..end) {
                if h + heap.len > at {
                    heap.note(fence, value);
                }
            }
        }
    }

    /// Bytes under open heaps.
    pub fn imported(&self) -> usize {
        self.lock().imported
    }

    /// The open heaps, as `(start, len)`, by address.
    pub fn heaps(&self) -> Vec<(usize, usize)> {
        let s = self.lock();
        s.regions
            .values()
            .flat_map(|r| r.heaps.iter().map(|(&at, h)| (at, h.len)))
            .collect()
    }

    /// Returns once every heap there is to open is open (or failed, or
    /// does not fit under the cap), memory being changed apart.
    pub fn settle(&self) {
        if !self.background {
            while self.open_next() {}
            return;
        }
        let mut s = self.lock();
        while s.dead.is_none() && (has_work(&s) || s.regions.values().any(|r| r.opening)) {
            s = self.wait(s);
        }
    }

    /// The opener's thread: opens heaps as memory is committed.
    fn run(&self) {
        loop {
            let mut s = self.lock();
            while s.dead.is_none() && !has_work(&s) {
                s = self.wait(s);
            }
            if s.dead.is_some() {
                return;
            }
            drop(s);
            while self.open_next() {}
        }
    }

    /// Opens one heap over committed memory that has none. False when
    /// there is none to open.
    fn open_next(&self) -> bool {
        let devices = match self.devices() {
            Ok(devices) => devices,
            Err(reason) => {
                let mut s = self.lock();
                if s.dead.is_none() {
                    tracing::warn!(reason, "uma: no heaps over title memory");
                    s.dead = Some(reason.to_owned());
                }
                self.changed.notify_all();
                return false;
            }
        };
        let whole = devices.device13.is_none();
        let mut s = self.lock();
        let room = self.options.cap.saturating_sub(s.imported);
        let mut job = None;
        let mut starved = false;
        for (&start, region) in s.regions.iter_mut() {
            if !region.dirty || region.opening || region.changing > 0 {
                continue;
            }
            match region.next(start, whole, room) {
                Next::Open(at, len) => {
                    region.opening = true;
                    job = Some((start, at, len));
                    break;
                }
                Next::Starved => {
                    (region.dirty, region.starved) = (false, true);
                    starved = true;
                }
                Next::Nothing => region.dirty = false,
            }
        }
        if starved && !s.told_cap {
            s.told_cap = true;
            tracing::info!(
                imported_mib = s.imported / MIB,
                cap_mib = self.options.cap / MIB,
                "uma: the cap is reached; the rest of title memory stays on shadows"
            );
        }
        let Some((start, at, len)) = job else {
            self.changed.notify_all();
            return false;
        };
        s.imported += len;
        drop(s);

        let started = Instant::now();
        let made = devices.open(at, len, whole);
        let ms = started.elapsed().as_secs_f64() * 1e3;

        let mut s = self.lock();
        let kept = match s.regions.get_mut(&start) {
            // Opening holds off whatever would remove the region.
            None => Err("its region is gone".to_owned()),
            Some(region) => {
                region.opening = false;
                match made {
                    Ok(heap) => {
                        region.heaps.insert(at, heap);
                        Ok(region.kind)
                    }
                    Err(e) => {
                        region.failed.push((at, len));
                        Err(e)
                    }
                }
            }
        };
        match kept {
            Ok(kind) => tracing::debug!(
                at = format_args!("{at:#x}"),
                kib = len / KIB,
                ?kind,
                ms,
                "uma: heap opened"
            ),
            Err(error) => {
                s.imported -= len;
                s.failures += 1;
                if s.failures <= 16 {
                    tracing::warn!(at = format_args!("{at:#x}"), len, error, "uma: no heap");
                } else {
                    tracing::debug!(at = format_args!("{at:#x}"), len, error, "uma: no heap");
                }
            }
        }
        self.changed.notify_all();
        true
    }
}

/// Whether the opener has a region to look at.
fn has_work(s: &State) -> bool {
    s.regions
        .values()
        .any(|r| r.dirty && !r.opening && r.changing == 0)
}

/// Releases `heaps` once the GPU is done with them; out of the registry's
/// lock, which the submission that signals a fence may need first.
fn release(heaps: Vec<(usize, Heap)>) {
    if heaps.is_empty() {
        return;
    }
    let started = Instant::now();
    let bytes: usize = heaps.iter().map(|(_, h)| h.len).sum();
    for (_, heap) in &heaps {
        for (fence, value) in &heap.uses {
            // SAFETY: plain fence calls; with no event, the second returns
            // once the fence reaches the value.
            unsafe {
                if fence.GetCompletedValue() < *value {
                    let _ = fence.SetEventOnCompletion(*value, HANDLE::default());
                }
            }
        }
    }
    drop(heaps);
    tracing::debug!(
        kib = bytes / KIB,
        ms = started.elapsed().as_secs_f64() * 1e3,
        "uma: heaps released"
    );
}

/// Title memory about to change ([`Registry::changing`]): call
/// [`Change::done`] once it has. Dropping it does the same as `done(false)`.
#[must_use = "call `done` once the memory has changed"]
pub struct Change<'a> {
    registry: Option<&'a Registry>,
    at: usize,
    len: usize,
    regions: Vec<usize>,
    /// Heaps were released for it.
    released: bool,
}

/// How long [`Change::call`] lets a released heap take to let go of its
/// pages: 1.2–1.5 ms seen on WARP, more on a loaded machine.
const GRACE: Duration = Duration::from_secs(2);

impl Change<'_> {
    fn none() -> Change<'static> {
        Change {
            registry: None,
            at: 0,
            len: 0,
            regions: Vec::new(),
            released: false,
        }
    }

    /// Runs `call`, the host call that changes the memory; true once it
    /// succeeds. The runtime lets go of a released heap's pages a little
    /// after its last reference goes (WARP at least), and the host refuses
    /// the call until then (error 87): so while it does, after heaps were
    /// released here, `call` runs again, for up to [`GRACE`]. The thread's
    /// last error is the host's from the last call.
    pub fn call(&self, mut call: impl FnMut() -> bool) -> bool {
        if call() {
            return true;
        }
        let started = Instant::now();
        loop {
            // SAFETY: a plain read of this thread's last error.
            let pinned = unsafe { GetLastError() } == ERROR_INVALID_PARAMETER;
            if !self.released || !pinned || started.elapsed() > GRACE {
                if self.released && pinned {
                    tracing::warn!(
                        at = format_args!("{:#x}", self.at),
                        len = self.len,
                        "uma: the host still refuses after the heaps' release"
                    );
                }
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
            if call() {
                tracing::debug!(
                    ms = started.elapsed().as_secs_f64() * 1e3,
                    "uma: the host let go of released heaps"
                );
                return true;
            }
        }
    }

    /// The memory changed. `gone`: it is no longer the title's (released,
    /// unmapped), and its regions go. Else what is still committed there is
    /// opened again.
    pub fn done(mut self, gone: bool) {
        self.finish(gone);
    }

    fn finish(&mut self, gone: bool) {
        if let Some(registry) = self.registry.take() {
            registry.finish(self.at, self.len, &self.regions, gone);
        }
    }
}

impl Drop for Change<'_> {
    fn drop(&mut self) {
        self.finish(false);
    }
}

/// `[at, at + len)` widened to whole host pages, as the host's memory
/// calls take it.
fn pages(at: usize, len: usize) -> (usize, usize) {
    let start = at & !(HOST_PAGE - 1);
    let end = at
        .saturating_add(len)
        .checked_next_multiple_of(HOST_PAGE)
        .unwrap_or(usize::MAX & !(HOST_PAGE - 1));
    (start, end - start)
}

/// The regions overlapping `[at, at + len)`, by start.
fn overlapping(regions: &BTreeMap<usize, Region>, at: usize, len: usize) -> Vec<usize> {
    let end = at.saturating_add(len);
    let before = regions
        .range(..at)
        .next_back()
        .filter(|&(&start, r)| start + r.len > at)
        .map(|(&start, _)| start);
    before
        .into_iter()
        .chain(regions.range(at..end).map(|(&start, _)| start))
        .collect()
}

/// The committed runs of `[base, base + len)`, as `(start, len)`, leaving
/// out pages no access may touch (`PAGE_NOACCESS`, guard pages).
fn committed_runs(base: usize, len: usize) -> Vec<(usize, usize)> {
    let end = base + len;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut at = base;
    while at < end {
        // SAFETY: plain data; zero is a valid value.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a query of our own address space into a sized structure.
        if unsafe { VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let next = (info.BaseAddress as usize + info.RegionSize).min(end);
        if next <= at {
            break;
        }
        if info.State == MEM_COMMIT && info.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 {
            match runs.last_mut() {
                Some((start, n)) if *start + *n == at => *n = next - *start,
                _ => runs.push((at, next - at)),
            }
        }
        at = next;
    }
    runs
}

/// What to open in a region `[base, base + len)`: its committed `runs`,
/// cut inward to multiples of `grain` from `base` (or to the region's end),
/// less the `open` heaps (by address), in pieces of at most [`MAX`].
fn spans(
    base: usize,
    len: usize,
    grain: usize,
    runs: &[(usize, usize)],
    open: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let end = base + len;
    let up = |x: usize| (base + (x - base).div_ceil(grain) * grain).min(end);
    let down = |x: usize| {
        if x >= end {
            end
        } else {
            base + (x - base) / grain * grain
        }
    };
    let piece = MAX / grain * grain;
    let mut out = Vec::new();
    let mut cut = |from: usize, to: usize| {
        let mut at = from;
        while at < to {
            let next = (at + piece).min(to);
            out.push((at, next - at));
            at = next;
        }
    };
    for &(at, n) in runs {
        let (mut from, to) = (up(at.max(base)), down((at + n).min(end)));
        for &(h, hlen) in open {
            if h + hlen <= from || h >= to {
                continue;
            }
            if h > from {
                cut(from, h);
            }
            from = from.max(h + hlen);
        }
        if from < to {
            cut(from, to);
        }
    }
    out
}

/// Whether heaps open over title memory: `XTP_UMA=1`.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("XTP_UMA").is_ok_and(|v| v == "1");
        if on {
            tracing::info!("uma: on (XTP_UMA=1)");
        }
        on
    })
}

/// The process's registry, on the host device, with its opener thread.
fn global() -> &'static Registry {
    static GLOBAL: LazyLock<Registry> = LazyLock::new(|| Registry {
        background: true,
        ..Registry::new(None, Options::from_env())
    });
    static OPENER: std::sync::Once = std::sync::Once::new();
    let registry: &'static Registry = &GLOBAL;
    OPENER.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("xtp-uma".into())
            .spawn(|| global().run());
        if let Err(e) = spawned {
            registry.lock().dead = Some(format!("no opener thread: {e}"));
            tracing::warn!(error = %e, "uma: no opener thread");
        }
    });
    registry
}

/// [`Registry::reserved`] on the process's registry, when [`enabled`].
pub fn reserved(at: usize, len: usize) {
    if enabled() {
        global().reserved(at, len);
    }
}

/// [`Registry::mapped`] on the process's registry, when [`enabled`].
pub fn mapped(at: usize, len: usize) {
    if enabled() {
        global().mapped(at, len);
    }
}

/// [`Registry::committed`] on the process's registry, when [`enabled`].
pub fn committed(at: usize, len: usize) {
    if enabled() {
        global().committed(at, len);
    }
}

/// [`Registry::changing`] on the process's registry, when [`enabled`];
/// else a change with nothing to do.
pub fn changing(at: usize, len: usize) -> Change<'static> {
    if enabled() {
        global().changing(at, len)
    } else {
        Change::none()
    }
}

/// [`Registry::lookup`] on the process's registry, when [`enabled`].
pub fn lookup(at: usize, len: usize) -> Option<Placed> {
    enabled().then(|| global().lookup(at, len)).flatten()
}

/// [`Registry::used`] on the process's registry, when [`enabled`].
pub fn used(at: usize, len: usize, fence: &ID3D12Fence, value: u64) {
    if enabled() {
        global().used(at, len, fence, value);
    }
}

/// [`Registry::settle`] on the process's registry, when [`enabled`].
pub fn settle() {
    if enabled() {
        global().settle();
    }
}

/// [`Registry::ready`] on the process's registry; an error when off.
pub fn ready() -> Result<(), String> {
    if !enabled() {
        return Err("off (XTP_UMA is not 1)".into());
    }
    global().ready()
}

#[cfg(test)]
mod tests;
