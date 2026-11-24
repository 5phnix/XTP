//! G15: can title memory, allocated the way `xtp-hle`'s xmem allocates it,
//! be opened as a D3D12 heap that shaders read and write in place, and at
//! what cost? (`docs/analysis/gpu.md` phase 3; the findings are in
//! `docs/analysis/uma-feasibility.md`.)
//!
//! Each kind of title memory is opened with `OpenExistingHeapFromAddress`,
//! `OpenExistingHeapFromAddress1` or `OpenExistingHeapFromFileMapping`, a
//! buffer is placed over the whole heap, and compute shaders (HLSL compiled
//! by the system's FXC, no DXC) read and write it. Only the `windows` crates
//! are used, nothing of `xtp-gpu`.
//!
//! - `title_memory_import_matrix` runs on the high-performance adapter and on
//!   WARP, and skips when there is no D3D12 device.
//! - The registry built on these findings is `src/uma.rs` (G15b). Its tests
//!   go through the matrix's main rows and the two "under an open heap"
//!   cases again, through the registry; these stay as the raw API's.
//! - The `#[ignore]`d tests are the report's benchmarks. They print Markdown
//!   tables and read the process's commit, so run them optimised and one at a
//!   time: `cargo test -p xtp-gpu --release --test uma_feasibility --
//!   --ignored --nocapture --test-threads=1`. `XTP_UMA_ADAPTER=warp` (or any
//!   part of an adapter's name) keeps one adapter. They load the GPU for
//!   minutes: never run them on a machine someone is using.
//! - Shaders write imported memory 4 bytes a thread only. On the RTX 4080,
//!   16-byte stores there run at 0.1 GB/s, and 4 GiB of them hung the GPU
//!   (`DXGI_ERROR_DEVICE_HUNG`); `imported_memory_by_kind` still times them,
//!   over 16 MiB.
#![cfg(windows)]

use std::alloc::Layout;
use std::cell::Cell;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, PCSTR, s};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory as vm;
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;
const GIB: usize = 1 << 30;
/// The console's page, and xmem's.
const PAGE: usize = 64 * KIB;
/// The host's page: write watch's granularity.
const HOST_PAGE: usize = 4 * KIB;
/// What xmem adds to the title's reservations (not in windows-sys' memory
/// module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

// ---------------------------------------------------------------------------
// Title memory, as xmem makes it

/// How a reservation is committed.
#[derive(Clone, Copy)]
enum Commit {
    /// `MEM_RESERVE | MEM_COMMIT` in one call (`XMemAllocDefault`'s 32-bit
    /// path, title heaps allocated committed).
    AtOnce,
    /// `MEM_RESERVE`, then `MEM_COMMIT` of the first bytes in a second call.
    Later(usize),
}

/// Title memory of one kind; everything given back when dropped. Heaps over
/// it must be released first: callers declare them after it.
struct Title {
    /// Where the title (the CPU) sees it.
    at: *mut u8,
    len: usize,
    /// Write-watched: `GetWriteWatch` reports the CPU's writes.
    watched: bool,
    /// The section behind physical pages.
    section: Option<*mut c_void>,
    /// A second address of the same physical pages (the console maps a page
    /// at several addresses).
    alias: Option<*mut u8>,
    reservation: Option<*mut u8>,
    views: Vec<*mut u8>,
    placeholders: Vec<*mut u8>,
    block: Option<Layout>,
}

impl Title {
    fn empty(len: usize) -> Title {
        Title {
            at: std::ptr::null_mut(),
            len,
            watched: false,
            section: None,
            alias: None,
            reservation: None,
            views: Vec::new(),
            placeholders: Vec::new(),
            block: None,
        }
    }

    /// `XMemVirtualAlloc(null, len, …)`: a reservation between 4 GiB and
    /// 2^40 (xmem's range for the title's GPU heap, which has no address
    /// field), 2 MiB-aligned, write-watched when `watched` (xmem watches
    /// every reservation it places).
    fn virtual_alloc(len: usize, commit: Commit, watched: bool) -> Option<Title> {
        let mut kind = vm::MEM_RESERVE;
        if watched {
            kind |= MEM_WRITE_WATCH;
        }
        if let Commit::AtOnce = commit {
            kind |= vm::MEM_COMMIT;
        }
        let at = reserve(len, kind, vm::PAGE_READWRITE, 2 * MIB);
        if at.is_null() {
            return None;
        }
        let mut title = Title::empty(len);
        (title.at, title.watched, title.reservation) = (at, watched, Some(at));
        if let Commit::Later(part) = commit
            && part > 0
        {
            // SAFETY: the first `part` bytes of our reservation.
            let done =
                unsafe { vm::VirtualAlloc(at.cast(), part, vm::MEM_COMMIT, vm::PAGE_READWRITE) };
            if done.is_null() {
                return None;
            }
        }
        Some(title)
    }

    /// `XMemAllocatePhysicalPages` then `XMemMapPhysicalPages`: a
    /// pagefile-backed section (`SEC_RESERVE` as xmem makes it, `SEC_COMMIT`
    /// as under Wine) shown in a placeholder (`MapViewOfFile3`,
    /// `MEM_REPLACE_PLACEHOLDER`), or by a plain view with `placeholder`
    /// false (DirectStorage's `page_window`, formats.md F3's permanent view).
    /// Views of a `SEC_RESERVE` section are committed, as xmem does. With
    /// `alias`, a second view shows the same pages.
    fn physical(len: usize, commit: bool, placeholder: bool, alias: bool) -> Option<Title> {
        let section = section(len, commit)?;
        let mut title = Title::empty(len);
        title.section = Some(section);
        let views = if alias { 2 } else { 1 };
        if placeholder {
            let base = reserve(
                views * len,
                vm::MEM_RESERVE | vm::MEM_RESERVE_PLACEHOLDER,
                vm::PAGE_NOACCESS,
                0,
            );
            if base.is_null() {
                return None;
            }
            title.placeholders.push(base);
            if alias {
                // SAFETY: splits our placeholder in two.
                let split = unsafe {
                    vm::VirtualFree(
                        base.cast(),
                        len,
                        vm::MEM_RELEASE | vm::MEM_PRESERVE_PLACEHOLDER,
                    )
                };
                if split == 0 {
                    return None;
                }
                title.placeholders.push(base.wrapping_add(len));
            }
        }
        for k in 0..views {
            let view = if placeholder {
                let piece = title.placeholders.remove(0);
                // SAFETY: replaces a placeholder piece of ours by a view of
                // our section.
                let view = unsafe {
                    vm::MapViewOfFile3(
                        section,
                        GetCurrentProcess(),
                        piece.cast(),
                        0,
                        len,
                        vm::MEM_REPLACE_PLACEHOLDER,
                        vm::PAGE_READWRITE,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if view.Value.is_null() {
                    title.placeholders.insert(0, piece);
                    return None;
                }
                view.Value.cast::<u8>()
            } else {
                // SAFETY: a view of our section, anywhere.
                let view = unsafe { vm::MapViewOfFile(section, vm::FILE_MAP_WRITE, 0, 0, len) };
                if view.Value.is_null() {
                    return None;
                }
                view.Value.cast::<u8>()
            };
            title.views.push(view);
            if !commit && !commit_view(view, len) {
                return None;
            }
            if k == 0 {
                title.at = view;
            } else {
                title.alias = Some(view);
            }
        }
        Some(title)
    }

    /// `XMemAllocDefault` without the 32-bit flag: a block of the process
    /// heap.
    fn heap_block(len: usize) -> Option<Title> {
        let layout = Layout::from_size_align(len, PAGE).ok()?;
        // SAFETY: a non-zero size.
        let at = unsafe { std::alloc::alloc_zeroed(layout) };
        if at.is_null() {
            return None;
        }
        let mut title = Title::empty(len);
        (title.at, title.block) = (at, Some(layout));
        Some(title)
    }
}

impl Drop for Title {
    fn drop(&mut self) {
        // SAFETY: each is ours, made by the constructor; nothing uses the
        // memory any more (heaps over it are released before it).
        unsafe {
            for &view in &self.views {
                vm::UnmapViewOfFile(vm::MEMORY_MAPPED_VIEW_ADDRESS { Value: view.cast() });
            }
            for &at in self.placeholders.iter().chain(&self.reservation) {
                vm::VirtualFree(at.cast(), 0, vm::MEM_RELEASE);
            }
            if let Some(section) = self.section {
                CloseHandle(section);
            }
            if let Some(layout) = self.block {
                std::alloc::dealloc(self.at, layout);
            }
        }
    }
}

/// A pagefile-backed section of `len` bytes, committed whole or reserved
/// (xmem's physical pages).
fn section(len: usize, commit: bool) -> Option<*mut c_void> {
    let flags = vm::PAGE_READWRITE
        | if commit {
            vm::SEC_COMMIT
        } else {
            vm::SEC_RESERVE
        };
    let size = len as u64;
    // SAFETY: plain section creation.
    let section = unsafe {
        vm::CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            flags,
            (size >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    (!section.is_null()).then_some(section)
}

/// Commits the section pages a view of a `SEC_RESERVE` section shows, as
/// xmem's `commit` does.
fn commit_view(view: *mut u8, len: usize) -> bool {
    // SAFETY: a whole view of a reserved section of ours.
    !unsafe { vm::VirtualAlloc(view.cast(), len, vm::MEM_COMMIT, vm::PAGE_READWRITE) }.is_null()
}

/// A reservation of `len` bytes between 4 GiB and 2^40, as xmem's
/// `XMemVirtualAlloc` places it; null if the host refuses.
fn reserve(len: usize, kind: u32, protect: u32, alignment: usize) -> *mut u8 {
    let mut requirements = vm::MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: (1usize << 32) as *mut c_void,
        HighestEndingAddress: ((1usize << 40) - 1) as *mut c_void,
        Alignment: alignment,
    };
    let mut parameter = vm::MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = vm::MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // SAFETY: a new reservation; the parameter points at `requirements`,
    // which outlives the call.
    unsafe {
        vm::VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            len,
            kind,
            protect,
            &mut parameter,
            1,
        )
    }
    .cast()
}

/// Starts watching `[at, at + len)` afresh.
fn reset_watch(at: *mut u8, len: usize) {
    // SAFETY: a range of ours; the host refuses one that is not watched.
    unsafe { vm::ResetWriteWatch(at.cast(), len) };
}

/// Host pages of `[at, at + len)` written since the last reset; `None` if
/// the host refuses (memory that is not watched).
fn written_pages(at: *mut u8, len: usize) -> Option<usize> {
    let mut pages = vec![std::ptr::null_mut(); len.div_ceil(HOST_PAGE)];
    let (mut count, mut granularity) = (pages.len(), 0);
    // SAFETY: room for `count` addresses; a range of ours.
    let failed = unsafe {
        vm::GetWriteWatch(
            0,
            at.cast(),
            len,
            pages.as_mut_ptr(),
            &mut count,
            &mut granularity,
        )
    };
    (failed == 0).then_some(count)
}

/// How many bytes from `at` on are committed, up to `len`.
fn committed_prefix(at: *mut u8, len: usize) -> usize {
    let mut done = 0;
    while done < len {
        let here = at.wrapping_add(done);
        // SAFETY: all-zero is a valid MEMORY_BASIC_INFORMATION.
        let mut info: vm::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a query of our own address space into a sized structure.
        let got = unsafe { vm::VirtualQuery(here.cast(), &mut info, size_of_val(&info)) };
        if got == 0 || info.State != vm::MEM_COMMIT {
            break;
        }
        done += info.RegionSize - (here as usize - info.BaseAddress as usize);
    }
    done.min(len)
}

/// Commit and residency, the process's and the system's, and this process's
/// use of the adapter's memory.
#[derive(Clone, Copy, Default)]
struct Usage {
    /// The process's commit charge.
    private: usize,
    working_set: usize,
    /// The system's commit charge.
    system: usize,
    available: usize,
    /// Video memory, and system memory the adapter counts against us.
    local: u64,
    non_local: u64,
}

fn usage(gpu: &Gpu) -> Usage {
    let mut process = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let mut system = PERFORMANCE_INFORMATION {
        cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    let mut local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    let mut non_local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    // SAFETY: sized structures of ours; our own process.
    unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut process).cast(), process.cb);
        K32GetPerformanceInfo(&mut system, system.cb);
        let _ = gpu
            .adapter
            .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local);
        let _ = gpu.adapter.QueryVideoMemoryInfo(
            0,
            DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
            &mut non_local,
        );
    }
    Usage {
        private: process.PrivateUsage,
        working_set: process.WorkingSetSize,
        system: system.CommitTotal * system.PageSize,
        available: system.PhysicalAvailable * system.PageSize,
        local: local.CurrentUsage,
        non_local: non_local.CurrentUsage,
    }
}

