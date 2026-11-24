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
    /// The targets' title formats ([`blend_without_alpha`]).
    title_targets: D3D12_RT_FORMAT_ARRAY,
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
            title_targets: D3D12_RT_FORMAT_ARRAY::default(),
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
                8 => {
                    self.blend = data.cast::<D3D12_BLEND_DESC>().read_unaligned();
                    blend_without_alpha(&mut self.blend, &self.title_targets);
                }
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
                    // The blend and the formats come in either order: each
                    // applies it, and twice changes nothing more.
                    self.title_targets = self.targets;
                    blend_without_alpha(&mut self.blend, &self.title_targets);
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

/// A pipeline's blending for render targets of title formats `targets`,
/// with those whose stand-in has an alpha the title's format lacks
/// ([`crate::resource::format::alpha_one`]: R9G9B9E5 as RGBA16F, B5G6R5
/// as RGBA8) neither writing alpha nor blending with the target's alpha,
/// as on the console, where that alpha reads one: in the colour factors,
/// destination alpha becomes one, its inverse zero, and saturated source
/// alpha (`min(As, 1 - Ad)`) zero. The alpha factors stay: what they make
/// is not written. Applied as a stream is read, so pipeline names in the
/// library change with it (the review of the #492 port, from dark-world).
fn blend_without_alpha(blend: &mut D3D12_BLEND_DESC, targets: &D3D12_RT_FORMAT_ARRAY) {
    blend_without(blend, targets, crate::resource::format::alpha_one);
}

/// [`blend_without_alpha`] for the title formats `lacks_alpha` names. One
/// blending for every target stays one where every target lacks alpha,
/// and becomes one per target where some target keeps its alpha, unless
/// it has a logic operation: D3D12 takes none with blending per target
/// (`IndependentBlendEnable` must be false), so such a pipeline keeps its
/// blending and writes the alpha of its targets without one.
// shortcut: a logic operation on targets with and without alpha still
// writes the stand-in's alpha (rare: no logic operation on float targets,
// R9G9B9E5's stand-in is one).
fn blend_without(
    blend: &mut D3D12_BLEND_DESC,
    targets: &D3D12_RT_FORMAT_ARRAY,
    lacks_alpha: impl Fn(u32) -> bool,
) {
    let count = (targets.NumRenderTargets as usize).min(8);
    let bound = || (0..count).filter(|&i| targets.RTFormats[i].0 != 0);
    let lacking: Vec<usize> = bound()
        .filter(|&i| lacks_alpha(targets.RTFormats[i].0 as u32))
        .collect();
    if lacking.is_empty() {
        return;
    }
    let keeping = bound().any(|i| !lacks_alpha(targets.RTFormats[i].0 as u32));
    if !blend.IndependentBlendEnable.as_bool() && keeping {
        if blend.RenderTarget[0].LogicOpEnable.as_bool() {
            return;
        }
        blend.IndependentBlendEnable = true.into();
        blend.RenderTarget = [blend.RenderTarget[0]; 8];
    }
    let slots = if blend.IndependentBlendEnable.as_bool() {
        lacking
    } else {
        vec![0]
    };
    let factor = |f: D3D12_BLEND| match f {
        D3D12_BLEND_DEST_ALPHA => D3D12_BLEND_ONE,
        D3D12_BLEND_INV_DEST_ALPHA | D3D12_BLEND_SRC_ALPHA_SAT => D3D12_BLEND_ZERO,
        other => other,
    };
    for i in slots {
        let t = &mut blend.RenderTarget[i];
        t.SrcBlend = factor(t.SrcBlend);
        t.DestBlend = factor(t.DestBlend);
        t.RenderTargetWriteMask &= !(D3D12_COLOR_WRITE_ENABLE_ALPHA.0 as u8);
    }
}

/// Whether root SRVs are bound as one-view tables of the buffer's host
/// copy (reads past its end return zeros) rather than root descriptors
/// (unbounded: a mesh shader's vertex fetch, which has no record limit,
/// read past the copy into memory the host had freed). Set
/// `XTP_ROOT_SRV_DIRECT=1` to go back.
pub(crate) fn bounded_root_srvs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XTP_ROOT_SRV_DIRECT").is_none())
}

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

