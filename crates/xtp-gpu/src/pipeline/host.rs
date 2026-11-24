//! Host pipelines for title pipelines (decision 0007, step 3): the
//! title's graphics state with the translated shaders, under a root
//! signature of our own that the translated shaders' bindings define
//! (`xtp_shader::emit`): per stage one block of root constants for the
//! user data, root views for the title's root views, static samplers for
//! the immediate samplers, and directly indexed heaps.

use std::ffi::{CStr, CString};
use std::sync::Arc;

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;
use xtp_shader::emit::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};

use super::{Shader, Translated};
use crate::host::host;

/// A stream's graphics state, owned (the title's stream lives only for the
/// creation call). Subobjects the stream leaves out keep D3D12's defaults.
pub(crate) struct GraphicsState {
    blend: D3D12_BLEND_DESC,
    sample_mask: u32,
    rasterizer: D3D12_RASTERIZER_DESC,
    depth: D3D12_DEPTH_STENCIL_DESC,
    /// Input elements, their names pointing into `names`.
    inputs: Vec<D3D12_INPUT_ELEMENT_DESC>,
    names: Vec<CString>,
    strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE,
    pub(crate) topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    pub(crate) targets: D3D12_RT_FORMAT_ARRAY,
    depth_format: DXGI_FORMAT,
    samples: DXGI_SAMPLE_DESC,
}

impl Default for GraphicsState {
    fn default() -> Self {
        let target = D3D12_RENDER_TARGET_BLEND_DESC {
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_ZERO,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_ZERO,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        let keep = D3D12_DEPTH_STENCILOP_DESC {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
            StencilPassOp: D3D12_STENCIL_OP_KEEP,
            StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        };
        GraphicsState {
            blend: D3D12_BLEND_DESC {
                RenderTarget: [target; 8],
                ..Default::default()
            },
            sample_mask: u32::MAX,
            rasterizer: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_BACK,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            depth: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D12_COMPARISON_FUNC_LESS,
                StencilReadMask: 0xff,
                StencilWriteMask: 0xff,
                FrontFace: keep,
                BackFace: keep,
                ..Default::default()
            },
            inputs: Vec::new(),
            names: Vec::new(),
            strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
            topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE_UNDEFINED,
            targets: D3D12_RT_FORMAT_ARRAY::default(),
            depth_format: DXGI_FORMAT_UNKNOWN,
            samples: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
        }
    }
}

