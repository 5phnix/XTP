//! Host textures placed in large heaps of our own (task 450ec9ba,
//! `docs/analysis/vram.md`).
//!
//! A committed resource is a heap of its own, made resident on its own.
//! Past about 9.5 GB resident on the PC's RTX 4080, VidMm admits about one
//! new resident allocation a second whatever its size, so a world loading
//! thousands of textures stalled for good (task 6bb65399). Here the
//! textures the title fills itself are placed (`CreatePlacedResource`) in
//! heaps of 256 MiB (by default) that a thread of ours makes ahead of need:
//! - two classes by placement alignment: 4 KiB for textures D3D12's small
//!   resource rule allows ([`may_be_small`]), 64 KiB for the others. MSAA
//!   textures (4 MiB) are always targets, which stay committed;
//! - heaps are made not resident and not zeroed (the title's contents are
//!   uploaded over every texel before a shader reads them), then asked to
//!   become resident within the budget, one request a heap
//!   (`residency`): no request for any texture;
//! - the thread keeps a reserve of free bytes in each class (heaps still
//!   becoming resident count), so the title's thread only places. A
//!   texture placed in a heap not resident yet is handed to the GPU once
//!   it is (`HostTexture::ready`); only in the first [`HEAP_WAIT`] of a
//!   heap does a placement wait for it, [`PLACE_WAIT`] at most, so a heap
//!   costs the title's thread that much at most, never a texture. Only
//!   when the reserve is gone does the title's thread make a heap itself
//!   (not resident, not waited for beyond that);
//! - past the video memory budget (QueryVideoMemoryInfo, and its change
//!   notification) no heap is made: the texture is made as a committed one
//!   (asked to become resident within the budget, retried) and the refusal
//!   is logged;
//! - a texture's range goes back when its host copy is dropped (lists hold
//!   it until their submission has run); an empty heap is released unless
//!   the reserve needs it, and all empty ones go when usage passes the
//!   budget.
//!
//! The pool is off by default: `XTP_TEXTURE_POOL=1` turns it on (unset or
//! `0`: every host texture a committed resource, as before; measured in
//! game in docs/analysis/vram.md §3.2). `XTP_TEXTURE_HEAP_MIB` sets the heap size (the small class gets
//! an eighth), `XTP_TEXTURE_RESERVE_MIB` the reserve (one heap).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter3, IDXGIFactory4,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

use super::ranges::Ranges;
use super::residency::Residency;

const KIB_4: u64 = 4 << 10;
const KIB_64: u64 = 64 << 10;
const MIB: u64 = 1 << 20;

/// How long after its heap was made a placement may wait for the heap's
/// residency, and at most how long one placement waits: below VidMm's
/// ceiling a fresh heap is resident well within it; past the ceiling the
/// title's thread waits that long at most a heap, not a texture.
const HEAP_WAIT: Duration = Duration::from_millis(100);
const PLACE_WAIT: Duration = Duration::from_millis(10);

/// Whether host textures go to the pool: only with `XTP_TEXTURE_POOL` set
/// (and not `0`). Off by default: in game (Series X, docs/analysis/vram.md
/// §3.2) it reached the world without loading faster than committed
/// textures, with more video memory resident (its reserve).
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = asks_pool(std::env::var("XTP_TEXTURE_POOL").ok().as_deref());
        if on {
            tracing::info!("XTP_TEXTURE_POOL: host textures the title fills go to pooled heaps");
        }
        on
    })
}

fn asks_pool(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

static POOL: OnceLock<Option<Pool>> = OnceLock::new();

/// The run's pool if it was made (the frame log never makes it).
pub(crate) fn global_if_made() -> Option<&'static Pool> {
    POOL.get()?.as_ref()
}

/// The run's pool, made with the host device on first use; `None` when
/// switched off or without a host GPU.
pub(crate) fn global() -> Option<&'static Pool> {
    POOL.get_or_init(|| {
        if !enabled() {
            return None;
        }
        let host = crate::host::host()?;
        let config = Config::from_env();
        tracing::info!(
            heap_mib = config.heap[Class::Normal.index()] / MIB,
            small_heap_mib = config.heap[Class::Small.index()] / MIB,
            reserve_mib = config.reserve[Class::Normal.index()] / MIB,
            largest_mib = config.largest / MIB,
            "host textures placed in pooled heaps"
        );
        Some(Pool::new(
            host.device.clone(),
            config,
            Box::new(crate::dxgi::video_memory),
        ))
    })
    .as_ref()
}

/// Placement classes, by alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// 4 KiB: D3D12's small resources.
    Small,
    /// 64 KiB: every other texture that is no target.
    Normal,
}

impl Class {
    const ALL: [Class; 2] = [Class::Small, Class::Normal];

    fn index(self) -> usize {
        match self {
            Class::Small => 0,
            Class::Normal => 1,
        }
    }

