//! The registry on the host device (the hardware adapter unless
//! `XTP_TEST_ADAPTER` says otherwise) and on WARP. They take over
//! `tests/uma_feasibility.rs`' import matrix (xmem's memory, both ways, in
//! place) and its two "under an open heap" cases, through the registry.
//!
//! GPU work here is copies on a copy queue over a few MiB: shader stores of
//! 16 bytes into imported memory hung an RTX 4080 (uma-feasibility
//! section 5).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIFactory4,
};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory as vm;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::*;

const PAGE: usize = 64 * KIB;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;
/// What the host answers a call it refuses under an open heap.
const ERROR_INVALID_PARAMETER: i32 = 87;

#[test]
fn pages_widen_to_host_pages() {
    assert_eq!(pages(4097, 1), (4096, 4096));
    assert_eq!(pages(4096, 4096), (4096, 4096));
    assert_eq!(pages(4095, 2), (0, 8192));
    assert_eq!(pages(8192, 0), (8192, 0));
    assert_eq!(pages(usize::MAX - 10, 100).0, usize::MAX & !4095);
}

#[test]
fn spans_follow_commits_grains_and_open_heaps() {
    let base = 1 << 32;
    let k = 64 * KIB;
    // Cut inward to the grain: a run from 4 KiB in to 4 KiB past 3 grains.
    assert_eq!(
        spans(base, 16 * k, k, &[(base + 4096, 3 * k)], &[]),
        [(base + k, 2 * k)]
    );
    // Less than a grain: nothing.
    assert_eq!(spans(base, 16 * k, k, &[(base + 4096, k)], &[]), []);
    // The region's end is a boundary, on the grain or not.
    let len = 3 * k + 8192;
    assert_eq!(
        spans(base, len, k, &[(base + k, 2 * k + 8192)], &[]),
        [(base + k, 2 * k + 8192)]
    );
    // Open heaps are left out, whatever is on either side.
    let runs = [(base, 10 * k)];
    assert_eq!(
        spans(
            base,
            16 * k,
            k,
            &runs,
            &[(base + 2 * k, k), (base + 5 * k, 2 * k)]
        ),
        [(base, 2 * k), (base + 3 * k, 2 * k), (base + 7 * k, 3 * k)]
    );
    assert_eq!(spans(base, 16 * k, k, &runs, &[(base, 10 * k)]), []);
    // Two runs, each on its own.
    assert_eq!(
        spans(base, 16 * k, k, &[(base, k), (base + 4 * k, k)], &[]),
        [(base, k), (base + 4 * k, k)]
    );
    // Pieces of at most `MAX`.
    let big = 9 << 30;
    assert_eq!(
        spans(base, big, k, &[(base, big)], &[]),
        [
            (base, MAX),
            (base + MAX, MAX),
            (base + 2 * MAX, big - 2 * MAX)
        ]
    );
    // Physical pages: whole blocks of 64 MiB, the last one shorter.
    let block = Kind::Physical.grain();
    let len = 2 * block + 3 * k;
    assert_eq!(
        spans(base, len, block, &[(base, block + k)], &[]),
        [(base, block)]
    );
    assert_eq!(
        spans(
            base,
            len,
            block,
            &[(base + block - k, block + 3 * k + k)],
            &[]
        ),
        [(base + block, block + 3 * k)]
    );
    // Pieces of whole blocks: 63 of them.
    let big = 5 << 30;
    let piece = 63 * block;
    assert_eq!(
        spans(base, big, block, &[(base, big)], &[]),
        [(base, piece), (base + piece, big - piece)]
    );
}

/// With `XTP_UMA` unset nothing opens, and the hooks change nothing.
#[test]
fn uma_is_off_unless_asked() {
    if std::env::var_os("XTP_UMA").is_some() {
        return; // asked for by whoever runs the tests
    }
    assert!(!enabled());
    let a = reserve(PAGE, PAGE);
    reserved(a, PAGE);
    committed(a, PAGE);
    settle();
    assert!(lookup(a, 1).is_none());
    assert!(ready().is_err());
    let change = changing(a, PAGE);
    assert!(change.call(|| free(a).is_ok()));
    change.done(true);
}