impl GraphicsState {
    /// Takes in one subobject of a stream: its type, then its data at +4
    /// (+8 for those holding pointers).
    ///
    /// # Safety
    /// `at` points at a whole subobject of type `kind`.
    pub(crate) unsafe fn read(&mut self, kind: u32, at: *const u8) {
        // SAFETY: the caller passes a whole subobject.
        unsafe {
            let data = at.add(4);
            match kind {
                8 => self.blend = data.cast::<D3D12_BLEND_DESC>().read_unaligned(),
                9 => self.sample_mask = data.cast::<u32>().read_unaligned(),
                10 => self.rasterizer = data.cast::<D3D12_RASTERIZER_DESC>().read_unaligned(),
                11 => self.depth = data.cast::<D3D12_DEPTH_STENCIL_DESC>().read_unaligned(),
                21 => {
                    // shortcut: the depth bounds test is dropped (the host
                    // pipeline uses the plain description); bring it back
                    // with a stream if a title enables it.
                    let d = data.cast::<D3D12_DEPTH_STENCIL_DESC1>().read_unaligned();
                    self.depth = D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: d.DepthEnable,
                        DepthWriteMask: d.DepthWriteMask,
                        DepthFunc: d.DepthFunc,
                        StencilEnable: d.StencilEnable,
                        StencilReadMask: d.StencilReadMask,
                        StencilWriteMask: d.StencilWriteMask,
                        FrontFace: d.FrontFace,
                        BackFace: d.BackFace,
                    };
                }
                12 => {
                    let layout = at.add(8).cast::<D3D12_INPUT_LAYOUT_DESC>().read_unaligned();
                    if layout.pInputElementDescs.is_null() {
                        return;
                    }
                    let elements = std::slice::from_raw_parts(
                        layout.pInputElementDescs,
                        layout.NumElements as usize,
                    );
                    self.names = elements
                        .iter()
                        .map(|e| CStr::from_ptr(e.SemanticName.0.cast()).to_owned())
                        .collect();
                    self.inputs = elements
                        .iter()
                        .zip(&self.names)
                        .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
                            SemanticName: PCSTR(name.as_ptr().cast()),
                            Format: DXGI_FORMAT(
                                crate::resource::pc_vertex_format(e.Format.0 as u32) as i32,
                            ),
                            ..*e
                        })
                        .collect();
                }
                13 => {
                    self.strip_cut =
                        D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(data.cast::<i32>().read_unaligned())
                }
                14 => {
                    self.topology =
                        D3D12_PRIMITIVE_TOPOLOGY_TYPE(data.cast::<i32>().read_unaligned())
                }
                15 => {
                    self.targets = data.cast::<D3D12_RT_FORMAT_ARRAY>().read_unaligned();
                    for f in &mut self.targets.RTFormats {
                        f.0 = crate::resource::pc_format(f.0 as u32) as i32;
                    }
                }
                16 => {
                    let f = data.cast::<u32>().read_unaligned();
                    self.depth_format = DXGI_FORMAT(crate::resource::pc_format(f) as i32);
                }
                17 => self.samples = data.cast::<DXGI_SAMPLE_DESC>().read_unaligned(),
                _ => {}
            }
        }
    }

    /// Everything the state holds, as text: part of a pipeline's name in
    /// the library and of the key a title pipeline is started early by.
    pub(crate) fn describe(&self) -> String {
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .zip(&self.names)
            .map(|(e, name)| {
                format!(
                    "{name:?} {} {} {} {} {} {}",
                    e.SemanticIndex,
                    e.Format.0,
                    e.InputSlot,
                    e.AlignedByteOffset,
                    e.InputSlotClass.0,
                    e.InstanceDataStepRate
                )
            })
            .collect();
        format!(
            "{:?} {} {:?} {:?} {inputs:?} {} {} {:?} {} {:?}",
            self.blend,
            self.sample_mask,
            self.rasterizer,
            self.depth,
            self.strip_cut.0,
            self.topology.0,
            self.targets,
            self.depth_format.0,
            self.samples
        )
    }

    /// Why the host cannot make a pipeline with this state (its target,
    /// depth and vertex formats, sample count, blending); `None` if it can.
    pub(crate) fn refusal(&self, caps: &impl crate::resource::format::Caps) -> Option<String> {
        use crate::resource::format::uses::*;
        let count = self.samples.Count;
        let targets = (self.targets.NumRenderTargets as usize).min(8);
        for (i, f) in self.targets.RTFormats[..targets].iter().enumerate() {
            let f = f.0 as u32;
            if f == 0 {
                continue;
            }
            let support = caps.support1(f);
            let blend = if self.blend.IndependentBlendEnable.as_bool() {
                self.blend.RenderTarget[i]
            } else {
                self.blend.RenderTarget[0]
            };
            if support & RENDER_TARGET == 0 {
                return Some(format!("render target format {f}: no render targets"));
            }
            if blend.BlendEnable.as_bool() && support & BLENDABLE == 0 {
                return Some(format!("render target format {f}: no blending"));
            }
            if !caps.samples(f, count) {
                return Some(format!("render target format {f}: no {count} samples"));
            }
        }
        let depth = self.depth_format.0 as u32;
        if depth != 0 {
            if caps.support1(depth) & DEPTH_STENCIL == 0 {
                return Some(format!("depth format {depth}: no depth-stencil targets"));
            }
            if !caps.samples(depth, count) {
                return Some(format!("depth format {depth}: no {count} samples"));
            }
        }
        self.inputs.iter().find_map(|e| {
            let f = e.Format.0 as u32;
            (caps.support1(f) & VERTEX_BUFFER == 0)
                .then(|| format!("vertex format {f}: not a vertex buffer format"))
        })
    }

    /// The vertex buffer slots the input layout reads.
    fn slots(&self) -> u32 {
        self.inputs
            .iter()
            .fold(0, |m, e| m | 1 << e.InputSlot.min(31))
    }

    /// The input elements with their byte offsets worked out.
    fn elements(&self) -> Vec<Input> {
        let mut next = [0u32; 32];
        self.inputs
            .iter()
            .map(|e| {
                let slot = e.InputSlot.min(31) as usize;
                let format = e.Format.0 as u32;
                let offset = if e.AlignedByteOffset == D3D12_APPEND_ALIGNED_ELEMENT {
                    next[slot]
                } else {
                    e.AlignedByteOffset
                };
                let bytes = crate::resource::element(format).map_or(0, |(b, _)| b as u32);
                next[slot] = offset + bytes;
                Input {
                    slot: slot as u32,
                    offset,
                    format,
                    per_instance: e.InputSlotClass == D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                }
            })
            .collect()
    }
}