    pub(crate) fn alignment(self) -> u64 {
        match self {
            Class::Small => KIB_4,
            Class::Normal => KIB_64,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Class::Small => "4 KiB",
            Class::Normal => "64 KiB",
        }
    }
}

/// Sizes, by class (index [`Class::index`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Config {
    /// Bytes of one heap.
    pub(crate) heap: [u64; 2],
    /// Free bytes kept ahead.
    pub(crate) reserve: [u64; 2],
    /// The largest texture placed; larger ones are committed.
    pub(crate) largest: u64,
}

impl Config {
    /// From `XTP_TEXTURE_HEAP_MIB` (256) and `XTP_TEXTURE_RESERVE_MIB`
    /// (one heap: making a heap not resident takes about a millisecond).
    pub(crate) fn from_env() -> Config {
        let mib = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|&v| v > 0)
        };
        let heap = mib("XTP_TEXTURE_HEAP_MIB").unwrap_or(256).clamp(8, 4096) * MIB;
        let reserve = mib("XTP_TEXTURE_RESERVE_MIB").map_or(heap, |v| v * MIB);
        Config::sized(heap, reserve)
    }

    /// Heaps of `heap` bytes and a reserve of `reserve` for the 64 KiB
    /// class; an eighth of each for the small class.
    pub(crate) fn sized(heap: u64, reserve: u64) -> Config {
        let small = (heap / 8).next_multiple_of(KIB_64).max(KIB_64);
        Config {
            heap: [small, heap],
            reserve: [(reserve / 8).max(small.min(reserve)), reserve],
            largest: heap / 4,
        }
    }
}

/// Whether D3D12's small resource rule may give the host texture `d` a
/// 4 KiB placement: no target, one sample, one array slice, and a most
/// detailed mip of at most 16 tiles of 4 KiB in the standard tile shapes.
/// The device has the last word (`GetResourceAllocationInfo` grants it or
/// not).
pub(crate) fn may_be_small(d: &D3D12_RESOURCE_DESC) -> bool {
    let targets = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL;
    let texture = matches!(
        d.Dimension,
        D3D12_RESOURCE_DIMENSION_TEXTURE1D
            | D3D12_RESOURCE_DIMENSION_TEXTURE2D
            | D3D12_RESOURCE_DIMENSION_TEXTURE3D
    );
    if !texture || d.Flags.0 & targets.0 != 0 || d.SampleDesc.Count > 1 || d.DepthOrArraySize != 1 {
        return false;
    }
    let Some((bytes, block)) = super::element(d.Format.0 as u32) else {
        return false;
    };
    // Texels (or blocks) of a 4 KiB tile, by element size.
    let (tw, th) = match bytes {
        1 => (64, 64),
        2 => (64, 32),
        4 => (32, 32),
        8 => (32, 16),
        16 => (16, 16),
        _ => return false,
    };
    let (w, h) = (d.Width.div_ceil(block), u64::from(d.Height).div_ceil(block));
    w.div_ceil(tw) * h.div_ceil(th) <= 16
}

/// The pool: heaps by class, and the thread that keeps their reserve.
pub(crate) struct Pool {
    inner: Arc<Inner>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The budget and usage of local video memory for a device.
type Budget = Box<dyn Fn(&ID3D12Device) -> Option<(u64, u64)> + Send + Sync>;

struct Inner {
    device: ID3D12Device,
    config: Config,
    budget: Budget,
    state: Mutex<State>,
    /// Wakes the thread.
    wake: Condvar,
    /// DXGI's budget change notification, if it gave one.
    changes: Option<BudgetEvent>,
    /// Heaps refuse `CREATE_NOT_ZEROED` (an older runtime): made without.
    zeroed: AtomicBool,
}

// SAFETY: D3D12 devices and heaps are free-threaded; the state is behind
// its lock.
unsafe impl Send for Inner {}
// SAFETY: as above.
unsafe impl Sync for Inner {}

struct State {
    classes: [Vec<Heap>; 2],
    /// Heaps to release (on the thread).
    releasing: Vec<ID3D12Heap>,
    stop: bool,
    woken: bool,
    next_id: u64,
    /// Whether heaps are held back by the budget now (logged once each
    /// time it starts).
    over_budget: bool,
    stats: Stats,
}

/// One heap of the pool.
struct Heap {
    id: u64,
    heap: ID3D12Heap,
    size: u64,
    ranges: Ranges,
    textures: u32,
    residency: Arc<Residency>,
    /// When it was made.
    made: Instant,
}

/// What the pool did since it was made.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stats {
    /// Heaps made, and of them by the title's thread (the reserve was
    /// empty).
    pub(crate) heaps: u64,
    pub(crate) emergency: u64,
    pub(crate) released: u64,
    /// Residency requests made (one a heap).
    pub(crate) asked: u64,
    /// Textures placed, and those the pool did not place (committed
    /// instead).
    pub(crate) placed: u64,
    pub(crate) not_placed: u64,
    /// Textures handed out before their heap was resident (views made of
    /// them meanwhile are null).
    pub(crate) early: u64,
    /// Heaps not made because of the budget.
    pub(crate) over_budget: u64,
}

