//! Host DXR builds recorded alongside the title's builder commands.

use std::collections::BTreeMap;
pub(crate) mod compaction;
mod instances;
#[cfg(feature = "diagnostics")]
pub(crate) mod probe;

pub(crate) use instances::BlasMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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

fn width(buffer: &ID3D12Resource) -> u64 {
    // SAFETY: the borrowed COM resource is live for this synchronous descriptor query.
    unsafe { buffer.GetDesc().Width }
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

    fn build(&self, inputs: &Inputs, reuse: Option<Arc<ID3D12Resource>>) -> Option<Structure> {
        let mut sizes = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        // SAFETY: the caller owns all geometry descriptions for this call.
        unsafe {
            self.device
                .GetRaytracingAccelerationStructurePrebuildInfo(inputs, &mut sizes)
        };
        if sizes.ResultDataMaxSizeInBytes == 0 {
            return None;
        }
        // A retired allocation can serve a later generation at the same title address.
        let result = match reuse {
            Some(resource) if width(&resource) >= sizes.ResultDataMaxSizeInBytes => resource,
            _ => Arc::new(buffer(
                self.device,
                sizes.ResultDataMaxSizeInBytes,
                D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
            )?),
        };
        let mut scratch = self
            .list
            .rt_scratch
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let bytes = sizes.ScratchDataSizeInBytes;
        let mut scratch_address = 0;
        if bytes != 0 {
            if scratch.as_ref().is_none_or(|buffer| width(buffer) < bytes) {
                *scratch = Some(buffer(
                    self.device,
                    bytes,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )?);
            }
            let current = scratch.as_ref()?;
            self.list.keep(current);
            // SAFETY: current is a buffer in UAV state of at least the host prebuild scratch size.
            // The list kept this exact allocation even if its scratch slot is replaced later.
            scratch_address = unsafe { current.GetGPUVirtualAddress() };
        }
        let postbuild = inputs
            .Flags
            .contains(D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_COMPACTION)
            .then(|| compaction::request(self.list))
            .flatten();
        self.list.keep_rt_result(&result);
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
                postbuild
                    .as_ref()
                    .map(|(desc, _)| std::slice::from_ref(desc)),
            );
            self.commands.ResourceBarrier(&[uav_barrier()]);
        }
        Some(Structure {
            producer: Producer::recorded(self.list),
            result,
            instances: None,
            compacted_size: postbuild.map(|(_, size)| size),
            owner: None,
        })
    }

    fn copy_structure(
        &self,
        source: &Structure,
        mode: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE,
    ) -> Option<Structure> {
        let capacity = width(&source.result);
        let bytes = match mode {
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE => capacity,
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT => {
                let measured = source.compacted_size.as_ref()?.load(Ordering::Acquire);
                // A compact copy recorded before the producing fence (including on the
                // same list) still fits the original host allocation's upper bound.
                if measured == 0 {
                    capacity
                } else {
                    if measured > capacity {
                        return None;
                    }
                    measured
                }
            }
            _ => return None,
        };
        let dest = buffer(
            self.device,
            bytes,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
        )?;
        let dest = Arc::new(dest);
        self.list.keep_rt_result(&dest);
        // SAFETY: independent AS resources in the required state, retained through execution.
        unsafe {
            self.commands.CopyRaytracingAccelerationStructure(
                dest.GetGPUVirtualAddress(),
                source.result.GetGPUVirtualAddress(),
                mode,
            );
            self.commands.ResourceBarrier(&[uav_barrier()]);
        }
        Some(Structure {
            producer: Producer::recorded(self.list),
            result: dest,
            ..source.clone()
        })
    }
}

struct Producer {
    recording: Arc<()>,
    submitted: AtomicBool,
}

impl Producer {
    fn recorded(list: &HostList) -> Arc<Self> {
        let producer = Arc::new(Self {
            recording: list.rt_recording(),
            submitted: AtomicBool::new(false),
        });
        let mark = producer.clone();
        // Host submissions are serialized. Deferred steps run in list order before ExecuteCommandLists.
        list.defer_host((0, 0), move || {
            mark.submitted.store(true, Ordering::Release)
        });
        producer
    }

    fn available(&self, recording: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.recording, recording) || self.submitted.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
struct Structure {
    producer: Arc<Producer>,
    result: Arc<ID3D12Resource>,
    // Only TLAS generations refer to a snapshot of the BLAS address space.
    instances: Option<Arc<BlasMap>>,
    owner: Option<usize>,
    compacted_size: Option<Arc<AtomicU64>>,
}

impl Structure {
    fn retain(&self, list: &HostList) {
        list.keep_rt_result(&self.result);
        if let Some(map) = &self.instances {
            list.keep_rt_map(map);
        }
    }
}

#[derive(Default)]
struct Registry {
    structures: BTreeMap<u64, Structure>,
    snapshot: Option<Arc<BlasMap>>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    structures: BTreeMap::new(),
    snapshot: None,
});

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