/// One input element: vertex buffer slot, byte offset, format.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Input {
    pub(crate) slot: u32,
    pub(crate) offset: u32,
    pub(crate) format: u32,
    pub(crate) per_instance: bool,
}

/// The console's own primitive topology types.
const RECTANGLES: i32 = 19;
const QUADS: i32 = 32;

/// One stage of a host pipeline and where its bindings go.
pub(crate) struct Stage {
    pub(crate) shader: Arc<Shader>,
    /// The root parameter of its user data, and its size in dwords.
    pub(crate) user_data: Option<(u32, u32)>,
    /// Root parameters of its root views: (title root parameter, ours).
    pub(crate) root_cbvs: Vec<(u8, u32)>,
    pub(crate) root_srvs: Vec<(u8, u32)>,
}

impl Stage {
    pub(crate) fn translated(&self) -> &Translated {
        self.shader
            .translated()
            .expect("host pipelines are made from translated shaders")
    }
}

pub(crate) struct HostPipeline {
    /// A compute pipeline (else graphics).
    pub(crate) compute: bool,
    /// A mesh pipeline (graphics without vertex input).
    pub(crate) mesh: bool,
    pub(crate) pso: ID3D12PipelineState,
    pub(crate) root: ID3D12RootSignature,
    /// Vertex, then pixel stage, or the compute stage.
    pub(crate) stages: Vec<Stage>,
    /// The vertex buffer slots it reads (bit per slot).
    pub(crate) slots: u32,
    pub(crate) inputs: Vec<Input>,
    /// Its shaders, for reports of slow submissions.
    pub(crate) name: String,
}

/// Pipelines that got no host pipeline since the start, by reason.
static REFUSED: std::sync::Mutex<std::collections::BTreeMap<String, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Counts a pipeline that gets no host pipeline for `reason`; true for the
/// first of that reason.
fn note_refusal(reason: String) -> bool {
    let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    let n = refused.entry(reason).or_default();
    *n += 1;
    *n == 1
}

/// Counts a pipeline that gets no host pipeline, before any driver call;
/// logs the first of each reason.
fn refuse(reason: impl Into<String>, shaders: &[Arc<Shader>]) -> Option<HostPipeline> {
    let reason = reason.into();
    if note_refusal(reason.clone()) {
        tracing::warn!(?shaders, "no host pipeline: {reason} (logged once)");
    }
    None
}

/// The pipelines refused since the start, by reason, most first (`None`
/// if none was).
pub(crate) fn refusals() -> Option<String> {
    let refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    if refused.is_empty() {
        return None;
    }
    let mut counts: Vec<(&String, &u64)> = refused.iter().collect();
    counts.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let counts: Vec<String> = counts.iter().map(|(r, n)| format!("{n} {r}")).collect();
    Some(format!(
        "host pipelines refused so far: {}",
        counts.join("; ")
    ))
}