/// xmem's memory, write-watched and committed, opened in place: lookups
/// give each byte's buffer and offset; the GPU reads what the CPU wrote and
/// the CPU sees what the GPU wrote, with write watch still seeing the CPU.
#[test]
fn heaps_show_title_memory_in_place() {
    const TEST: &str = "heaps_show_title_memory_in_place";
    const LEN: usize = 4 * MIB;
    let (_gpu, listed) = devices(TEST);
    for (name, device) in listed {
        let Some(registry) = registry(TEST, name, &device, all()) else {
            continue;
        };
        let (a, b) = (reserve(LEN, LEN), reserve(LEN, LEN));
        registry.reserved(a, LEN);
        registry.reserved(b, LEN);
        registry.settle();
        assert_eq!(registry.heaps(), sorted([(a, LEN), (b, LEN)]), "{name}");
        assert_eq!(registry.imported(), 2 * LEN);

        {
            // Held, these keep their heap open: dropped before the frees.
            let whole = registry.lookup(a, LEN).expect("all of it");
            assert_eq!(whole.offset, 0);
            // SAFETY: a plain query of a buffer.
            assert_eq!(whole.gpu, unsafe { whole.buffer.GetGPUVirtualAddress() });
            let inside = registry.lookup(a + 100, 16).expect("inside");
            assert_eq!(inside.buffer.as_raw(), whole.buffer.as_raw());
            assert_eq!((inside.offset, inside.gpu), (100, whole.gpu + 100));
        }
        assert!(registry.lookup(a + LEN - 8, 16).is_none(), "past the end");
        assert!(registry.lookup(a - 8, 16).is_none(), "before it");

        // SAFETY: our own reservation, write-watched.
        unsafe { vm::ResetWriteWatch(a as *mut c_void, LEN) };
        fill(a, LEN / 2, 1);
        assert_eq!(
            written_pages(a, LEN),
            LEN / 2 / HOST_PAGE,
            "{name}: write watch"
        );
        let half = |at| registry.lookup(at, LEN / 2).expect("open");
        Copier::new(&device).copy(&half(b), &half(a), LEN / 2);
        assert_eq!(wrong(b, b, LEN / 2, 1), 0, "{name}: the GPU's copy");

        for at in [a, b] {
            let change = registry.changing(at, LEN);
            assert!(change.call(|| free(at).is_ok()), "{name}");
            change.done(true);
        }
        assert_eq!(registry.heaps(), []);
        assert_eq!(registry.imported(), 0);
        assert!(registry.lookup(a, 1).is_none());
    }
}

