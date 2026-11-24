//! Host buffers placed in heaps reserved ahead (frame-prof's session A,
//! task 7112c207). Past the machine's VidMm threshold every new allocation
//! waits for one admission, about 0.4 s, and a committed resource per host
//! copy of title memory, per upload, budget word or scratch chunk paid one
//! on a title thread: 25 % of the loading screen's time and 33 % of the
//! world's, with `shadow::find`'s lock held across the wait by the thread
//! that paid it. Here each buffer is a placed resource in a heap of
//! [`HEAP`] bytes that a thread of ours made beforehand (one admission per
//! heap): placing one allocates nothing in VidMm.
//!
//! Each buffer keeps its own resource (and so its own resource state:
//! copies, indirect arguments and #334's barriers move whole resources),
//! aligned as D3D12 places buffers, to 64 KiB. Its range goes back to its
//! heap when the resource is destroyed, which XTP does only once no
//! submission uses it (lists keep what they name until they are retired):
//! the resource holds a [`Lease`] as private data, which D3D12 releases
//! with it.
//!
//! The reserve thread keeps at least [`RESERVE`] bytes free in each pool,
//! makes a heap when a thread waits for room, and releases empty heaps
//! past one heap's worth of spare room. A buffer larger than a heap gets a
//! heap of its own, made on the asking thread. `XTP_BUFFER_POOL=0` brings
//! back the committed buffers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

/// What a buffer is for, and so the heaps it comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// System memory the CPU reads and writes cached and shaders read and
    /// write: host copies of title memory, budget words (a custom heap,
    /// write-back, L0). Starts in the common state, zeroed.
    System,
    /// Upload memory (write-combined, read by the GPU): staging, scratch,
    /// list uploads. In the generic read state; its bytes are the caller's
    /// to write.
    Upload,
}

/// The placement unit of buffers.
const ALIGN: u64 = D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT as u64;
/// One heap's bytes (smaller in tests, to see heaps fill up).
#[cfg(not(test))]
const HEAP: u64 = 256 << 20;
#[cfg(test)]
const HEAP: u64 = 4 << 20;
/// Free bytes the reserve thread keeps in each pool.
const RESERVE: u64 = HEAP / 2;

/// Free ranges of a heap: best fit, merged when given back.
#[derive(Default)]
struct Ranges {
    /// Offset → length.
    by_offset: BTreeMap<u64, u64>,
    /// (length, offset).
    by_size: BTreeSet<(u64, u64)>,
    free: u64,
}

impl Ranges {
    fn new(size: u64) -> Ranges {
        let mut r = Ranges::default();
        r.insert(0, size);
        r
    }

    fn insert(&mut self, offset: u64, len: u64) {
        self.by_offset.insert(offset, len);
        self.by_size.insert((len, offset));
        self.free += len;
    }

    fn remove(&mut self, offset: u64, len: u64) {
        self.by_offset.remove(&offset);
        self.by_size.remove(&(len, offset));
        self.free -= len;
    }

    /// The offset of `len` bytes taken from the smallest free range that
    /// holds them.
    fn take(&mut self, len: u64) -> Option<u64> {
        let &(size, offset) = self.by_size.range((len, 0)..).next()?;
        self.remove(offset, size);
        if size > len {
            self.insert(offset + len, size - len);
        }
        Some(offset)
    }

    /// Gives `[offset, offset + len)` back, merged with its free neighbours.
    fn give(&mut self, mut offset: u64, mut len: u64) {
        if let Some((&before, &n)) = self.by_offset.range(..offset).next_back()
            && before + n == offset
        {
            self.remove(before, n);
            (offset, len) = (before, n + len);
        }
        if let Some(&n) = self.by_offset.get(&(offset + len)) {
            self.remove(offset + len, n);
            len += n;
        }
        self.insert(offset, len);
    }

    fn largest(&self) -> u64 {
        self.by_size.last().map_or(0, |&(len, _)| len)
    }
}

struct Heap {
    id: u64,
    heap: ID3D12Heap,
    size: u64,
    free: Ranges,
    /// Per [`ALIGN`] unit: whether a buffer had it (so it may hold old
    /// bytes; a new heap is zeroed).
    used: Vec<bool>,
}