/// `after - before` in MiB, signed.
fn delta_mib(before: u64, after: u64) -> String {
    format!("{:+.0}", (after as f64 - before as f64) / MIB as f64)
}

// ---------------------------------------------------------------------------
// The GPU side

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), UAV(u0), UAV(u1)"
cbuffer Args : register(b0) { uint count; uint stride; uint seed; uint unused; };
RWByteAddressBuffer src : register(u0);
RWByteAddressBuffer dst : register(u1);

// Reads `count` 16-byte words of `src`; stores their fold only if it equals
// `seed`, so no load is dead.
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void read_all(uint3 id : SV_DispatchThreadID) {
    uint4 fold = uint4(0, 0, 0, 0);
    for (uint i = id.x; i < count; i += stride) {
        fold ^= src.Load4(i * 16);
    }
    uint x = fold.x ^ fold.y ^ fold.z ^ fold.w;
    if (x == seed) {
        dst.Store(0, x);
    }
}

// Writes `count` 16-byte words of `dst`; word i starts with i.
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void write_all(uint3 id : SV_DispatchThreadID) {
    for (uint i = id.x; i < count; i += stride) {
        dst.Store4(i * 16, uint4(i, i ^ seed, ~i, seed));
    }
}

// dst[i] = src[i] + 1 over `count` 32-bit words.
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void add_one(uint3 id : SV_DispatchThreadID) {
    for (uint i = id.x; i < count; i += stride) {
        dst.Store(i * 4, src.Load(i * 4) + 1);
    }
}

// Writes `count` 32-bit words of `dst`, one a thread at a time.
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void write_words(uint3 id : SV_DispatchThreadID) {
    for (uint i = id.x; i < count; i += stride) {
        dst.Store(i * 4, i ^ seed);
    }
}
"#;

#[derive(Clone, Copy)]
enum Program {
    Read,
    Write,
    AddOne,
    WriteWords,
}

/// The three programs' bytecode, compiled once per process.
fn compiled() -> Result<&'static [Vec<u8>], String> {
    static CODE: OnceLock<Result<Vec<Vec<u8>>, String>> = OnceLock::new();
    CODE.get_or_init(|| {
        [
            s!("read_all"),
            s!("write_all"),
            s!("add_one"),
            s!("write_words"),
        ]
        .into_iter()
        .map(compile)
        .collect()
    })
    .as_deref()
    .map_err(Clone::clone)
}

fn compile(entry: PCSTR) -> Result<Vec<u8>, String> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; the out pointers
    // are ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("uma_feasibility"),
            None,
            None,
            entry,
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    match (compiled, code) {
        (Ok(()), Some(code)) => Ok(bytes(&code).to_vec()),
        (result, _) => Err(format!(
            "FXC: {result:?} {}",
            errors
                .map(|e| String::from_utf8_lossy(bytes(&e)).into_owned())
                .unwrap_or_default()
        )),
    }
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob's own bytes, alive as long as it is.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

fn text(e: windows::core::Error) -> String {
    format!("{:#010x} {}", e.code().0 as u32, e.message().trim())
}

/// GPU and wall seconds of one submission.
#[derive(Clone, Copy)]
struct Timing {
    /// Between timestamps at the list's start and end, when the queue has
    /// them.
    gpu: Option<f64>,
    /// From submission to completion seen by the CPU.
    wall: f64,
}

impl Timing {
    fn cpu(wall: f64) -> Timing {
        Timing { gpu: None, wall }
    }

    fn best(&self) -> f64 {
        self.gpu.unwrap_or(self.wall)
    }
}

/// A queue with one list, re-recorded or re-submitted, and its fence.
struct Queue {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: Cell<u64>,
    event: HANDLE,
    /// Timestamps around each list: the query heap, where they resolve, and
    /// ticks a second.
    stamps: Option<(ID3D12QueryHeap, ID3D12Resource, u64)>,
}

impl Queue {
    fn new(
        device: &ID3D12Device,
        kind: D3D12_COMMAND_LIST_TYPE,
        stamps: bool,
    ) -> Result<Queue, String> {
        // SAFETY: plain object creation.
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: kind,
                    ..Default::default()
                })
                .map_err(text)?;
            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(kind).map_err(text)?;
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, kind, &allocator, None)
                .map_err(text)?;
            list.Close().map_err(text)?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(text)?;
            let event = CreateEventW(None, false, false, None).map_err(text)?;
            let stamps = if stamps {
                let mut heap: Option<ID3D12QueryHeap> = None;
                device
                    .CreateQueryHeap(
                        &D3D12_QUERY_HEAP_DESC {
                            Type: if kind == D3D12_COMMAND_LIST_TYPE_COPY {
                                D3D12_QUERY_HEAP_TYPE_COPY_QUEUE_TIMESTAMP
                            } else {
                                D3D12_QUERY_HEAP_TYPE_TIMESTAMP
                            },
                            Count: 2,
                            NodeMask: 0,
                        },
                        &mut heap,
                    )
                    .map_err(text)?;
                let readback = committed(device, D3D12_HEAP_TYPE_READBACK, 16)?;
                let frequency = queue.GetTimestampFrequency().map_err(text)?;
                Some((heap.ok_or("no query heap")?, readback, frequency))
            } else {
                None
            };
            Ok(Queue {
                queue,
                allocator,
                list,
                fence,
                value: Cell::new(0),
                event,
                stamps,
            })
        }
    }

    /// Records the queue's list with `record`, between two timestamps when
    /// the queue has them. Callers keep what it uses alive while they submit
    /// it.
    fn record(&self, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> Result<(), String> {
        // SAFETY: our allocator and list, idle: every submission is waited
        // for before the next recording.
        unsafe {
            self.allocator.Reset().map_err(text)?;
            self.list.Reset(&self.allocator, None).map_err(text)?;
            if let Some((heap, _, _)) = &self.stamps {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            }
            record(&self.list);
            if let Some((heap, readback, _)) = &self.stamps {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                self.list
                    .ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, readback, 0);
            }
            self.list.Close().map_err(text)
        }
    }

    /// Submits the recorded list; the fence value that marks its end.
    fn submit(&self) -> Result<u64, String> {
        let value = self.value.get() + 1;
        self.value.set(value);
        let list: ID3D12CommandList = self.list.cast().map_err(text)?;
        // SAFETY: a closed list whose resources the caller keeps alive until
        // it has waited.
        unsafe {
            self.queue.ExecuteCommandLists(&[Some(list)]);
            self.queue.Signal(&self.fence, value).map_err(text)?;
        }
        Ok(value)
    }

    /// Waits for `value`, spinning on the fence or sleeping on an event.
    fn wait(&self, device: &ID3D12Device, value: u64, spin: bool) -> Result<(), String> {
        let timeout = Duration::from_secs(60);
        // SAFETY: our fence and event.
        unsafe {
            if spin {
                let start = Instant::now();
                while self.fence.GetCompletedValue() < value {
                    if start.elapsed() > timeout {
                        return Err(removed(device, "timeout"));
                    }
                    std::hint::spin_loop();
                }
            } else if self.fence.GetCompletedValue() < value {
                self.fence
                    .SetEventOnCompletion(value, self.event)
                    .map_err(text)?;
                if WaitForSingleObject(self.event, timeout.as_millis() as u32) != WAIT_OBJECT_0 {
                    return Err(removed(device, "timeout"));
                }
            }
            if self.fence.GetCompletedValue() == u64::MAX {
                return Err(removed(device, "fence at u64::MAX"));
            }
        }
        Ok(())
    }

    /// Submits the recorded list and waits for it.
    fn run(&self, device: &ID3D12Device) -> Result<Timing, String> {
        let start = Instant::now();
        let value = self.submit()?;
        self.wait(device, value, false)?;
        let wall = start.elapsed().as_secs_f64();
        let gpu = self.stamps.as_ref().and_then(|(_, readback, frequency)| {
            let [start, end] = read_back::<[u64; 2]>(readback)?;
            Some(end.saturating_sub(start) as f64 / *frequency as f64)
        });
        Ok(Timing { gpu, wall })
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        // SAFETY: our event; the queue is idle (every submission was waited
        // for, or the device is gone).
        unsafe { CloseHandle(self.event.0) };
    }
}