/// The spike's "virtual memory decommitted under an open heap": the host
/// refuses, so the registry releases the heap first, then opens heaps
/// again over what remains, and over what the title commits again.
#[test]
fn decommit_and_release_succeed_under_open_heaps() {
    const TEST: &str = "decommit_and_release_succeed_under_open_heaps";
    const LEN: usize = 16 * PAGE;
    let (_gpu, listed) = devices(TEST);
    for (name, device) in listed {
        let Some(registry) = registry(TEST, name, &device, all()) else {
            continue;
        };
        if !ranges(TEST, name, &device) {
            continue;
        }
        let a = reserve(LEN, LEN);
        registry.reserved(a, LEN);
        registry.settle();
        fill(a, LEN, 3);
        // Unhooked, the host refuses: the heap pins its pages.
        assert_eq!(decommit(a + 4 * PAGE, PAGE), Err(ERROR_INVALID_PARAMETER));

        let change = registry.changing(a + 4 * PAGE, PAGE);
        assert!(
            change.call(|| decommit(a + 4 * PAGE, PAGE).is_ok()),
            "{name}"
        );
        change.done(false);
        registry.settle();
        assert_eq!(
            registry.heaps(),
            [(a, 4 * PAGE), (a + 5 * PAGE, 11 * PAGE)],
            "{name}: what remains"
        );
        assert!(registry.lookup(a + 4 * PAGE, 1).is_none());
        assert_eq!(
            decommit(a + 6 * PAGE, PAGE),
            Err(ERROR_INVALID_PARAMETER),
            "{name}: open again"
        );
        // What the title keeps is intact, and the GPU's in place again.
        assert_eq!(wrong(a, a, 4 * PAGE, 3), 0);
        assert_eq!(wrong(a, a + 5 * PAGE, 11 * PAGE, 3), 0);
        let b = reserve(LEN, LEN);
        registry.reserved(b, LEN);
        registry.settle();
        {
            let from = registry.lookup(a + 5 * PAGE, 4 * PAGE).expect("reopened");
            let to = registry.lookup(b, 4 * PAGE).expect("open");
            Copier::new(&device).copy(&to, &from, 4 * PAGE);
        }
        // `b` now holds the words from page 5 of `a` on.
        assert_eq!(wrong(b - 5 * PAGE, b, 4 * PAGE, 3), 0, "{name}");

        // A `Placed` held keeps its heap open, released by the registry or
        // not: the title's call fails.
        let held = registry.lookup(a, PAGE).expect("open");
        let change = registry.changing(a + PAGE, PAGE);
        assert_eq!(decommit(a + PAGE, PAGE), Err(ERROR_INVALID_PARAMETER));
        drop(held);
        change.done(false);
        registry.settle();
        assert_eq!(registry.heaps().len(), 3, "{name}: `a`'s two and `b`'s");

        // The title commits the page again: it gets a heap of its own.
        commit(a + 4 * PAGE, PAGE);
        registry.committed(a + 4 * PAGE, PAGE);
        registry.settle();
        assert!(registry.lookup(a + 4 * PAGE, PAGE).is_some(), "{name}");
        assert_eq!(registry.heaps().len(), 4);

        for at in [a, b] {
            let change = registry.changing(at, LEN);
            assert!(change.call(|| free(at).is_ok()), "{name}");
            change.done(true);
        }
        assert_eq!(registry.heaps(), []);
        assert_eq!(registry.imported(), 0);
    }
}

/// Physical pages: a heap on the runtime's permanent view of a section,
/// once a block of it is all committed. The GPU's writes there show through
/// the title's views of the same pages, and the title still unmaps its
/// views (the spike's "physical pages remapped under an open heap" is
/// refused only for the view the heap is on).
#[test]
fn physical_pages_open_on_the_permanent_view() {
    const TEST: &str = "physical_pages_open_on_the_permanent_view";
    const LEN: usize = 2 * MIB;
    let (_gpu, listed) = devices(TEST);
    for (name, device) in listed {
        let Some(registry) = registry(TEST, name, &device, all()) else {
            continue;
        };
        let section = Section::new(LEN);
        registry.mapped(section.view, LEN);
        section.commit(0, LEN / 2);
        registry.committed(section.view, LEN / 2);
        registry.settle();
        assert_eq!(
            registry.heaps(),
            [],
            "{name}: a block opens once all committed"
        );
        section.commit(LEN / 2, LEN / 2);
        registry.committed(section.view + LEN / 2, LEN / 2);
        registry.settle();
        assert_eq!(registry.heaps(), [(section.view, LEN)], "{name}");

        let title = section.title_view();
        let a = reserve(LEN, LEN);
        registry.reserved(a, LEN);
        registry.settle();
        fill(a, LEN, 7);
        {
            let to = registry.lookup(section.view, LEN).expect("open");
            let from = registry.lookup(a, LEN).expect("open");
            Copier::new(&device).copy(&to, &from, LEN);
        }
        assert_eq!(
            wrong(title, title, LEN, 7),
            0,
            "{name}: through the title's view"
        );

        assert!(unmap_title_view(title), "{name}: the title's view unmaps");
        assert!(registry.lookup(section.view, LEN).is_some());

        let change = registry.changing(section.view, LEN);
        assert!(
            change.call(|| section.unmap()),
            "{name}: our view unmaps once its heap is gone"
        );
        change.done(true);
        let change = registry.changing(a, LEN);
        assert!(change.call(|| free(a).is_ok()));
        change.done(true);
        assert_eq!(registry.heaps(), []);
    }
}