#[derive(Default)]
struct State {
    heaps: Vec<Heap>,
    next_id: u64,
    /// The reserve thread is making a heap.
    growing: bool,
    /// Threads waiting for room.
    waiting: usize,
    /// The last heap the reserve thread tried could not be made.
    failed: bool,
}

impl State {
    fn free(&self) -> u64 {
        self.heaps.iter().map(|h| h.free.free).sum()
    }

    fn reserved(&self) -> u64 {
        self.heaps.iter().map(|h| h.size).sum()
    }

    /// Takes `len` bytes from the heap whose best fit is smallest: the
    /// heap, its id, the offset, and the units that may hold old bytes.
    fn take(&mut self, len: u64) -> Option<Room> {
        let heap = self
            .heaps
            .iter_mut()
            .filter(|h| h.free.largest() >= len)
            .min_by_key(|h| h.free.largest())?;
        let offset = heap.free.take(len)?;
        let mut dirty: Vec<(u64, u64)> = Vec::new();
        for unit in offset / ALIGN..(offset + len) / ALIGN {
            if std::mem::replace(&mut heap.used[unit as usize], true) {
                let at = unit * ALIGN - offset;
                match dirty.last_mut() {
                    Some((from, n)) if *from + *n == at => *n += ALIGN,
                    _ => dirty.push((at, ALIGN)),
                }
            }
        }
        Some(Room {
            heap: heap.heap.clone(),
            id: heap.id,
            offset,
            dirty,
        })
    }

    fn add(&mut self, heap: ID3D12Heap, size: u64) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.heaps.push(Heap {
            id,
            heap,
            size,
            free: Ranges::new(size),
            used: vec![false; (size / ALIGN) as usize],
        });
        id
    }
}

/// Room taken for a buffer: its heap, the heap's id, the offset, and the
/// pieces (offset in the buffer, bytes) that may hold old bytes.
struct Room {
    heap: ID3D12Heap,
    id: u64,
    offset: u64,
    dirty: Vec<(u64, u64)>,
}

/// The heaps of one kind of buffer.
pub(crate) struct Pool {
    kind: Kind,
    device: ID3D12Device,
    state: Mutex<State>,
    /// Signalled when room is given back or wanted, and when a heap is made.
    changed: Condvar,
    /// Microseconds title threads waited for room, and how many times.
    waited: [AtomicU64; 2],
}

// SAFETY: D3D12 devices and heaps are free-threaded.
unsafe impl Send for Pool {}
// SAFETY: as above.
unsafe impl Sync for Pool {}

/// The pools, System then Upload.
static POOLS: OnceLock<[Pool; 2]> = OnceLock::new();

/// Whether buffers come from the pools: unless `XTP_BUFFER_POOL=0`.
fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("XTP_BUFFER_POOL")
            .ok()
            .is_none_or(|v| v.trim() != "0")
    })
}

/// Starts the pools of the host device and their reserve threads, which
/// make the first heaps at once.
pub(crate) fn start(device: &ID3D12Device) {
    if !enabled() {
        return;
    }
    let mut made = false;
    let pools = POOLS.get_or_init(|| {
        made = true;
        [Kind::System, Kind::Upload].map(|kind| Pool {
            kind,
            device: device.clone(),
            state: Mutex::default(),
            changed: Condvar::new(),
            waited: Default::default(),
        })
    });
    if !made {
        return;
    }
    for pool in pools {
        let spawned = std::thread::Builder::new()
            .name(format!("xtp-gpu-{:?}-buffers", pool.kind).to_lowercase())
            .spawn(move || pool.reserve());
        if let Err(e) = spawned {
            tracing::warn!(error = %e, kind = ?pool.kind, "no buffer reserve thread");
        }
    }
}

/// A buffer of `size` bytes of `kind`, mapped, placed in a pool's heap;
/// `None` without pools (the caller makes a committed one).
pub(crate) fn buffer(kind: Kind, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    let pool = &POOLS.get()?[kind as usize];
    pool.place(size)
}

