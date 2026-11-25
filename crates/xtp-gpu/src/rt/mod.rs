//! Host DXR builds recorded alongside the title's builder commands.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use crate::host::{Host, HostList, uav_barrier};
use crate::object::Object;

type Inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS;
type Build = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC;
type Geometry = D3D12_RAYTRACING_GEOMETRY_DESC;

fn device(host: &Host) -> Option<&'static ID3D12Device5> {
    static DEVICE: OnceLock<Option<ID3D12Device5>> = OnceLock::new();
    DEVICE
        .get_or_init(|| {
            let mut caps = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
            // SAFETY: the output matches the feature and byte count.
            unsafe {
                host.device.CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS5,
                    (&raw mut caps).cast(),
                    size_of_val(&caps) as u32,
                )
            }
            .ok()?;
            if caps.RaytracingTier == D3D12_RAYTRACING_TIER_NOT_SUPPORTED {
                return None;
            }
            host.device.cast().ok()
        })
        .as_ref()
}

fn buffer(
    device: &ID3D12Device5,
    bytes: u64,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
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
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut resource = None;
    // SAFETY: one committed, shader-writable buffer with a state matching its use.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut resource,
        )
    }
    .ok()?;
    resource
}

struct Recorder<'a> {
    device: &'a ID3D12Device5,
    commands: ID3D12GraphicsCommandList4,
    list: &'a HostList,
}

impl<'a> Recorder<'a> {
    fn new(list: &'a HostList) -> Option<Self> {
        Some(Self {
            device: device(crate::host::made()?)?,
            commands: list.list.cast().ok()?,
            list,
        })
    }

    fn build(&self, inputs: &Inputs) -> Option<ID3D12Resource> {
        let mut sizes = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        // SAFETY: the caller owns all geometry descriptions for this call.
        unsafe {
            self.device
                .GetRaytracingAccelerationStructurePrebuildInfo(inputs, &mut sizes)
        };
        if sizes.ResultDataMaxSizeInBytes == 0 {
            return None;
        }
        let result = buffer(
            self.device,
            sizes.ResultDataMaxSizeInBytes,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
        )?;
        let mut scratch = self
            .list
            .rt_scratch
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let bytes = sizes.ScratchDataSizeInBytes;
        let mut scratch_address = 0;
        if bytes != 0 {
            if scratch.as_ref().is_none_or(|buffer| {
                // SAFETY: the list holds the scratch resource while GetDesc reads its width.
                let width = unsafe { buffer.GetDesc().Width };
                width < bytes
            }) {
                *scratch = Some(buffer(
                    self.device,
                    bytes,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )?);
            }
            let current = scratch.as_ref()?;
            self.list.keep(current);
            // SAFETY: a live default-heap buffer.
            scratch_address = unsafe { current.GetGPUVirtualAddress() };
        }
        self.list.keep(&result);
        // SAFETY: all GPU ranges are owned by this recording. The global barriers order prior
        // geometry writers, scratch reuse on the host queue and subsequent reads of the result.
        unsafe {
            self.commands.ResourceBarrier(&[uav_barrier()]);
            self.commands.BuildRaytracingAccelerationStructure(
                &Build {
                    Inputs: *inputs,
                    DestAccelerationStructureData: result.GetGPUVirtualAddress(),
                    ScratchAccelerationStructureData: scratch_address,
                    SourceAccelerationStructureData: 0,
                },
                None,
            );
            self.commands.ResourceBarrier(&[uav_barrier()]);
        }
        Some(result)
    }

    fn clone_structure(&self, source: &ID3D12Resource) -> Option<ID3D12Resource> {
        // SAFETY: source is a live AS previously built by this module.
        let bytes = unsafe { source.GetDesc().Width };
        let dest = buffer(
            self.device,
            bytes,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
        )?;
        self.list.keep(source);
        self.list.keep(&dest);
        // SAFETY: independent AS resources in the required state, retained through execution.
        unsafe {
            self.commands.CopyRaytracingAccelerationStructure(
                dest.GetGPUVirtualAddress(),
                source.GetGPUVirtualAddress(),
                D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE,
            );
            self.commands.ResourceBarrier(&[uav_barrier()]);
        }
        Some(dest)
    }
}

/// BLAS recorded through a live title builder. Lists retain each generation they reference.
#[derive(Default)]
pub struct Builder {
    blas: Mutex<BTreeMap<u64, ID3D12Resource>>,
}

