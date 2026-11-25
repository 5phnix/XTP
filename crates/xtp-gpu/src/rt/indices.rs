//! The indices of a host BLAS build, checked on the GPU where the build reads
//! them (Codex review 11f27db1, P1-2). A check of title memory when the
//! build is recorded saw another version than the build: the host copy is
//! refreshed at submission, and a copy or a shader of the same submission
//! may write it before the build. So each indexed geometry's indices are
//! copied, as 32-bit ones, into a buffer of the build's own by a pass of
//! ours, ordered after every earlier write; one past the geometry's
//! vertices raises the build's flag, and then every index of the build
//! becomes 0: degenerate triangles, which DXR leaves inactive, so the
//! build takes none of them, and the refusal is logged once the
//! submission has run ([`Check`]).
//!
//! shortcut: a refused build still takes the triangles of its geometries
//! without indices; only the indexed ones are left out.

use super::*;
use windows::core::s;

/// One indexed geometry's indices, as the pass reads them.
pub(super) struct Job {
    /// Which geometry of the build.
    pub(super) geometry: usize,
    /// Its indices' host copy, at the 4-byte word they start in.
    pub(super) source: u64,
    /// Bytes from `source` to the first index (0 or 2).
    pub(super) offset: u32,
    pub(super) count: u32,
    /// 32-bit indices, else 16-bit.
    pub(super) wide: bool,
    /// The geometry's vertex count: indices must be below it.
    pub(super) vertices: u32,
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

/// Records, before a BLAS build, the checked copies of `jobs`' indices,
/// and points each of their geometries at its copy, as 32-bit indices.
/// `at` is the BLAS's title address, for the log.
pub(super) fn record(
    rec: &Recorder<'_>,
    title_list: *const Object,
    geometries: &mut [Geometry],
    jobs: &[Job],
    at: u64,
) -> Option<()> {
    let Some(first_job) = jobs.first() else {
        return Some(());
    };
    let (root, pso) = pipeline(rec.device)?;
    // The flag word, then each geometry's indices, 16-byte aligned.
    let mut firsts = Vec::with_capacity(jobs.len());
    let mut bytes = 16u64;
    for job in jobs {
        firsts.push(u32::try_from(bytes).ok()?);
        bytes = bytes
            .checked_add(u64::from(job.count) * 4)?
            .next_multiple_of(16);
    }
    u32::try_from(bytes).ok()?;
    let built = evicting(|| buffer(rec, bytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS))?;
    rec.list.keep(&built);
    let flag = flag_slot(rec.device);
    // SAFETY: our pass's root signature and pipeline; every root view lies
    // in a live buffer the list keeps (the indices' host copies were
    // checked to hold every word read); the built buffer is ours, in the
    // states named.
    unsafe {
        let gpu = built.GetGPUVirtualAddress();
        let c = &rec.commands;
        c.SetComputeRootSignature(root);
        c.SetPipelineState(pso);
        c.SetComputeRootUnorderedAccessView(2, gpu);
        let run = |job: &Job, first: u32, pass: u32| {
            let constants = [
                job.count,
                u32::from(job.wide),
                job.vertices,
                first,
                job.offset,
                pass,
            ];
            c.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            c.SetComputeRootShaderResourceView(1, job.source);
            let (x, y) = if pass == 0 { (1, 1) } else { groups(job.count) };
            c.Dispatch(x, y, 1);
        };
        run(first_job, 0, 0);
        // The title's and our writes of the indices, and the flag's reset,
        // before they are read.
        c.ResourceBarrier(&[uav_barrier()]);
        for (job, &first) in jobs.iter().zip(&firsts) {
            run(job, first, 1);
        }
        c.ResourceBarrier(&[uav_barrier()]);
        for (job, &first) in jobs.iter().zip(&firsts) {
            run(job, first, 2);
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
            t.IndexFormat = DXGI_FORMAT_R32_UINT;
            t.IndexBuffer = gpu + u64::from(first);
        }
    }
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
            let raised = unsafe { f.mapped.add(self.slot as usize).read_volatile() } != 0;
            f.free.push(self.slot);
            raised
        };
        if raised {
            refused_on_gpu(self.at);
        }
    }
}

/// Logs a BLAS whose indices the GPU found past their vertices (counted;
/// at powers of two).
fn refused_on_gpu(at: u64) {
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    let refused = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    if refused.is_power_of_two() {
        tracing::warn!(
            address = format_args!("{at:#x}"),
            refused,
            "host DXR BLAS input refused on the GPU: an index past its vertices, as the build \
             read it; the build took none of its indexed triangles"
        );
    }
}
