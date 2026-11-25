//! The inputs of a host BLAS build, made by a pass of ours on the GPU,
//! where the build reads them (Codex review 11f27db1, P1-2). A check of
//! title memory when the build is recorded saw another version than the
//! build: the host copy is refreshed at submission, and a copy or a shader
//! of the same submission may write it before the build. So, ordered after
//! every earlier write, the pass makes every input of the build in a
//! buffer of the build's own (`indices.hlsl`):
//!
//! - each indexed geometry's indices, as 32-bit ones: copied from 16-bit
//!   or 32-bit indices, or decoded from the console builder's
//!   meshlet-packed ones ([`console`]). One past the geometry's vertices,
//!   or a packed part outside the bytes recorded for it, raises the
//!   build's flag, and then every index of the build becomes 0: degenerate
//!   triangles, which give no intersection, so the build takes none of
//!   them, and the refusal is logged once the submission has run
//!   ([`Check`]);
//! - each geometry's vertices: decoded from the console builder's
//!   quantized ones to `R32G32B32_FLOAT` positions, or copied;
//! - each geometry's transform, copied.
//!
//! The pass reads the title's copies as UAVs, as the instance rewrite does
//! (`instances::rewrite`): a shader of the title that wrote one earlier in
//! the submission left it promoted to that state, which a shader resource
//! read may not take (Codex review dcc6e70a, P1-1). It then returns them to
//! the common state, as the title's later work and the next pass take
//! them; the build reads only the pass's buffer, so no title copy is left
//! in a read state the next pass could not read as a UAV.
//!
//! shortcut: a refused build still takes the triangles of its geometries
//! without indices; only the indexed ones are left out.
//!
//! shortcut: a title copy a draw of the same submission read before the
//! pass is promoted to a read state, which a UAV read may not take; the
//! debug layer may report it (the instance rewrite shares it).

use super::*;
use windows::core::s;

/// How the pass makes one input of a build (`kind` in `indices.hlsl`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Kind {
    /// 16-bit indices, made 32-bit.
    Narrow,
    /// 32-bit indices.
    Wide,
    /// The console builder's meshlet-packed indices ([`console`]): the
    /// pass reads `limit` bytes of the window at most, and `flip` swaps
    /// each triangle's first and third corners.
    Packed { limit: u32, flip: bool },
    /// The console builder's quantized vertices ([`console`]), the window
    /// at their header, `stride` bytes apart: decoded to float3 positions.
    Quantized { stride: u32 },
    /// Words copied as they are.
    Words,
}

impl Kind {
    fn code(self) -> u32 {
        match self {
            Kind::Narrow => 0,
            Kind::Wide => 1,
            Kind::Packed { .. } => 2,
            Kind::Quantized { .. } => 3,
            Kind::Words => 4,
        }
    }
}

/// What a made input is to its geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Input {
    /// Its indices, as 32-bit ones.
    Indices,
    /// Its vertices, in `format`, `stride` bytes apart, the first `skew`
    /// bytes into the made copy (copied 2-byte aligned vertices start
    /// inside a word).
    Vertices {
        format: DXGI_FORMAT,
        stride: u64,
        skew: u32,
    },
    /// Its transform.
    Transform,
}

/// One input of a build, as the pass makes it.
pub(super) struct Job {
    /// Which geometry of the build.
    pub(super) geometry: usize,
    pub(super) input: Input,
    pub(super) kind: Kind,
    /// The title's host copy that holds the window.
    pub(super) shadow: Arc<crate::shadow::Shadow>,
    /// The window's first byte, a host address: a 16-bit index's, or a
    /// word's.
    pub(super) source: u64,
    /// The indices, vertices or words it makes.
    pub(super) count: u32,
    /// Indices: the geometry's vertex count, which they must be below.
    pub(super) vertices: u32,
}