fn removed(device: &ID3D12Device, what: &str) -> String {
    // SAFETY: a plain query.
    match unsafe { device.GetDeviceRemovedReason() } {
        Ok(()) => what.into(),
        Err(e) => format!("{what}: device removed ({})", text(e)),
    }
}

/// A D3D12 device on one adapter, its queues and our programs.
struct Gpu {
    name: String,
    driver: String,
    desc: DXGI_ADAPTER_DESC1,
    adapter: IDXGIAdapter3,
    device: ID3D12Device,
    /// `OpenExistingHeapFromAddress` / `…FromFileMapping` (D3D12_FEATURE_EXISTING_HEAPS).
    existing_heaps: bool,
    /// `OpenExistingHeapFromAddress1`.
    device13: Option<ID3D12Device13>,
    /// CPU-visible video memory (`D3D12_HEAP_TYPE_GPU_UPLOAD`, resizable BAR).
    gpu_upload: bool,
    architecture: D3D12_FEATURE_DATA_ARCHITECTURE1,
    direct: Queue,
    copy: Queue,
    root: ID3D12RootSignature,
    programs: Vec<ID3D12PipelineState>,
}

impl Gpu {
    fn new(adapter: &IDXGIAdapter1) -> Result<Gpu, String> {
        // SAFETY: plain adapter and device queries; the feature structures
        // are ours and sized.
        unsafe {
            let desc = adapter.GetDesc1().map_err(text)?;
            let end = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
            let name = String::from_utf16_lossy(&desc.Description[..end]);
            let driver = adapter
                .CheckInterfaceSupport(&IDXGIDevice::IID)
                .map(|v| {
                    let v = v as u64;
                    format!(
                        "{}.{}.{}.{}",
                        v >> 48,
                        v >> 32 & 0xffff,
                        v >> 16 & 0xffff,
                        v & 0xffff
                    )
                })
                .unwrap_or_default();
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, &mut device)
                .map_err(|e| format!("{name}: {}", text(e)))?;
            let device = device.ok_or("no device")?;
            let feature = |which, data: *mut c_void, size: usize| {
                device.CheckFeatureSupport(which, data, size as u32).is_ok()
            };
            let mut heaps = D3D12_FEATURE_DATA_EXISTING_HEAPS::default();
            let existing_heaps = feature(
                D3D12_FEATURE_EXISTING_HEAPS,
                (&raw mut heaps).cast(),
                size_of_val(&heaps),
            ) && heaps.Supported.as_bool();
            let mut options16 = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
            let gpu_upload = feature(
                D3D12_FEATURE_D3D12_OPTIONS16,
                (&raw mut options16).cast(),
                size_of_val(&options16),
            ) && options16.GPUUploadHeapSupported.as_bool();
            let mut options3 = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
            let copy_stamps = feature(
                D3D12_FEATURE_D3D12_OPTIONS3,
                (&raw mut options3).cast(),
                size_of_val(&options3),
            ) && options3.CopyQueueTimestampQueriesSupported.as_bool();
            let mut architecture = D3D12_FEATURE_DATA_ARCHITECTURE1::default();
            feature(
                D3D12_FEATURE_ARCHITECTURE1,
                (&raw mut architecture).cast(),
                size_of_val(&architecture),
            );
            let code = compiled()?;
            let root: ID3D12RootSignature =
                device.CreateRootSignature(0, &code[0]).map_err(text)?;
            let programs = code
                .iter()
                .map(|code| {
                    device
                        .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                            // A borrowed pointer; the description never releases it.
                            pRootSignature: std::mem::transmute_copy(&root),
                            CS: D3D12_SHADER_BYTECODE {
                                pShaderBytecode: code.as_ptr().cast(),
                                BytecodeLength: code.len(),
                            },
                            ..Default::default()
                        })
                        .map_err(text)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Gpu {
                name,
                driver,
                desc,
                adapter: adapter.cast().map_err(text)?,
                existing_heaps,
                device13: device.cast().ok(),
                gpu_upload,
                architecture,
                direct: Queue::new(&device, D3D12_COMMAND_LIST_TYPE_DIRECT, true)?,
                copy: Queue::new(&device, D3D12_COMMAND_LIST_TYPE_COPY, copy_stamps)?,
                device,
                root,
                programs,
            })
        }
    }

    fn software(&self) -> bool {
        self.desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
    }

    /// Records `program` over `count` elements (16-byte words for `Read` and
    /// `Write`, 32-bit ones for the others) at GPU addresses `src` and `dst`.
    fn dispatch(
        &self,
        list: &ID3D12GraphicsCommandList,
        program: Program,
        src: u64,
        dst: u64,
        count: usize,
        seed: u32,
    ) {
        let count = u32::try_from(count).expect("at most 2^32 - 1 elements");
        let groups = count.div_ceil(256).clamp(1, 4096);
        let constants = [count, groups * 256, seed, 0];
        // SAFETY: an open list; our root signature and program; addresses in
        // buffers the caller keeps alive until the list completes.
        unsafe {
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.programs[program as usize]);
            list.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
            list.SetComputeRootUnorderedAccessView(1, src);
            list.SetComputeRootUnorderedAccessView(2, dst);
            list.Dispatch(groups, 1, 1);
        }
    }

    /// Prints what the adapter is and offers.
    fn header(&self) {
        let mut local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut non_local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: sized structures of ours.
        unsafe {
            let _ =
                self.adapter
                    .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local);
            let _ = self.adapter.QueryVideoMemoryInfo(
                0,
                DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
                &mut non_local,
            );
        }
        println!(
            "\n### {} (driver {}, {} MiB video memory, budget {} MiB local / {} MiB non-local)\n\n\
             existing heaps {}, ID3D12Device13 {}, GPU upload heap {}, UMA {}, cache-coherent UMA {}\n",
            self.name,
            self.driver,
            self.desc.DedicatedVideoMemory / MIB,
            local.Budget >> 20,
            non_local.Budget >> 20,
            self.existing_heaps,
            self.device13.is_some(),
            self.gpu_upload,
            self.architecture.UMA.as_bool(),
            self.architecture.CacheCoherentUMA.as_bool(),
        );
    }
}

/// The high-performance hardware adapter and WARP, those with a D3D12
/// device, kept by `XTP_UMA_ADAPTER` (part of a name) when set.
fn gpus() -> Vec<Gpu> {
    let keep = std::env::var("XTP_UMA_ADAPTER")
        .ok()
        .map(|k| k.to_lowercase());
    let mut adapters = Vec::new();
    // SAFETY: plain DXGI queries.
    unsafe {
        if let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) {
            if let Ok(a) = factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(
                0,
                DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
            ) && a
                .GetDesc1()
                .is_ok_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0)
            {
                adapters.push(a);
            }
            if let Ok(warp) = factory.EnumWarpAdapter::<IDXGIAdapter1>() {
                adapters.push(warp);
            }
        }
    }
    let mut gpus = Vec::new();
    for adapter in &adapters {
        let start = Instant::now();
        match Gpu::new(adapter) {
            Ok(gpu) => {
                eprintln!(
                    "{}: device, queues and programs in {} ms",
                    gpu.name,
                    start.elapsed().as_millis()
                );
                if keep
                    .as_ref()
                    .is_none_or(|k| gpu.name.to_lowercase().contains(k))
                {
                    gpus.push(gpu);
                }
            }
            Err(e) => eprintln!("no D3D12 device: {e}"),
        }
    }
    if gpus.is_empty() {
        eprintln!("skipped: no D3D12 device here");
    }
    gpus
}

fn buffer_desc(len: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: len,
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
    }
}

/// A committed buffer of `len` bytes in a heap of type `heap`.
fn committed(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    len: usize,
) -> Result<ID3D12Resource, String> {
    let (flags, state) = if heap == D3D12_HEAP_TYPE_UPLOAD {
        (D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ)
    } else if heap == D3D12_HEAP_TYPE_READBACK {
        (D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST)
    } else {
        (
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        )
    };
    let mut resource: Option<ID3D12Resource> = None;
    // SAFETY: plain resource creation.
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: heap,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(len as u64, flags),
            state,
            None,
            &mut resource,
        )
    }
    .map_err(text)?;
    resource.ok_or_else(|| "no resource".into())
}

/// The CPU address of a buffer in a CPU-visible heap, mapped for good.
fn map(resource: &ID3D12Resource) -> Result<*mut u8, String> {
    let mut at = std::ptr::null_mut();
    // SAFETY: a buffer in a CPU-visible heap.
    unsafe { resource.Map(0, None, Some(&mut at)) }.map_err(text)?;
    Ok(at.cast())
}

/// The first bytes of a readback buffer.
fn read_back<T: Copy>(resource: &ID3D12Resource) -> Option<T> {
    let mut at = std::ptr::null_mut();
    // SAFETY: a readback buffer at least `size_of::<T>()` long that the GPU
    // has finished writing.
    unsafe {
        resource.Map(0, None, Some(&mut at)).ok()?;
        let value = at.cast::<T>().read_unaligned();
        resource.Unmap(0, None);
        Some(value)
    }
}