/// For the frame-rate line: per pool, heaps, MiB reserved and used, and
/// the title's waits for room (count, ms) since the last call.
pub(crate) fn summary() -> Option<String> {
    let pools = POOLS.get()?;
    let parts: Vec<String> = pools
        .iter()
        .map(|p| {
            let s = p.lock();
            let (reserved, free) = (s.reserved(), s.free());
            let waits = p.waited[1].swap(0, Ordering::Relaxed);
            let us = p.waited[0].swap(0, Ordering::Relaxed);
            format!(
                "{:?} {} heaps {} MiB, {} MiB used, {waits} waits {:.1} ms",
                p.kind,
                s.heaps.len(),
                reserved >> 20,
                (reserved - free) >> 20,
                us as f64 / 1e3
            )
        })
        .collect();
    Some(parts.join("; "))
}

impl Pool {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn heap_properties(&self) -> D3D12_HEAP_PROPERTIES {
        match self.kind {
            Kind::System => D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            },
            Kind::Upload => D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_UPLOAD,
                ..Default::default()
            },
        }
    }

    /// A new heap of `size` bytes (resident once made: the VidMm wait).
    fn make_heap(&self, size: u64) -> Option<ID3D12Heap> {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: size,
            Properties: self.heap_properties(),
            Alignment: ALIGN,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
        };
        let mut heap: Option<ID3D12Heap> = None;
        let started = std::time::Instant::now();
        // SAFETY: plain creation with a valid description.
        match unsafe { self.device.CreateHeap(&desc, &mut heap) } {
            Ok(()) => {
                tracing::debug!(
                    kind = ?self.kind,
                    mib = size >> 20,
                    ms = started.elapsed().as_millis(),
                    "buffer heap made"
                );
                heap
            }
            Err(e) => {
                tracing::warn!(error = %e, kind = ?self.kind, size, "no buffer heap");
                None
            }
        }
    }

    /// The reserve thread: makes heaps while the pool has less than
    /// [`RESERVE`] free or a thread waits for room, and releases empty
    /// heaps past one heap's worth of spare room.
    fn reserve(&self) {
        loop {
            let surplus = {
                let mut s = self.lock();
                loop {
                    let spare = s.free();
                    if (spare < RESERVE || s.waiting > 0) && !s.failed {
                        break None;
                    }
                    // An empty heap the rest of the reserve can spare.
                    if let Some(i) = s.heaps.iter().position(|h| h.free.free == h.size)
                        && spare - s.heaps[i].size >= RESERVE + HEAP
                    {
                        let heap = s.heaps.swap_remove(i);
                        break Some(heap.heap);
                    }
                    // After a failure, again only once room is wanted anew.
                    s = self.changed.wait(s).unwrap_or_else(|e| e.into_inner());
                    if s.waiting > 0 && s.failed {
                        s.failed = false;
                    }
                }
            };
            if let Some(heap) = surplus {
                drop(heap);
                continue;
            }
            self.lock().growing = true;
            let heap = self.make_heap(HEAP);
            let mut s = self.lock();
            s.growing = false;
            match heap {
                Some(heap) => {
                    s.add(heap, HEAP);
                }
                None => s.failed = true,
            }
            self.changed.notify_all();
        }
    }

    /// Room for `len` bytes (a multiple of [`ALIGN`]): taken from a heap,
    /// or from one the reserve thread makes meanwhile (waiting for it, no
    /// lock of ours held but the pool's, which the wait releases), or from
    /// a heap of its own for a buffer larger than a heap. `None` if no heap
    /// can be made.
    fn room(&self, len: u64) -> Option<Room> {
        let mut s = self.lock();
        if let Some(found) = s.take(len) {
            if s.free() < RESERVE {
                self.changed.notify_all();
            }
            return Some(found);
        }
        if len > HEAP {
            drop(s);
            let heap = self.make_heap(len)?;
            let mut s = self.lock();
            s.add(heap, len);
            return s.take(len);
        }
        let started = std::time::Instant::now();
        s.waiting += 1;
        self.changed.notify_all();
        let found = loop {
            if let Some(found) = s.take(len) {
                break Some(found);
            }
            if s.failed && !s.growing {
                break None;
            }
            s = self.changed.wait(s).unwrap_or_else(|e| e.into_inner());
        };
        s.waiting -= 1;
        self.waited[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.waited[1].fetch_add(1, Ordering::Relaxed);
        found
    }

    /// Gives a buffer's range back (its resource is gone).
    fn give(&self, heap: u64, offset: u64, len: u64) {
        let mut s = self.lock();
        if let Some(h) = s.heaps.iter_mut().find(|h| h.id == heap) {
            h.free.give(offset, len);
        }
        // Room for a waiting thread, or an empty heap to release.
        self.changed.notify_all();
    }

    fn place(&'static self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        #[cfg(test)]
        before_placing(size);
        let len = size.max(1).next_multiple_of(ALIGN);
        let Room {
            heap,
            id,
            offset,
            dirty,
        } = self.room(len)?;
        let (state, flags) = match self.kind {
            Kind::System => (
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ),
            Kind::Upload => (D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_FLAG_NONE),
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: a buffer placed in our heap, in a range of its own;
        // mapped as committed ones are.
        let made = unsafe {
            self.device
                .CreatePlacedResource(&heap, offset, &desc, state, None, &mut buffer)
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))?;
                    let lease = Lease::interface(self, id, offset, len);
                    b.SetPrivateDataInterface(&LEASE, &lease)
                })
        };
        let buffer = match (made, buffer) {
            (Ok(()), Some(buffer)) => buffer,
            (made, buffer) => {
                // Not leased (or the lease went with the resource, giving
                // the range back): give it back if it did not.
                let leased = buffer.is_some_and(|b| has_lease(&b));
                if !leased {
                    self.give(id, offset, len);
                }
                if let Err(e) = made {
                    tracing::warn!(error = %e, kind = ?self.kind, size, "no placed buffer");
                }
                return None;
            }
        };
        let data: *mut u8 = data.cast();
        if self.kind == Kind::System {
            // A committed buffer starts zeroed; a range used before is not.
            for (at, n) in dirty {
                let n = n.min(size.saturating_sub(at));
                // SAFETY: inside the buffer's mapping.
                unsafe { data.add(at as usize).write_bytes(0, n as usize) };
            }
        }
        Some((buffer, data))
    }
}