/// What a translated shader asks of the host: the wave lanes it runs in
/// (`PSV0`'s expected lane counts; 0 and `u32::MAX`: any) and its shader
/// model (the `DXIL` program header's version).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Needs {
    pub(crate) lanes: (u32, u32),
    pub(crate) model: (u32, u32),
}

impl Needs {
    /// What the shader in a DXIL container needs; `None` if the container
    /// does not say.
    pub(crate) fn of(dxil: &[u8]) -> Option<Needs> {
        use xtp_shader::{Container, FourCC};
        let c = Container::parse(dxil).ok()?;
        let word = |data: &[u8], at: usize| {
            Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
        };
        let psv = c.part(FourCC::PSV0)?.data;
        // The runtime information's size, then its stage-specific 16
        // bytes, then the lane counts.
        let lanes = if word(psv, 0)? >= 24 {
            (word(psv, 20)?, word(psv, 24)?)
        } else {
            (0, u32::MAX)
        };
        let version = word(c.part(FourCC::DXIL)?.data, 0)?;
        Some(Needs {
            lanes,
            model: (version >> 4 & 0xf, version & 0xf),
        })
    }

    /// Why a host with waves of `host` lanes (min, max) and shader model
    /// `model` cannot run it; `None` if it can.
    pub(crate) fn refusal(&self, host: (u32, u32), model: (u32, u32)) -> Option<String> {
        let (min, max) = self.lanes;
        if min != 0 && (max < host.0 || min > host.1) {
            let wanted = if min == max {
                format!("{min}-lane waves")
            } else {
                format!("waves of {min} to {max} lanes")
            };
            return Some(format!(
                "needs {wanted}; the host has {} to {}",
                host.0, host.1
            ));
        }
        (self.model > model).then(|| {
            format!(
                "needs shader model {}.{}; the host has {}.{}",
                self.model.0, self.model.1, model.0, model.1
            )
        })
    }
}

/// Why the host cannot run `shader`'s translation; `None` if it can (or the
/// host's lanes are unknown).
fn shader_refusal(shader: &Translated) -> Option<String> {
    let needs = Needs::of(&shader.host.dxil)?;
    let lanes = crate::device::host_wave_lanes()?;
    let model = crate::device::host_shader_model()?;
    needs.refusal(lanes, model)
}