fn gpu_va(resource: &ID3D12Resource) -> u64 {
    // SAFETY: a plain query of a buffer.
    unsafe { resource.GetGPUVirtualAddress() }
}

fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    // SAFETY: an open list and a live resource.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

fn copy(list: &ID3D12GraphicsCommandList, dst: &ID3D12Resource, src: &ID3D12Resource, len: usize) {
    // SAFETY: an open list; both buffers hold `len` bytes and outlive it.
    unsafe { list.CopyBufferRegion(dst, 0, src, 0, len as u64) };
}

// ---------------------------------------------------------------------------
// Opening title memory as a heap

/// How a heap is opened over title memory.
#[derive(Clone, Copy)]
enum Open {
    /// `OpenExistingHeapFromAddress` this many bytes into the memory.
    Address(usize),
    /// `OpenExistingHeapFromAddress1` over `.1` bytes from `.0` on.
    Range(usize, usize),
    /// `OpenExistingHeapFromFileMapping` on the section.
    Mapping,
}

impl std::fmt::Display for Open {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            Open::Address(0) => write!(f, "FromAddress"),
            Open::Address(at) => write!(f, "FromAddress at +{} KiB", at / KIB),
            Open::Range(at, len) => write!(f, "FromAddress1 +{} KiB, {} KiB", at / KIB, len / KIB),
            Open::Mapping => write!(f, "FromFileMapping"),
        }
    }
}

/// A heap over title memory and a buffer placed over all of it.
struct Imported {
    _heap: ID3D12Heap,
    buffer: ID3D12Resource,
    /// The buffer's GPU address, where the CPU's `cpu` is.
    va: u64,
    cpu: *mut u8,
    len: usize,
    desc: D3D12_HEAP_DESC,
    open: Duration,
    place: Duration,
}

/// Opens `title` as a heap with `open` and places a buffer over all of it
/// (`ALLOW_CROSS_ADAPTER` unless `plain`).
fn import(gpu: &Gpu, title: &Title, open: Open, plain: bool) -> Result<Imported, String> {
    let device3: ID3D12Device3 = gpu.device.cast().map_err(text)?;
    let start = Instant::now();
    let (heap, cpu) = match open {
        Open::Address(at) => {
            let cpu = title.at.wrapping_add(at);
            // SAFETY: title memory of ours, which outlives the heap.
            (
                unsafe { device3.OpenExistingHeapFromAddress::<ID3D12Heap>(cpu.cast()) },
                cpu,
            )
        }
        Open::Range(at, len) => {
            let Some(device13) = &gpu.device13 else {
                return Err("no ID3D12Device13".into());
            };
            let cpu = title.at.wrapping_add(at);
            // SAFETY: as above.
            (
                unsafe { device13.OpenExistingHeapFromAddress1::<ID3D12Heap>(cpu.cast(), len) },
                cpu,
            )
        }
        Open::Mapping => {
            let Some(section) = title.section else {
                return Err("no section".into());
            };
            // SAFETY: our section, which outlives the heap.
            let heap =
                unsafe { device3.OpenExistingHeapFromFileMapping::<ID3D12Heap>(HANDLE(section)) };
            (heap, title.at)
        }
    };
    let heap = heap.map_err(|e| format!("open: {}", text(e)))?;
    let open = start.elapsed();
    // SAFETY: a plain query.
    let desc = unsafe { heap.GetDesc() };
    let flags = if plain {
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
    } else {
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER
    };
    let start = Instant::now();
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: a buffer over the whole heap.
    unsafe {
        gpu.device.CreatePlacedResource(
            &heap,
            0,
            &buffer_desc(desc.SizeInBytes, flags),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut buffer,
        )
    }
    .map_err(|e| format!("heap {}, place: {}", heap_text(&desc), text(e)))?;
    let place = start.elapsed();
    let buffer = buffer.ok_or("place: no buffer")?;
    Ok(Imported {
        va: gpu_va(&buffer),
        _heap: heap,
        buffer,
        cpu,
        len: desc.SizeInBytes as usize,
        desc,
        open,
        place,
    })
}

fn heap_text(desc: &D3D12_HEAP_DESC) -> String {
    let p = &desc.Properties;
    let kind = match p.Type.0 {
        1 => "default",
        2 => "upload",
        3 => "readback",
        4 => "custom",
        5 => "GPU upload",
        _ => "?",
    };
    let page = match p.CPUPageProperty.0 {
        1 => "no CPU",
        2 => "write-combined",
        3 => "write-back",
        _ => "?",
    };
    let pool = match p.MemoryPoolPreference.0 {
        1 => "L0",
        2 => "L1",
        _ => "?",
    };
    format!(
        "{} KiB {kind} {page} {pool} flags {:#x}",
        desc.SizeInBytes >> 10,
        desc.Flags.0
    )
}

/// What a round trip through an imported heap showed.
struct RoundTrip {
    /// Bytes checked (the committed part of the heap).
    len: usize,
    /// Words the CPU found wrong: the GPU read other than the CPU wrote, or
    /// the CPU does not see what the GPU wrote.
    wrong: usize,
    /// Host pages write watch reported after the CPU wrote `.1` of them, and
    /// after the GPU wrote as many.
    watch: Option<(usize, usize, usize)>,
    /// Words wrong through the second view of physical pages.
    alias_wrong: Option<usize>,
}

fn pattern(i: usize, seed: u32) -> u32 {
    ((i as u32).wrapping_mul(0x9e37_79b9) ^ seed) & 0x7fff_ffff
}

/// Words of `words` that are not the pattern plus one.
fn wrong_words(words: &[u32], seed: u32) -> usize {
    words
        .iter()
        .enumerate()
        .filter(|&(i, &w)| w != pattern(i, seed) + 1)
        .count()
}

/// The CPU writes a pattern into the first half of the heap's memory, a
/// shader adds one to each word into the second half, and the CPU checks
/// the second half, through the alias view too.
fn round_trip(
    gpu: &Gpu,
    imported: &Imported,
    title: &Title,
    seed: u32,
) -> Result<RoundTrip, String> {
    let offset = imported.cpu as usize - title.at as usize;
    let len = committed_prefix(imported.cpu, imported.len.min(title.len - offset));
    let half = len / 2 / PAGE * PAGE;
    if half == 0 {
        return Err(format!("only {len} bytes committed"));
    }
    let words = half / 4;
    if title.watched {
        reset_watch(imported.cpu, 2 * half);
    }
    // SAFETY: the first half of committed read-write title memory of ours,
    // which nothing else uses now.
    let first = unsafe { std::slice::from_raw_parts_mut(imported.cpu.cast::<u32>(), words) };
    for (i, w) in first.iter_mut().enumerate() {
        *w = pattern(i, seed);
    }
    let by_cpu = title
        .watched
        .then(|| written_pages(imported.cpu, half))
        .flatten();
    gpu.direct.record(|l| {
        gpu.dispatch(
            l,
            Program::AddOne,
            imported.va,
            imported.va + half as u64,
            words,
            0,
        )
    })?;
    gpu.direct.run(&gpu.device)?;
    let second_at = imported.cpu.wrapping_add(half);
    let by_gpu = title
        .watched
        .then(|| written_pages(second_at, half))
        .flatten();
    // SAFETY: the second half, which the GPU has finished writing.
    let second = unsafe { std::slice::from_raw_parts(second_at.cast::<u32>(), words) };
    let wrong = wrong_words(second, seed);
    let alias_wrong = title.alias.map(|alias| {
        // SAFETY: the same committed pages through the other view.
        let words = unsafe {
            std::slice::from_raw_parts(alias.wrapping_add(offset + half).cast::<u32>(), words)
        };
        wrong_words(words, seed)
    });
    Ok(RoundTrip {
        len: 2 * half,
        wrong,
        watch: by_cpu
            .zip(by_gpu)
            .map(|(cpu, gpu)| (cpu, half / HOST_PAGE, gpu)),
        alias_wrong,
    })
}

// ---------------------------------------------------------------------------
// What imports

struct Case {
    kind: &'static str,
    make: fn(usize) -> Option<Title>,
    open: Open,
    /// The buffer without `ALLOW_CROSS_ADAPTER`.
    plain: bool,
}

fn cases(len: usize) -> Vec<Case> {
    let case = |kind, make, open| Case {
        kind,
        make,
        open,
        plain: false,
    };
    let virtual_watched: fn(usize) -> Option<Title> =
        |n| Title::virtual_alloc(n, Commit::AtOnce, true);
    let placeholder_reserve: fn(usize) -> Option<Title> = |n| Title::physical(n, false, true, true);
    let plain_reserve: fn(usize) -> Option<Title> = |n| Title::physical(n, false, false, true);
    let placeholder_commit: fn(usize) -> Option<Title> = |n| Title::physical(n, true, true, true);
    let half_committed: fn(usize) -> Option<Title> =
        |n| Title::virtual_alloc(n, Commit::Later(n / 2), true);
    vec![
        case(
            "virtual, reserved and committed at once, write-watched (xmem)",
            virtual_watched,
            Open::Address(0),
        ),
        Case {
            plain: true,
            ..case(
                "the same, buffer without ALLOW_CROSS_ADAPTER",
                virtual_watched,
                Open::Address(0),
            )
        },
        case("the same", virtual_watched, Open::Range(0, len)),
        case(
            "virtual, committed at once, not watched",
            |n| Title::virtual_alloc(n, Commit::AtOnce, false),
            Open::Address(0),
        ),
        case(
            "virtual, reserved, then all committed",
            |n| Title::virtual_alloc(n, Commit::Later(n), true),
            Open::Address(0),
        ),
        case(
            "virtual, reserved, first half committed",
            half_committed,
            Open::Address(0),
        ),
        case("the same", half_committed, Open::Range(0, len / 2)),
        case(
            "virtual, reserved only",
            |n| Title::virtual_alloc(n, Commit::Later(0), true),
            Open::Address(0),
        ),
        case(
            "virtual, committed: inside the allocation",
            virtual_watched,
            Open::Address(PAGE),
        ),
        case(
            "virtual, committed: inside the allocation",
            virtual_watched,
            Open::Range(PAGE, len / 2),
        ),
        case(
            "physical pages: SEC_RESERVE section, committed views in placeholders (xmem)",
            placeholder_reserve,
            Open::Address(0),
        ),
        case("the same", placeholder_reserve, Open::Mapping),
        case("the same", placeholder_reserve, Open::Range(0, len)),
        case(
            "the same: the view's second half",
            placeholder_reserve,
            Open::Range(len / 2, len / 2),
        ),
        case(
            "physical pages: SEC_RESERVE section, committed plain views (page_window, F3)",
            plain_reserve,
            Open::Address(0),
        ),
        case("the same", plain_reserve, Open::Mapping),
        case(
            "physical pages: SEC_COMMIT section, views in placeholders",
            placeholder_commit,
            Open::Address(0),
        ),
        case("the same", placeholder_commit, Open::Mapping),
        case(
            "the same, plain views",
            |n| Title::physical(n, true, false, true),
            Open::Range(0, len),
        ),
        case(
            "XMemAllocDefault block (process heap)",
            Title::heap_block,
            Open::Address(0),
        ),
    ]
}