#[cfg(feature = "diagnostics")]
thread_local! {
    /// The reason of the last refusal on this thread (diagnostics: what
    /// draws dropped for want of the pipeline are counted under).
    static LAST_REFUSAL: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The reason of the last refusal on this thread since the last call
/// (diagnostics).
#[cfg(feature = "diagnostics")]
pub(crate) fn take_refusal() -> Option<String> {
    LAST_REFUSAL.with(|r| r.borrow_mut().take())
}

/// Counts a pipeline that gets no host pipeline for `reason`; true for the
/// first of that reason.
fn note_refusal(reason: String) -> bool {
    #[cfg(feature = "diagnostics")]
    LAST_REFUSAL.with(|r| *r.borrow_mut() = Some(reason.clone()));
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
/// (`PSV0`'s expected lane counts; 0 and `u32::MAX`: any), its shader
/// model (the `DXIL` program header's version), and whether it is a mesh
/// program that needs the console's 64-lane waves whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Needs {
    pub(crate) lanes: (u32, u32),
    pub(crate) model: (u32, u32),
    /// Why a console mesh program that runs in 64-lane waves cannot run on
    /// a host without them, if it cannot: its translation declares no wave
    /// size (only compute translations do), so `lanes` does not say so.
    ///
    /// The translator decides (`xtp-shader` `emit/lower/mesh.rs`,
    /// `docs/analysis/mesh-wave64.md`). Translated for a target without
    /// 64-lane waves, which the host's is on NVIDIA, a wave64 mesh program
    /// gets its console wave's first lane by proof or through group-shared
    /// memory, and is refused there when neither holds. What stays here:
    ///
    /// * `XTP_WAVE64_MESH=refuse` ([`wave64_mesh_refused`]): VIctory #578's
    ///   refusal of every such program, for an A/B in game (the RTX 5090
    ///   VIctory saw removed as hung ran a translator without that form);
    /// * a translation made for a target with 64-lane waves
    ///   (`XTP_HOST_LANES`), which takes the console waves to be whole, on
    ///   a host without them.
    pub(crate) wave64_mesh: Option<&'static str>,
}

/// What a wave64 mesh program is refused as with `XTP_WAVE64_MESH=refuse`
/// ([`Needs::wave64_mesh`]).
const MESH_REFUSED: &str =
    "a mesh program written for 64-lane waves (XTP_WAVE64_MESH=refuse, as VIctory #578)";

/// What a wave64 mesh program translated for a target with 64-lane waves
/// is refused as on a host without them ([`Needs::wave64_mesh`]).
const MESH_WAVE64: &str = "a mesh program translated for 64-lane waves (XTP_HOST_LANES)";

/// Whether `XTP_WAVE64_MESH=refuse` refuses every wave64 mesh program on
/// hosts without 64-lane waves ([`Needs::wave64_mesh`]); read once, and
/// logged with what it means on this host.
pub(crate) fn wave64_mesh_refused() -> bool {
    static REFUSE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REFUSE.get_or_init(|| {
        let refuse = std::env::var("XTP_WAVE64_MESH").is_ok_and(|v| v == "refuse");
        // The host's own lanes, not the translations' target: mesh
        // translations declare no wave size.
        match crate::device::host_wave_lanes() {
            Some((min, max)) if max < 64 && refuse => tracing::warn!(
                lanes = format_args!("{min} to {max}"),
                "XTP_WAVE64_MESH=refuse: wave64 mesh programs are refused on a host without \
                 64-lane waves (VIctory #578)"
            ),
            Some((min, max)) if max < 64 => tracing::info!(
                lanes = format_args!("{min} to {max}"),
                "the host has no 64-lane waves: wave64 mesh programs read their console wave's \
                 first lane by proof or through group-shared memory, or do not translate \
                 (XTP_WAVE64_MESH=refuse refuses them all)"
            ),
            _ => {}
        }
        refuse
    })
}

impl Needs {
    /// What a translated `stage` shader needs; `None` if its container does
    /// not say and it is no wave64 mesh program the host cannot run (that
    /// check does not depend on the container).
    pub(crate) fn of_translated(stage: &str, shader: &Translated) -> Option<Needs> {
        Self::translated_with(stage, shader, Self::mesh_switches())
    }

    /// [`Needs::of_translated`] with `switches` ([`Needs::mesh_switches`]).
    fn translated_with(stage: &str, shader: &Translated, switches: (bool, bool)) -> Option<Needs> {
        let wave64_mesh = Self::mesh_reason(stage, shader.wave64, switches);
        match Needs::of(&shader.host.dxil) {
            Some(needs) => Some(Needs {
                wave64_mesh,
                ..needs
            }),
            None => wave64_mesh.map(|_| Needs {
                lanes: (0, u32::MAX),
                model: (0, 0),
                wave64_mesh,
            }),
        }
    }

    /// What a `stage` shader whose console program says it of itself
    /// (`native`) needs, as far as that decides: whether it is a wave64 mesh
    /// program the host cannot run, with any lanes and shader model (mesh
    /// translations declare no wave size, so the translation would not say
    /// more on that); `None` if it is none.
    pub(crate) fn of_native(stage: &str, native: &super::Native) -> Option<Needs> {
        Self::native_with(stage, native, Self::mesh_switches())
    }

    /// [`Needs::of_native`] with `switches` ([`Needs::mesh_switches`]).
    fn native_with(stage: &str, native: &super::Native, switches: (bool, bool)) -> Option<Needs> {
        Self::mesh_reason(stage, native.wave64, switches).map(|what| Needs {
            lanes: (0, u32::MAX),
            model: (0, 0),
            wave64_mesh: Some(what),
        })
    }

    /// What decides [`Needs::wave64_mesh`] besides the program: whether
    /// `XTP_WAVE64_MESH=refuse` is set, and whether shaders are translated
    /// for a target with 64-lane waves ([`crate::device::host_target`]).
    fn mesh_switches() -> (bool, bool) {
        (wave64_mesh_refused(), crate::device::host_target().runs(64))
    }

    /// [`Needs::wave64_mesh`] for a `stage` shader whose console program
    /// runs in 64-lane waves or not (`wave64`), with `(refuse, target_64)`
    /// ([`Needs::mesh_switches`]).
    fn mesh_reason(
        stage: &str,
        wave64: bool,
        (refuse, target_64): (bool, bool),
    ) -> Option<&'static str> {
        if stage != "mesh" || !wave64 {
            None
        } else if refuse {
            Some(MESH_REFUSED)
        } else if target_64 {
            Some(MESH_WAVE64)
        } else {
            None
        }
    }

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
            wave64_mesh: None,
        })
    }

    /// Why a host with waves of `host` lanes (min, max) and shader model
    /// `model` cannot run it; `None` if it can. One reason, the first of:
    /// the wave size its translation declares (a wave64 program the
    /// translator made 32-lane declares `WaveSize(32)` and runs on a 32-lane
    /// host); a wave64 mesh program that needs its waves whole
    /// ([`Needs::wave64_mesh`]), which declares none, on a host without
    /// 64-lane waves; its shader model.
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
        // shortcut: a host whose range holds 64 lanes (AMD RDNA: 32 to 64)
        // is taken to run mesh programs in 64-lane waves. D3D12 gives mesh
        // shaders no wave size to ask for, so its driver may pick 32 (VIctory
        // #578 takes the same; refusing there would drop every mesh program
        // on AMD too).
        if let Some(what) = self.wave64_mesh
            && min == 0
            && host.1 < 64
        {
            return Some(format!("is {what}; the host has {} to {}", host.0, host.1));
        }
        (self.model > model).then(|| {
            format!(
                "needs shader model {}.{}; the host has {}.{}",
                self.model.0, self.model.1, model.0, model.1
            )
        })
    }
}