impl Job {
    /// The bytes it makes.
    fn bytes(&self) -> u64 {
        let each = match self.kind {
            Kind::Quantized { .. } => 12,
            _ => 4,
        };
        u64::from(self.count) * each
    }
}

/// The root view the pass reads `source` through, 16-byte aligned as a raw
/// view's base must be, and the byte of `source` in it (Codex review
/// dcc6e70a, P1-2).
pub(super) fn view(source: u64) -> (u64, u32) {
    (source & !15, (source & 15) as u32)
}

fn pipeline(device: &ID3D12Device5) -> Option<&'static (ID3D12RootSignature, ID3D12PipelineState)> {
    static PIPELINE: OnceLock<Option<(ID3D12RootSignature, ID3D12PipelineState)>> = OnceLock::new();
    PIPELINE
        .get_or_init(|| {
            instances::compute_pass(device, include_bytes!("indices.hlsl"), s!("rt_indices"))
        })
        .as_ref()
}

/// Thread groups for `count` threads of 64: rows of at most 65,535 groups.
fn groups(count: u32) -> (u32, u32) {
    let groups = count.div_ceil(64);
    (groups.min(65535), groups.div_ceil(65535).max(1))
}

/// The pass's steps (`step` in `indices.hlsl`).
const CLEAR: u32 = 0;
const MAKE: u32 = 1;
const ZERO_IF_FLAGGED: u32 = 2;

