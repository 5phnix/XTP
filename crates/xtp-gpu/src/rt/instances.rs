use super::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::core::s;

/// GPU-produced instances can select any entry. The upload and the corresponding
/// resources share one immutable lifetime across TLAS builds, copies and dispatches.
pub(crate) struct BlasMap {
    upload: ID3D12Resource,
    generations: Vec<(u64, Arc<ID3D12Resource>, Arc<Producer>)>,
}

impl BlasMap {
    pub(super) fn new(entries: &BTreeMap<u64, Structure>) -> Option<Self> {
        let blas = entries.iter().filter(|(_, s)| s.instances.is_none());
        let bytes = u32::try_from(blas.clone().count()).ok()?.checked_mul(16)?;
        let (upload, data) = crate::host::made()?.upload_buffer(u64::from(bytes.max(16)))?;
        let mut generations = Vec::with_capacity((bytes / 16) as usize);
        for (i, (&title, s)) in blas.enumerate() {
            // SAFETY: the mapped upload holds one aligned pair per entry. Both resources are live.
            unsafe {
                data.cast::<[u64; 2]>()
                    .add(i)
                    .write([title, s.result.GetGPUVirtualAddress()])
            };
            generations.push((title, s.result.clone(), s.producer.clone()));
        }
        Some(Self {
            upload,
            generations,
        })
    }

    /// A producer on another list may be submitted after this recording, or
    /// abandoned. Resolve those rows in queue order without exposing unbuilt AS memory.
    fn for_list(self: &Arc<Self>, list: &HostList) -> Option<u64> {
        let recording = list.rt_recording();
        if self
            .generations
            .iter()
            .all(|(_, _, p)| p.available(&recording))
        {
            // SAFETY: the TLAS recording retains this immutable upload through execution.
            return Some(unsafe { self.upload.GetGPUVirtualAddress() });
        }
        let (gpu, data) = list.scratch(16 * self.generations.len() as u64)?;
        let mapped = data as usize;
        let map = self.clone();
        list.defer_host((0, 0), move || {
            for (i, (title, resource, producer)) in map.generations.iter().enumerate() {
                // SAFETY: one aligned row per entry in this recording's private upload.
                // List replay waits for its prior execution before rewriting these rows.
                unsafe {
                    let address = if producer.submitted.load(Ordering::Acquire) {
                        resource.GetGPUVirtualAddress()
                    } else {
                        0
                    };
                    (mapped as *mut [u64; 2]).add(i).write([*title, address]);
                }
            }
        });
        Some(gpu)
    }
}

struct Rewrite {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

fn pipeline(device: &ID3D12Device5) -> Option<&'static Rewrite> {
    static PIPELINE: OnceLock<Option<Rewrite>> = OnceLock::new();
    PIPELINE.get_or_init(|| {
        let source = include_bytes!("instances.hlsl");
        let mut code: Option<ID3DBlob> = None;
        let mut errors = None;
        // SAFETY: the compiler reads this static source and writes the two local blob pointers.
        let compiled = unsafe { D3DCompile(source.as_ptr().cast(), source.len(), s!("rt_instances"), None, None,
            s!("main"), s!("cs_5_1"), 0, 0, &mut code, Some(&mut errors)) };
        if let Err(error) = compiled {
            if let Some(errors) = errors {
                // SAFETY: the blob owns its diagnostic bytes for this scope.
                let bytes = unsafe { std::slice::from_raw_parts(errors.GetBufferPointer().cast(), errors.GetBufferSize()) };
                tracing::warn!(%error, "instance rewrite shader: {}", String::from_utf8_lossy(bytes));
            }
            return None;
        }
        let code = code?;
        // SAFETY: DXBC embeds the matching root signature. Its bytes and the borrowed root
        // signature remain live until both synchronous device calls have returned.
        unsafe {
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize());
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).ok()?;
            let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                ..Default::default()
            }).ok()?;
            Some(Rewrite { root, pso })
        }
    }).as_ref()
}

pub(super) fn rewrite(
    rec: &Recorder<'_>,
    title_list: *const Object,
    inputs: &Inputs,
    map: &Arc<BlasMap>,
) -> Option<Inputs> {
    let count = inputs.NumDescs;
    if inputs.DescsLayout != D3D12_ELEMENTS_LAYOUT_ARRAY
        || count > D3D12_RAYTRACING_MAX_INSTANCES_PER_TOP_LEVEL_ACCELERATION_STRUCTURE
    {
        return None;
    }
    let mut translated = *inputs;
    translated.Anonymous.InstanceDescs = 0;
    if count == 0 {
        return Some(translated);
    }
    let bytes = u64::from(count) * 64;
    // SAFETY: the caller selected TOP_LEVEL, whose active union member is the instance GPU address.
    let address = unsafe { inputs.Anonymous.InstanceDescs };
    let src = range(rec.list, address, bytes, 16)?;
    let dst = buffer(rec.device, bytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;
    let pass = pipeline(rec.device)?;
    let mapping = map.for_list(rec.list)?;
    rec.list.keep(&dst);
    rec.list.keep_rt_map(map);
    let groups = count.div_ceil(64);
    let constants = [count, map.generations.len() as u32];
    // SAFETY: each root view covers the shader's checked range. The recording retains its
    // inputs and output, and the transition orders rewrite stores before the DXR build.
    unsafe {
        let gpu = dst.GetGPUVirtualAddress();
        rec.commands.ResourceBarrier(&[uav_barrier()]);
        rec.commands.SetComputeRootSignature(&pass.root);
        rec.commands.SetPipelineState(&pass.pso);
        rec.commands
            .SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
        rec.commands.SetComputeRootShaderResourceView(1, mapping);
        rec.commands.SetComputeRootUnorderedAccessView(2, src);
        rec.commands.SetComputeRootUnorderedAccessView(3, gpu);
        rec.commands
            .Dispatch(groups.min(65535), groups.div_ceil(65535), 1);
        rec.commands.ResourceBarrier(&[crate::host::transition(
            &dst,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
        translated.Anonymous.InstanceDescs = gpu;
    }
    crate::list::forget_host_bindings(title_list);
    Some(translated)
}
