use super::*;
use crate::host::transition;

#[derive(Clone)]
pub(crate) struct Sample {
    _buffer: ID3D12Resource,
    mapped: usize,
    title: u64,
}

impl Sample {
    /// Called after the submission fence and the device-removal check.
    pub(crate) fn report(&self) {
        // SAFETY: this sample owns the mapped buffer. The submission containing both
        // copies has finished and list replay waits for the previous submission.
        let [vertices, indices] = unsafe { (self.mapped as *const [u64; 2]).read_volatile() };
        tracing::info!(
            title = self.title,
            vertex_nonzero = vertices != 0,
            index_nonzero = indices != 0,
            "rehydrated geometry GPU sample"
        );
    }
}

pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("XTP_RT_REHYDRATE_PROBE").is_some())
}

/// Samples bytes from the very resources used by an indexed build. The actual
/// build has already checked the full ranges. Only booleans reach the log.
///
/// # Safety
/// The successful build validated the active CPU descriptors. They remain live for the probe.
pub(super) unsafe fn record(list: &HostList, desc: &Build) -> Option<()> {
    if desc.Inputs.Type != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL
        || desc.Inputs.NumDescs == 0
    {
        return None;
    }
    // SAFETY: the successful build validated the layout, pointers and triangle discriminant.
    let tri = unsafe {
        let first = if desc.Inputs.DescsLayout == D3D12_ELEMENTS_LAYOUT_ARRAY {
            desc.Inputs.Anonymous.pGeometryDescs
        } else {
            *desc.Inputs.Anonymous.ppGeometryDescs
        };
        (*first).Anonymous.Triangles
    };
    if tri.VertexCount < 3
        || tri.IndexCount < 3
        || tri.VertexFormat != DXGI_FORMAT_R32G32_FLOAT
        || !matches!(
            tri.IndexFormat,
            DXGI_FORMAT_R16_UINT | DXGI_FORMAT_R16_TYPELESS
        )
    {
        return None;
    }
    let (dst, mapped) = crate::host::made()?.shared_buffer(16)?;
    let sample = Sample {
        _buffer: dst.clone(),
        mapped: mapped as usize,
        title: desc.DestAccelerationStructureData,
    };
    // SAFETY: this is a new, eight-byte-aligned mapped allocation of 16 bytes.
    unsafe { mapped.cast::<[u64; 2]>().write([0; 2]) };
    let vertices = u64::from(tri.VertexCount - 1) * tri.VertexBuffer.StrideInBytes + 8;
    let indices = u64::from(tri.IndexCount) * 2;
    for (at, address, bytes, range) in [
        (0, tri.VertexBuffer.StartAddress, 8, vertices),
        (8, tri.IndexBuffer, 6, indices),
    ] {
        let shadow = crate::shadow::existing(address, range)?;
        let offset = shadow.offset(address);
        let common = D3D12_RESOURCE_STATE_COMMON;
        let source = D3D12_RESOURCE_STATE_COPY_SOURCE;
        list.keep(&dst);
        list.keep(&shadow.buffer);
        // SAFETY: both subranges fit the resources retained by the recording. The GPU
        // reads the same shadow immediately after the build, before any later title writer.
        unsafe {
            list.list
                .ResourceBarrier(&[transition(&shadow.buffer, common, source)]);
            list.list
                .CopyBufferRegion(&dst, at, &shadow.buffer, offset, bytes);
            list.list
                .ResourceBarrier(&[transition(&shadow.buffer, source, common)]);
        }
    }
    list.rt_sample(sample);
    Some(())
}