impl HostPipeline {
    /// A host pipeline for vertex and pixel shaders with the graphics
    /// state `g`, or for one compute shader.
    pub(crate) fn new(shaders: &[Arc<Shader>], g: Option<&GraphicsState>) -> Option<HostPipeline> {
        let find = |stage| shaders.iter().find(|s| s.stage == stage);
        let compute = match shaders {
            [cs] if cs.stage == "compute" => Some(cs),
            _ => None,
        };
        if compute.is_none()
            && shaders
                .iter()
                .any(|s| !matches!(s.stage, "vertex" | "pixel" | "mesh"))
        {
            return refuse(
                "only vertex, mesh, pixel and compute stages are translated",
                shaders,
            );
        }
        let mesh = find("mesh");
        let stages = if let Some(cs) = compute {
            vec![(cs, D3D12_SHADER_VISIBILITY_ALL)]
        } else {
            let first = match (mesh, find("vertex")) {
                (Some(ms), _) => (ms, D3D12_SHADER_VISIBILITY_MESH),
                (None, Some(vs)) => (vs, D3D12_SHADER_VISIBILITY_VERTEX),
                (None, None) => return refuse("no vertex or mesh shader", shaders),
            };
            let mut stages = vec![first];
            stages.extend(find("pixel").map(|ps| (ps, D3D12_SHADER_VISIBILITY_PIXEL)));
            stages
        };
        if mesh.is_some() && !host()?.mesh_shaders() {
            return refuse(
                "the host GPU has no mesh shaders (or no shader model 6.6)",
                shaders,
            );
        }
        for (s, _) in &stages {
            if let Some(why) = shader_refusal(s.translated()?) {
                return refuse(format!("{} shader {why}", s.stage), shaders);
            }
        }
        let host = host()?;

        // The root signature the translated shaders' bindings define.
        let mut params: Vec<D3D12_ROOT_PARAMETER1> = Vec::new();
        let mut samplers = Vec::new();
        let mut bound = Vec::new();
        for &(shader, visibility) in &stages {
            let t = &shader.translated()?.host;
            let mut stage = Stage {
                shader: shader.clone(),
                user_data: None,
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
            };
            let dwords = t.user_data_dwords();
            if dwords > 0 {
                stage.user_data = Some((params.len() as u32, dwords));
                params.push(D3D12_ROOT_PARAMETER1 {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER1_0 {
                        Constants: D3D12_ROOT_CONSTANTS {
                            ShaderRegister: 0,
                            RegisterSpace: USER_DATA_SPACE,
                            Num32BitValues: dwords,
                        },
                    },
                    ShaderVisibility: visibility,
                });
            }
            for (slots, kind, out) in [
                (
                    &t.root_cbvs,
                    D3D12_ROOT_PARAMETER_TYPE_CBV,
                    &mut stage.root_cbvs,
                ),
                (
                    &t.root_srvs,
                    D3D12_ROOT_PARAMETER_TYPE_SRV,
                    &mut stage.root_srvs,
                ),
            ] {
                for &slot in slots {
                    out.push((slot, params.len() as u32));
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: kind,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                                ShaderRegister: u32::from(slot),
                                RegisterSpace: ROOT_CBV_SPACE,
                                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                            },
                        },
                        ShaderVisibility: visibility,
                    });
                }
            }
            for (n, s) in t.static_samplers.iter().enumerate() {
                samplers.push(D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER(s.filter as i32),
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.address[0] as i32),
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.address[1] as i32),
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.address[2] as i32),
                    MipLODBias: s.mip_lod_bias,
                    MaxAnisotropy: s.max_anisotropy,
                    ComparisonFunc: D3D12_COMPARISON_FUNC(s.comparison as i32),
                    BorderColor: D3D12_STATIC_BORDER_COLOR(s.border as i32),
                    MinLOD: s.min_lod,
                    MaxLOD: s.max_lod,
                    ShaderRegister: n as u32,
                    RegisterSpace: SAMPLER_SPACE,
                    ShaderVisibility: visibility,
                });
            }
            bound.push(stage);
        }
        let (root, root_blob) =
            match root_signature(&host.device, &params, &samplers, compute.is_some()) {
                Ok(root) => root,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host root signature refused");
                    note_refusal(format!(
                        "the driver refused its root signature ({})",
                        e.code()
                    ));
                    return None;
                }
            };
        let code = |i: usize| {
            stages.get(i).and_then(|(s, _)| s.translated()).map_or(
                D3D12_SHADER_BYTECODE::default(),
                |t| D3D12_SHADER_BYTECODE {
                    pShaderBytecode: t.host.dxil.as_ptr().cast(),
                    BytecodeLength: t.host.dxil.len(),
                },
            )
        };
        if compute.is_some() {
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                // SAFETY: a borrowed pointer the description never releases.
                pRootSignature: unsafe { std::mem::transmute_copy(&root) },
                CS: code(0),
                ..Default::default()
            };
            let name = super::library::name("compute", &root_blob, &[&code_bytes(&code(0))], "");
            let made = super::library::pipeline(
                &name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadComputePipeline(name, &desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateComputePipelineState(&desc) },
            );
            let pso = match made {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                    host.log_messages();
                    note_refusal(format!("the driver refused it ({})", e.code()));
                    return None;
                }
            };
            tracing::info!(?shaders, "host pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: true,
                mesh: false,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
            });
        }
        let Some(g) = g else {
            return refuse("graphics state not kept (plain description)", shaders);
        };
        if let Some(why) = g.refusal(&crate::resource::format::HostCaps) {
            return refuse(why, shaders);
        }
        let state = g.describe();
        if mesh.is_some() {
            let name = super::library::name(
                "mesh",
                &root_blob,
                &[&code_bytes(&code(0)), &code_bytes(&code(1))],
                &state,
            );
            let pso = match mesh_pipeline(&host.device, &root, code(0), code(1), g, &name) {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host mesh pipeline refused");
                    host.log_messages();
                    note_refusal(format!("the driver refused it ({})", e.code()));
                    return None;
                }
            };
            tracing::info!(?shaders, "host mesh pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: false,
                mesh: true,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
            });
        }

        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: code(0),
            PS: code(1),
            BlendState: g.blend,
            SampleMask: g.sample_mask,
            RasterizerState: g.rasterizer,
            DepthStencilState: g.depth,
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: g.inputs.as_ptr(),
                NumElements: g.inputs.len() as u32,
            },
            IBStripCutValue: g.strip_cut,
            // The console's rectangle and quad types draw as triangles on
            // the host (see `draw`).
            PrimitiveTopologyType: match g.topology.0 {
                RECTANGLES | QUADS => D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                _ => g.topology,
            },
            NumRenderTargets: g.targets.NumRenderTargets,
            RTVFormats: g.targets.RTFormats,
            DSVFormat: g.depth_format,
            SampleDesc: g.samples,
            ..Default::default()
        };
        let name = super::library::name(
            "graphics",
            &root_blob,
            &[&code_bytes(&code(0)), &code_bytes(&code(1))],
            &state,
        );
        let made = super::library::pipeline(
            &name,
            // SAFETY: the description points at live data.
            |library, name| unsafe { library.LoadGraphicsPipeline(name, &desc) },
            // SAFETY: as above.
            || unsafe { host.device.CreateGraphicsPipelineState(&desc) },
        );
        let pso = match made {
            Ok(pso) => pso,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                host.log_messages();
                note_refusal(format!("the driver refused it ({})", e.code()));
                return None;
            }
        };
        tracing::info!(?shaders, "host pipeline");
        crate::host::name(&pso, || format!("{shaders:?}"));
        tracing::debug!(
            ?shaders,
            depth = ?g.depth,
            dsv = g.depth_format.0,
            targets = ?&g.targets.RTFormats[..g.targets.NumRenderTargets.min(8) as usize],
            blend = ?g.blend.RenderTarget[0],
            cull = g.rasterizer.CullMode.0,
            "host graphics state"
        );
        Some(HostPipeline {
            compute: false,
            mesh: false,
            pso,
            root,
            stages: bound,
            name: format!("{shaders:?}"),
            slots: g.slots(),
            inputs: g.elements(),
        })
    }
}