/// The private data key of a buffer's [`Lease`].
const LEASE: GUID = GUID::from_u128(0x6a1f_02c4_3b7d_4e1a_9c55_8d0e_b2f3_7a61);

/// Whether a resource holds a lease (and so gives its range back itself).
fn has_lease(resource: &ID3D12Resource) -> bool {
    let mut size = 0u32;
    // SAFETY: asks the size of our private data only.
    unsafe { resource.GetPrivateData(&LEASE, &mut size, None) }.is_ok()
}

/// A placed buffer's range in its heap: a COM object (`IUnknown` alone)
/// that the resource holds as private data, which D3D12 releases with the
/// resource. Its last release gives the range back.
#[repr(C)]
struct Lease {
    /// First, as in every COM object.
    vtable: &'static IUnknown_Vtbl,
    refs: std::sync::atomic::AtomicU32,
    pool: &'static Pool,
    heap: u64,
    offset: u64,
    len: u64,
}

static LEASE_VTABLE: IUnknown_Vtbl = IUnknown_Vtbl {
    QueryInterface: lease_query,
    AddRef: lease_add_ref,
    Release: lease_release,
};

impl Lease {
    /// A new lease, as the interface that holds its one reference.
    fn interface(pool: &'static Pool, heap: u64, offset: u64, len: u64) -> IUnknown {
        let lease = Box::into_raw(Box::new(Lease {
            vtable: &LEASE_VTABLE,
            refs: std::sync::atomic::AtomicU32::new(1),
            pool,
            heap,
            offset,
            len,
        }));
        // SAFETY: a COM object of ours with one reference, which the
        // interface takes over.
        unsafe { IUnknown::from_raw(lease.cast()) }
    }
}