/// A reservation committed in parts: a heap over each part committed, at
/// interior addresses too (`OpenExistingHeapFromAddress1`); none across
/// parts, none over less than 64 KiB.
#[test]
fn partial_commits_open_what_is_committed() {
    const TEST: &str = "partial_commits_open_what_is_committed";
    const LEN: usize = 8 * MIB;
    let (_gpu, listed) = devices(TEST);
    for (name, device) in listed {
        let Some(registry) = registry(TEST, name, &device, all()) else {
            continue;
        };
        if !ranges(TEST, name, &device) {
            continue;
        }
        let a = reserve(LEN, 0);
        registry.reserved(a, LEN);
        registry.settle();
        assert_eq!(registry.heaps(), []);
        let parts = [(0, 2 * MIB), (5 * MIB, MIB), (2 * MIB, 3 * MIB)];
        for (k, &(at, len)) in parts.iter().enumerate() {
            commit(a + at, len);
            registry.committed(a + at, len);
            registry.settle();
            let mut want: Vec<_> = parts[..=k].iter().map(|&(at, len)| (a + at, len)).collect();
            want.sort_unstable();
            assert_eq!(registry.heaps(), want, "{name}");
        }
        assert!(
            registry.lookup(a + 2 * MIB - 8, 16).is_none(),
            "across two heaps"
        );
        assert_eq!(
            registry.lookup(a + 5 * MIB + 10, 16).map(|p| p.offset),
            Some(10)
        );
        commit(a + 7 * MIB, HOST_PAGE);
        registry.committed(a + 7 * MIB, HOST_PAGE);
        registry.settle();
        assert_eq!(registry.heaps().len(), 3, "{name}: one host page alone");

        let change = registry.changing(a, LEN);
        assert!(change.call(|| free(a).is_ok()), "{name}");
        change.done(true);
        assert_eq!(registry.imported(), 0);
    }
}

/// What does not fit under the cap stays on shadows until heaps go.
#[test]
fn the_cap_keeps_the_rest_on_shadows() {
    const TEST: &str = "the_cap_keeps_the_rest_on_shadows";
    const LEN: usize = 2 * MIB;
    let (_gpu, listed) = devices(TEST);
    for (name, device) in listed {
        let options = Options {
            cap: 3 * MIB,
            ..all()
        };
        let Some(registry) = registry(TEST, name, &device, options) else {
            continue;
        };
        let (a, b) = (reserve(LEN, LEN), reserve(LEN, LEN));
        registry.reserved(a, LEN);
        registry.settle();
        registry.reserved(b, LEN);
        registry.settle();
        assert_eq!(registry.heaps(), [(a, LEN)], "{name}");
        assert_eq!(registry.imported(), LEN);
        assert!(registry.lookup(b, 1).is_none());

        let change = registry.changing(a, LEN);
        assert!(change.call(|| free(a).is_ok()));
        change.done(true);
        registry.settle();
        assert_eq!(registry.heaps(), [(b, LEN)], "{name}: room again");

        let change = registry.changing(b, LEN);
        assert!(change.call(|| free(b).is_ok()));
        change.done(true);
    }
}