/// What the pool holds now.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Holding {
    pub(crate) heaps: u64,
    /// Heaps not resident yet.
    pub(crate) pending: u64,
    pub(crate) bytes: u64,
    pub(crate) used: u64,
    pub(crate) textures: u64,
}

/// A texture's range in the pool, given back when dropped.
pub(crate) struct Slot {
    inner: Arc<Inner>,
    class: Class,
    heap: u64,
    offset: u64,
    len: u64,
}

impl Slot {
    /// Its class, heap offset and length.
    #[cfg(test)]
    pub(crate) fn place(&self) -> (Class, u64, u64) {
        (self.class, self.offset, self.len)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.inner.give_back(self);
    }
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot")
            .field("class", &self.class)
            .field("heap", &self.heap)
            .field("offset", &self.offset)
            .field("len", &self.len)
            .finish()
    }
}

/// A texture placed by [`Pool::place`].
pub(crate) struct Placed {
    pub(crate) texture: ID3D12Resource,
    pub(crate) slot: Slot,
    /// Its heap's residency.
    pub(crate) residency: Arc<Residency>,
    /// The bytes it takes.
    pub(crate) bytes: u64,
}

/// DXGI's budget change event and the adapter it is registered with.
struct BudgetEvent {
    adapter: IDXGIAdapter3,
    event: HANDLE,
    cookie: u32,
}

impl BudgetEvent {
    fn register(device: &ID3D12Device) -> Option<BudgetEvent> {
        // SAFETY: plain DXGI calls; the event is closed if it is not kept.
        unsafe {
            let luid = device.GetAdapterLuid();
            let adapter: IDXGIAdapter3 =
                CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
                    .and_then(|f| f.EnumAdapterByLuid(luid))
                    .ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            match adapter.RegisterVideoMemoryBudgetChangeNotificationEvent(event) {
                Ok(cookie) => Some(BudgetEvent {
                    adapter,
                    event,
                    cookie,
                }),
                Err(e) => {
                    tracing::info!(error = %e, "no video memory budget notification");
                    let _ = CloseHandle(event);
                    None
                }
            }
        }
    }