/// Records, before a BLAS build, what `jobs` make, and points each of
/// their geometries at it. `at` is the BLAS's title address, for the log.
pub(super) fn record(
    rec: &Recorder<'_>,
    title_list: *const Object,
    geometries: &mut [Geometry],
    jobs: &[Job],
    at: u64,
) -> Option<()> {
    if jobs.is_empty() {
        return Some(());
    }
    let (root, pso) = pipeline(rec.device)?;
    // The flag word, then each job's output, 16-byte aligned.
    let mut bytes = 16u64;
    let firsts = (jobs.iter())
        .map(|job| {
            let first = u32::try_from(bytes).ok()?;
            bytes = bytes.checked_add(job.bytes())?.next_multiple_of(16);
            Some(first)
        })
        .collect::<Option<Vec<u32>>>()?;
    u32::try_from(bytes).ok()?;
    let built = evicting(|| buffer(rec, bytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS))?;
    rec.list.keep(&built);
    let indexed = jobs.iter().any(|job| job.input == Input::Indices);
    // Only indices can raise the flag.
    let flag = if indexed { flag_slot(rec.device) } else { None };
    // Each title copy the pass reads, once.
    let mut sources: Vec<&ID3D12Resource> = Vec::new();
    for job in jobs {
        let buffer = &job.shadow.buffer;
        if !sources.iter().any(|s| s.as_raw() == buffer.as_raw()) {
            rec.list.keep(buffer);
            sources.push(buffer);
        }
    }
    #[cfg(feature = "diagnostics")]
    rec.list.note_op(|| {
        format!(
            "host DXR BLAS inputs made for title address {at:#x}: {} jobs, {bytes} bytes",
            jobs.len()
        )
    });
    // SAFETY: our pass's root signature and pipeline; every root view lies
    // in a live buffer the list keeps (each window was checked to lie in
    // its title copy, a packed index buffer's within its limit); the
    // buffers are in the states named.
    unsafe {
        let gpu = built.GetGPUVirtualAddress();
        let c = &rec.commands;
        c.SetComputeRootSignature(root);
        c.SetPipelineState(pso);
        c.SetComputeRootUnorderedAccessView(2, gpu);
        // Steps that read no title copy name the pass's own buffer.
        let run = |job: Option<(&Job, u32)>, step: u32| {
            let Some((job, first)) = job else {
                let constants = [0, 0, 0, 0, 0, step, 0, 0, 0];
                c.SetComputeRoot32BitConstants(0, 9, constants.as_ptr().cast(), 0);
                c.SetComputeRootUnorderedAccessView(1, gpu);
                c.Dispatch(1, 1, 1);
                return;
            };
            let (source, offset) = if step == MAKE {
                view(job.source)
            } else {
                (gpu, 0)
            };
            let (limit, flip, stride) = match job.kind {
                Kind::Packed { limit, flip } => (limit, u32::from(flip), 0),
                Kind::Quantized { stride } => (0, 0, stride),
                _ => (0, 0, 0),
            };
            let constants = [
                job.count,
                job.kind.code(),
                job.vertices,
                first,
                offset,
                step,
                limit,
                flip,
                stride,
            ];
            c.SetComputeRoot32BitConstants(0, 9, constants.as_ptr().cast(), 0);
            c.SetComputeRootUnorderedAccessView(1, source);
            let (x, y) = groups(job.count);
            c.Dispatch(x, y, 1);
        };
        run(None, CLEAR);
        // The title's and our writes of the inputs, and the flag's reset,
        // before they are read.
        c.ResourceBarrier(&[uav_barrier()]);
        for (job, &first) in jobs.iter().zip(&firsts) {
            run(Some((job, first)), MAKE);
        }
        // What the pass made before it is read; the title's copies back
        // to the common state.
        let mut barriers = vec![uav_barrier()];
        barriers.extend(sources.iter().map(|source| {
            crate::host::transition(
                source,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            )
        }));
        c.ResourceBarrier(&barriers);
        if indexed {
            for (job, &first) in jobs.iter().zip(&firsts) {
                if job.input == Input::Indices {
                    run(Some((job, first)), ZERO_IF_FLAGGED);
                }
            }
        }
        let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        if let Some((readback, slot)) = &flag {
            c.ResourceBarrier(&[crate::host::transition(
                &built,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            c.CopyBufferRegion(readback, u64::from(*slot) * 4, &built, 0, 4);
            c.ResourceBarrier(&[crate::host::transition(
                &built,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                read,
            )]);
        } else {
            c.ResourceBarrier(&[crate::host::transition(
                &built,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                read,
            )]);
        }
        for (job, &first) in jobs.iter().zip(&firsts) {
            let t = &mut geometries[job.geometry].Anonymous.Triangles;
            let made = gpu + u64::from(first);
            match job.input {
                Input::Indices => {
                    t.IndexFormat = DXGI_FORMAT_R32_UINT;
                    t.IndexBuffer = made;
                }
                Input::Vertices {
                    format,
                    stride,
                    skew,
                } => {
                    t.VertexFormat = format;
                    t.VertexBuffer = D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                        StartAddress: made + u64::from(skew),
                        StrideInBytes: stride,
                    };
                }
                Input::Transform => t.Transform3x4 = made,
            }
        }
    }
    decoded(jobs);
    if let Some((_, slot)) = flag {
        // Dropped once the list's submission has run (or the list is reset
        // unsubmitted), which reads the flag.
        let check = Check { slot, at };
        rec.list.defer_host((0, 0), move || {
            let _ = &check;
        });
    }
    // Our pass replaced the list's compute root signature and pipeline.
    crate::list::forget_host_bindings(title_list);
    Some(())
}

/// Counts the geometries decoded from the console builder's formats, and
/// logs them at powers of two: what a session decodes.
fn decoded(jobs: &[Job]) {
    static PACKED: AtomicU64 = AtomicU64::new(0);
    static VERTICES: AtomicU64 = AtomicU64::new(0);
    let count = |kind: fn(&Kind) -> bool| jobs.iter().filter(|job| kind(&job.kind)).count() as u64;
    let packed = count(|kind| matches!(kind, Kind::Packed { .. }));
    let quantized = count(|kind| matches!(kind, Kind::Quantized { .. }));
    if packed + quantized == 0 {
        return;
    }
    let before = PACKED.fetch_add(packed, Ordering::Relaxed);
    let before_quantized = VERTICES.fetch_add(quantized, Ordering::Relaxed);
    let (was, now) = (
        before + before_quantized,
        before + before_quantized + packed + quantized,
    );
    if now.ilog2() != was.checked_ilog2().unwrap_or(u32::MAX) {
        tracing::info!(
            packed = before + packed,
            quantized = before_quantized + quantized,
            "host DXR BLAS geometries decoded on the GPU from the console builder's \
             meshlet-packed indices and quantized vertices"
        );
    }
}

/// Flag words the GPU copies the builds' flags into, read back on the CPU.
struct Flags {
    buffer: ID3D12Resource,
    mapped: *mut u32,
    free: Vec<u32>,
}

// SAFETY: the mapping is the buffer's, persistent while it lives.
unsafe impl Send for Flags {}

/// Flags in flight at most; a build past them is still checked, without a
/// log.
const FLAG_SLOTS: u32 = 16384;

static FLAGS: Mutex<Option<Flags>> = Mutex::new(None);

/// A free flag word, zeroed, and its buffer.
fn flag_slot(device: &ID3D12Device5) -> Option<(ID3D12Resource, u32)> {
    let mut flags = FLAGS.lock().unwrap_or_else(|e| e.into_inner());
    if flags.is_none() {
        *flags = make_flags(device);
    }
    let f = flags.as_mut()?;
    let slot = f.free.pop()?;
    // SAFETY: a word of the persistent mapping that no build in flight uses.
    unsafe { f.mapped.add(slot as usize).write_volatile(0) };
    Some((f.buffer.clone(), slot))
}

fn make_flags(device: &ID3D12Device5) -> Option<Flags> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: u64::from(FLAG_SLOTS) * 4,
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
    let mut buffer: Option<ID3D12Resource> = None;
    let mut mapped = std::ptr::null_mut();
    // SAFETY: one read-back buffer in the state its heap requires, mapped
    // whole for as long as it lives.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        buffer.Map(0, None, Some(&mut mapped)).ok()?;
        crate::host::name(&buffer, || "host DXR index check flags".into());
        Some(Flags {
            buffer,
            mapped: mapped.cast(),
            free: (0..FLAG_SLOTS).rev().collect(),
        })
    }
}