impl Registry {
    fn replace(&mut self, dst: u64, built: Option<Structure>) -> bool {
        let old = self.structures.remove(&dst);
        if old.is_some_and(|s| s.instances.is_none())
            || built.as_ref().is_some_and(|s| s.instances.is_none())
        {
            self.snapshot = None;
        }
        if let Some(mut structure) = built {
            structure.owner = crate::shadow::buffer_owner(dst);
            self.structures.insert(dst, structure);
            true
        } else {
            false
        }
    }

    fn blas_map(&mut self) -> Option<Arc<BlasMap>> {
        if self.snapshot.is_none() {
            self.snapshot = Some(Arc::new(BlasMap::new(&self.structures)?));
        }
        self.snapshot.clone()
    }
}

pub(crate) fn release_buffer(resource: usize, start: u64, end: u64) {
    let mut table = registry();
    let released: Vec<_> = table
        .structures
        .range(start..end)
        .filter(|(_, s)| s.owner == Some(resource))
        .map(|(&address, _)| address)
        .collect();
    for address in released {
        table.replace(address, None);
    }
}

/// Resolves one descriptor's title TLAS address at the time of lookup.
/// Use `host_tlas_for_list` when recording a GPU command to retain that generation.
pub fn host_tlas(title_tlas: u64) -> Option<u64> {
    let table = registry();
    let entry = table.structures.get(&title_tlas)?;
    entry.instances.as_ref()?;
    // SAFETY: the registry holds the TLAS resource throughout the lookup.
    Some(unsafe { entry.result.GetGPUVirtualAddress() })
}

/// Resolves and pins the TLAS and its possible BLAS references for this recording.
///
/// # Safety
/// `list` is a live title command list.
pub unsafe fn host_tlas_for_list(list: *const Object, title_tlas: u64) -> Option<u64> {
    let list = crate::list::host_list(list)?;
    let table = registry();
    let entry = table.structures.get(&title_tlas)?;
    entry.instances.as_ref()?;
    if !entry.producer.available(&list.rt_recording()) {
        return None;
    }
    entry.retain(list);
    // SAFETY: the list owns this resource until the GPU has finished the recording.
    Some(unsafe { entry.result.GetGPUVirtualAddress() })
}

/// Mirrors a title build and invalidates an older destination on failure.
///
/// # Safety
/// `title_list` is a live title list and the active CPU arrays follow the builder ABI.
pub unsafe fn build(title_list: *const Object, desc: &Build) -> bool {
    let dst = desc.DestAccelerationStructureData;
    let reuse = {
        let mut table = registry();
        let previous = table.structures.remove(&dst);
        if previous
            .as_ref()
            .is_some_and(|entry| entry.instances.is_none())
        {
            table.snapshot = None;
        }
        previous
            .map(|s| s.result)
            .filter(|r| Arc::strong_count(r) == 1)
    };
    let built = (|| {
        if dst == 0 {
            return None;
        }
        // The title supplies full geometry on refits. Rebuilding it also handles
        // changed index data without relying on DXR's fixed-topology update contract.
        let mut inputs = desc.Inputs;
        inputs.Flags &= D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_UPDATE
            | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_COMPACTION
            | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE
            | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_BUILD
            | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_MINIMIZE_MEMORY;
        let list = crate::list::host_list(title_list)?;
        let recorder = Recorder::new(list)?;
        match inputs.Type {
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL => {
                // SAFETY: the descriptors are borrowed from this title call only.
                let geometries = unsafe { triangles(list, &inputs) }?;
                let inputs = Inputs {
                    DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                    Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                        pGeometryDescs: geometries.as_ptr(),
                    },
                    ..inputs
                };
                recorder.build(&inputs, reuse)
            }
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL => {
                let map = registry().blas_map()?;
                let inputs = instances::rewrite(&recorder, title_list, &inputs, &map)?;
                Some(Structure {
                    instances: Some(map),
                    ..recorder.build(&inputs, reuse)?
                })
            }
            _ => None,
        }
    })();
    registry().replace(dst, built)
}

/// Rebuilds a rehydrated title entry and optionally samples its inputs on the GPU.
///
/// # Safety
/// The title list and CPU arrays obey the same contract as `build`.
pub unsafe fn rehydrate(list: *const Object, desc: &Build) -> bool {
    // SAFETY: the caller supplies the same live inputs required by build.
    let recorded = unsafe { build(list, desc) };
    #[cfg(feature = "diagnostics")]
    if probe::enabled() {
        let sampled = recorded
            && crate::list::host_list(list).is_some_and(|list| {
                // SAFETY: the CPU descriptions remain borrowed for this entire rehydration call.
                unsafe { probe::record(list, desc).is_some() }
            });
        tracing::info!(recorded, sampled, "rehydrated geometry validation");
    }
    recorded
}

/// Mirrors a clone or compact copy across title builders.
///
/// # Safety
/// `list` is a live title list.
pub unsafe fn copy(list: *const Object, dst: u64, src: u64, mode: i32) -> bool {
    let copied = (|| {
        if dst == 0 {
            return None;
        }
        let source = registry().structures.get(&src)?.clone();
        let recorder = Recorder::new(crate::list::host_list(list)?)?;
        if !source.producer.available(&recorder.list.rt_recording()) {
            return None;
        }
        source.retain(recorder.list);
        recorder.copy_structure(
            &source,
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE(mode),
        )
    })();
    registry().replace(dst, copied)
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