/// Without `ID3D12Device13` (forced): whole allocations, all committed,
/// with `OpenExistingHeapFromAddress`; nothing over a partial one.
#[test]
fn without_device13_whole_allocations_only() {
    const TEST: &str = "without_device13_whole_allocations_only";
    const LEN: usize = 4 * MIB;
    let (_gpu, listed) = devices(TEST);
    for (name, device) in listed {
        let options = Options {
            whole_allocations: true,
            ..all()
        };
        let Some(registry) = registry(TEST, name, &device, options) else {
            continue;
        };
        let a = reserve(LEN, LEN / 2);
        registry.reserved(a, LEN);
        registry.settle();
        assert_eq!(registry.heaps(), [], "{name}: half committed");
        commit(a + LEN / 2, LEN / 2);
        registry.committed(a + LEN / 2, LEN / 2);
        registry.settle();
        assert_eq!(registry.heaps(), [(a, LEN)], "{name}: all committed");
        assert_eq!(
            registry.lookup(a + MIB, 16).map(|p| p.offset),
            Some(MIB as u64)
        );

        let change = registry.changing(a + PAGE, PAGE);
        assert!(change.call(|| decommit(a + PAGE, PAGE).is_ok()), "{name}");
        change.done(false);
        registry.settle();
        assert_eq!(registry.heaps(), [], "{name}: no longer all committed");
        assert_eq!(registry.imported(), 0);

        // A permanent view of physical pages, all committed, opens whole.
        let section = Section::new(MIB);
        section.commit(0, MIB);
        registry.mapped(section.view, MIB);
        registry.settle();
        assert_eq!(registry.heaps(), [(section.view, MIB)], "{name}");

        let change = registry.changing(section.view, MIB);
        assert!(change.call(|| section.unmap()), "{name}");
        change.done(true);
        let change = registry.changing(a, LEN);
        assert!(change.call(|| free(a).is_ok()));
        change.done(true);
        assert_eq!(registry.heaps(), []);
    }
}

/// Releasing a heap waits for the GPU work [`Registry::used`] named.
#[test]
fn release_waits_for_the_gpu() {
    const TEST: &str = "release_waits_for_the_gpu";
    let (_gpu, listed) = devices(TEST);
    for (name, device) in listed {
        let Some(registry) = registry(TEST, name, &device, all()) else {
            continue;
        };
        let a = reserve(MIB, MIB);
        registry.reserved(a, MIB);
        registry.settle();
        // SAFETY: plain object creation.
        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.unwrap();
        registry.used(a + 64, 4, &fence, 1);
        let signalled = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                signalled.store(true, Ordering::SeqCst);
                // SAFETY: a fence of ours, signalled from the CPU as a
                // queue would once its work is done.
                unsafe { fence.Signal(1) }.unwrap();
            });
            let change = registry.changing(a, MIB);
            assert!(signalled.load(Ordering::SeqCst), "{name}: released early");
            assert!(change.call(|| free(a).is_ok()));
            change.done(true);
        });
    }
}

// ---------------------------------------------------------------------------
// Devices