/// The flag bits (`indices.hlsl`): an index past its vertices; a part of
/// a packed index buffer outside the bytes recorded for it.
const PAST_VERTICES: u32 = 1;
const OUTSIDE: u32 = 2;

/// A build's flag word, read once its submission has run: raised, the
/// build was refused on the GPU.
struct Check {
    slot: u32,
    at: u64,
}

impl Drop for Check {
    fn drop(&mut self) {
        let raised = {
            let mut flags = FLAGS.lock().unwrap_or_else(|e| e.into_inner());
            let Some(f) = flags.as_mut() else {
                return;
            };
            // SAFETY: this check's word of the persistent mapping, which the
            // GPU wrote, if at all, before the submission finished.
            let raised = unsafe { f.mapped.add(self.slot as usize).read_volatile() };
            f.free.push(self.slot);
            raised
        };
        if raised != 0 {
            refused_on_gpu(self.at, raised);
        }
    }
}

/// Logs a BLAS whose indices the GPU refused (counted; at powers of two).
fn refused_on_gpu(at: u64, raised: u32) {
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    let refused = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    if refused.is_power_of_two() {
        let why = match (raised & PAST_VERTICES != 0, raised & OUTSIDE != 0) {
            (true, true) => {
                "an index past its vertices, and a part of its meshlet-packed indices \
                 outside the bytes title memory held when the build was recorded"
            }
            (false, true) => {
                "a part of its meshlet-packed indices outside the bytes title memory held \
                 when the build was recorded"
            }
            _ => "an index past its vertices",
        };
        tracing::warn!(
            address = format_args!("{at:#x}"),
            refused,
            "host DXR BLAS input refused on the GPU: {why}, as the build read it; the build \
             took none of its indexed triangles"
        );
    }
}