    /// Whether the budget changed since the last look.
    fn fired(&self) -> bool {
        // SAFETY: our own event.
        unsafe { WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for BudgetEvent {
    fn drop(&mut self) {
        // SAFETY: registered and made by `register`.
        unsafe {
            self.adapter
                .UnregisterVideoMemoryBudgetChangeNotification(self.cookie);
            let _ = CloseHandle(self.event);
        }
    }
}

impl Pool {
    /// A pool on `device`; its thread starts filling the reserve.
    pub(crate) fn new(device: ID3D12Device, config: Config, budget: Budget) -> Pool {
        let changes = BudgetEvent::register(&device);
        let inner = Arc::new(Inner {
            device,
            config,
            budget,
            state: Mutex::new(State {
                classes: [Vec::new(), Vec::new()],
                releasing: Vec::new(),
                stop: false,
                woken: true,
                next_id: 1,
                over_budget: false,
                stats: Stats::default(),
            }),
            wake: Condvar::new(),
            changes,
            zeroed: AtomicBool::new(false),
        });
        let thread = {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name("xtp-texture-heaps".into())
                .spawn(move || inner.run())
        };
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(e) => {
                tracing::warn!(error = %e, "no texture heap thread: heaps made when textures need them");
                None
            }
        };
        Pool { inner, thread }
    }

    /// `d` (a host texture description, no target) placed in the pool in
    /// `state`; `Err` says why not (then it is committed).
    pub(crate) fn place(
        &self,
        d: &D3D12_RESOURCE_DESC,
        state: D3D12_RESOURCE_STATES,
    ) -> Result<Placed, &'static str> {
        let inner = &self.inner;
        let refused = |why| {
            inner.lock().stats.not_placed += 1;
            Err(why)
        };
        let Some((class, desc, len)) = inner.class_of(d) else {
            return refused("no 4 or 64 KiB placement");
        };
        if len > inner.config.largest {
            return refused("too large for the pool");
        }
        let taken = match inner.take(class, len) {
            Some(taken) => taken,
            None => {
                // The reserve is gone: a heap now, not waited for.
                let size = inner.config.heap[class.index()].max(len.next_multiple_of(KIB_64));
                if !inner.add_heap(class, size, true) {
                    return refused("no heap (budget or driver)");
                }
                match inner.take(class, len) {
                    Some(taken) => taken,
                    None => return refused("no room in a new heap"),
                }
            }
        };
        let (heap, id, offset, residency, made) = taken;
        let slot = Slot {
            inner: inner.clone(),
            class,
            heap: id,
            offset,
            len,
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: a live heap, a range of it no other live resource uses,
        // aligned for the description's placement; the out pointer is ours.
        let placed = unsafe {
            inner
                .device
                .CreatePlacedResource(&heap, offset, &desc, state, None, &mut texture)
        };
        match (placed, texture) {
            (Ok(()), Some(texture)) => {
                // A heap just made is usually resident within a few
                // milliseconds: its first textures wait that long, so the
                // title can use them at once. Past VidMm's ceiling the rest
                // are handed out not resident, without waiting.
                if !residency.is_ready() {
                    let left = (made + HEAP_WAIT).saturating_duration_since(Instant::now());
                    if left.is_zero() || !residency.wait(left.min(PLACE_WAIT)) {
                        inner.lock().stats.early += 1;
                    }
                }
                Ok(Placed {
                    texture,
                    slot,
                    residency,
                    bytes: len,
                })
            }
            (result, _) => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(
                        ?result,
                        ?desc,
                        "a placed host texture refused: committed instead"
                    )
                });
                drop(slot);
                refused("placement refused")
            }
        }
    }

    /// What it did since it was made.
    pub(crate) fn stats(&self) -> Stats {
        self.inner.lock().stats
    }

    /// What it holds now.
    pub(crate) fn holding(&self) -> Holding {
        self.inner.holding()
    }

    /// The frame log's part: what the pool holds and did.
    pub(crate) fn report(&self) -> String {
        let (h, s) = (self.holding(), self.stats());
        format!(
            "texture pool: {} heaps ({} not resident yet), {} MiB, {} MiB placed in {} textures; \
             so far {} heaps made ({} by the title's thread), {} released, {} textures not placed, \
             {} handed out before their heap was resident, {} heaps held back by the budget",
            h.heaps,
            h.pending,
            h.bytes / MIB,
            h.used / MIB,
            h.textures,
            s.heaps,
            s.emergency,
            s.released,
            s.not_placed,
            s.early,
            s.over_budget
        )
    }

    /// Wakes the thread and waits until it has nothing left to make (at
    /// most `timeout`): tests.
    #[cfg(test)]
    pub(crate) fn settle(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        while Instant::now() < until {
            let short = {
                let s = self.inner.lock();
                Class::ALL.iter().any(|&c| self.inner.short(&s, c))
            };
            if !short {
                return true;
            }
            self.inner.notify();
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.inner.lock().stop = true;
        self.inner.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn notify(&self) {
        self.lock().woken = true;
        self.wake.notify_all();
    }

    /// The class of `d`, its description as placed and the bytes it takes
    /// (a multiple of the class's alignment); `None` for an alignment the
    /// pool has no class for.
    fn class_of(&self, d: &D3D12_RESOURCE_DESC) -> Option<(Class, D3D12_RESOURCE_DESC, u64)> {
        let size = |desc: &D3D12_RESOURCE_DESC| {
            // SAFETY: a host description.
            let info = unsafe {
                self.device
                    .GetResourceAllocationInfo(0, std::slice::from_ref(desc))
            };
            (info.SizeInBytes != u64::MAX && info.SizeInBytes > 0).then_some(info)
        };
        let mut desc = *d;
        let small = Class::Small.alignment();
        if may_be_small(d) {
            desc.Alignment = small;
            if let Some(info) = size(&desc)
                && info.Alignment == small
            {
                return Some((Class::Small, desc, info.SizeInBytes.next_multiple_of(small)));
            }
        }
        desc.Alignment = 0;
        let info = size(&desc)?;
        let normal = Class::Normal.alignment();
        (info.Alignment <= normal).then(|| {
            (
                Class::Normal,
                desc,
                info.SizeInBytes.next_multiple_of(normal),
            )
        })
    }

    /// Free bytes of `class`.
    fn free(s: &State, class: Class) -> u64 {
        s.classes[class.index()]
            .iter()
            .map(|h| h.ranges.free())
            .sum()
    }

    /// Whether `class` has less than its reserve free.
    fn short(&self, s: &State, class: Class) -> bool {
        Self::free(s, class) < self.config.reserve[class.index()]
    }

    /// `len` bytes of a heap of `class`: the tightest fit in a resident
    /// heap, else in one still becoming resident. The thread is woken if
    /// the reserve runs short.
    fn take(
        &self,
        class: Class,
        len: u64,
    ) -> Option<(ID3D12Heap, u64, u64, Arc<Residency>, Instant)> {
        let mut s = self.lock();
        let heaps = &mut s.classes[class.index()];
        let (_, _, at) = heaps
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                h.ranges
                    .fit(len)
                    .map(|fit| (!h.residency.is_ready(), fit, i))
            })
            .min()?;
        let h = &mut heaps[at];
        let offset = h.ranges.take(len)?;
        h.textures += 1;
        let taken = (h.heap.clone(), h.id, offset, h.residency.clone(), h.made);
        s.stats.placed += 1;
        if self.short(&s, class) {
            s.woken = true;
            self.wake.notify_all();
        }
        Some(taken)
    }

    /// Gives a texture's range back; its heap goes if it is empty and the
    /// reserve does not need it.
    fn give_back(&self, slot: &Slot) {
        let mut s = self.lock();
        let reserve = self.config.reserve[slot.class.index()];
        let heaps = &mut s.classes[slot.class.index()];
        let Some(at) = heaps.iter().position(|h| h.id == slot.heap) else {
            return;
        };
        heaps[at].ranges.give(slot.offset, slot.len);
        heaps[at].textures -= 1;
        if heaps[at].textures > 0 {
            return;
        }
        let others: u64 = heaps
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != at)
            .map(|(_, h)| h.ranges.free())
            .sum();
        if others >= reserve {
            let heap = heaps.swap_remove(at);
            s.releasing.push(heap.heap);
            s.stats.released += 1;
            s.woken = true;
            self.wake.notify_all();
        }
    }

    /// Makes a heap of `size` bytes for `class`, not resident, and asks for
    /// it to become resident (not waited for). False, logged, past the
    /// budget or if the driver refuses.
    fn add_heap(&self, class: Class, size: u64, emergency: bool) -> bool {
        let pending: u64 = {
            let s = self.lock();
            s.classes
                .iter()
                .flatten()
                .filter(|h| !h.residency.is_ready())
                .map(|h| h.size)
                .sum()
        };
        if let Some((budget, usage)) = (self.budget)(&self.device)
            && usage.saturating_add(pending).saturating_add(size) > budget
        {
            let mut s = self.lock();
            s.stats.over_budget += 1;
            if !std::mem::replace(&mut s.over_budget, true) {
                tracing::warn!(
                    class = class.name(),
                    mib = size / MIB,
                    budget_mib = budget / MIB,
                    usage_mib = usage / MIB,
                    pending_mib = pending / MIB,
                    "no texture heap: past the video memory budget (textures made committed meanwhile, within the budget)"
                );
            }
            return false;
        }
        let started = Instant::now();
        let Some(heap) = self.make_heap(size) else {
            return false;
        };
        let pageable: Option<ID3D12Pageable> = heap.cast().ok();
        let residency = Arc::new(Residency::ask(&self.device, vec![pageable]));
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut s = self.lock();
        if std::mem::replace(&mut s.over_budget, false) {
            tracing::info!("texture heaps within the video memory budget again");
        }
        let id = s.next_id;
        s.next_id += 1;
        s.stats.heaps += 1;
        s.stats.asked += 1;
        if emergency {
            s.stats.emergency += 1;
        }
        let n = s.stats.heaps;
        s.classes[class.index()].push(Heap {
            id,
            heap,
            size,
            ranges: Ranges::new(size),
            textures: 0,
            residency,
            made: Instant::now(),
        });
        drop(s);
        if emergency || ms > 100.0 || n <= 4 || n.is_power_of_two() {
            tracing::info!(
                n,
                class = class.name(),
                mib = size / MIB,
                ms,
                emergency,
                "texture heap made (not resident yet)"
            );
        }
        true
    }

    /// A default heap of `size` bytes for textures that are no targets,
    /// not resident and (if the runtime can) not zeroed.
    fn make_heap(&self, size: u64) -> Option<ID3D12Heap> {
        let flags = |zeroed: bool| {
            let base =
                D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT;
            if zeroed {
                base
            } else {
                base | D3D12_HEAP_FLAG_CREATE_NOT_ZEROED
            }
        };
        let desc = |zeroed| D3D12_HEAP_DESC {
            SizeInBytes: size,
            Properties: D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            Alignment: 0,
            Flags: flags(zeroed),
        };
        let make = |zeroed| {
            let mut heap: Option<ID3D12Heap> = None;
            // SAFETY: a plain heap description; the out pointer is ours.
            unsafe { self.device.CreateHeap(&desc(zeroed), &mut heap) }.map(|()| heap)
        };
        let zeroed = self.zeroed.load(Ordering::Relaxed);
        let made = match make(zeroed) {
            Err(e) if !zeroed => {
                tracing::info!(error = %e, "texture heaps refuse CREATE_NOT_ZEROED: made zeroed");
                self.zeroed.store(true, Ordering::Relaxed);
                make(true)
            }
            made => made,
        };
        match made {
            Ok(Some(heap)) => Some(heap),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, mib = size / MIB, "no texture heap: the driver refused it");
                None
            }
        }
    }

    fn holding(&self) -> Holding {
        let s = self.lock();
        let mut h = Holding::default();
        for heap in s.classes.iter().flatten() {
            h.heaps += 1;
            h.pending += u64::from(!heap.residency.is_ready());
            h.bytes += heap.size;
            h.used += heap.size - heap.ranges.free();
            h.textures += u64::from(heap.textures);
        }
        h
    }

    /// The thread: heaps released, the budget followed, refused heaps asked
    /// again, the reserve kept; then sleep until woken (or a second).
    fn run(&self) {
        loop {
            let releasing = {
                let mut s = self.lock();
                if s.stop {
                    return;
                }
                s.woken = false;
                std::mem::take(&mut s.releasing)
            };
            drop(releasing);
            if self.changes.as_ref().is_some_and(BudgetEvent::fired) {
                self.budget_changed();
            }
            // Looking at a refused heap asks for it again when due.
            let _ = self.holding();
            for class in Class::ALL {
                while self.short(&self.lock(), class) {
                    if !self.add_heap(class, self.config.heap[class.index()], false) {
                        break;
                    }
                }
            }
            let s = self.lock();
            if s.stop {
                return;
            }
            let _ = self
                .wake
                .wait_timeout_while(s, Duration::from_secs(1), |s| !s.stop && !s.woken);
        }
    }

    /// The budget changed: logged, and past it every empty heap goes.
    fn budget_changed(&self) {
        let Some((budget, usage)) = (self.budget)(&self.device) else {
            return;
        };
        tracing::info!(
            budget_mib = budget / MIB,
            usage_mib = usage / MIB,
            "video memory budget changed"
        );
        if usage <= budget {
            return;
        }
        let mut s = self.lock();
        let mut gone = Vec::new();
        for heaps in &mut s.classes {
            let (empty, kept): (Vec<Heap>, Vec<Heap>) = std::mem::take(heaps)
                .into_iter()
                .partition(|h| h.textures == 0);
            *heaps = kept;
            gone.extend(empty.into_iter().map(|h| h.heap));
        }
        s.stats.released += gone.len() as u64;
        drop(s);
        if !gone.is_empty() {
            tracing::info!(
                heaps = gone.len(),
                "empty texture heaps released: past the budget"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    fn texture(width: u64, height: u32, format: DXGI_FORMAT, mips: u16) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: width,
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: mips,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        }
    }

    #[test]
    fn the_small_rule_counts_standard_tiles() {
        // 64x64 RGBA8 is 4 tiles of 32x32; 128x128 is 16; 256x128 is 32.
        assert!(may_be_small(&texture(
            64,
            64,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            1
        )));
        assert!(may_be_small(&texture(
            128,
            128,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            1
        )));
        assert!(!may_be_small(&texture(
            256,
            128,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            1
        )));
        // BC7: 16-byte blocks, 16x16 blocks a tile: 256x256 texels is one.
        assert!(may_be_small(&texture(256, 256, DXGI_FORMAT_BC7_UNORM, 9)));
        assert!(!may_be_small(&texture(
            1024,
            1024,
            DXGI_FORMAT_BC7_UNORM,
            9
        )));
        // Targets, samples and arrays never.
        let mut rt = texture(16, 16, DXGI_FORMAT_R8G8B8A8_UNORM, 1);
        rt.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
        assert!(!may_be_small(&rt));
        let mut array = texture(16, 16, DXGI_FORMAT_R8G8B8A8_UNORM, 1);
        array.DepthOrArraySize = 6;
        assert!(!may_be_small(&array));
        // Formats without a known element size: no.
        assert!(!may_be_small(&texture(16, 16, DXGI_FORMAT_NV12, 1)));
    }

    #[test]
    fn the_pool_is_asked_for_by_its_variable() {
        assert!(asks_pool(Some("1")));
        assert!(!asks_pool(Some("0")));
        assert!(!asks_pool(Some("")));
        assert!(!asks_pool(None), "off by default");
    }

    #[test]
    fn sizes_follow_the_heap_size() {
        let c = Config::sized(256 * MIB, 512 * MIB);
        assert_eq!(c.heap, [32 * MIB, 256 * MIB]);
        assert_eq!(c.reserve, [64 * MIB, 512 * MIB]);
        assert_eq!(c.largest, 64 * MIB);
    }

    /// A pool on the test device with small heaps (1 MiB a 64 KiB heap).
    fn pool(test: &str, budget: Budget) -> Option<(crate::testgpu::Gpu, Pool)> {
        let gpu = crate::testgpu::device(test)?;
        let pool = Pool::new(gpu.device.clone(), Config::sized(MIB, MIB), budget);
        Some((gpu, pool))
    }

    fn unlimited() -> Budget {
        Box::new(|_| Some((u64::MAX / 4, 0)))
    }

    /// Textures of both classes are placed at their alignment, reuse what
    /// is given back, and cost no residency request of their own: one a
    /// heap. Empty heaps beyond the reserve go.
    #[test]
    fn textures_are_placed_by_class_and_ranges_reused() {
        let test = "resource::pool::tests::textures_are_placed_by_class_and_ranges_reused";
        let Some((_gpu, pool)) = pool(test, unlimited()) else {
            return;
        };
        assert!(
            pool.settle(Duration::from_secs(5)),
            "the thread fills the reserve"
        );
        let before = pool.stats();
        assert_eq!(before.emergency, 0);
        let state = D3D12_RESOURCE_STATE_COPY_DEST;
        let small = texture(64, 64, DXGI_FORMAT_R8G8B8A8_UNORM, 1);
        let normal = texture(256, 256, DXGI_FORMAT_R8G8B8A8_UNORM, 1);
        let mut held = Vec::new();
        for _ in 0..24 {
            for d in [&small, &normal] {
                let p = pool.place(d, state).expect("placed");
                let (class, offset, len) = p.slot.place();
                assert_eq!(offset % class.alignment(), 0);
                assert_eq!(len % class.alignment(), 0);
                assert_eq!(
                    class,
                    if std::ptr::eq(d, &small) {
                        Class::Small
                    } else {
                        Class::Normal
                    }
                );
                held.push(p);
            }
        }
        let made = pool.stats();
        assert_eq!(made.placed - before.placed, 48);
        assert_eq!(
            made.asked, made.heaps,
            "one residency request a heap, none a texture"
        );
        assert!(made.heaps < 48, "{made:?}");
        // A range given back is taken again.
        let gone = held.remove(1);
        let (class, offset, _) = gone.slot.place();
        drop(gone);
        let again = pool.place(&normal, state).unwrap();
        assert_eq!(again.slot.place().0, class);
        assert_eq!(again.slot.place().1, offset, "the same place");
        held.push(again);
        // Everything back: the heaps the reserve does not need go.
        let holding = pool.holding();
        assert_eq!(holding.textures, 48);
        drop(held);
        let after = pool.holding();
        assert_eq!((after.textures, after.used), (0, 0));
        assert!(after.heaps < holding.heaps, "{after:?} after {holding:?}");
        assert!(pool.stats().released > 0);
        // The reserve is kept (1 MiB for 64 KiB textures, 128 KiB small).
        assert!(after.bytes >= MIB + MIB / 8, "{after:?}");
    }

    /// The heap of a placed texture becomes resident; the texture then
    /// holds what a copy writes into it.
    #[test]
    fn a_placed_texture_holds_what_is_copied_into_it() {
        let test = "resource::pool::tests::a_placed_texture_holds_what_is_copied_into_it";
        let Some((gpu, pool)) = pool(test, unlimited()) else {
            return;
        };
        let host = gpu.host();
        let d = texture(16, 16, DXGI_FORMAT_R8G8B8A8_UNORM, 1);
        let placed = pool.place(&d, D3D12_RESOURCE_STATE_COPY_DEST).unwrap();
        assert!(placed.residency.wait(Duration::from_secs(5)));
        let rows = 16 * 256u64;
        let (upload, data) = host.upload_buffer(rows).unwrap();
        for i in 0..rows as usize {
            // SAFETY: inside the mapped buffer.
            unsafe { data.add(i).write((i % 251) as u8) };
        }
        let list = host.new_list().unwrap();
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&placed.texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&upload) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        Width: 16,
                        Height: 16,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        list.close();
        host.execute(&[&list]);
        host.drain();
        const COPY_DEST: u32 = 0x400;
        let read = super::super::tests_support::read_back(host, &placed.texture, COPY_DEST);
        for row in 0..16usize {
            let at = row * 256;
            let want: Vec<u8> = (at..at + 64).map(|i| (i % 251) as u8).collect();
            assert_eq!(read[at..at + 64], want[..], "row {row}");
        }
        assert_eq!(host.log_messages(), None);
    }

    /// Past the budget no heap is made and the texture is not placed (it is
    /// made committed, within the budget); nothing is asked of VidMm.
    #[test]
    fn past_the_budget_no_heap_is_made() {
        let test = "resource::pool::tests::past_the_budget_no_heap_is_made";
        let Some((_gpu, pool)) = pool(test, Box::new(|_| Some((MIB, MIB)))) else {
            return;
        };
        let d = texture(256, 256, DXGI_FORMAT_R8G8B8A8_UNORM, 1);
        assert!(pool.place(&d, D3D12_RESOURCE_STATE_COPY_DEST).is_err());
        let s = pool.stats();
        assert_eq!((s.heaps, s.asked, s.placed), (0, 0, 0));
        assert!(s.over_budget > 0 && s.not_placed == 1, "{s:?}");
        assert!(pool.report().contains("held back by the budget"));
    }

    /// The process's private bytes (WARP's video memory is the process's
    /// own).
    fn private_bytes() -> u64 {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: plain data.
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        // SAFETY: the structure and its size are ours.
        let ok = unsafe {
            K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counters).cast(), counters.cb)
        };
        if ok == 0 {
            0
        } else {
            counters.PrivateUsage as u64
        }
    }

    /// One path of the benchmark: `n` textures made by `make` (which says
    /// how many residency requests it made), each creation timed, then
    /// `ready` waited for; video memory and private bytes before and after.
    fn bench_path<T>(
        name: &str,
        host: &crate::host::Host,
        n: usize,
        mut make: impl FnMut() -> T,
        ready: impl Fn(&[T]) -> bool,
        asked: impl Fn(&[T]) -> u64,
    ) -> String {
        let memory = || crate::dxgi::video_memory(&host.device).map_or(0, |(_, usage)| usage);
        let (vram0, private0) = (memory(), private_bytes());
        let started = Instant::now();
        let mut times = Vec::with_capacity(n);
        let mut made = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            made.push(make());
            times.push(t.elapsed());
        }
        let creating = started.elapsed();
        while !ready(&made) {
            assert!(
                started.elapsed() < Duration::from_secs(600),
                "{name}: never resident"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let all = started.elapsed();
        let (vram1, private1) = (memory(), private_bytes());
        let requests = asked(&made);
        drop(made);
        times.sort();
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        let mean = us(creating) / n as f64;
        let at = |q: f64| us(times[((n as f64 * q) as usize).min(n - 1)]);
        let per = |a: u64, b: u64| b.saturating_sub(a) as f64 / n as f64 / 1024.0;
        format!(
            "| {name} | {n} | {mean:.1} | {:.1} | {:.1} | {:.1} | {:.0} | {:.0} | {:.0} | {requests} |",
            at(0.5),
            at(0.99),
            us(*times.last().unwrap()),
            all.as_secs_f64() * 1000.0,
            per(vram0, vram1),
            per(private0, private1),
        )
    }

    /// The micro-benchmark of `docs/analysis/vram.md`: `XTP_POOL_BENCH`
    /// (5,000) BC7 1024² textures of 9 mips, made as committed resources
    /// resident at creation (before 2026-11-24), committed not resident and
    /// made resident one by one (dark-world's path, each waited for), and
    /// placed in the pool (256 MiB heaps). Run on its own:
    /// `cargo test -p xtp-gpu --release -- --ignored --nocapture pool::tests::bench`.
    #[test]
    #[ignore = "a benchmark: several GB of video memory"]
    fn bench_5000_bc7_textures() {
        let test = "resource::pool::tests::bench_5000_bc7_textures";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let n: usize = std::env::var("XTP_POOL_BENCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5000);
        let d = texture(1024, 1024, DXGI_FORMAT_BC7_UNORM, 9);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let committed = |flags: D3D12_HEAP_FLAGS| {
            let mut t: Option<ID3D12Resource> = None;
            // SAFETY: plain creation.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        flags,
                        &d,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut t,
                    )
                    .unwrap();
            }
            t.unwrap()
        };
        // SAFETY: a host description.
        let info = unsafe { host.device.GetResourceAllocationInfo(0, &[d]) };
        let mut rows = vec![
            format!(
                "WARP: {} ({} KiB a texture as D3D12 sizes it, {} alignment)",
                n,
                info.SizeInBytes / 1024,
                info.Alignment
            ),
            "| path | textures | µs/texture (mean) | p50 µs | p99 µs | max µs | ms until all resident | VRAM KiB/texture | private KiB/texture | residency requests |".into(),
            "|---|---|---|---|---|---|---|---|---|---|".into(),
        ];
        rows.push(bench_path(
            "committed, resident at creation",
            host,
            n,
            || committed(D3D12_HEAP_FLAG_NONE),
            |_| true,
            |made| made.len() as u64,
        ));
        rows.push(bench_path(
            "committed, not resident + EnqueueMakeResident each",
            host,
            n,
            || {
                let t = committed(D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT);
                let r = Residency::ask(&host.device, vec![t.cast().ok()]);
                assert!(r.wait(Duration::from_secs(5)));
                (t, r)
            },
            |made| made.iter().all(|(_, r)| r.is_ready()),
            |made| made.len() as u64,
        ));
        let pool = Pool::new(
            host.device.clone(),
            Config::sized(256 * MIB, 512 * MIB),
            Box::new(crate::dxgi::video_memory),
        );
        assert!(pool.settle(Duration::from_secs(30)));
        let before = pool.stats();
        let at_start = pool.holding().bytes;
        rows.push(bench_path(
            "pooled (256 MiB heaps, 512 MiB reserve)",
            host,
            n,
            || pool.place(&d, D3D12_RESOURCE_STATE_COPY_DEST).unwrap(),
            |made| made.iter().all(|p| p.residency.is_ready()),
            |_| pool.stats().asked - before.asked,
        ));
        let s = pool.stats();
        rows.push(format!(
            "pool: {} heaps made ({} by the title's thread), {} MiB held before the run; {:?}",
            s.heaps - before.heaps,
            s.emergency - before.emergency,
            at_start / MIB,
            pool.holding()
        ));
        drop(pool);
        eprintln!("{}", rows.join("\n"));
    }

    /// Larger textures than a quarter heap and alignments the pool has no
    /// class for are not placed.
    #[test]
    fn what_the_pool_does_not_take() {
        let test = "resource::pool::tests::what_the_pool_does_not_take";
        let Some((_gpu, pool)) = pool(test, unlimited()) else {
            return;
        };
        let big = texture(1024, 1024, DXGI_FORMAT_R8G8B8A8_UNORM, 1);
        assert_eq!(
            pool.place(&big, D3D12_RESOURCE_STATE_COPY_DEST).err(),
            Some("too large for the pool")
        );
    }
}