/// The devices to test on: the host's (`crate::testgpu`), and WARP
/// besides unless the host's is WARP. None (the skip reported) without a
/// host device. The test keeps the guard until it ends (`testgpu::Gpu`).
fn devices(
    test: &str,
) -> (
    Option<crate::testgpu::Gpu>,
    Vec<(&'static str, ID3D12Device)>,
) {
    let Some(gpu) = crate::testgpu::device(test) else {
        return (None, Vec::new());
    };
    let mut all = vec![("host device", gpu.device.clone())];
    if crate::testgpu::software() != Some(Ok(true)) {
        match warp() {
            Some(device) => all.push(("WARP", device.clone())),
            None => eprintln!("{test}: no WARP device"),
        }
    }
    (Some(gpu), all)
}

fn warp() -> Option<&'static ID3D12Device> {
    static WARP: OnceLock<Option<ID3D12Device>> = OnceLock::new();
    WARP.get_or_init(|| {
        // SAFETY: plain DXGI and D3D12 object creation.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    })
    .as_ref()
}

/// A registry on `device`, or `None` (the skip reported) when it cannot
/// open existing heaps.
fn registry(test: &str, name: &str, device: &ID3D12Device, options: Options) -> Option<Registry> {
    let registry = Registry::new(Some(device.clone()), options);
    match registry.ready() {
        Ok(()) => Some(registry),
        Err(reason) => {
            crate::testgpu::skip(&format!("{test} on {name}"), &reason);
            None
        }
    }
}

/// Whether `device` opens any committed range; reports the skip if not.
fn ranges(test: &str, name: &str, device: &ID3D12Device) -> bool {
    let has = device.cast::<ID3D12Device13>().is_ok();
    if !has {
        crate::testgpu::skip(&format!("{test} on {name}"), "no ID3D12Device13");
    }
    has
}

/// No cap, any committed range.
fn all() -> Options {
    Options {
        cap: usize::MAX,
        whole_allocations: false,
    }
}

fn sorted<const N: usize>(mut spans: [(usize, usize); N]) -> [(usize, usize); N] {
    spans.sort_unstable();
    spans
}

/// Copies on a copy queue of a device, each waited for.
struct Copier {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: std::cell::Cell<u64>,
}

impl Copier {
    fn new(device: &ID3D12Device) -> Copier {
        // SAFETY: plain object creation.
        unsafe {
            let queue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    ..Default::default()
                })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)
                .unwrap();
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)
                .unwrap();
            list.Close().unwrap();
            Copier {
                queue,
                allocator,
                list,
                fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
                value: std::cell::Cell::new(0),
            }
        }
    }

    /// `len` bytes from `src` to `dst` (in two buffers), done when it
    /// returns.
    fn copy(&self, dst: &Placed, src: &Placed, len: usize) {
        let value = self.value.get() + 1;
        self.value.set(value);
        // SAFETY: our list and queue; both buffers hold the bytes and
        // outlive the copy, waited for here. Buffers in `COMMON` are
        // promoted to copy states on a copy queue, and decay back after.
        unsafe {
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
            self.list.CopyBufferRegion(
                &dst.buffer,
                dst.offset,
                &src.buffer,
                src.offset,
                len as u64,
            );
            self.list.Close().unwrap();
            self.queue
                .ExecuteCommandLists(&[Some(self.list.cast::<ID3D12CommandList>().unwrap())]);
            self.queue.Signal(&self.fence, value).unwrap();
            self.fence
                .SetEventOnCompletion(value, HANDLE::default())
                .unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// Title memory, as xmem makes it

/// A write-watched reservation of `len` bytes, the first `committed` of
/// them committed.
fn reserve(len: usize, committed: usize) -> usize {
    // SAFETY: a new reservation of ours.
    let at = unsafe {
        vm::VirtualAlloc(
            std::ptr::null(),
            len,
            vm::MEM_RESERVE | MEM_WRITE_WATCH,
            vm::PAGE_READWRITE,
        )
    } as usize;
    assert_ne!(at, 0, "reserving: {}", std::io::Error::last_os_error());
    commit(at, committed);
    at
}

fn commit(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: part of a reservation of ours.
    let done =
        unsafe { vm::VirtualAlloc(at as *const c_void, len, vm::MEM_COMMIT, vm::PAGE_READWRITE) };
    assert!(
        !done.is_null(),
        "committing: {}",
        std::io::Error::last_os_error()
    );
}

/// The host's own calls, no hook: their error when they fail.
fn decommit(at: usize, len: usize) -> Result<(), i32> {
    // SAFETY: part of a reservation of ours, which nothing uses after.
    host_result(unsafe { vm::VirtualFree(at as *mut c_void, len, vm::MEM_DECOMMIT) })
}

fn free(at: usize) -> Result<(), i32> {
    // SAFETY: a reservation of ours, which nothing uses after.
    host_result(unsafe { vm::VirtualFree(at as *mut c_void, 0, vm::MEM_RELEASE) })
}

fn host_result(done: i32) -> Result<(), i32> {
    match done {
        0 => Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)),
        _ => Ok(()),
    }
}

/// Host pages of `[at, at + len)` written since write watch was reset.
fn written_pages(at: usize, len: usize) -> usize {
    let mut found = vec![std::ptr::null_mut(); len / HOST_PAGE];
    let (mut count, mut granularity) = (found.len(), 0);
    // SAFETY: room for `count` addresses; a watched range of ours.
    let failed = unsafe {
        vm::GetWriteWatch(
            0,
            at as *const c_void,
            len,
            found.as_mut_ptr(),
            &mut count,
            &mut granularity,
        )
    };
    assert_eq!(failed, 0, "write watch");
    count
}