/// A mesh pipeline from the title's graphics state: the pipeline-state
/// stream form (no input layout or topology).
fn mesh_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    ms: D3D12_SHADER_BYTECODE,
    ps: D3D12_SHADER_BYTECODE,
    g: &GraphicsState,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    /// A stream subobject: its type, then its description, pointer-aligned.
    #[repr(C, align(8))]
    struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);
    #[repr(C)]
    struct Stream {
        root: Sub<*mut std::ffi::c_void>,
        ms: Sub<D3D12_SHADER_BYTECODE>,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D3D12_DEPTH_STENCIL_DESC>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
    }
    let mut stream = Stream {
        root: Sub(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
            root.as_raw(),
        ),
        ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, ms),
        ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, ps),
        blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, g.blend),
        mask: Sub(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK,
            g.sample_mask,
        ),
        raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, g.rasterizer),
        depth: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL, g.depth),
        targets: Sub(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
            g.targets,
        ),
        dsv: Sub(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
            g.depth_format,
        ),
        samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, g.samples),
    };
    let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: size_of::<Stream>(),
        pPipelineStateSubobjectStream: (&raw mut stream).cast(),
    };
    let device = device.cast::<ID3D12Device2>()?;
    super::library::pipeline(
        name,
        // SAFETY: the stream holds whole subobjects pointing at live data.
        |library, name| unsafe { library.LoadPipeline(name, &desc) },
        // SAFETY: as above.
        || unsafe { device.CreatePipelineState(&desc) },
    )
}