/// Each kind of title memory xmem makes, opened each way, on every adapter:
/// what opens, whether the GPU and the CPU then see each other's writes
/// (both ways, every word), whether write watch still reports the CPU's
/// writes, and whether a second view of physical pages sees the GPU's.
#[test]
fn title_memory_import_matrix() {
    const LEN: usize = 4 * MIB;
    for gpu in &gpus() {
        let start = Instant::now();
        gpu.header();
        if !gpu.existing_heaps {
            println!("no existing heaps on {}: skipped", gpu.name);
            continue;
        }
        println!(
            "| title memory ({} MiB) | opened with | result | GPU and CPU see each other | write watch: CPU writes / GPU writes | second view |",
            LEN / MIB
        );
        println!("|---|---|---|---|---|---|");
        let mut opened = Vec::new();
        for (k, case) in cases(LEN).iter().enumerate() {
            let Some(title) = (case.make)(LEN) else {
                println!(
                    "| {} | {} | allocation failed | | | |",
                    case.kind, case.open
                );
                continue;
            };
            let outcome = import(gpu, &title, case.open, case.plain).and_then(|imported| {
                let trip = round_trip(gpu, &imported, &title, 0x5eed_0000 | k as u32)?;
                Ok((heap_text(&imported.desc), trip))
            });
            let row = match &outcome {
                Err(e) => format!("| {} | {} | {e} | | | |", case.kind, case.open),
                Ok((heap, trip)) => format!(
                    "| {} | {} | heap {heap} | {} | {} | {} |",
                    case.kind,
                    case.open,
                    match trip.wrong {
                        0 => format!("yes ({} KiB)", trip.len / KIB),
                        n => format!("**no**: {n} words wrong"),
                    },
                    trip.watch
                        .map_or("not watched".into(), |(cpu, of, gpu)| format!(
                            "{cpu} of {of} / {gpu}"
                        )),
                    trip.alias_wrong.map_or(String::new(), |n| match n {
                        0 => "sees the GPU's writes".into(),
                        n => format!("**{n} words wrong**"),
                    }),
                ),
            };
            println!("{row}");
            opened.push((case.kind, case.open, outcome));
        }
        // No silent fallback: whatever opens is the title's memory, both
        // ways and through every view.
        for (kind, open, outcome) in &opened {
            if let Ok((_, trip)) = outcome {
                assert_eq!(trip.wrong, 0, "{}: {kind}, {open}: words wrong", gpu.name);
                assert_eq!(
                    trip.alias_wrong.unwrap_or(0),
                    0,
                    "{}: {kind}, {open}: the second view differs",
                    gpu.name
                );
            }
        }
        // G16's ground: xmem's committed, write-watched memory opens in
        // place and write watch still sees the CPU's writes.
        let (_, _, base) = &opened[0];
        let trip = match base {
            Ok((_, trip)) => trip,
            Err(e) => panic!("{}: xmem's committed memory does not open: {e}", gpu.name),
        };
        let (by_cpu, written, _) = trip.watch.expect("write watch answers");
        assert_eq!(by_cpu, written, "{}: write watch lost CPU writes", gpu.name);
        eprintln!(
            "{}: import matrix in {} ms",
            gpu.name,
            start.elapsed().as_millis()
        );
    }
}