/// Why the host cannot run the translation of a `stage` shader; `None` if
/// it can (or the host's lanes are unknown).
fn shader_refusal(stage: &str, shader: &Translated) -> Option<String> {
    let needs = Needs::of_translated(stage, shader)?;
    let lanes = crate::device::host_wave_lanes()?;
    let model = crate::device::host_shader_model()?;
    needs.refusal(lanes, model)
}

/// Why the host cannot run a `stage` shader, known from its console
/// program alone ([`Needs::of_native`]): the reason [`shader_refusal`]
/// would give once it is translated. `None` if only the translation can
/// say (or the host's lanes are unknown).
fn native_refusal(stage: &str, shader: &Shader) -> Option<String> {
    let needs = Needs::of_native(stage, shader.native().ok()?)?;
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
        // What the console programs refuse by themselves, before any
        // translation: one takes a worker up to seconds, and in the world
        // the workers spent them on wave64 mesh programs refused right
        // after (a quarter of the world's shaders, docs/analysis/drops.md),
        // while the pipelines that would run waited behind them.
        for (s, _) in &stages {
            if let Some(why) = native_refusal(s.stage, s) {
                return refuse(format!("{} shader {why}", s.stage), shaders);
            }
        }
        for (s, _) in &stages {
            if let Some(why) = shader_refusal(s.stage, s.translated()?) {
                return refuse(format!("{} shader {why}", s.stage), shaders);
            }
        }
        let host = host()?;

        // The root signature the translated shaders' bindings define.
        let mut params: Vec<D3D12_ROOT_PARAMETER1> = Vec::new();
        // Ranges the table parameters point at, alive until the root
        // signature is made.
        let mut ranges: Vec<Box<D3D12_DESCRIPTOR_RANGE1>> = Vec::new();
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
                    if kind == D3D12_ROOT_PARAMETER_TYPE_SRV && bounded_root_srvs() {
                        // A one-view table: the view's size bounds the reads,
                        // which a root descriptor leaves unbounded.
                        ranges.push(Box::new(D3D12_DESCRIPTOR_RANGE1 {
                            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                            NumDescriptors: 1,
                            BaseShaderRegister: u32::from(slot),
                            RegisterSpace: ROOT_CBV_SPACE,
                            Flags: D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                                | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
                            OffsetInDescriptorsFromTableStart: 0,
                        }));
                        let range: *const D3D12_DESCRIPTOR_RANGE1 = &**ranges.last()?;
                        out.push((slot, params.len() as u32));
                        params.push(D3D12_ROOT_PARAMETER1 {
                            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                                    NumDescriptorRanges: 1,
                                    pDescriptorRanges: range,
                                },
                            },
                            ShaderVisibility: visibility,
                        });
                        continue;
                    }
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

    /// A real console wave64 mesh program of two console waves (it reads
    /// its group id, `v5`, from the first lane and exports a primitive),
    /// translated for a host running waves of `lanes` lanes.
    fn wave64_mesh(lanes: (u32, u32)) -> xtp_shader::emit::HostShader {
        use xtp_shader::native::{MeshIo, StageIo};
        let words: [u32; 4] = [
            0x7E04_0505, // v_readfirstlane_b32 s2, v5
            0xF800_0941,
            0x0000_0009, // exp prim, v9, off, off, off done
            0xBF81_0000, // s_endpgm
        ];
        let native = xtp_shader::NativeProgram {
            stage: xtp_shader::HwStage::Ms,
            wave64: true,
            vgprs: 16,
            layout: Vec::new(),
            used: Vec::new(),
            io: StageIo::Ms(MeshIo {
                outputs: Vec::new(),
                clip_cntl: 0,
                threads: 128,
                max_vertices: 128,
                max_primitives: 128,
                topology: 2,
                lds_bytes: 0,
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let c = xtp_shader::Container {
            digest: [0; 16],
            version: (1, 0),
            parts: Vec::new(),
        };
        let target = xtp_shader::emit::Target {
            lanes_min: lanes.0,
            lanes_max: lanes.1,
        };
        xtp_shader::emit::translate_program_for(&c, &native, target).expect("it translates")
    }

    /// A refusal known from the console program alone is the one its
    /// translation gets, for real mesh translations: they declare no wave
    /// size, so their container adds nothing to what decides. For every
    /// switch, target and host; and there is none for other stages and
    /// wave32 programs, which only their translation can refuse.
    #[test]
    fn mesh_refusals_before_and_after_translation_agree() {
        for (target, target_64) in [((32, 32), false), ((4, 4), false), ((32, 64), true)] {
            let host = wave64_mesh(target);
            assert_eq!(
                Needs::of(&host.dxil).map(|n| n.lanes),
                Some((0, u32::MAX)),
                "a mesh translation for {target:?} declares no wave size"
            );
            for (stage, wave64, refuse) in ["mesh", "pixel", "compute"]
                .into_iter()
                .flat_map(|s| [(s, true), (s, false)])
                .flat_map(|(s, w)| [(s, w, false), (s, w, true)])
            {
                let switches = (refuse, target_64);
                let native = super::super::Native {
                    used: Vec::new(),
                    wave64,
                    threads: None,
                };
                let translated = Translated {
                    host: host.clone(),
                    used: Vec::new(),
                    wave64,
                };
                for lanes in [(32, 32), (4, 4), (32, 64), (64, 64)] {
                    let before = Needs::native_with(stage, &native, switches)
                        .and_then(|n| n.refusal(lanes, (6, 8)));
                    let after = Needs::translated_with(stage, &translated, switches)
                        .and_then(|n| n.refusal(lanes, (6, 8)));
                    let what = format!("{stage} {wave64} {switches:?} {target:?} {lanes:?}");
                    if before.is_some() {
                        assert_eq!(before, after, "{what}");
                    }
                    if stage != "mesh" || !wave64 || lanes.1 >= 64 {
                        assert_eq!(before, None, "{what}");
                    }
                }
            }
        }
    }

    /// A wave64 mesh pipeline is refused before any of its shaders is
    /// translated only with `XTP_WAVE64_MESH=refuse` (or a target with
    /// 64-lane waves forced on a host without them): by default the
    /// translator decides, so on a host without 64-lane waves (WARP,
    /// NVIDIA) its shaders are translated first.
    #[test]
    fn wave64_mesh_pipelines_are_refused_before_translation() {
        let test = "pipeline::host::tests::wave64_mesh_pipelines_are_refused_before_translation";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let lanes = crate::device::host_wave_lanes().expect("the device's lanes");
        if !host.mesh_shaders() || lanes.1 >= 64 {
            // Nothing is refused for its waves on this host: not a missing
            // device, so no failure in strict mode either.
            eprintln!("SKIPPED {test}: no mesh shaders, or 64-lane waves");
            return;
        }
        let shader = |stage, hash: &str| {
            Arc::new(Shader {
                stage,
                hash: hash.into(),
                container: b"not translated".to_vec(),
                native: std::sync::OnceLock::from(Ok(super::super::Native {
                    used: Vec::new(),
                    wave64: true,
                    threads: None,
                })),
                translated: std::sync::OnceLock::new(),
            })
        };
        let (ms, ps) = (
            shader("mesh", "6d65736836340000"),
            shader("pixel", "7069786c36340000"),
        );
        let made = HostPipeline::new(&[ms.clone(), ps.clone()], Some(&GraphicsState::default()));
        assert!(made.is_none(), "refused, or not translated");
        let refused = refusals().unwrap_or_default();
        let early = refused.contains("mesh shader is a mesh program written for 64-lane waves")
            || refused.contains("mesh shader is a mesh program translated for 64-lane waves");
        if Needs::mesh_switches() == (false, false) {
            assert!(
                ms.translated.get().is_some(),
                "the mesh shader was translated"
            );
            assert!(!early, "{refused}");
        } else {
            assert!(
                ms.translated.get().is_none(),
                "the mesh shader was not translated"
            );
            assert!(ps.translated.get().is_none(), "nor the pixel shader");
            assert!(early, "{refused}");
        }
    }

    #[test]
    fn wave64_programs_are_refused_on_32_lane_hosts() {
        let wave64 = Needs::of(&container(48, (64, 64), CS_6_6)).unwrap();
        assert_eq!(
            wave64,
            Needs {
                lanes: (64, 64),
                model: (6, 6),
                wave64_mesh: None,
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

    /// What the wave64 port emits for a 32-lane host, `WaveSize(32)`, runs
    /// there; WARP's 4-lane waves run neither (VIctory #578's PSV0 check,
    /// which fb3db31's [`Needs`] already made).
    #[test]
    fn wave32_translations_run_on_32_lane_hosts() {
        let wave32 = Needs::of(&container(48, (32, 32), CS_6_6)).unwrap();
        assert_eq!(wave32.refusal((32, 32), (6, 8)), None);
        assert_eq!(wave32.refusal((32, 64), (6, 8)), None);
        assert_eq!(
            wave32.refusal((4, 4), (6, 8)).as_deref(),
            Some("needs 32-lane waves; the host has 4 to 4")
        );
        // An SM 6.8 range overlapping the host's runs.
        let range = Needs::of(&container(48, (32, 64), CS_6_6)).unwrap();
        assert_eq!(range.refusal((32, 32), (6, 8)), None);
    }

    /// A wave64 mesh translation declares no wave size. The translator
    /// makes it run in the waves of the target it is made for, so it is
    /// refused only where that target has 64-lane waves and the host none,
    /// or with `XTP_WAVE64_MESH=refuse` (VIctory #578's refusal); one
    /// reason only, and the same without a container that says more.
    #[test]
    fn wave64_mesh_programs_run_where_their_target_runs() {
        let any = Needs::of(&container(48, (0, u32::MAX), CS_6_6)).unwrap();
        let reason = |stage, wave64, switches| Needs::mesh_reason(stage, wave64, switches);
        assert_eq!(reason("mesh", true, (false, false)), None, "by default");
        assert_eq!(reason("mesh", true, (true, false)), Some(MESH_REFUSED));
        assert_eq!(reason("mesh", true, (true, true)), Some(MESH_REFUSED));
        assert_eq!(reason("mesh", true, (false, true)), Some(MESH_WAVE64));
        for switches in [(false, false), (true, false), (false, true)] {
            assert_eq!(reason("mesh", false, switches), None, "wave32");
            assert_eq!(reason("compute", true, switches), None, "compute");
        }
        for (what, text) in [
            (
                MESH_REFUSED,
                "is a mesh program written for 64-lane waves (XTP_WAVE64_MESH=refuse, as \
                 VIctory #578); the host has 32 to 32",
            ),
            (
                MESH_WAVE64,
                "is a mesh program translated for 64-lane waves (XTP_HOST_LANES); the host \
                 has 32 to 32",
            ),
        ] {
            let mesh = Needs {
                wave64_mesh: Some(what),
                ..any
            };
            assert_eq!(mesh.refusal((32, 32), (6, 8)).as_deref(), Some(text));
            assert!(
                mesh.refusal((4, 4), (6, 5))
                    .is_some_and(|r| r.ends_with("the host has 4 to 4")),
                "the lanes are the reason, not the shader model too"
            );
            assert_eq!(mesh.refusal((32, 64), (6, 8)), None);
            // A translation that declares its wave size is judged by it.
            let declared = Needs {
                lanes: (32, 32),
                ..mesh
            };
            assert_eq!(declared.refusal((32, 32), (6, 8)), None);
        }
        assert_eq!(any.refusal((32, 32), (6, 8)), None, "no mesh refusal");

        // A container without a PSV0 part does not let it through (#578's
        // check depended on one).
        let unsaid = Translated {
            host: xtp_shader::emit::HostShader {
                stage: xtp_shader::HwStage::Ms,
                dxil: b"not a container".to_vec(),
                user_data: Vec::new(),
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
            },
            used: Vec::new(),
            wave64: true,
        };
        let needs = Needs::translated_with("mesh", &unsaid, (true, false)).expect("the mesh check");
        let refused = needs.refusal((32, 32), (6, 8)).unwrap_or_default();
        assert!(refused.starts_with("is a mesh program"), "{refused}");
        assert_eq!(needs.refusal((32, 64), (6, 5)), None, "nor anything else");
        assert_eq!(
            Needs::translated_with("mesh", &unsaid, (false, false)),
            None
        );
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

    /// A target whose stand-in has an alpha the title's format lacks
    /// neither writes alpha nor blends with it; the others keep their
    /// blending, per target only where some target keeps its alpha
    /// (dark-world's test from the review of the #492 port, extended).
    #[test]
    fn targets_without_alpha_blend_with_alpha_one() {
        let target = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D12_BLEND_INV_DEST_ALPHA,
            DestBlend: D3D12_BLEND_DEST_ALPHA,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_SRC_ALPHA_SAT,
            DestBlendAlpha: D3D12_BLEND_SRC_ALPHA,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            RenderTargetWriteMask: 0xf,
            ..Default::default()
        };
        let shared = D3D12_BLEND_DESC {
            RenderTarget: [target; 8],
            ..Default::default()
        };
        let formats = |list: &[i32]| {
            let mut formats = [DXGI_FORMAT(0); 8];
            for (f, &title) in formats.iter_mut().zip(list) {
                f.0 = title;
            }
            D3D12_RT_FORMAT_ARRAY {
                RTFormats: formats,
                NumRenderTargets: list.len() as u32,
            }
        };
        let lacks = |f| f == 67 || f == 85;
        let text = |b: &D3D12_BLEND_DESC| format!("{b:?}");

        // Nothing to change for formats with their own alpha.
        let mut same = shared;
        blend_without(&mut same, &formats(&[28, 0]), lacks);
        assert_eq!(text(&same), text(&shared));

        // A target keeping its alpha beside one without: per target.
        let mut mixed = shared;
        blend_without(&mut mixed, &formats(&[28, 67]), lacks);
        assert!(mixed.IndependentBlendEnable.as_bool());
        let (kept, changed) = (mixed.RenderTarget[0], mixed.RenderTarget[1]);
        assert_eq!(
            text(&D3D12_BLEND_DESC {
                RenderTarget: [kept; 8],
                ..shared
            }),
            text(&shared)
        );
        assert_eq!(changed.SrcBlend, D3D12_BLEND_ZERO);
        assert_eq!(changed.DestBlend, D3D12_BLEND_ONE);
        assert_eq!(
            changed.SrcBlendAlpha, D3D12_BLEND_SRC_ALPHA_SAT,
            "not written"
        );
        assert_eq!(changed.RenderTargetWriteMask, 0x7);
        // The blend and the formats come in either order: twice is once.
        let mut twice = mixed;
        blend_without(&mut twice, &formats(&[28, 67]), lacks);
        assert_eq!(text(&twice), text(&mixed));

        // Every bound target without alpha: still one blending for all.
        let mut alone = shared;
        blend_without(&mut alone, &formats(&[67, 0, 85]), lacks);
        assert!(!alone.IndependentBlendEnable.as_bool());
        assert_eq!(alone.RenderTarget[0].SrcBlend, D3D12_BLEND_ZERO);
        assert_eq!(alone.RenderTarget[0].DestBlend, D3D12_BLEND_ONE);
        assert_eq!(alone.RenderTarget[0].RenderTargetWriteMask, 0x7);

        // A logic operation never becomes per target (D3D12 refuses it):
        // the alpha goes out of the writes only where every target lacks it.
        let logic = D3D12_BLEND_DESC {
            RenderTarget: [D3D12_RENDER_TARGET_BLEND_DESC {
                BlendEnable: false.into(),
                LogicOpEnable: true.into(),
                LogicOp: D3D12_LOGIC_OP_XOR,
                ..target
            }; 8],
            ..Default::default()
        };
        let mut kept = logic;
        blend_without(&mut kept, &formats(&[28, 85]), lacks);
        assert_eq!(text(&kept), text(&logic), "unchanged");
        let mut masked = logic;
        blend_without(&mut masked, &formats(&[85]), lacks);
        assert!(!masked.IndependentBlendEnable.as_bool());
        assert!(masked.RenderTarget[0].LogicOpEnable.as_bool());
        assert_eq!(masked.RenderTarget[0].RenderTargetWriteMask, 0x7);
    }

    /// The acceptance test of the wave64 wiring (VIctory #223), on the
    /// test's device (WARP on the person's PC, whose waves are 4 lanes):
    /// translations for a target forced to an NVIDIA host's 32 to 32 lanes
    /// (what `XTP_HOST_LANES=32-32` makes it), kept in a shader cache under
    /// that target's key, made into host pipelines as a title's are.
    /// - A console wave64 compute program whose lanes read only inside
    ///   their half declares `WaveSize(32)`, and is found again under its
    ///   own key, apart from its translation for any host (`WaveSize(64)`).
    ///   Where the device runs 32-lane waves its pipeline is made and gives
    ///   the console's result. Where it does not (WARP) it is refused for
    ///   its lanes before any driver call, and the driver refuses it too:
    ///   the early refusal drops nothing the device would run.
    /// - The same program without lane reads declares no wave size and runs
    ///   on every device, which also checks the dispatch read back here.
    #[test]
    fn wave64_compute_for_a_forced_32_lane_target() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use xtp_shader::emit::{HostShader, Target};
        let test = "pipeline::host::tests::wave64_compute_for_a_forced_32_lane_target";
        let Some(device) = crate::testgpu::device(test) else {
            return;
        };
        let lanes = crate::device::host_wave_lanes().expect("the device's lanes");
        let target = crate::device::target_of(Some(lanes), Some("32-32")).unwrap();
        assert_eq!(
            target,
            Target {
                lanes_min: 32,
                lanes_max: 32
            }
        );

        let dir = tempfile::tempdir().unwrap();
        let cache = xtp_pipeline::Cache::open(dir.path()).unwrap();
        let runs = AtomicU64::new(0);
        let _serial = crate::shader_cache::serial();
        let translate = |in_half: bool, target: Target| {
            let (c, native) = crate::shader_cache::wave64_program(in_half);
            let container: &[u8] = if in_half {
                b"wave64 reading in half"
            } else {
                b"wave64 reading its own lane"
            };
            crate::shader_cache::kept(&cache, container, target, |_| {
                runs.fetch_add(1, Ordering::Relaxed);
                xtp_shader::emit::translate_program_for(&c, &native, target)
                    .map_err(|(step, e)| format!("{}: {e}", step.name()))
            })
            .unwrap()
        };
        let halves = translate(true, target);
        assert_eq!(halves.wave_size(), Some(32), "two 32-lane waves");
        assert_eq!(translate(true, target), halves, "found again");
        assert_eq!(runs.load(Ordering::Relaxed), 1, "under its own key");
        let whole = translate(true, Target::ANY);
        assert_eq!(whole.wave_size(), Some(64), "for any host: one wave64");
        assert_eq!(runs.load(Ordering::Relaxed), 2, "under another key");
        let local = translate(false, target);
        assert_eq!(local.wave_size(), None);

        let pipeline = |host: HostShader, hash: &str| {
            let shader = Arc::new(Shader {
                stage: "compute",
                hash: hash.into(),
                container: Vec::new(),
                native: std::sync::OnceLock::new(),
                translated: std::sync::OnceLock::from(Some(Translated {
                    host,
                    used: Vec::new(),
                    wave64: true,
                })),
            });
            HostPipeline::new(&[shader], None)
        };
        // The driver check below tells a refusal from a description it takes.
        assert!(
            !driver_refuses(device, &local),
            "the driver takes a pipeline of no wave size"
        );
        let groups = 2;
        let made = pipeline(local, "forced-32-own-lane").expect("a pipeline of no wave size");
        let own: Vec<u32> = (0..groups).flat_map(|_| 0..64).collect();
        assert_eq!(run_wave64_program(device, &made, groups), own);

        let made = pipeline(halves.clone(), "forced-32-in-half");
        if lanes.0 <= 32 && 32 <= lanes.1 {
            let made = made.unwrap_or_else(|| panic!("a host of {lanes:?} lanes makes it"));
            let console: Vec<u32> = (0..groups).flat_map(|_| (0..64).map(|t| t ^ 31)).collect();
            assert_eq!(run_wave64_program(device, &made, groups), console);
        } else {
            assert!(made.is_none(), "{lanes:?}");
            let why = format!(
                "compute shader needs 32-lane waves; the host has {} to {}",
                lanes.0, lanes.1
            );
            let refused = refusals().unwrap_or_default();
            assert!(refused.contains(&why), "{refused}");
            assert!(
                driver_refuses(device, &halves),
                "the device makes WaveSize(32) pipelines though its waves are {lanes:?} lanes: \
                 Needs refuses them wrongly"
            );
        }
    }

    /// Runs `p`, a pipeline of [`crate::shader_cache::wave64_program`],
    /// over `groups` groups: the dwords its threads store.
    fn run_wave64_program(host: &crate::host::Host, p: &HostPipeline, groups: u32) -> Vec<u32> {
        use crate::testgpu::{TitleBuffer, TitleList};
        let words = 64 * groups;
        let bytes = 4 * u64::from(words);
        let dst = TitleBuffer::new(1 << 16);
        dst.set_words(|_| 0xdead_beef);
        let to = dst.shadow();
        // SAFETY: plain heap creation.
        let heap: ID3D12DescriptorHeap = unsafe {
            host.device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .expect("a view heap")
        };
        // The raw UAV at slot 1: the table's user data is its offset in the
        // heap at the console's 32 bytes per descriptor.
        let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: to.offset(dst.address()) / 4,
                    NumElements: words,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of the heap made above, a live buffer covering the
        // view.
        unsafe {
            let step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            let start = heap.GetCPUDescriptorHandleForHeapStart();
            host.device.CreateUnorderedAccessView(
                &to.buffer,
                None,
                Some(&uav),
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start.ptr + step as usize,
                },
            );
        }
        let (param, dwords) = p.stages[0].user_data.expect("the table's user data");
        let mut data = vec![0u32; dwords as usize];
        data[0] = 32;
        let list = TitleList::new();
        let l = list.host();
        to.refresh(l, dst.address(), bytes);
        to.gpu_writes(l, dst.address(), bytes);
        l.keep(&heap);
        l.keep(&p.root);
        l.keep(&p.pso);
        // SAFETY: an open list; live objects kept until it is reset.
        unsafe {
            l.list.SetDescriptorHeaps(&[Some(heap.clone())]);
            l.list.SetComputeRootSignature(&p.root);
            l.list.SetPipelineState(&p.pso);
            l.list
                .SetComputeRoot32BitConstants(param, dwords, data.as_ptr().cast(), 0);
            l.list.Dispatch(groups, 1, 1);
        }
        list.run();
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
        dst.words()[..words as usize].to_vec()
    }

    /// Whether the device's driver refuses a compute pipeline of `shader`,
    /// made without [`Needs`]' checks.
    fn driver_refuses(host: &crate::host::Host, shader: &xtp_shader::emit::HostShader) -> bool {
        let params = [D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: USER_DATA_SPACE,
                    Num32BitValues: shader.user_data_dwords().max(1),
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }];
        let (root, _) =
            root_signature(&host.device, &params, &[], true).expect("its root signature");
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: shader.dxil.as_ptr().cast(),
                BytecodeLength: shader.dxil.len(),
            },
            ..Default::default()
        };
        // SAFETY: the description points at live data.
        let made: windows::core::Result<ID3D12PipelineState> =
            unsafe { host.device.CreateComputePipelineState(&desc) };
        made.is_err()
    }
}