unsafe extern "system" fn lease_query(
    this: *mut std::ffi::c_void,
    iid: *const GUID,
    out: *mut *mut std::ffi::c_void,
) -> HRESULT {
    const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
    const E_POINTER: HRESULT = HRESULT(0x8000_4003_u32 as i32);
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    // SAFETY: the caller's pointers, checked above; `this` is a lease.
    unsafe {
        if *iid == IUnknown::IID {
            lease_add_ref(this);
            *out = this;
            HRESULT(0)
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn lease_add_ref(this: *mut std::ffi::c_void) -> u32 {
    // SAFETY: a live lease (the caller holds a reference).
    let lease = unsafe { &*this.cast::<Lease>() };
    lease.refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn lease_release(this: *mut std::ffi::c_void) -> u32 {
    let lease = this.cast::<Lease>();
    // SAFETY: a live lease (the caller gives up its reference).
    let left = unsafe { &*lease }.refs.fetch_sub(1, Ordering::Release) - 1;
    if left == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        // SAFETY: the last reference: the box made in `Lease::new`.
        let lease = unsafe { Box::from_raw(lease) };
        lease.pool.give(lease.heap, lease.offset, lease.len);
    }
    left
}

/// For tests: a buffer size whose placing waits until the receiver gets a
/// message (or closes), as a creation that takes long.
#[cfg(test)]
pub(crate) static HOLD: Mutex<Option<(u64, std::sync::mpsc::Receiver<()>)>> = Mutex::new(None);

#[cfg(test)]
fn before_placing(size: u64) {
    let held = {
        let mut hold = HOLD.lock().unwrap_or_else(|e| e.into_inner());
        match &*hold {
            Some((held, _)) if *held == size => hold.take(),
            _ => None,
        }
    };
    if let Some((_, release)) = held {
        let _ = release.recv_timeout(std::time::Duration::from_secs(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_take_the_best_fit_and_merge_what_comes_back() {
        let mut r = Ranges::new(10 * ALIGN);
        let a = r.take(3 * ALIGN).unwrap();
        let b = r.take(2 * ALIGN).unwrap();
        let c = r.take(ALIGN).unwrap();
        assert_eq!((a, b, c), (0, 3 * ALIGN, 5 * ALIGN));
        r.give(b, 2 * ALIGN);
        // The 2-unit hole fits a 2-unit buffer better than the 4-unit tail.
        assert_eq!(r.take(2 * ALIGN), Some(b));
        r.give(a, 3 * ALIGN);
        r.give(b, 2 * ALIGN);
        assert_eq!(r.largest(), 5 * ALIGN, "merged with each other");
        r.give(c, ALIGN);
        assert_eq!(r.largest(), 10 * ALIGN, "and with the tail");
        assert_eq!((r.free, r.by_offset.len()), (10 * ALIGN, 1));
        assert_eq!(r.take(11 * ALIGN), None);
    }

    /// Against a model: random takes and gives keep the free ranges
    /// disjoint, merged and accounted for.
    #[test]
    fn ranges_match_a_model() {
        let units = 64u64;
        let mut r = Ranges::new(units * ALIGN);
        let mut held: Vec<(u64, u64)> = Vec::new();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..5000 {
            if next() % 2 == 0 || held.is_empty() {
                let len = (next() % 6 + 1) * ALIGN;
                if let Some(at) = r.take(len) {
                    assert!(held.iter().all(|&(o, n)| at + len <= o || o + n <= at));
                    held.push((at, len));
                }
            } else {
                let (at, len) = held.swap_remove((next() % held.len() as u64) as usize);
                r.give(at, len);
            }
            let used: u64 = held.iter().map(|&(_, n)| n).sum();
            assert_eq!(r.free + used, units * ALIGN);
            let mut end = None;
            for (&o, &n) in &r.by_offset {
                assert!(end.is_none_or(|e| e < o), "free ranges merged");
                end = Some(o + n);
            }
        }
    }

    /// Buffers of a pool on the host device: placed, mapped, distinct; a
    /// range used before is zeroed for a system buffer; the pool grows by
    /// whole heaps, a buffer larger than a heap gets one of its own, and
    /// ranges come back when the resources go.
    #[test]
    fn pooled_buffers_are_placed_zeroed_and_given_back() {
        let test = "host::buffers::tests::pooled_buffers_are_placed_zeroed_and_given_back";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        if !enabled() {
            return;
        }
        // A pool of the test's own, without a reserve thread: every heap is
        // made when a buffer waits for room (none here: `room` would wait
        // for ever), so it is given heaps by hand.
        let pool: &'static Pool = Box::leak(Box::new(Pool {
            kind: Kind::System,
            device: host.device.clone(),
            state: Mutex::default(),
            changed: Condvar::new(),
            waited: Default::default(),
        }));
        let add = || {
            let heap = pool.make_heap(HEAP).expect("a heap");
            pool.lock().add(heap, HEAP);
        };
        add();
        let units = HEAP / ALIGN;
        let mut buffers: Vec<(ID3D12Resource, *mut u8)> = (0..units)
            .map(|i| {
                pool.place(if i % 2 == 0 { 256 } else { ALIGN })
                    .expect("a buffer")
            })
            .collect();
        // SAFETY: plain queries of live buffers.
        let mut vas: Vec<u64> = buffers
            .iter()
            .map(|(b, _)| unsafe { b.GetGPUVirtualAddress() })
            .collect();
        vas.sort_unstable();
        vas.dedup();
        assert_eq!(vas.len() as u64, units, "each its own range");
        assert!(vas.windows(2).all(|w| w[1] - w[0] >= ALIGN));
        assert_eq!(pool.lock().free(), 0);
        // Written, released, placed again: zeroed.
        let (b, data) = buffers.pop().unwrap();
        // SAFETY: the buffer's mapping, ALIGN bytes.
        unsafe { data.write_bytes(0xab, ALIGN as usize) };
        drop(b);
        assert_eq!(pool.lock().free(), ALIGN, "given back with its resource");
        let (again, data) = pool.place(ALIGN).expect("the range again");
        // SAFETY: the new buffer's mapping.
        let bytes = unsafe { std::slice::from_raw_parts(data, ALIGN as usize) };
        assert!(bytes.iter().all(|&b| b == 0), "zeroed");
        buffers.push((again, data));
        // A second heap; a buffer past a heap's size gets its own.
        add();
        let (big, _) = pool.place(HEAP + ALIGN).expect("a heap of its own");
        assert_eq!(pool.lock().heaps.len(), 3);
        drop(big);
        buffers.clear();
        let s = pool.lock();
        assert_eq!(s.free(), s.reserved(), "everything given back");
    }

    /// A thread waiting for room gets a heap from the reserve thread, and
    /// spare empty heaps are released once the buffers go (a pool and
    /// reserve thread of the test's own).
    #[test]
    fn the_reserve_thread_makes_room_and_releases_spare_heaps() {
        let test = "host::buffers::tests::the_reserve_thread_makes_room_and_releases_spare_heaps";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let pool: &'static Pool = Box::leak(Box::new(Pool {
            kind: Kind::Upload,
            device: host.device.clone(),
            state: Mutex::default(),
            changed: Condvar::new(),
            waited: Default::default(),
        }));
        std::thread::spawn(move || pool.reserve());
        // More than the reserve holds: most of them wait for heaps.
        let buffers: Vec<(ID3D12Resource, *mut u8)> = (0..3 * HEAP / ALIGN)
            .map(|_| pool.place(ALIGN).expect("an upload buffer"))
            .collect();
        assert!(pool.lock().heaps.len() >= 3);
        assert!(pool.waited[1].load(Ordering::Relaxed) > 0, "it waited");
        drop(buffers);
        let started = std::time::Instant::now();
        while pool.lock().reserved() > RESERVE + 2 * HEAP
            && started.elapsed() < std::time::Duration::from_secs(10)
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let s = pool.lock();
        assert!(s.reserved() <= RESERVE + 2 * HEAP, "spare heaps released");
        assert_eq!(s.free(), s.reserved());
    }

    /// The host's buffers are placed (they hold a lease), and
    /// `XTP_BUFFER_POOL=0` brings back committed ones: checked in a child
    /// process, where the switch is read afresh.
    #[test]
    fn committed_buffers_come_back_with_xtp_buffer_pool_0() {
        const CHILD: &str = "XTP_TEST_BUFFER_POOL_OFF";
        let test = "host::buffers::tests::committed_buffers_come_back_with_xtp_buffer_pool_0";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let (shared, data) = host.shared_buffer(256).expect("a shared buffer");
        let (upload, _) = host.upload_buffer(256).expect("an upload buffer");
        if std::env::var_os(CHILD).is_some() {
            assert!(POOLS.get().is_none(), "no pools");
            assert!(!has_lease(&shared) && !has_lease(&upload), "committed");
            // SAFETY: the buffer's mapping, 256 bytes.
            unsafe { data.write_bytes(7, 256) };
            return;
        }
        assert_eq!(
            (has_lease(&shared), has_lease(&upload)),
            (enabled(), enabled()),
            "placed"
        );
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("XTP_BUFFER_POOL", "0")
            .output()
            .unwrap();
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(out.status.success(), "the child: {stdout}{stderr}");
        assert!(stdout.contains("1 passed"), "{stdout}");
    }
}