/// Physical pages as xmem keeps them: a section of reserved pages and the
/// runtime's permanent view of all of it, through which pages are committed.
struct Section {
    handle: windows_sys::Win32::Foundation::HANDLE,
    view: usize,
    len: usize,
}

impl Section {
    fn new(len: usize) -> Section {
        // SAFETY: plain section creation, and a view of all of it anywhere.
        unsafe {
            let handle = vm::CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                vm::PAGE_READWRITE | vm::SEC_RESERVE,
                (len as u64 >> 32) as u32,
                len as u32,
                std::ptr::null(),
            );
            assert!(!handle.is_null(), "section");
            let view = vm::MapViewOfFile3(
                handle,
                GetCurrentProcess(),
                std::ptr::null(),
                0,
                len,
                0,
                vm::PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
            .Value as usize;
            assert_ne!(view, 0, "view");
            Section { handle, view, len }
        }
    }

    /// Commits `[offset, offset + len)` of the section through our view, as
    /// xmem's `Section::commit` does.
    fn commit(&self, offset: usize, len: usize) {
        commit(self.view + offset, len);
    }

    /// A title view of all of it in a placeholder, as
    /// `XMemMapPhysicalPages` maps one.
    fn title_view(&self) -> usize {
        // SAFETY: a new placeholder, then our section in place of it.
        unsafe {
            let base = vm::VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                self.len,
                vm::MEM_RESERVE | vm::MEM_RESERVE_PLACEHOLDER,
                vm::PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            );
            assert!(!base.is_null(), "placeholder");
            let view = vm::MapViewOfFile3(
                self.handle,
                GetCurrentProcess(),
                base,
                0,
                self.len,
                vm::MEM_REPLACE_PLACEHOLDER,
                vm::PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(view.Value, base, "title view");
            base as usize
        }
    }

    /// Unmaps our view: false if the host refuses (as under an open heap).
    fn unmap(&self) -> bool {
        // SAFETY: our view; nothing uses it after.
        unsafe {
            vm::UnmapViewOfFile(vm::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view as *mut c_void,
            }) != 0
        }
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: our handle; its views keep the section alive.
        unsafe { CloseHandle(self.handle) };
    }
}

/// `XMemMapPhysicalPages`' unmapping of a title view (back to a
/// placeholder), then the placeholder released. False if the host refuses.
fn unmap_title_view(at: usize) -> bool {
    // SAFETY: a title view of ours, then the placeholder it leaves.
    unsafe {
        vm::UnmapViewOfFile2(
            GetCurrentProcess(),
            vm::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: at as *mut c_void,
            },
            vm::MEM_PRESERVE_PLACEHOLDER,
        ) != 0
            && vm::VirtualFree(at as *mut c_void, 0, vm::MEM_RELEASE) != 0
    }
}

fn pattern(i: usize, seed: u32) -> u32 {
    (i as u32).wrapping_mul(0x9e37_79b9) ^ seed
}

/// Writes the pattern of `seed` over `[at, at + len)`, word `i` at
/// `at + 4i`.
fn fill(at: usize, len: usize, seed: u32) {
    // SAFETY: committed read-write memory of ours, which nothing else uses
    // now.
    let words = unsafe { std::slice::from_raw_parts_mut(at as *mut u32, len / 4) };
    for (i, w) in words.iter_mut().enumerate() {
        *w = pattern(i, seed);
    }
}

/// Words of `[at, at + len)` other than the pattern [`fill`] wrote from
/// `base` on.
fn wrong(base: usize, at: usize, len: usize, seed: u32) -> usize {
    // SAFETY: committed memory of ours that nothing writes now.
    let words = unsafe { std::slice::from_raw_parts(at as *const u32, len / 4) };
    let first = (at - base) / 4;
    words
        .iter()
        .enumerate()
        .filter(|&(i, &w)| w != pattern(first + i, seed))
        .count()
}