impl Builder {
    /// Mirrors a new triangle BLAS. A later unsupported build at the same address invalidates it.
    ///
    /// # Safety
    /// `list` is a live title list, and the active CPU arrays in `desc` follow the builder ABI.
    pub unsafe fn build(&self, list: *const Object, desc: &Build) -> bool {
        let dst = desc.DestAccelerationStructureData;
        let built = (|| {
            if dst == 0
                || desc.Inputs.Type != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL
                || desc
                    .Inputs
                    .Flags
                    .contains(D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE)
            {
                return None;
            }
            let list = crate::list::host_list(list)?;
            let recorder = Recorder::new(list)?;
            // SAFETY: the descriptors are borrowed from this title call only.
            let geometries = unsafe { triangles(list, &desc.Inputs) }?;
            let inputs = Inputs {
                DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                    pGeometryDescs: geometries.as_ptr(),
                },
                ..desc.Inputs
            };
            recorder.build(&inputs)
        })();
        self.replace(dst, built)
    }

    /// Mirrors a clone, or invalidates the destination for another copy mode.
    ///
    /// # Safety
    /// `list` is a live title list.
    pub unsafe fn copy(&self, list: *const Object, dst: u64, src: u64, mode: i32) -> bool {
        let copied = (|| {
            if dst == 0 || mode != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE.0 {
                return None;
            }
            let source = self
                .blas
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&src)?
                .clone();
            Recorder::new(crate::list::host_list(list)?)?.clone_structure(&source)
        })();
        self.replace(dst, copied)
    }

    fn replace(&self, dst: u64, result: Option<ID3D12Resource>) -> bool {
        let mut table = self.blas.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(resource) = result {
            table.insert(dst, resource);
            true
        } else {
            table.remove(&dst);
            false
        }
    }
}

fn range(list: &HostList, address: u64, bytes: u64, alignment: u64) -> Option<u64> {
    let end = address.checked_add(bytes)?;
    if address == 0
        || !address.is_multiple_of(alignment)
        || bytes == 0
        || !crate::shadow::holders(address)
            .iter()
            .any(|&(base, len)| end <= base + len)
    {
        return None;
    }
    let shadow = crate::shadow::find(address, bytes)?;
    let gpu = shadow.gpu(address);
    if !gpu.is_multiple_of(alignment) {
        return None;
    }
    shadow.refresh(list, address, bytes);
    Some(gpu)
}

unsafe fn triangles(list: &HostList, inputs: &Inputs) -> Option<Vec<Geometry>> {
    let mut geometries = Vec::new();
    for i in 0..inputs.NumDescs as usize {
        // SAFETY: the caller provides NumDescs CPU descriptions in the selected layout.
        let mut g = unsafe {
            let p = match inputs.DescsLayout {
                D3D12_ELEMENTS_LAYOUT_ARRAY if !inputs.Anonymous.pGeometryDescs.is_null() => {
                    inputs.Anonymous.pGeometryDescs.add(i)
                }
                D3D12_ELEMENTS_LAYOUT_ARRAY_OF_POINTERS
                    if !inputs.Anonymous.ppGeometryDescs.is_null() =>
                {
                    *inputs.Anonymous.ppGeometryDescs.add(i)
                }
                _ => return None,
            };
            *p.as_ref()?
        };
        if g.Type != D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES {
            return None;
        }
        // SAFETY: the geometry type selects Triangles in our own copy.
        let t = unsafe { &mut g.Anonymous.Triangles };
        let (width, alignment) = match t.VertexFormat {
            DXGI_FORMAT_R32G32B32_FLOAT => (12, 4),
            DXGI_FORMAT_R32G32_FLOAT => (8, 4),
            DXGI_FORMAT_R16G16B16A16_FLOAT | DXGI_FORMAT_R16G16B16A16_SNORM => (6, 2),
            DXGI_FORMAT_R16G16_FLOAT | DXGI_FORMAT_R16G16_SNORM => (4, 2),
            _ => return None,
        };
        let stride = t.VertexBuffer.StrideInBytes;
        if stride > u32::MAX.into() || !stride.is_multiple_of(alignment) {
            return None;
        }
        let bytes = u64::from(t.VertexCount.checked_sub(1)?)
            .checked_mul(stride)?
            .checked_add(width)?;
        t.VertexBuffer.StartAddress = range(list, t.VertexBuffer.StartAddress, bytes, alignment)?;
        if t.IndexFormat == DXGI_FORMAT_R16_TYPELESS {
            t.IndexFormat = DXGI_FORMAT_R16_UINT;
        }
        let index_size = match t.IndexFormat {
            DXGI_FORMAT_UNKNOWN if t.IndexCount == 0 && t.IndexBuffer == 0 => 0,
            DXGI_FORMAT_R16_UINT => 2,
            DXGI_FORMAT_R32_UINT => 4,
            _ => return None,
        };
        if index_size != 0 {
            t.IndexBuffer = range(
                list,
                t.IndexBuffer,
                u64::from(t.IndexCount) * index_size,
                index_size,
            )?;
        }
        if t.Transform3x4 != 0 {
            t.Transform3x4 = range(list, t.Transform3x4, 48, 16)?;
        }
        geometries.push(g);
    }
    Some(geometries)
}

#[cfg(test)]
mod tests;