/// What the GPU reads through a heap opened over a view of physical pages
/// once the title maps other pages at that address (`XMemMapPhysicalPages`
/// over a mapping): the pages the heap was opened on, or those mapped there
/// now. It decides whether heaps can follow views or must sit on pages.
#[test]
fn physical_pages_remapped_under_an_open_heap() {
    const LEN: usize = 4 * PAGE;
    let words = LEN / 4;
    for gpu in &gpus() {
        if !gpu.existing_heaps {
            continue;
        }
        let title = Title::physical(LEN, false, true, false).expect("physical pages");
        let at = title.at;
        let fill = |seed| {
            // SAFETY: the committed pages the view shows, ours alone.
            let all = unsafe { std::slice::from_raw_parts_mut(at.cast::<u32>(), words) };
            for (i, w) in all.iter_mut().enumerate() {
                *w = pattern(i, seed);
            }
        };
        fill(1);
        let imported =
            import(gpu, &title, Open::Address(0), false).expect("a view of physical pages opens");
        let unmapped = unmap_to_placeholder(at);
        let outcome = if !unmapped {
            format!(
                "the view cannot be unmapped while the heap is open: {}",
                std::io::Error::last_os_error()
            )
        } else {
            let other = section(LEN, false).expect("a second section");
            // SAFETY: maps the second section into the placeholder the view
            // left; the title's drop unmaps it.
            let view = unsafe {
                vm::MapViewOfFile3(
                    other,
                    GetCurrentProcess(),
                    at.cast(),
                    0,
                    LEN,
                    vm::MEM_REPLACE_PLACEHOLDER,
                    vm::PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value.cast(), at, "the placeholder takes other pages");
            assert!(commit_view(at, LEN));
            fill(2);
            let vram = committed(&gpu.device, D3D12_HEAP_TYPE_DEFAULT, LEN).unwrap();
            let readback = committed(&gpu.device, D3D12_HEAP_TYPE_READBACK, LEN).unwrap();
            let vram_va = gpu_va(&vram);
            gpu.direct
                .record(|l| {
                    transition(
                        l,
                        &vram,
                        D3D12_RESOURCE_STATE_COMMON,
                        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    );
                    gpu.dispatch(l, Program::AddOne, imported.va, vram_va, words, 0);
                    transition(
                        l,
                        &vram,
                        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                    );
                    copy(l, &readback, &vram, LEN);
                })
                .unwrap();
            gpu.direct.run(&gpu.device).unwrap();
            let read = map(&readback).unwrap();
            // SAFETY: the readback buffer the GPU has finished writing.
            let read = unsafe { std::slice::from_raw_parts(read.cast::<u32>(), words) };
            // SAFETY: our second section's handle; its view keeps it alive.
            unsafe { CloseHandle(other) };
            match (wrong_words(read, 1), wrong_words(read, 2)) {
                (0, _) => "the pages it was opened on (now mapped nowhere)".into(),
                (_, 0) => "the pages mapped at the address now".into(),
                (a, b) => format!("**neither** ({a} / {b} words differ)"),
            }
        };
        println!(
            "{}: a heap over a view of physical pages, after the title maps other pages there, reads {outcome}",
            gpu.name
        );
        assert!(!outcome.contains("neither"), "{}: {outcome}", gpu.name);
        drop(imported);
        if !unmapped {
            // Once the heap is gone the same view unmaps: the heap held it.
            let after = once_released(|| unmap_to_placeholder(at)).unwrap_or_else(|| {
                panic!(
                    "{}: the view stays mapped after the heap is released",
                    gpu.name
                )
            });
            println!(
                "{}: once the heap is released, the view unmaps ({after:?} later)",
                gpu.name
            );
            // SAFETY: the placeholder the view left, ours; the title's drop
            // must not unmap it again.
            unsafe { vm::VirtualFree(at.cast(), 0, vm::MEM_RELEASE) };
            let mut title = title;
            title.views.clear();
        }
        // The control: without a heap, the same unmapping works.
        let control = Title::physical(LEN, false, true, false).expect("physical pages");
        assert!(
            unmap_to_placeholder(control.at),
            "a view with no heap over it unmaps"
        );
        let mut control = control;
        control.placeholders.push(control.at);
        control.views.clear();
    }
}

/// `XMemMapPhysicalPages`' unmapping: the view at `at` back to a
/// placeholder. False if the host refuses.
fn unmap_to_placeholder(at: *mut u8) -> bool {
    // SAFETY: a view of ours; nothing on the GPU uses it now.
    unsafe {
        vm::UnmapViewOfFile2(
            GetCurrentProcess(),
            vm::MEMORY_MAPPED_VIEW_ADDRESS { Value: at.cast() },
            vm::MEM_PRESERVE_PLACEHOLDER,
        ) != 0
    }
}

/// The title decommitting (`VirtualFree(MEM_DECOMMIT)`) or releasing part of
/// its virtual memory while a heap is open over it.
#[test]
fn virtual_memory_decommitted_under_an_open_heap() {
    const LEN: usize = 4 * PAGE;
    for gpu in &gpus() {
        if !gpu.existing_heaps {
            continue;
        }
        let title = Title::virtual_alloc(LEN, Commit::AtOnce, true).expect("title memory");
        let imported = import(gpu, &title, Open::Address(0), false).expect("xmem memory opens");
        // SAFETY: a page of our reservation; nothing on the GPU uses the heap
        // now, and nothing reads the page afterwards.
        let decommitted =
            unsafe { vm::VirtualFree(title.at.add(PAGE).cast(), PAGE, vm::MEM_DECOMMIT) } != 0;
        let error = std::io::Error::last_os_error();
        let state = committed_prefix(title.at, LEN);
        println!(
            "{}: decommitting a page under an open heap {} (committed prefix now {} KiB)",
            gpu.name,
            if decommitted {
                "succeeds".to_string()
            } else {
                format!("fails: {error}")
            },
            state / KIB
        );
        drop(imported);
        if !decommitted {
            let later = once_released(|| {
                // SAFETY: as above, with the heap gone.
                unsafe { vm::VirtualFree(title.at.add(PAGE).cast(), PAGE, vm::MEM_DECOMMIT) != 0 }
            });
            println!(
                "{}: once the heap is released, decommitting {}",
                gpu.name,
                match later {
                    Some(after) => format!("succeeds ({after:?} later)"),
                    None => "still fails".into(),
                }
            );
            assert!(
                later.is_some(),
                "{}: the page stays committed without a heap",
                gpu.name
            );
        }
    }
}

/// Runs `call`, a host call an open heap refused, until it succeeds. A
/// released heap lets go of its pages some time after its last reference
/// goes, on WARP at least: an unmap or a decommit right after the release
/// failed in about one run of three on 2026-11-24, more often under load.
/// Up to 10 s, a margin for a loaded machine; how long it took, or `None`.
fn once_released(mut call: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    while !call() {
        if start.elapsed() > Duration::from_secs(10) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Some(start.elapsed())
}

// ---------------------------------------------------------------------------
// Benchmarks

/// Repetitions measured after one warm-up; the median is reported.
const REPS: usize = 5;

fn median(mut run: impl FnMut() -> Result<Timing, String>) -> Result<Timing, String> {
    run()?;
    let mut all = (0..REPS).map(|_| run()).collect::<Result<Vec<_>, _>>()?;
    all.sort_by(|a, b| a.best().total_cmp(&b.best()));
    Ok(all[REPS / 2])
}

fn gbps(len: usize, seconds: f64) -> String {
    format!("{:.1}", len as f64 / seconds / 1e9)
}

/// Copies `len` bytes from `src` to `dst`, both memory of ours that nothing
/// else uses now, cycling over `src` or `dst` when one is shorter: CPU wall
/// seconds.
fn cpu_copy(dst: *mut u8, dst_len: usize, src: *const u8, src_len: usize, len: usize) -> Timing {
    let start = Instant::now();
    let mut done = 0;
    while done < len {
        let (d, s) = (done % dst_len, done % src_len);
        let n = (len - done).min(dst_len - d).min(src_len - s);
        // SAFETY: both ranges lie in their buffers; they do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(src.add(s), dst.add(d), n) };
        std::hint::black_box(dst);
        done += n;
    }
    Timing::cpu(start.elapsed().as_secs_f64())
}

/// Records `record` on `queue` and times it.
fn gpu_time(
    gpu: &Gpu,
    queue: &Queue,
    record: impl FnOnce(&ID3D12GraphicsCommandList),
) -> Result<Timing, String> {
    queue.record(record)?;
    median(|| queue.run(&gpu.device))
}

/// Opening title memory of each size, each way: whether it opens, how long
/// it takes, what it costs in commit, residency and the adapter's
/// accounting, and whether the GPU reaches the far end.
#[test]
#[ignore = "benchmark: commits up to 6 GiB; run optimised, alone, never on a machine in use"]
fn import_limits_and_commit_cost() {
    let sizes = [
        256 * MIB,
        GIB,
        2 * GIB,
        4 * GIB - PAGE,
        4 * GIB,
        4 * GIB + PAGE,
        6 * GIB,
    ];
    /// What to allocate, and how to open it for a size.
    type Kind = (&'static str, fn(usize) -> Option<Title>, fn(usize) -> Open);
    /// The most one heap takes.
    const MOST: usize = 4 * GIB - PAGE;
    let kinds: [Kind; 7] = [
        (
            "virtual, committed, write-watched",
            |n| Title::virtual_alloc(n, Commit::AtOnce, true),
            |_| Open::Address(0),
        ),
        (
            "virtual, committed, write-watched, every page written first",
            |n| {
                let title = Title::virtual_alloc(n, Commit::AtOnce, true)?;
                for page in (0..n).step_by(HOST_PAGE) {
                    // SAFETY: a byte of committed title memory of ours.
                    unsafe { title.at.add(page).write_volatile(1) };
                }
                Some(title)
            },
            |_| Open::Address(0),
        ),
        (
            "virtual, committed, write-watched",
            |n| Title::virtual_alloc(n, Commit::AtOnce, true),
            |n| Open::Range(0, n),
        ),
        (
            "virtual, reserved, first 4 GiB - 64 KiB committed",
            |n| Title::virtual_alloc(n, Commit::Later(n.min(MOST)), true),
            |n| Open::Range(0, n.min(MOST)),
        ),
        (
            "physical pages: SEC_RESERVE section, committed view in a placeholder",
            |n| Title::physical(n, false, true, false),
            |_| Open::Address(0),
        ),
        (
            "physical pages: the same view, its last 4 GiB - 64 KiB at most",
            |n| Title::physical(n, false, true, false),
            |n| Open::Range(n - n.min(MOST), n.min(MOST)),
        ),
        (
            "physical pages: SEC_COMMIT section, view in a placeholder",
            |n| Title::physical(n, true, true, false),
            |_| Open::Mapping,
        ),
    ];
    for gpu in &gpus() {
        gpu.header();
        println!(
            "| title memory | size | opened with | result | allocate ms | open ms | place ms | commit Δ MiB at allocation (process / system) | commit Δ MiB at opening (process / system) | working set Δ MiB at opening | adapter non-local Δ MiB at opening | far end | heap release ms | memory free ms |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        let commit = |a: &Usage, b: &Usage| {
            format!(
                "{} / {}",
                delta_mib(a.private as u64, b.private as u64),
                delta_mib(a.system as u64, b.system as u64)
            )
        };
        for &len in &sizes {
            for (kind, make, open) in &kinds {
                let open = open(len);
                let before = usage(gpu);
                let start = Instant::now();
                let Some(title) = make(len) else {
                    println!(
                        "| {kind} | {} | {open} | allocation failed: {} |{}",
                        size_text(len),
                        std::io::Error::last_os_error(),
                        " |".repeat(10)
                    );
                    continue;
                };
                let allocate = start.elapsed();
                let allocated = usage(gpu);
                let imported = import(gpu, &title, open, false);
                let opened = usage(gpu);
                let (result, times, far) = match &imported {
                    Err(e) => (e.clone(), String::from(" | "), String::new()),
                    Ok(i) => (
                        format!("heap {}", heap_text(&i.desc)),
                        format!(
                            "{:.1} | {:.1}",
                            i.open.as_secs_f64() * 1e3,
                            i.place.as_secs_f64() * 1e3
                        ),
                        far_end(gpu, i).unwrap_or_else(|e| e),
                    ),
                };
                let start = Instant::now();
                drop(imported);
                let close = start.elapsed();
                let start = Instant::now();
                drop(title);
                let release = start.elapsed();
                println!(
                    "| {kind} | {} | {open} | {result} | {:.1} | {times} | {} | {} | {} | {} | {far} | {:.1} | {:.1} |",
                    size_text(len),
                    allocate.as_secs_f64() * 1e3,
                    commit(&before, &allocated),
                    commit(&allocated, &opened),
                    delta_mib(allocated.working_set as u64, opened.working_set as u64),
                    delta_mib(allocated.non_local, opened.non_local),
                    close.as_secs_f64() * 1e3,
                    release.as_secs_f64() * 1e3,
                );
            }
        }
        let now = usage(gpu);
        println!(
            "\nsystem commit {} MiB, available RAM {} MiB, adapter local {} MiB",
            now.system / MIB,
            now.available / MIB,
            now.local >> 20
        );
    }
}

/// `len` in MiB below 1 GiB, else in GiB give or take some KiB.
fn size_text(len: usize) -> String {
    let gib = (len + GIB / 2) / GIB;
    match len as i64 - (gib * GIB) as i64 {
        _ if len < GIB => format!("{} MiB", len / MIB),
        0 => format!("{gib} GiB"),
        rest => format!("{gib} GiB {:+} KiB", rest / KIB as i64),
    }
}

/// The CPU writes the heap's next-to-last page, the GPU adds one into the
/// last, and the CPU checks it.
fn far_end(gpu: &Gpu, imported: &Imported) -> Result<String, String> {
    let at = imported.len - 2 * PAGE;
    let words = PAGE / 4;
    // SAFETY: the heap's last two pages, committed title memory of ours.
    let (src, dst) = unsafe {
        (
            std::slice::from_raw_parts_mut(imported.cpu.add(at).cast::<u32>(), words),
            std::slice::from_raw_parts(imported.cpu.add(at + PAGE).cast::<u32>(), words),
        )
    };
    for (i, w) in src.iter_mut().enumerate() {
        *w = pattern(i, 7);
    }
    let va = imported.va + at as u64;
    gpu.direct
        .record(|l| gpu.dispatch(l, Program::AddOne, va, va + PAGE as u64, words, 0))?;
    gpu.direct.run(&gpu.device)?;
    Ok(match wrong_words(dst, 7) {
        0 => "reached".into(),
        n => format!("**{n} words wrong**"),
    })
}

/// Bandwidth of each way title data can move between the CPU and the GPU:
/// in place through an imported heap, through today's staging buffers, and
/// through video memory (copies, the CPU-visible GPU upload heap).
#[test]
#[ignore = "benchmark: run optimised, alone, never on a machine in use"]
fn bandwidth() {
    for gpu in &gpus() {
        gpu.header();
        let sizes: &[usize] = if gpu.software() {
            &[16 * MIB, 256 * MIB]
        } else {
            &[16 * MIB, 256 * MIB, GIB, 4 * GIB - PAGE]
        };
        println!("| path | size | GB/s (GPU timestamps) | GB/s (wall) |");
        println!("|---|---|---|---|");
        let row = |path: &str, len: usize, t: Result<Timing, String>| match t {
            Ok(t) => println!(
                "| {path} | {} | {} | {} |",
                size_text(len),
                t.gpu.map_or("".into(), |g| gbps(len, g)),
                gbps(len, t.wall)
            ),
            Err(e) => println!("| {path} | {} | {e} | |", size_text(len)),
        };
        let scratch = committed(&gpu.device, D3D12_HEAP_TYPE_DEFAULT, PAGE).expect("scratch");
        let scratch_va = gpu_va(&scratch);
        let source = vec![0x5au8; 64 * MIB];
        let mut sink = vec![0u8; 64 * MIB];
        for &len in sizes {
            let title = Title::virtual_alloc(len, Commit::AtOnce, true).expect("title memory");
            let imported = import(gpu, &title, Open::Address(0), false).expect("import");
            let (va, cpu) = (imported.va, imported.cpu);
            row(
                "CPU writes title memory (memcpy)",
                len,
                median(|| Ok(cpu_copy(cpu, len, source.as_ptr(), source.len(), len))),
            );
            row(
                "GPU reads imported title memory (shader)",
                len,
                gpu_time(gpu, &gpu.direct, |l| {
                    gpu.dispatch(l, Program::Read, va, scratch_va, len / 16, 1)
                }),
            );
            row(
                "GPU writes imported title memory (shader, 4 B a thread)",
                len,
                gpu_time(gpu, &gpu.direct, |l| {
                    gpu.dispatch(l, Program::WriteWords, 0, va, len / 4, 1)
                }),
            );
            row(
                "CPU reads title memory the GPU wrote (memcpy)",
                len,
                median(|| Ok(cpu_copy(sink.as_mut_ptr(), sink.len(), cpu, len, len))),
            );
            if len > GIB {
                continue;
            }
            let vram = committed(&gpu.device, D3D12_HEAP_TYPE_DEFAULT, len).expect("vram");
            let upload = committed(&gpu.device, D3D12_HEAP_TYPE_UPLOAD, len).expect("upload");
            let readback = committed(&gpu.device, D3D12_HEAP_TYPE_READBACK, len).expect("readback");
            let (upload_cpu, readback_cpu) = (map(&upload).unwrap(), map(&readback).unwrap());
            let vram_va = gpu_va(&vram);
            row(
                "GPU reads video memory (shader)",
                len,
                gpu_time(gpu, &gpu.direct, |l| {
                    gpu.dispatch(l, Program::Read, vram_va, scratch_va, len / 16, 1)
                }),
            );
            row(
                "GPU writes video memory (shader)",
                len,
                gpu_time(gpu, &gpu.direct, |l| {
                    gpu.dispatch(l, Program::Write, 0, vram_va, len / 16, 1)
                }),
            );
            row(
                "copy imported title memory → video memory (direct queue)",
                len,
                gpu_time(gpu, &gpu.direct, |l| copy(l, &vram, &imported.buffer, len)),
            );
            row(
                "copy imported title memory → video memory (copy queue)",
                len,
                gpu_time(gpu, &gpu.copy, |l| copy(l, &vram, &imported.buffer, len)),
            );
            row(
                "copy video memory → imported title memory (copy queue)",
                len,
                gpu_time(gpu, &gpu.copy, |l| copy(l, &imported.buffer, &vram, len)),
            );
            row(
                "CPU writes an upload heap (memcpy, today's staging)",
                len,
                median(|| Ok(cpu_copy(upload_cpu, len, cpu, len, len))),
            );
            row(
                "copy upload heap → video memory (copy queue)",
                len,
                gpu_time(gpu, &gpu.copy, |l| copy(l, &vram, &upload, len)),
            );
            row(
                "copy video memory → readback heap (copy queue)",
                len,
                gpu_time(gpu, &gpu.copy, |l| copy(l, &readback, &vram, len)),
            );
            row(
                "CPU reads a readback heap (memcpy)",
                len,
                median(|| Ok(cpu_copy(cpu, len, readback_cpu, len, len))),
            );
            if gpu.gpu_upload {
                let rebar = committed(&gpu.device, D3D12_HEAP_TYPE_GPU_UPLOAD, len)
                    .expect("GPU upload heap");
                let (rebar_cpu, rebar_va) = (map(&rebar).unwrap(), gpu_va(&rebar));
                row(
                    "CPU writes the GPU upload heap (memcpy into video memory)",
                    len,
                    median(|| Ok(cpu_copy(rebar_cpu, len, cpu, len, len))),
                );
                row(
                    "GPU reads the GPU upload heap (shader)",
                    len,
                    gpu_time(gpu, &gpu.direct, |l| {
                        gpu.dispatch(l, Program::Read, rebar_va, scratch_va, len / 16, 1)
                    }),
                );
            }
        }
    }
}

/// Shader and copy-queue bandwidth into each kind of imported title memory,
/// to tell what slows the GPU's shader writes there: the heap's write-watch
/// flag, the kind of memory, or the width of each write. The 16-byte stores
/// cover `WIDE` bytes only: on the RTX 4080 they run at 0.1 GB/s, and 4 GiB
/// of them hung the GPU.
#[test]
#[ignore = "benchmark: run optimised, alone, never on a machine in use"]
fn imported_memory_by_kind() {
    const LEN: usize = 256 * MIB;
    const WIDE: usize = 16 * MIB;
    type Make = fn(usize) -> Option<Title>;
    let kinds: [(&str, Make, Open); 6] = [
        (
            "virtual, committed, write-watched (xmem)",
            |n| Title::virtual_alloc(n, Commit::AtOnce, true),
            Open::Address(0),
        ),
        (
            "virtual, committed, write-watched (xmem)",
            |n| Title::virtual_alloc(n, Commit::AtOnce, true),
            Open::Range(0, LEN),
        ),
        (
            "virtual, committed, not watched",
            |n| Title::virtual_alloc(n, Commit::AtOnce, false),
            Open::Address(0),
        ),
        (
            "physical pages: SEC_RESERVE section, committed view (xmem)",
            |n| Title::physical(n, false, true, false),
            Open::Address(0),
        ),
        (
            "physical pages: SEC_COMMIT section",
            |n| Title::physical(n, true, true, false),
            Open::Mapping,
        ),
        (
            "physical pages: SEC_COMMIT section",
            |n| Title::physical(n, true, true, false),
            Open::Address(0),
        ),
    ];
    for gpu in &gpus() {
        gpu.header();
        let vram = committed(&gpu.device, D3D12_HEAP_TYPE_DEFAULT, LEN).expect("vram");
        let scratch = committed(&gpu.device, D3D12_HEAP_TYPE_DEFAULT, PAGE).expect("scratch");
        let scratch_va = gpu_va(&scratch);
        println!(
            "| title memory ({} MiB) | opened with | heap | shader reads GB/s | shader writes 16 B a thread GB/s (first {} MiB) | shader writes 4 B a thread GB/s | copy queue writes GB/s | copy queue reads GB/s |",
            LEN / MIB,
            WIDE / MIB
        );
        println!("|---|---|---|---|---|---|---|---|");
        for (kind, make, open) in &kinds {
            let title = make(LEN).expect("title memory");
            let imported = match import(gpu, &title, *open, false) {
                Ok(i) => i,
                Err(e) => {
                    println!("| {kind} | {open} | {e} | | | | | |");
                    continue;
                }
            };
            let va = imported.va;
            let rate = |t: Result<Timing, String>| t.map_or_else(|e| e, |t| gbps(LEN, t.best()));
            let reads = rate(gpu_time(gpu, &gpu.direct, |l| {
                gpu.dispatch(l, Program::Read, va, scratch_va, LEN / 16, 1)
            }));
            let wide = gpu_time(gpu, &gpu.direct, |l| {
                gpu.dispatch(l, Program::Write, 0, va, WIDE / 16, 1)
            })
            .map_or_else(|e| e, |t| gbps(WIDE, t.best()));
            let narrow = rate(gpu_time(gpu, &gpu.direct, |l| {
                gpu.dispatch(l, Program::WriteWords, 0, va, LEN / 4, 1)
            }));
            let copy_in = rate(gpu_time(gpu, &gpu.copy, |l| {
                copy(l, &imported.buffer, &vram, LEN)
            }));
            let copy_out = rate(gpu_time(gpu, &gpu.copy, |l| {
                copy(l, &vram, &imported.buffer, LEN)
            }));
            println!(
                "| {kind} | {open} | {} | {reads} | {wide} | {narrow} | {copy_in} | {copy_out} |",
                heap_text(&imported.desc)
            );
        }
    }
}

/// Latency of one word written by the CPU, read and answered by the GPU,
/// and seen again by the CPU: in place through an imported heap, and
/// through today's upload, video memory and readback buffers.
#[test]
#[ignore = "benchmark: run optimised, alone, never on a machine in use"]
fn latency() {
    const ROUNDS: usize = 300;
    for gpu in &gpus() {
        gpu.header();
        println!("| round trip | min µs | median µs | p99 µs | max µs |");
        println!("|---|---|---|---|---|");
        let title = Title::virtual_alloc(2 * PAGE, Commit::AtOnce, true).expect("title memory");
        let imported = import(gpu, &title, Open::Address(0), false).expect("import");
        let vram = committed(&gpu.device, D3D12_HEAP_TYPE_DEFAULT, 2 * PAGE).unwrap();
        let upload = committed(&gpu.device, D3D12_HEAP_TYPE_UPLOAD, PAGE).unwrap();
        let readback = committed(&gpu.device, D3D12_HEAP_TYPE_READBACK, PAGE).unwrap();
        let (upload_cpu, readback_cpu) = (map(&upload).unwrap(), map(&readback).unwrap());
        let word_in = imported.cpu.cast::<u32>();
        let word_out = imported.cpu.wrapping_add(PAGE).cast::<u32>();
        // Each round: `write` puts a value where the GPU reads it, then the
        // list is submitted and `seen` waits until the answer is visible.
        let measure =
            |path: &str, write: &dyn Fn(u32), seen: &dyn Fn(u64, u32) -> Result<(), String>| {
                let mut us = Vec::with_capacity(ROUNDS);
                for round in 1..=ROUNDS as u32 {
                    write(round);
                    let start = Instant::now();
                    let value = match gpu.direct.submit() {
                        Ok(v) => v,
                        Err(e) => return println!("| {path} | {e} | | | |"),
                    };
                    if let Err(e) = seen(value, round) {
                        return println!("| {path} | {e} | | | |");
                    }
                    us.push(start.elapsed().as_secs_f64() * 1e6);
                    let _ = gpu.direct.wait(&gpu.device, value, true);
                }
                us.sort_by(f64::total_cmp);
                println!(
                    "| {path} | {:.1} | {:.1} | {:.1} | {:.1} |",
                    us[0],
                    us[ROUNDS / 2],
                    us[ROUNDS * 99 / 100],
                    us[ROUNDS - 1]
                );
            };
        let fence = |spin| move |value, _| gpu.direct.wait(&gpu.device, value, spin);
        gpu.direct.record(|_| {}).unwrap();
        measure("empty list, fence event", &|_| {}, &fence(false));
        measure("empty list, fence spin", &|_| {}, &fence(true));
        let write_in = |v: u32| {
            // SAFETY: a word of committed title memory of ours.
            unsafe { word_in.write_volatile(v) }
        };
        let check = |at: *const u32| {
            move |value, round: u32| {
                gpu.direct.wait(&gpu.device, value, true)?;
                // SAFETY: a word of ours the GPU has finished writing.
                match unsafe { at.read_volatile() } {
                    v if v == round + 1 => Ok(()),
                    v => Err(format!("read {v}, wanted {}", round + 1)),
                }
            }
        };
        gpu.direct
            .record(|l| {
                gpu.dispatch(
                    l,
                    Program::AddOne,
                    imported.va,
                    imported.va + PAGE as u64,
                    1,
                    0,
                )
            })
            .unwrap();
        measure(
            "imported: CPU write → shader → CPU read, after the fence",
            &write_in,
            &check(word_out),
        );
        measure(
            "imported: the same, the CPU polling the word (no fence)",
            &write_in,
            &|_, round| {
                let start = Instant::now();
                // SAFETY: a word of ours the GPU writes.
                while unsafe { word_out.read_volatile() } != round + 1 {
                    if start.elapsed() > Duration::from_secs(10) {
                        return Err("the answer never showed".into());
                    }
                    std::hint::spin_loop();
                }
                Ok(())
            },
        );
        let vram_va = gpu_va(&vram);
        gpu.direct
            .record(|l| {
                // SAFETY: an open list; buffers of ours that outlive it.
                unsafe { l.CopyBufferRegion(&vram, 0, &upload, 0, 4) };
                transition(
                    l,
                    &vram,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                );
                gpu.dispatch(l, Program::AddOne, vram_va, vram_va + PAGE as u64, 1, 0);
                transition(
                    l,
                    &vram,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                );
                // SAFETY: as above.
                unsafe { l.CopyBufferRegion(&readback, 0, &vram, PAGE as u64, 4) };
            })
            .unwrap();
        measure(
            "staged: CPU write → upload → video memory → shader → readback → CPU read",
            // SAFETY: the mapped upload buffer's first word.
            &|v| unsafe { upload_cpu.cast::<u32>().write_volatile(v) },
            &check(readback_cpu.cast::<u32>()),
        );
    }
}

/// What a world frame's title data costs each way: 256 MiB (the 220–285 MB
/// a frame copies today) once to the GPU and once back, through today's
/// staging buffers, in place through an imported heap, and through a video
/// memory mirror the GPU fills from imported memory (or the CPU through the
/// GPU upload heap).
#[test]
#[ignore = "benchmark: run optimised, alone, never on a machine in use"]
fn frame_cost() {
    const LEN: usize = 256 * MIB;
    let words = LEN / 16;
    for gpu in &gpus() {
        gpu.header();
        let title = Title::virtual_alloc(LEN, Commit::AtOnce, true).expect("title memory");
        let imported = import(gpu, &title, Open::Address(0), false).expect("import");
        let vram = committed(&gpu.device, D3D12_HEAP_TYPE_DEFAULT, LEN).unwrap();
        let upload = committed(&gpu.device, D3D12_HEAP_TYPE_UPLOAD, LEN).unwrap();
        let readback = committed(&gpu.device, D3D12_HEAP_TYPE_READBACK, LEN).unwrap();
        let scratch = committed(&gpu.device, D3D12_HEAP_TYPE_DEFAULT, PAGE).unwrap();
        let (upload_cpu, readback_cpu) = (map(&upload).unwrap(), map(&readback).unwrap());
        let (vram_va, scratch_va, cpu) = (gpu_va(&vram), gpu_va(&scratch), imported.cpu);
        let source = vec![0x33u8; 64 * MIB];
        cpu_copy(cpu, LEN, source.as_ptr(), source.len(), LEN);
        println!("| direction | path | ms per 256 MiB | GB/s |");
        println!("|---|---|---|---|");
        let row = |direction: &str, path: &str, t: Result<f64, String>| match t {
            Ok(s) => println!(
                "| {direction} | {path} | {:.1} | {} |",
                s * 1e3,
                gbps(LEN, s)
            ),
            Err(e) => println!("| {direction} | {path} | {e} | |"),
        };
        let wall = |t: Result<Timing, String>| t.map(|t| t.wall);
        let to_gpu = "CPU → GPU";
        let from_gpu = "GPU → CPU";

        // Today: the CPU copies into an upload heap, the GPU copies it into
        // video memory, then reads it there.
        let staged_up = gpu.direct.record(|l| {
            copy(l, &vram, &upload, LEN);
            transition(
                l,
                &vram,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            gpu.dispatch(l, Program::Read, vram_va, scratch_va, words, 1);
        });
        row(
            to_gpu,
            "staged (today): memcpy to upload heap, copy to video memory, shader reads it",
            staged_up.and_then(|()| {
                wall(median(|| {
                    let c = cpu_copy(upload_cpu, LEN, cpu, LEN, LEN);
                    Ok(Timing::cpu(c.wall + gpu.direct.run(&gpu.device)?.wall))
                }))
            }),
        );
        let in_place = gpu
            .direct
            .record(|l| gpu.dispatch(l, Program::Read, imported.va, scratch_va, words, 1));
        row(
            to_gpu,
            "imported (G16): shader reads title memory in place",
            in_place.and_then(|()| wall(median(|| gpu.direct.run(&gpu.device)))),
        );
        let pulled = gpu
            .copy
            .record(|l| copy(l, &vram, &imported.buffer, LEN))
            .and_then(|()| {
                gpu.direct
                    .record(|l| gpu.dispatch(l, Program::Read, vram_va, scratch_va, words, 1))
            });
        row(
            to_gpu,
            "mirror (G17): copy queue pulls imported title memory into video memory, shader reads it",
            pulled.and_then(|()| {
                wall(median(|| {
                    let c = gpu.copy.run(&gpu.device)?;
                    Ok(Timing::cpu(c.wall + gpu.direct.run(&gpu.device)?.wall))
                }))
            }),
        );
        if gpu.gpu_upload {
            let rebar = committed(&gpu.device, D3D12_HEAP_TYPE_GPU_UPLOAD, LEN).unwrap();
            let (rebar_cpu, rebar_va) = (map(&rebar).unwrap(), gpu_va(&rebar));
            let read = gpu
                .direct
                .record(|l| gpu.dispatch(l, Program::Read, rebar_va, scratch_va, words, 1));
            row(
                to_gpu,
                "mirror: memcpy into the GPU upload heap (video memory), shader reads it",
                read.and_then(|()| {
                    wall(median(|| {
                        let c = cpu_copy(rebar_cpu, LEN, cpu, LEN, LEN);
                        Ok(Timing::cpu(c.wall + gpu.direct.run(&gpu.device)?.wall))
                    }))
                }),
            );
        }

        // And back: the GPU writes, the CPU reads.
        let staged_down = gpu.direct.record(|l| {
            transition(
                l,
                &vram,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            gpu.dispatch(l, Program::Write, 0, vram_va, words, 1);
            transition(
                l,
                &vram,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            copy(l, &readback, &vram, LEN);
        });
        row(
            from_gpu,
            "staged (today): shader writes video memory, copy to readback heap, memcpy to title memory",
            staged_down.and_then(|()| {
                wall(median(|| {
                    let g = gpu.direct.run(&gpu.device)?;
                    Ok(Timing::cpu(
                        g.wall + cpu_copy(cpu, LEN, readback_cpu, LEN, LEN).wall,
                    ))
                }))
            }),
        );
        // 4 bytes a thread: 16-byte stores into imported memory crawl (see
        // `imported_memory_by_kind`).
        let in_place = gpu
            .direct
            .record(|l| gpu.dispatch(l, Program::WriteWords, 0, imported.va, LEN / 4, 1));
        row(
            from_gpu,
            "imported (G16): shader writes title memory in place, 4 B a thread",
            in_place.and_then(|()| wall(median(|| gpu.direct.run(&gpu.device)))),
        );
        let pushed = gpu
            .direct
            .record(|l| gpu.dispatch(l, Program::Write, 0, vram_va, words, 1))
            .and_then(|()| gpu.copy.record(|l| copy(l, &imported.buffer, &vram, LEN)));
        row(
            from_gpu,
            "mirror (G17): shader writes video memory, copy queue pushes it to title memory",
            pushed.and_then(|()| {
                wall(median(|| {
                    let g = gpu.direct.run(&gpu.device)?;
                    Ok(Timing::cpu(g.wall + gpu.copy.run(&gpu.device)?.wall))
                }))
            }),
        );
    }
}
