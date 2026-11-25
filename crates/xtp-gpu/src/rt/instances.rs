use super::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::core::s;

/// Immutable AS addresses and the GPU upload that names them.
pub(super) struct UploadMap {
    upload: ID3D12Resource,
    generations: Vec<(u64, Arc<ResultBuffer>)>,
}

impl UploadMap {
    fn resolve(recorded: &Candidates, current: &Candidates) -> Option<Arc<Self>> {
        // Recorded slots survive title release. Include producers first recorded
        // on other threads after the TLAS, if they have reached the queue.
        let mut entries = BTreeMap::new();
        for (address, slot) in recorded
            .iter()
            .chain(current.iter().filter(|(a, _)| !recorded.contains_key(a)))
        {
            let slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = slot.as_ref().filter(|s| s.instances.is_none()) {
                entries.insert(*address, s.result.clone());
            }
        }
        let generations: Vec<_> = entries.into_iter().collect();
        static LAST: Mutex<std::sync::Weak<UploadMap>> = Mutex::new(std::sync::Weak::new());
        let cached = LAST.lock().unwrap_or_else(|e| e.into_inner()).upgrade();
        if let Some(cached) = cached
            && cached.generations.len() == generations.len()
            && cached
                .generations
                .iter()
                .zip(&generations)
                .all(|((a, r), (b, s))| a == b && Arc::ptr_eq(r, s))
        {
            return Some(cached);
        }
        let bytes = u32::try_from(generations.len()).ok()?.checked_mul(16)?;
        if bytes == 0 {
            return None;
        }
        let (upload, data) = crate::host::made()?.upload_buffer(bytes.into())?;
        for (i, (title, result)) in generations.iter().enumerate() {
            // SAFETY: one aligned row per entry in the mapped upload.
            unsafe {
                data.cast::<[u64; 2]>().add(i).write([*title, result.gpu()]);
            }
        }
        let map = Arc::new(Self {
            upload,
            generations,
        });
        *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&map);
        Some(map)
    }
}

/// BLAS referenced by the latest execution of this TLAS generation. Each
/// consuming list takes its own execution-time pin, including TLAS copies.
#[derive(Default)]
pub(super) struct BlasMap(Mutex<Option<Arc<UploadMap>>>);

impl BlasMap {
    pub(super) fn current(&self) -> Option<Arc<UploadMap>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(super) fn capture(self: &Arc<Self>, list: &HostList) -> Arc<Self> {
        let captured = Arc::new(Self::default());
        let (dst, src) = (captured.clone(), self.clone());
        list.defer_host((0, 0), move || {
            *dst.0.lock().unwrap_or_else(|e| e.into_inner()) = src.current()
        });
        captured
    }
}

struct Rewrite {
    signature: ID3D12CommandSignature,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

fn pipeline(device: &ID3D12Device5) -> Option<&'static Rewrite> {
    static PIPELINE: OnceLock<Option<Rewrite>> = OnceLock::new();
    PIPELINE.get_or_init(|| make_pipeline(device)).as_ref()
}

fn make_pipeline(device: &ID3D12Device5) -> Option<Rewrite> {
    let source = include_bytes!("instances.hlsl");
    let mut code: Option<ID3DBlob> = None;
    let mut errors = None;
    // SAFETY: the compiler reads this static source and writes the two local blob pointers.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("rt_instances"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(error) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob owns its diagnostic bytes for this scope.
            let bytes = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer().cast(), errors.GetBufferSize())
            };
            tracing::warn!(%error, "instance rewrite shader: {}", String::from_utf8_lossy(bytes));
        }
        return None;
    }
    let code = code?;
    // SAFETY: DXBC embeds the matching root signature. Its bytes and the borrowed root
    // signature remain live until both synchronous device calls have returned.
    unsafe {
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).ok()?;
        let pso = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })
            .ok()?;
        let signature = dispatch::signature(&root, 1, true)?;
        Some(Rewrite {
            root,
            pso,
            signature,
        })
    }
}

pub(super) fn rewrite(
    rec: &Recorder<'_>,
    title_list: *const Object,
    inputs: &Inputs,
) -> Option<(Inputs, Arc<BlasMap>)> {
    let count = inputs.NumDescs;
    if inputs.DescsLayout != D3D12_ELEMENTS_LAYOUT_ARRAY
        || count > D3D12_RAYTRACING_MAX_INSTANCES_PER_TOP_LEVEL_ACCELERATION_STRUCTURE
    {
        return None;
    }
    let mut translated = *inputs;
    translated.Anonymous.InstanceDescs = 0;
    let map = Arc::new(BlasMap::default());
    if count == 0 {
        return Some((translated, map));
    }
    let bytes = u64::from(count) * 64;
    // SAFETY: the caller selected TOP_LEVEL, whose active union member is the instance GPU address.
    let address = unsafe { inputs.Anonymous.InstanceDescs };
    let src = input_range(rec.list, address, bytes, 16)?;
    let dst = buffer(rec, bytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;
    let pass = pipeline(rec.device)?;
    let (args, data) = rec.list.upload(crate::host::made()?, 24)?;
    let mapped = data as usize;
    let groups = count.div_ceil(64);
    // A valid empty table still runs the rewrite and nulls every missing BLAS.
    let empty = rec.list.scratch(16)?.0;
    let recorded = registry().blas_candidates();
    let destination = map.clone();
    rec.list.defer_host((0, 0), move || {
        let current = registry().blas_candidates();
        let chosen = UploadMap::resolve(&recorded, &current);
        let (gpu, count) = chosen.as_ref().map_or((empty, 0), |m| {
            // SAFETY: the map holds this immutable upload.
            (
                unsafe { m.upload.GetGPUVirtualAddress() },
                m.generations.len() as u32,
            )
        });
        // SAFETY: six packed words, rewritten only after this recording's prior fence.
        unsafe {
            (mapped as *mut [u32; 6]).write([
                gpu as u32,
                (gpu >> 32) as u32,
                count,
                groups.min(65535),
                groups.div_ceil(65535),
                1,
            ]);
        }
        *destination.0.lock().unwrap_or_else(|e| e.into_inner()) = chosen;
    });
    rec.list.keep(&dst);
    // SAFETY: each root view covers the shader's checked range. The recording retains its
    // inputs and output, and the transition orders rewrite stores before the DXR build.
    unsafe {
        let gpu = dst.GetGPUVirtualAddress();
        rec.commands.ResourceBarrier(&[
            uav_barrier(),
            crate::host::transition(
                &src.buffer,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            ),
        ]);
        rec.commands.SetComputeRootSignature(&pass.root);
        rec.commands.SetPipelineState(&pass.pso);
        rec.commands.SetComputeRoot32BitConstant(0, count, 0);
        rec.commands
            .SetComputeRootUnorderedAccessView(2, src.gpu(address));
        rec.commands.SetComputeRootUnorderedAccessView(3, gpu);
        rec.commands
            .ExecuteIndirect(&pass.signature, 1, &args, 0, None, 0);
        rec.commands.ResourceBarrier(&[
            crate::host::transition(
                &src.buffer,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            ),
            crate::host::transition(
                &dst,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            ),
        ]);
        translated.Anonymous.InstanceDescs = gpu;
    }
    crate::list::forget_host_bindings(title_list);
    Some((translated, map))
}