/// The bytes of shader code (none for an absent stage).
fn code_bytes(code: &D3D12_SHADER_BYTECODE) -> Vec<u8> {
    if code.pShaderBytecode.is_null() {
        return Vec::new();
    }
    // SAFETY: a translation's own bytes, alive while its shader is.
    unsafe { std::slice::from_raw_parts(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength) }
        .to_vec()
}

fn root_signature(
    device: &ID3D12Device,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    compute: bool,
) -> windows::core::Result<(ID3D12RootSignature, Vec<u8>)> {
    let input = if compute {
        D3D12_ROOT_SIGNATURE_FLAG_NONE
    } else {
        D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
    };
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: samplers.len() as u32,
                pStaticSamplers: samplers.as_ptr(),
                Flags: input
                    | D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                    | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
            },
        },
    };
    // SAFETY: the description points at live arrays; the blob is read in
    // full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut error: Option<ID3DBlob> = None;
        if let Err(e) = D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error)) {
            if let Some(error) = error {
                let text = std::slice::from_raw_parts(
                    error.GetBufferPointer().cast::<u8>(),
                    error.GetBufferSize(),
                );
                tracing::warn!("root signature: {}", String::from_utf8_lossy(text));
            }
            return Err(e);
        }
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        Ok((device.CreateRootSignature(0, bytes)?, bytes.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DXIL container with a `PSV0` part (runtime information of
    /// `info_size` bytes, the lane counts at 16) and a `DXIL` part starting
    /// with the program version.
    fn container(info_size: u32, lanes: (u32, u32), version: u32) -> Vec<u8> {
        let mut psv = info_size.to_le_bytes().to_vec();
        let mut info = vec![0u8; info_size as usize];
        if info_size >= 24 {
            info[16..20].copy_from_slice(&lanes.0.to_le_bytes());
            info[20..24].copy_from_slice(&lanes.1.to_le_bytes());
        }
        psv.extend(info);
        let dxil = [version.to_le_bytes(), [0; 4]].concat();
        let parts: [(&[u8; 4], &[u8]); 2] = [(b"PSV0", &psv), (b"DXIL", &dxil)];
        let header = 0x20 + 4 * parts.len();
        let mut body = Vec::new();
        let mut offsets = Vec::new();
        for (fourcc, data) in parts {
            offsets.push((header + body.len()) as u32);
            body.extend(*fourcc);
            body.extend((data.len() as u32).to_le_bytes());
            body.extend(data);
        }
        let mut out = b"DXBC".to_vec();
        out.extend([0; 16]);
        out.extend(1u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(((header + body.len()) as u32).to_le_bytes());
        out.extend((parts.len() as u32).to_le_bytes());
        for o in offsets {
            out.extend(o.to_le_bytes());
        }
        out.extend(body);
        out
    }

    /// Compute shader (kind 5), shader model 6.6.
    const CS_6_6: u32 = 5 << 16 | 6 << 4 | 6;

    #[test]
    fn wave64_programs_are_refused_on_32_lane_hosts() {
        let wave64 = Needs::of(&container(48, (64, 64), CS_6_6)).unwrap();
        assert_eq!(
            wave64,
            Needs {
                lanes: (64, 64),
                model: (6, 6)
            }
        );
        let nvidia = (32, 32);
        assert_eq!(
            wave64.refusal(nvidia, (6, 8)).as_deref(),
            Some("needs 64-lane waves; the host has 32 to 32")
        );
        // RDNA: 32 to 64 lanes.
        assert_eq!(wave64.refusal((32, 64), (6, 8)), None);
        // No wave size asked: anywhere.
        let any = Needs::of(&container(48, (0, u32::MAX), CS_6_6)).unwrap();
        assert_eq!(any.refusal(nvidia, (6, 8)), None);
        // Older runtime information has no lane counts.
        let old = Needs::of(&container(16, (64, 64), CS_6_6)).unwrap();
        assert_eq!(old.lanes, (0, u32::MAX));
    }

    #[test]
    fn programs_of_a_newer_shader_model_are_refused() {
        let needs = Needs::of(&container(48, (0, u32::MAX), CS_6_6)).unwrap();
        assert_eq!(
            needs.refusal((32, 32), (6, 5)).as_deref(),
            Some("needs shader model 6.6; the host has 6.5")
        );
        assert_eq!(needs.refusal((32, 32), (6, 6)), None);
        assert_eq!(Needs::of(b"not a container"), None);
    }

    /// A host that renders, blends and samples only `targets`, has depth in
    /// `depth`, vertex input in `vertex`, and 4 samples of nothing.
    struct Fake {
        targets: &'static [u32],
        blend: &'static [u32],
        depth: &'static [u32],
        vertex: &'static [u32],
    }

    impl crate::resource::format::Caps for Fake {
        fn support1(&self, format: u32) -> u32 {
            use crate::resource::format::uses::*;
            let has = |list: &[u32], bit| if list.contains(&format) { bit } else { 0 };
            has(self.targets, RENDER_TARGET)
                | has(self.blend, BLENDABLE)
                | has(self.depth, DEPTH_STENCIL)
                | has(self.vertex, VERTEX_BUFFER)
        }
        fn samples(&self, _: u32, count: u32) -> bool {
            count <= 1
        }
    }

    const HOST: Fake = Fake {
        targets: &[28, 57],
        blend: &[28],
        depth: &[40],
        vertex: &[2, 28],
    };

    fn state(targets: &[u32], depth: u32) -> GraphicsState {
        let mut g = GraphicsState::default();
        g.targets.NumRenderTargets = targets.len() as u32;
        for (slot, &f) in g.targets.RTFormats.iter_mut().zip(targets) {
            slot.0 = f as i32;
        }
        g.depth_format = DXGI_FORMAT(depth as i32);
        g
    }

    #[test]
    fn graphics_states_the_host_lacks_are_refused() {
        assert_eq!(state(&[28, 0], 40).refusal(&HOST), None);
        assert_eq!(
            state(&[115], 0).refusal(&HOST).as_deref(),
            Some("render target format 115: no render targets")
        );
        assert_eq!(
            state(&[28], 55).refusal(&HOST).as_deref(),
            Some("depth format 55: no depth-stencil targets")
        );
        // Blending a target that cannot blend (an integer format).
        let mut blended = state(&[28, 57], 0);
        blended.blend.RenderTarget[0].BlendEnable = true.into();
        assert_eq!(
            blended.refusal(&HOST).as_deref(),
            Some("render target format 57: no blending")
        );
        blended.blend.IndependentBlendEnable = true.into();
        assert_eq!(blended.refusal(&HOST), None, "only target 0 blends");
        let mut msaa = state(&[28], 0);
        msaa.samples.Count = 4;
        assert_eq!(
            msaa.refusal(&HOST).as_deref(),
            Some("render target format 28: no 4 samples")
        );
        let mut inputs = state(&[28], 0);
        inputs.inputs.push(D3D12_INPUT_ELEMENT_DESC {
            Format: DXGI_FORMAT(86),
            ..Default::default()
        });
        assert_eq!(
            inputs.refusal(&HOST).as_deref(),
            Some("vertex format 86: not a vertex buffer format")
        );
    }
}
