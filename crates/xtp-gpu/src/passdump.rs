//! Diagnostics: a frame pass by pass, and a chosen render target on screen.
//!
//! **`XTP_PRESENT_TARGET=<selector>[*<scale>]`** shows one texture the
//! title renders to or writes at every present, instead of the title's
//! image, stretched over a 1920×1080 image of ours (the window's). The
//! selector is
//! - `<width>x<height>[:f<format>][:<n>][@<program>]`: the `n`-th texture
//!   (from 0) of that size, and host format if given, that the frame writes
//!   (as a render target, depth or through unordered access), in
//!   first-write order, through an op of that program (16 hex digits) if
//!   given; once shown, the same texture stays while frames write it. The
//!   formats are the host's, as `XTP_DUMP_TARGETS`' file names give them:
//!   `1280x720:f62` is the world's R8_UINT street of `frame-1009-282`,
//!   `1280x720:f28@f0c91b4d2578f5bf` the RGBA8 target the material resolve
//!   writes;
//! - `0x<address>`: the title resource at that address (addresses change
//!   from run to run).
//!
//! Values are shown as they are (normalized and float formats; one channel
//! as grey, two as red and green), integers divided by 255, all multiplied
//! by `scale` (1 by default) and clamped. A frame that writes no match shows
//! the last texture shown again; before any, the title's image is shown.
//!
//! **`XTP_PASS_DUMP=<width>`**, with `XTP_CAPTURE=<folder>` and frames
//! listed by `XTP_CAPTURE_FRAMES` or `XTP_CAPTURE_WORLD`, writes
//! `passes-<n>.txt` into the folder at the present of each listed frame:
//! its passes in submission order, each with its ops (their text as
//! `HostList::note_op` gives it: programs, user data, views) and draws or
//! dispatches dropped, and what its outputs hold after it, sampled on the
//! GPU in list order. A pass is a run of ops on one list writing the same
//! outputs: render targets and depth for draws, unordered-access textures
//! for draws and dispatches, the output of a UAV clear or of the
//! FidelityFX stand-in's upscale. `<width>` leaves narrower outputs
//! unsampled (0 or empty: every one). Each sample is [`ROWS`] rows across
//! the texture (a depth texture is copied whole, as D3D12 asks), read back
//! at the present; the read-back memory is [`BUDGET`] bytes a frame.
//!
//! shortcut: a dropped draw or dispatch is put in the list the thread last
//! recorded an op, a binding or targets for (`dropped` is not told its
//! list); the copies, resolves and buffer writes of a list are listed in
//! its passes but their outputs are not sampled.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SHADER_MACRO, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

use crate::host::{Host, HostList};
use crate::object::This;
use crate::resource::HostTexture;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `XTP_PRESENT_TARGET` picks.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Pick {
    /// The title resource at this address.
    Address(usize),
    /// The `n`-th texture of this size (and host format) the frame writes
    /// (through an op of this program, if given).
    Written {
        width: u32,
        height: u32,
        format: Option<u32>,
        n: usize,
        program: Option<u64>,
    },
}

/// `XTP_PRESENT_TARGET`: what to show and the factor its values take.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Selector {
    pick: Pick,
    scale: f32,
}

fn parse(text: &str) -> Option<Selector> {
    let text = text.trim();
    let (pick, scale) = match text.split_once('*') {
        Some((pick, scale)) => (
            pick.trim(),
            scale.trim().parse::<f32>().ok().filter(|s| s.is_finite())?,
        ),
        None => (text, 1.0),
    };
    let pick = match pick.strip_prefix("0x").or_else(|| pick.strip_prefix("0X")) {
        Some(hex) => Pick::Address(usize::from_str_radix(hex, 16).ok().filter(|&a| a != 0)?),
        None => {
            let (pick, program) = match pick.split_once('@') {
                Some((pick, hash)) => (
                    pick,
                    Some(
                        u64::from_str_radix(hash.trim(), 16)
                            .ok()
                            .filter(|_| hash.trim().len() == 16)?,
                    ),
                ),
                None => (pick, None),
            };
            let mut fields = pick.split(':');
            let (w, h) = fields.next()?.split_once('x')?;
            let (width, height) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
            let (mut format, mut n) = (None, 0);
            for field in fields {
                match field.trim().strip_prefix('f') {
                    Some(f) => format = Some(f.parse().ok()?),
                    None => n = field.trim().parse().ok()?,
                }
            }
            Pick::Written {
                width,
                height,
                format,
                n,
                program,
            }
        }
    };
    Some(Selector { pick, scale })
}

fn selector() -> Option<Selector> {
    static SELECTOR: OnceLock<Option<Selector>> = OnceLock::new();
    *SELECTOR.get_or_init(|| {
        let text = std::env::var("XTP_PRESENT_TARGET").ok()?;
        let selector = parse(&text);
        match selector {
            Some(s) => tracing::info!(?s, "XTP_PRESENT_TARGET: a target is shown instead of the title's image"),
            None => tracing::warn!(
                text,
                "XTP_PRESENT_TARGET is not <width>x<height>[:f<format>][:<n>][@<program>] or 0x<address>, with an optional *<scale>"
            ),
        }
        selector
    })
}

/// The textures the frame wrote so far, and those matching the selector,
/// in first-write order.
#[derive(Default)]
struct Written {
    seen: HashSet<usize>,
    matches: Vec<Arc<HostTexture>>,
}

static WRITTEN: Mutex<Option<Written>> = Mutex::new(None);
/// The texture shown last, shown again by frames that write no match.
static SHOWN: Mutex<Option<Arc<HostTexture>>> = Mutex::new(None);

/// The program `XTP_PRESENT_TARGET`'s texture must be written by, if any.
fn pick_program() -> Option<u64> {
    match selector()?.pick {
        Pick::Written { program, .. } => program,
        Pick::Address(_) => None,
    }
}

/// Per list, for a pick by program: the render targets and depth set last,
/// and the unordered-access textures of the op being prepared.
static OUTPUTS: Mutex<Option<HashMap<usize, ListOutputs>>> = Mutex::new(None);

/// A list's render targets and depth, and its op's unordered-access
/// textures.
type ListOutputs = (Vec<usize>, Vec<usize>);

/// A texture the frame writes through `targets` or `written`: noted for
/// `XTP_PRESENT_TARGET`, at once or (a pick by program) when the op that
/// writes it turns out to be the program's.
fn note_output(list: &HostList, resource: This, target: bool) {
    if pick_program().is_none() {
        note_written(resource);
        return;
    }
    let mut outputs = lock(&OUTPUTS);
    let (targets, pending) = outputs
        .get_or_insert_with(HashMap::new)
        .entry(key(list))
        .or_default();
    let to = if target { targets } else { pending };
    if !to.contains(&(resource as usize)) {
        to.push(resource as usize);
    }
}

/// An op of `list` described by `text`, for a pick by program: its outputs
/// are noted if it runs the program.
fn note_op_outputs(list: &HostList, text: &str, program: u64) {
    let mut outputs = lock(&OUTPUTS);
    let (targets, pending) = outputs
        .get_or_insert_with(HashMap::new)
        .entry(key(list))
        .or_default();
    let uavs = std::mem::take(pending);
    if !programs(text)
        .iter()
        .any(|h| u64::from_str_radix(h, 16) == Ok(program))
    {
        return;
    }
    let written: Vec<usize> = match kind_of(text) {
        Kind::Graphics => targets.iter().chain(&uavs).copied().collect(),
        _ => uavs,
    };
    drop(outputs);
    for r in written {
        note_written(r as This);
    }
}

/// Notes a texture the frame writes, for `XTP_PRESENT_TARGET`.
fn note_written(resource: This) {
    let Some(Selector {
        pick:
            Pick::Written {
                width,
                height,
                format,
                ..
            },
        ..
    }) = selector()
    else {
        return;
    };
    let mut written = lock(&WRITTEN);
    let written = written.get_or_insert_with(Written::default);
    if !written.seen.insert(resource as usize) {
        return;
    }
    let Some(copy) = crate::resource::host_copy(resource) else {
        return;
    };
    // SAFETY: a live resource.
    let d = unsafe { copy.texture.GetDesc() };
    if d.Width == u64::from(width)
        && d.Height == height
        && format.is_none_or(|f| f == d.Format.0 as u32)
    {
        written.matches.push(copy);
    }
}

/// The texture `selector` picks at this present.
fn picked(selector: Selector) -> Option<Arc<HostTexture>> {
    let picked = match selector.pick {
        Pick::Address(address) => {
            let r = address as This;
            crate::resource::is_resource(r)
                .then(|| crate::resource::host_copy(r))
                .flatten()
        }
        Pick::Written { n, .. } => {
            let mut written = lock(&WRITTEN);
            let written = written.get_or_insert_with(Written::default);
            written.seen.clear();
            let matches = std::mem::take(&mut written.matches);
            // The texture shown stays while the frames write it: the order
            // of first writes changes from frame to frame.
            let shown = lock(&SHOWN);
            match shown
                .as_ref()
                .and_then(|s| matches.iter().find(|m| Arc::ptr_eq(s, m)))
            {
                Some(same) => Some(same.clone()),
                None => matches.into_iter().nth(n),
            }
        }
    };
    let mut shown = lock(&SHOWN);
    if let Some(p) = picked {
        if !shown.as_ref().is_some_and(|s| Arc::ptr_eq(s, &p)) {
            // SAFETY: a live resource.
            let d = unsafe { p.texture.GetDesc() };
            tracing::info!(
                width = d.Width,
                height = d.Height,
                format = d.Format.0,
                flags = format_args!("{:#x}", d.Flags.0),
                "XTP_PRESENT_TARGET: showing a texture"
            );
        }
        *shown = Some(p);
    }
    shown.clone()
}

/// The present of frame `frame` (`queue::present`): writes the frame's
/// pass dump if one was recorded, then shows the texture
/// `XTP_PRESENT_TARGET` picks. True if it was shown (the title's planes are
/// then not).
pub(crate) fn present(frame: u64) -> bool {
    report(frame);
    let Some(selector) = selector() else {
        return false;
    };
    let (Some(texture), Some(host)) = (picked(selector), crate::host::host()) else {
        return false;
    };
    let mut show = lock(&SHOW);
    if show.is_none() {
        match Show::new(host, OUT_WIDTH, OUT_HEIGHT) {
            Ok(s) => *show = Some(s),
            Err(e) => {
                static SAID: std::sync::Once = std::sync::Once::new();
                SAID.call_once(
                    || tracing::warn!(error = %e, "XTP_PRESENT_TARGET: no pass to show targets"),
                );
                return false;
            }
        }
    }
    let Some(image) = show
        .as_mut()
        .and_then(|s| s.draw(host, &texture, selector.scale))
    else {
        return false;
    };
    const RENDER_TARGET: u32 = 0x4;
    host.present(&image, RENDER_TARGET, frame).is_some()
}

/// The size of the image a shown target is stretched over.
const OUT_WIDTH: u32 = 1920;
const OUT_HEIGHT: u32 = 1080;

static SHOW: Mutex<Option<Show>> = Mutex::new(None);

/// How a host texture format is shown: the view format, the kind of its
/// values (0 float or normalized, 1 unsigned integer, 2 signed integer) and
/// its channels. A typeless format is viewed as its float, normalized or
/// depth member; a typed depth format cannot be viewed (`None`).
fn view_of(format: u32) -> Option<(u32, u32, u32)> {
    const F: u32 = 0;
    const U: u32 = 1;
    const S: u32 = 2;
    Some(match format {
        1 | 2 => (2, F, 4),
        3 => (3, U, 4),
        4 => (4, S, 4),
        5 | 6 => (6, F, 3),
        7 => (7, U, 3),
        8 => (8, S, 3),
        9 | 10 => (10, F, 4),
        11 | 13 => (format, F, 4),
        12 => (12, U, 4),
        14 => (14, S, 4),
        15 | 16 => (16, F, 2),
        17 => (17, U, 2),
        18 => (18, S, 2),
        // R32G8X24_TYPELESS: its depth plane.
        19 | 21 => (21, F, 1),
        23 | 24 => (24, F, 4),
        25 => (25, U, 4),
        26 => (26, F, 3),
        27 | 28 => (28, F, 4),
        29 | 31 => (format, F, 4),
        30 => (30, U, 4),
        32 => (32, S, 4),
        33 | 34 => (34, F, 2),
        35 | 37 => (format, F, 2),
        36 => (36, U, 2),
        38 => (38, S, 2),
        39 | 41 => (41, F, 1),
        42 => (42, U, 1),
        43 => (43, S, 1),
        // R24G8_TYPELESS: its depth plane.
        44 | 46 => (46, F, 1),
        48 | 49 => (49, F, 2),
        51 => (51, F, 2),
        50 => (50, U, 2),
        52 => (52, S, 2),
        // R16_TYPELESS: a 16-bit depth's.
        53 | 56 => (56, F, 1),
        54 | 58 => (format, F, 1),
        57 => (57, U, 1),
        59 => (59, S, 1),
        60 | 61 => (61, F, 1),
        63 => (63, F, 1),
        62 => (62, U, 1),
        64 => (64, S, 1),
        67 => (67, F, 3),
        85 => (85, F, 3),
        86 | 89 | 115 => (format, F, 4),
        87 | 90 => (87, F, 4),
        88 | 92 => (88, F, 4),
        91 | 93 => (format, F, 4),
        _ => return None,
    })
}

const SHOW_SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0, visibility = SHADER_VISIBILITY_PIXEL), " \
    "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
cbuffer Show : register(b0) {
    float scale;
    uint channels;
    uint width;
    uint height;
};
#if KIND == 1
Texture2D<uint4> source : register(t0);
#elif KIND == 2
Texture2D<int4> source : register(t0);
#else
Texture2D<float4> source : register(t0);
#endif
[RootSignature(RS)]
void vs(uint id : SV_VertexID, out float4 position : SV_Position, out float2 uv : TEXCOORD0) {
    uv = float2((id << 1) & 2, id & 2);
    position = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
[RootSignature(RS)]
float4 ps(float4 position : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    uint2 at = min(uint2(uv * float2(width, height)), uint2(width, height) - 1);
    float4 v = float4(source.Load(int3(at, 0)));
#if KIND != 0
    v /= 255.0;
#endif
    float3 rgb = channels == 1 ? v.rrr : (channels == 2 ? float3(v.rg, 0) : v.rgb);
    return float4(saturate(rgb * scale), 1);
}
"#;

fn compile(entry: PCSTR, target: PCSTR, kind: PCSTR) -> windows::core::Result<ID3DBlob> {
    let defines = [
        D3D_SHADER_MACRO {
            Name: s!("KIND"),
            Definition: kind,
        },
        D3D_SHADER_MACRO {
            Name: PCSTR::null(),
            Definition: PCSTR::null(),
        },
    ];
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source, defines and names live through the call; out
    // pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SHOW_SOURCE.as_ptr().cast(),
            SHOW_SOURCE.len(),
            s!("passdump"),
            Some(defines.as_ptr()),
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!(
                "XTP_PRESENT_TARGET's pass: {}",
                String::from_utf8_lossy(text)
            );
        }
        return Err(e);
    }
    code.ok_or_else(windows::core::Error::empty)
}

/// A blob's bytes.
fn bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob's own bytes, alive as long as it is.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

/// What shows a target: a full-screen triangle reading it, one pipeline per
/// kind of value, and the image it draws into.
struct Show {
    root: ID3D12RootSignature,
    /// Float, unsigned and signed integer sources.
    pipelines: [ID3D12PipelineState; 3],
    view: ID3D12DescriptorHeap,
    target_view: ID3D12DescriptorHeap,
    /// In the render target state between uses.
    image: ID3D12Resource,
    width: u32,
    height: u32,
    list: HostList,
}

// SAFETY: D3D12 objects are free-threaded; a `Show` is used under its lock.
unsafe impl Send for Show {}

impl Show {
    fn new(host: &Host, width: u32, height: u32) -> windows::core::Result<Show> {
        let device = &host.device;
        let vs = compile(s!("vs"), s!("vs_5_1"), s!("0"))?;
        let ps = [s!("0"), s!("1"), s!("2")].map(|kind| compile(s!("ps"), s!("ps_5_1"), kind));
        let [Ok(pf), Ok(pu), Ok(pi)] = ps else {
            return Err(windows::core::Error::empty());
        };
        // SAFETY: plain object creation; the root signature is in the shader.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes(&vs))?;
            let pipeline = |ps: &ID3DBlob| -> windows::core::Result<ID3D12PipelineState> {
                let mut blend = D3D12_BLEND_DESC::default();
                blend.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
                let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
                formats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
                let (vs, ps) = (bytes(&vs), bytes(ps));
                let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    VS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: vs.as_ptr().cast(),
                        BytecodeLength: vs.len(),
                    },
                    PS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: ps.as_ptr().cast(),
                        BytecodeLength: ps.len(),
                    },
                    BlendState: blend,
                    SampleMask: u32::MAX,
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        ..Default::default()
                    },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    RTVFormats: formats,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                };
                device.CreateGraphicsPipelineState(&desc)
            };
            let pipelines = [pipeline(&pf)?, pipeline(&pu)?, pipeline(&pi)?];
            let view: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })?;
            let target_view: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })?;
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(width),
                Height: height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let mut image: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                None,
                &mut image,
            )?;
            let image = image.ok_or_else(windows::core::Error::empty)?;
            crate::host::name(&image, || format!("XTP_PRESENT_TARGET {width}x{height}"));
            device.CreateRenderTargetView(
                &image,
                None,
                target_view.GetCPUDescriptorHandleForHeapStart(),
            );
            let list = host.new_list()?;
            list.close();
            Ok(Show {
                root,
                pipelines,
                view,
                target_view,
                image,
                width,
                height,
                list,
            })
        }
    }

    /// Draws `texture` (subresource 0) over the image and runs it; the
    /// image, in the render target state. `None` for a texture that cannot
    /// be shown (logged once per format and reason).
    fn draw(&mut self, host: &Host, texture: &HostTexture, scale: f32) -> Option<ID3D12Resource> {
        let t = &texture.texture;
        // SAFETY: a live resource.
        let d = unsafe { t.GetDesc() };
        let refused = if d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
            Some("not a 2D texture")
        } else if d.SampleDesc.Count > 1 {
            Some("multisampled")
        } else if d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 != 0 {
            Some("no shader access")
        } else {
            None
        };
        let view = view_of(d.Format.0 as u32);
        let (Some((format, kind, channels)), None) = (view, refused) else {
            static SAID: Mutex<Vec<(i32, &str)>> = Mutex::new(Vec::new());
            let why = refused.unwrap_or("format");
            let mut said = lock(&SAID);
            if !said.contains(&(d.Format.0, why)) {
                said.push((d.Format.0, why));
                tracing::warn!(
                    format = d.Format.0,
                    why,
                    "XTP_PRESENT_TARGET: a texture not shown"
                );
            }
            return None;
        };
        let list = &self.list;
        list.reset();
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        // Every submission before the present has been made: the state its
        // lists left the texture in.
        let state = crate::host::legal_state(t, texture.state.load(Ordering::Relaxed));
        let wanted = crate::host::legal_state(t, PIXEL_SHADER_RESOURCE);
        let states = |a: u32| D3D12_RESOURCE_STATES(a as i32);
        let constants = [scale.to_bits(), channels, d.Width as u32, d.Height];
        // SAFETY: our open list, heaps and image; a live texture.
        unsafe {
            let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(format as i32),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };
            host.device.CreateShaderResourceView(
                t,
                Some(&srv),
                self.view.GetCPUDescriptorHandleForHeapStart(),
            );
            let l = &list.list;
            if state != wanted {
                l.ResourceBarrier(&[crate::host::transition(t, states(state), states(wanted))]);
            }
            let target = self.target_view.GetCPUDescriptorHandleForHeapStart();
            l.OMSetRenderTargets(1, Some(&target), false, None);
            l.SetDescriptorHeaps(&[Some(self.view.clone())]);
            l.SetGraphicsRootSignature(&self.root);
            l.SetGraphicsRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
            l.SetGraphicsRootDescriptorTable(1, self.view.GetGPUDescriptorHandleForHeapStart());
            l.RSSetViewports(&[D3D12_VIEWPORT {
                Width: self.width as f32,
                Height: self.height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            l.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            }]);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            l.SetPipelineState(&self.pipelines[kind as usize]);
            l.DrawInstanced(3, 1, 0, 0);
            if state != wanted {
                l.ResourceBarrier(&[crate::host::transition(t, states(wanted), states(state))]);
            }
        }
        list.keep(t);
        list.close();
        host.execute(&[list]);
        Some(self.image.clone())
    }
}

// The pass dump.

/// Rows of each output sampled after a pass.
const ROWS: u32 = 24;
/// Bytes of read-back memory a frame's samples may take.
const BUDGET: u64 = 1 << 30;
/// Buffers this small that an op reads or writes are sampled before it.
const SMALL: u64 = 64;

/// Whether `XTP_PASS_DUMP` (and a capture folder) is set.
fn dump_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::capture::folder().is_some() && std::env::var_os("XTP_PASS_DUMP").is_some()
    })
}

/// The narrowest output sampled (`XTP_PASS_DUMP=<width>`).
fn dump_width() -> u64 {
    static WIDTH: OnceLock<u64> = OnceLock::new();
    *WIDTH.get_or_init(|| {
        std::env::var("XTP_PASS_DUMP")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    })
}

/// Whether the frame being built is one the pass dump records (asked of
/// `capture::listed` once a frame).
fn recording() -> bool {
    if !dump_on() {
        return false;
    }
    static ASKED: AtomicU64 = AtomicU64::new(0);
    let frame = crate::queue::presented() + 1;
    let asked = ASKED.load(Ordering::Relaxed);
    if asked >> 1 == frame {
        return asked & 1 == 1;
    }
    let listed = crate::capture::listed(frame);
    ASKED.store(frame << 1 | u64::from(listed), Ordering::Relaxed);
    listed
}

/// How a pass writes an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    Target,
    Depth,
    Unordered,
}

impl Out {
    fn name(self) -> &'static str {
        match self {
            Out::Target => "rt",
            Out::Depth => "ds",
            Out::Unordered => "uav",
        }
    }
}

/// Where an output's sampled rows are in the read-back buffer.
#[derive(Debug, Clone)]
struct Sample {
    width: u32,
    /// The format of the rows (a depth texture's first plane: its
    /// footprint's).
    format: u32,
    /// Each sampled row: its y and its offset.
    rows: Vec<(u32, u64)>,
}

/// A texture's width, height and host format, if it has a host copy.
type Desc = Option<(u32, u32, u32)>;

/// An output after its pass: the resource, how it was written, its size
/// and host format, and its sample (or why it has none).
#[derive(Debug)]
struct After {
    resource: usize,
    how: Out,
    desc: Desc,
    sample: Result<Sample, &'static str>,
}

/// A run of ops on one list writing the same outputs.
#[derive(Debug, Default)]
struct Pass {
    outputs: Vec<(usize, Out)>,
    ops: Vec<Op>,
    /// The outputs sampled after it (empty while it goes on).
    after: Vec<After>,
}

/// An op: whether it ran, its text, and the small buffers it names
/// (address, bytes, and where their sample is or why there is none).
#[derive(Debug)]
struct Op {
    ran: bool,
    text: String,
    buffers: Vec<(u64, u64, Result<u64, &'static str>)>,
}

/// What one list recorded.
#[derive(Debug, Default)]
struct Recording {
    passes: Vec<Pass>,
    /// The render targets and depth set last.
    targets: Vec<usize>,
    depth: usize,
    /// The unordered-access textures of the op being prepared.
    pending: Vec<usize>,
}

/// The read-back buffer samples go to, from offset 0 for each frame.
struct Readback {
    budget: u64,
    buffer: Option<ID3D12Resource>,
    used: u64,
    failed: bool,
}

impl Readback {
    fn reserve(
        &mut self,
        device: &ID3D12Device,
        bytes: u64,
        align: u64,
    ) -> Result<(ID3D12Resource, u64), &'static str> {
        if self.buffer.is_none() && !self.failed {
            match readback_buffer(device, self.budget) {
                Ok(b) => self.buffer = Some(b),
                Err(e) => {
                    tracing::warn!(error = %e, bytes = self.budget, "XTP_PASS_DUMP: no read-back buffer");
                    self.failed = true;
                }
            }
        }
        let buffer = self.buffer.clone().ok_or("no read-back buffer")?;
        let at = self.used.next_multiple_of(align);
        if at + bytes > self.budget {
            return Err("read-back budget spent");
        }
        self.used = at + bytes;
        Ok((buffer, at))
    }
}

fn readback_buffer(device: &ID3D12Device, size: u64) -> windows::core::Result<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
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
    // SAFETY: plain creation; the out pointer is ours.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut buffer,
        )?;
    }
    let buffer = buffer.ok_or_else(windows::core::Error::empty)?;
    crate::host::name(&buffer, || "XTP_PASS_DUMP samples".to_owned());
    Ok(buffer)
}

/// The recordings of lists being recorded, closed and submitted.
struct Dump {
    open: HashMap<usize, Recording>,
    closed: HashMap<usize, Recording>,
    submitted: Vec<Recording>,
    readback: Readback,
    /// The narrowest output sampled.
    width: u64,
}

impl Dump {
    fn new(budget: u64, width: u64) -> Dump {
        Dump {
            open: HashMap::new(),
            closed: HashMap::new(),
            submitted: Vec::new(),
            readback: Readback {
                budget,
                buffer: None,
                used: 0,
                failed: false,
            },
            width,
        }
    }

    fn targets(&mut self, list: &HostList, targets: &[usize], depth: usize) {
        let r = self.open.entry(key(list)).or_default();
        r.targets = targets.to_vec();
        r.depth = depth;
    }

    fn written(&mut self, list: &HostList, resource: usize) {
        let r = self.open.entry(key(list)).or_default();
        if !r.pending.contains(&resource) {
            r.pending.push(resource);
        }
    }

    /// An op about to be recorded in `list`: a new pass if it writes other
    /// outputs than the pass before, whose outputs are sampled first.
    fn op(&mut self, device: &ID3D12Device, list: &HostList, text: &str) {
        let Dump {
            open,
            readback,
            width,
            ..
        } = self;
        let r = open.entry(key(list)).or_default();
        let uavs = std::mem::take(&mut r.pending);
        let unordered = uavs.iter().map(|&u| (u, Out::Unordered));
        let outputs: Option<Vec<(usize, Out)>> = match kind_of(text) {
            Kind::Compute => Some(unordered.collect()),
            Kind::Graphics => Some(
                r.targets
                    .iter()
                    .map(|&t| (t, Out::Target))
                    .chain((r.depth != 0).then_some((r.depth, Out::Depth)))
                    .chain(unordered)
                    .collect(),
            ),
            Kind::Writes(resource) => Some(vec![(resource, Out::Unordered)]),
            Kind::Other => None,
        };
        let same = |p: &Pass, o: &[(usize, Out)]| {
            p.outputs.len() == o.len() && o.iter().all(|x| p.outputs.contains(x))
        };
        match outputs {
            Some(outputs) if !r.passes.last().is_some_and(|p| same(p, &outputs)) => {
                if let Some(p) = r.passes.last_mut() {
                    sample_pass(readback, device, list, p, *width);
                }
                r.passes.push(Pass {
                    outputs,
                    ..Pass::default()
                });
            }
            _ if r.passes.is_empty() => r.passes.push(Pass::default()),
            _ => {}
        }
        let buffers = small_buffers(text)
            .into_iter()
            .map(|(address, len)| {
                (
                    address,
                    len,
                    sample_buffer(readback, device, list, address, len),
                )
            })
            .collect();
        if let Some(p) = r.passes.last_mut() {
            p.ops.push(Op {
                ran: true,
                text: text.to_owned(),
                buffers,
            });
        }
    }

    /// A draw or dispatch of `list` that did not run.
    fn dropped(&mut self, list: usize, text: String) {
        let Some(r) = self.open.get_mut(&list) else {
            return;
        };
        r.pending.clear();
        if r.passes.is_empty() {
            r.passes.push(Pass::default());
        }
        if let Some(p) = r.passes.last_mut() {
            p.ops.push(Op {
                ran: false,
                text,
                buffers: Vec::new(),
            });
        }
    }

    /// `list` is about to close: its last pass's outputs are sampled.
    fn closed(&mut self, device: &ID3D12Device, list: &HostList) {
        let Some(mut r) = self.open.remove(&key(list)) else {
            return;
        };
        if let Some(p) = r.passes.last_mut()
            && p.after.is_empty()
        {
            sample_pass(&mut self.readback, device, list, p, self.width);
        }
        self.closed.insert(key(list), r);
    }

    /// Forgets what lists recorded and did not submit, and every sample.
    fn forget(&mut self) {
        let left = self.open.len() + self.closed.len();
        if left > 0 {
            tracing::info!(
                lists = left,
                "XTP_PASS_DUMP: lists recorded and not submitted, forgotten"
            );
        }
        self.open.clear();
        self.closed.clear();
        self.submitted.clear();
        self.readback.used = 0;
    }

    fn submitted(&mut self, runs: &[&HostList]) {
        for run in runs {
            if let Some(r) = self.closed.remove(&key(run)) {
                self.submitted.push(r);
            }
        }
    }

    /// The report of the lists submitted so far (every submission has
    /// finished), as `passes-<frame>.txt` holds it; `None` if there is none.
    /// The read-back memory starts again from its start when no list holds
    /// samples still.
    fn report(&mut self, frame: u64) -> Option<String> {
        if self.submitted.is_empty() {
            return None;
        }
        let lists = std::mem::take(&mut self.submitted);
        let used = self.readback.used;
        let text = match &self.readback.buffer {
            Some(buffer) if used > 0 => {
                let mut data = std::ptr::null_mut();
                let range = D3D12_RANGE {
                    Begin: 0,
                    End: used as usize,
                };
                // SAFETY: a read-back buffer; the copies into it finished.
                if unsafe { buffer.Map(0, Some(&range), Some(&mut data)) }.is_ok() {
                    // SAFETY: the mapping spans `used` bytes.
                    let bytes =
                        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), used as usize) };
                    let text = report_text(frame, &lists, bytes);
                    // SAFETY: mapped above; nothing written.
                    unsafe { buffer.Unmap(0, Some(&D3D12_RANGE::default())) };
                    text
                } else {
                    report_text(frame, &lists, &[])
                }
            }
            _ => report_text(frame, &lists, &[]),
        };
        if self.open.is_empty() && self.closed.is_empty() {
            self.readback.used = 0;
        }
        Some(text)
    }
}

/// A list's key: its address (a list is recorded on one thread at a time).
fn key(list: &HostList) -> usize {
    std::ptr::from_ref(list) as usize
}

/// What an op writes, from its text.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// A dispatch: its unordered-access textures.
    Compute,
    /// A draw or mesh dispatch: the render targets, depth and
    /// unordered-access textures.
    Graphics,
    /// A UAV clear or the FidelityFX stand-in's upscale: this texture.
    Writes(usize),
    /// Copies, resolves, buffer writes: not sampled.
    Other,
}

fn kind_of(text: &str) -> Kind {
    let hex = |after: &str| {
        let at = text.find(after)? + after.len();
        let digits: String = text[at..]
            .strip_prefix("0x")?
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        usize::from_str_radix(&digits, 16).ok().filter(|&a| a != 0)
    };
    if text.starts_with("dispatch ") || text.starts_with("indirect kind 2 ") {
        Kind::Compute
    } else if [
        "draw ",
        "indexed ",
        "mesh ",
        "indirect kind ",
        "indirect rectangles ",
    ]
    .iter()
    .any(|p| text.starts_with(p))
    {
        Kind::Graphics
    } else if text.starts_with("clear UAV ")
        && let Some(r) = hex(" of ")
    {
        Kind::Writes(r)
    } else if text.starts_with("FidelityFX ")
        && let Some(r) = hex("output: ")
    {
        Kind::Writes(r)
    } else {
        Kind::Other
    }
}

/// The buffers of at most [`SMALL`] bytes an op's text names
/// (`buffer Some((<address>, <bytes>))`), once each.
fn small_buffers(text: &str) -> Vec<(u64, u64)> {
    let mut found = Vec::new();
    for part in text.split("buffer Some((").skip(1) {
        let Some((address, rest)) = part.split_once(", ") else {
            continue;
        };
        let len: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let (Ok(address), Ok(len)) = (address.parse::<u64>(), len.parse::<u64>())
            && (1..=SMALL).contains(&len)
            && !found.contains(&(address, len))
        {
            found.push((address, len));
        }
    }
    found
}

/// Records a copy of a title buffer's host copy, `len` bytes at `address`,
/// into the read-back buffer, in `list` where it stands: where it is.
fn sample_buffer(
    readback: &mut Readback,
    device: &ID3D12Device,
    list: &HostList,
    address: u64,
    len: u64,
) -> Result<u64, &'static str> {
    let shadow = crate::shadow::find(address, len).ok_or("no host copy")?;
    let (buffer, at) = readback.reserve(device, len, 16)?;
    let (common, source) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    // SAFETY: an open list; the host copy (which the list keeps) and our
    // read-back buffer, the ranges inside both. Host copies are taken as
    // common, as `shadow` takes them.
    unsafe {
        list.list.ResourceBarrier(&[
            crate::host::uav_barrier(),
            crate::host::transition(&shadow.buffer, common, source),
        ]);
        list.list
            .CopyBufferRegion(&buffer, at, &shadow.buffer, shadow.offset(address), len);
        list.list
            .ResourceBarrier(&[crate::host::transition(&shadow.buffer, source, common)]);
    }
    list.keep(&shadow.buffer);
    Ok(at)
}

/// Samples the outputs of `pass`, in `list` after its ops.
fn sample_pass(
    readback: &mut Readback,
    device: &ID3D12Device,
    list: &HostList,
    pass: &mut Pass,
    width: u64,
) {
    for &(resource, how) in &pass.outputs {
        let (desc, sample) = sample(readback, device, list, resource as This, width);
        pass.after.push(After {
            resource,
            how,
            desc,
            sample,
        });
    }
}

/// The rows sampled of a texture `height` high.
fn rows_of(height: u32) -> Vec<u32> {
    let n = ROWS.min(height);
    (0..n).map(|i| ((2 * i + 1) * height) / (2 * n)).collect()
}

/// Records copies of rows of `resource`'s host copy (subresource 0) into
/// the read-back buffer, in `list` where it stands: its size and format,
/// and the sample or why there is none.
fn sample(
    readback: &mut Readback,
    device: &ID3D12Device,
    list: &HostList,
    resource: This,
    narrowest: u64,
) -> (Desc, Result<Sample, &'static str>) {
    let Some(copy) = crate::resource::host_copy(resource) else {
        return (None, Err("no host copy"));
    };
    let texture = &copy.texture;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let desc = Some((d.Width as u32, d.Height, d.Format.0 as u32));
    if d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
        return (desc, Err("not a 2D texture"));
    }
    if d.SampleDesc.Count > 1 {
        return (desc, Err("multisampled"));
    }
    if d.Width < narrowest {
        return (desc, Err("narrower than XTP_PASS_DUMP asks"));
    }
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    // SAFETY: a live texture's description; outputs for one subresource.
    unsafe {
        device.GetCopyableFootprints(
            &d,
            0,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        );
    }
    let f = footprint.Footprint;
    if bytes_per_texel(f.Format.0 as u32).is_none() {
        return (desc, Err("format not sampled"));
    }
    // D3D12 copies no part of a depth texture: it is copied whole.
    let whole = d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0;
    let pitch = u64::from(f.RowPitch);
    let ys = rows_of(d.Height);
    let row_bytes = pitch.next_multiple_of(512);
    let bytes = if whole {
        total
    } else {
        ys.len() as u64 * row_bytes
    };
    let (buffer, at) = match readback.reserve(device, bytes, 512) {
        Ok(r) => r,
        Err(why) => return (desc, Err(why)),
    };
    const COPY_SOURCE: u32 = 0x800;
    let before = list.transition(resource, COPY_SOURCE);
    let mut rows = Vec::with_capacity(ys.len());
    // SAFETY: an open list; a live texture (the list holds its host copy)
    // and our read-back buffer; every copy inside the bytes reserved.
    unsafe {
        let states = |a: u32| D3D12_RESOURCE_STATES(a as i32);
        if let Some((t, b)) = before {
            let after = crate::host::legal_state(t, COPY_SOURCE);
            list.list
                .ResourceBarrier(&[crate::host::transition(t, states(b), states(after))]);
        }
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let to = |placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: placed,
            },
        };
        if whole {
            let placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: at,
                Footprint: f,
            };
            list.list
                .CopyTextureRegion(&to(placed), 0, 0, 0, &from, None);
            rows.extend(ys.iter().map(|&y| (y, at + u64::from(y) * pitch)));
        } else {
            for (i, &y) in ys.iter().enumerate() {
                let offset = at + i as u64 * row_bytes;
                let placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: offset,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Height: 1,
                        Depth: 1,
                        ..f
                    },
                };
                let row = D3D12_BOX {
                    left: 0,
                    top: y,
                    front: 0,
                    right: f.Width,
                    bottom: y + 1,
                    back: 1,
                };
                list.list
                    .CopyTextureRegion(&to(placed), 0, 0, 0, &from, Some(&row));
                rows.push((y, offset));
            }
        }
    }
    if let Some((_, b)) = before {
        crate::list::ensure_state(list, resource, b);
    }
    (
        desc,
        Ok(Sample {
            width: f.Width,
            format: f.Format.0 as u32,
            rows,
        }),
    )
}

/// Bytes a texel of `format` takes in a copy; `None` for formats not
/// sampled (block-compressed, video, packed).
fn bytes_per_texel(format: u32) -> Option<u32> {
    Some(match format {
        1..=4 => 16,
        5..=8 => 12,
        9..=22 => 8,
        23..=47 | 67 | 87..=93 => 4,
        48..=59 | 85 | 86 | 115 => 2,
        60..=65 => 1,
        _ => return None,
    })
}

/// The channels of `format` that hold values (depth and stencil: two).
fn channels(format: u32) -> usize {
    match format {
        5..=8 | 26 | 67 | 85 => 3,
        15..=22 | 33..=38 | 44..=52 => 2,
        39..=43 | 53..=65 => 1,
        _ => 4,
    }
}

fn half(h: u16) -> f32 {
    let (sign, exp, man) = (h >> 15, i32::from((h >> 10) & 0x1f), f32::from(h & 0x3ff));
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 if man == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}

/// A float of R11G11B10's: a 5-bit exponent and `man` mantissa bits.
fn small_float(bits: u32, man: u32) -> f32 {
    let (exp, m) = ((bits >> man) & 0x1f, (bits & ((1 << man) - 1)) as f32);
    let one = (1u32 << man) as f32;
    match exp {
        0 => m / one * 2f32.powi(-14),
        31 if m == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        e => (1.0 + m / one) * 2f32.powi(e as i32 - 15),
    }
}

/// A texel of `format` (its bytes) as four values: normalized formats in
/// [0, 1] (or [-1, 1]), integers as they are, missing channels NaN.
fn texel(format: u32, b: &[u8]) -> [f32; 4] {
    let n = f32::NAN;
    let byte = |i: usize| f32::from(b[i]);
    let sbyte = |i: usize| f32::from(b[i] as i8);
    let word = |i: usize| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
    let sword = |i: usize| f32::from(word(i) as i16);
    let dword = |i: usize| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
    let float = |i: usize| f32::from_bits(dword(i));
    let snorm8 = |i: usize| (sbyte(i) / 127.0).max(-1.0);
    let snorm16 = |i: usize| (sword(i) / 32767.0).max(-1.0);
    let bits =
        |v: u32, at: u32, len: u32| ((v >> at) & ((1 << len) - 1)) as f32 / ((1 << len) - 1) as f32;
    match format {
        1 | 2 => [float(0), float(1), float(2), float(3)],
        3 => [0, 1, 2, 3].map(|i| dword(i) as f32),
        4 => [0, 1, 2, 3].map(|i| dword(i) as i32 as f32),
        5 | 6 => [float(0), float(1), float(2), n],
        7 => [dword(0) as f32, dword(1) as f32, dword(2) as f32, n],
        8 => [
            dword(0) as i32 as f32,
            dword(1) as i32 as f32,
            dword(2) as i32 as f32,
            n,
        ],
        9 | 10 => [0, 1, 2, 3].map(|i| half(word(i))),
        11 => [0, 1, 2, 3].map(|i| f32::from(word(i)) / 65535.0),
        12 => [0, 1, 2, 3].map(|i| f32::from(word(i))),
        13 => [0, 1, 2, 3].map(snorm16),
        14 => [0, 1, 2, 3].map(sword),
        15 | 16 => [float(0), float(1), n, n],
        17 => [dword(0) as f32, dword(1) as f32, n, n],
        18 => [dword(0) as i32 as f32, dword(1) as i32 as f32, n, n],
        19..=22 => [float(0), (dword(1) & 0xff) as f32, n, n],
        23 | 24 | 89 => {
            let v = dword(0);
            [
                bits(v, 0, 10),
                bits(v, 10, 10),
                bits(v, 20, 10),
                bits(v, 30, 2),
            ]
        }
        25 => {
            let v = dword(0);
            let at = |at: u32| ((v >> at) & 0x3ff) as f32;
            [at(0), at(10), at(20), (v >> 30) as f32]
        }
        26 => {
            let v = dword(0);
            [
                small_float(v & 0x7ff, 6),
                small_float(v >> 11 & 0x7ff, 6),
                small_float(v >> 22, 5),
                n,
            ]
        }
        27..=29 => [0, 1, 2, 3].map(|i| byte(i) / 255.0),
        30 => [0, 1, 2, 3].map(byte),
        31 => [0, 1, 2, 3].map(snorm8),
        32 => [0, 1, 2, 3].map(sbyte),
        33 | 34 => [half(word(0)), half(word(1)), n, n],
        35 => [
            f32::from(word(0)) / 65535.0,
            f32::from(word(1)) / 65535.0,
            n,
            n,
        ],
        36 => [f32::from(word(0)), f32::from(word(1)), n, n],
        37 => [snorm16(0), snorm16(1), n, n],
        38 => [sword(0), sword(1), n, n],
        39..=41 => [float(0), n, n, n],
        42 => [dword(0) as f32, n, n, n],
        43 => [dword(0) as i32 as f32, n, n, n],
        44..=47 => {
            let v = dword(0);
            [bits(v, 0, 24), (v >> 24) as f32, n, n]
        }
        48 | 49 => [byte(0) / 255.0, byte(1) / 255.0, n, n],
        50 => [byte(0), byte(1), n, n],
        51 => [snorm8(0), snorm8(1), n, n],
        52 => [sbyte(0), sbyte(1), n, n],
        53 | 54 => [half(word(0)), n, n, n],
        55 | 56 => [f32::from(word(0)) / 65535.0, n, n, n],
        57 => [f32::from(word(0)), n, n, n],
        58 => [snorm16(0), n, n, n],
        59 => [sword(0), n, n, n],
        60 | 61 => [byte(0) / 255.0, n, n, n],
        62 => [byte(0), n, n, n],
        63 => [snorm8(0), n, n, n],
        64 => [sbyte(0), n, n, n],
        65 => [byte(0) / 255.0, n, n, n],
        67 => {
            let v = dword(0);
            let scale = 2f32.powi((v >> 27) as i32 - 15 - 9);
            let at = |at: u32| ((v >> at) & 0x1ff) as f32 * scale;
            [at(0), at(9), at(18), n]
        }
        85 => {
            let v = u32::from(word(0));
            [bits(v, 11, 5), bits(v, 5, 6), bits(v, 0, 5), n]
        }
        86 => {
            let v = u32::from(word(0));
            [bits(v, 10, 5), bits(v, 5, 5), bits(v, 0, 5), bits(v, 15, 1)]
        }
        115 => {
            let v = u32::from(word(0));
            [bits(v, 8, 4), bits(v, 4, 4), bits(v, 0, 4), bits(v, 12, 4)]
        }
        87 | 88 | 90..=93 => [2, 1, 0, 3].map(|i| byte(i) / 255.0),
        _ => [n; 4],
    }
}

/// What a sample holds: per channel the lowest, highest and mean finite
/// value, the texels with a channel other than 0 ("lit"), and those with
/// a NaN or infinite one.
#[derive(Debug, Clone, PartialEq)]
struct Stats {
    channels: usize,
    min: [f32; 4],
    max: [f32; 4],
    sum: [f64; 4],
    finite: [u64; 4],
    texels: u64,
    lit: u64,
    odd: u64,
}

fn stats(sample: &Sample, bytes: &[u8]) -> Option<Stats> {
    let size = bytes_per_texel(sample.format)? as usize;
    let channels = channels(sample.format);
    let mut s = Stats {
        channels,
        min: [f32::INFINITY; 4],
        max: [f32::NEG_INFINITY; 4],
        sum: [0.0; 4],
        finite: [0; 4],
        texels: 0,
        lit: 0,
        odd: 0,
    };
    for &(_, at) in &sample.rows {
        let at = at as usize;
        let row = bytes.get(at..at + sample.width as usize * size)?;
        for t in row.chunks_exact(size) {
            let v = texel(sample.format, t);
            s.texels += 1;
            let mut lit = false;
            let mut odd = false;
            for (c, &x) in v.iter().enumerate().take(channels) {
                if x != 0.0 {
                    lit = true;
                }
                if x.is_finite() {
                    s.min[c] = s.min[c].min(x);
                    s.max[c] = s.max[c].max(x);
                    s.sum[c] += f64::from(x);
                    s.finite[c] += 1;
                } else {
                    odd = true;
                }
            }
            s.lit += u64::from(lit);
            s.odd += u64::from(odd);
        }
    }
    Some(s)
}

impl Stats {
    fn text(&self) -> String {
        let list = |f: &dyn Fn(usize) -> Option<f64>| {
            (0..self.channels)
                .map(|c| f(c).map_or("-".to_owned(), |v| format!("{v:.4}")))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let lit = if self.texels == 0 {
            0.0
        } else {
            100.0 * self.lit as f64 / self.texels as f64
        };
        let mut text = format!(
            "lit {lit:.1}% max ({}) mean ({}) min ({})",
            list(&|c| (self.finite[c] > 0).then(|| f64::from(self.max[c]))),
            list(&|c| (self.finite[c] > 0).then(|| self.sum[c] / self.finite[c] as f64)),
            list(&|c| (self.finite[c] > 0).then(|| f64::from(self.min[c]))),
        );
        if self.odd > 0 {
            let _ = write!(text, " NaN or infinite {}", self.odd);
        }
        text
    }
}

/// The program hashes an op's text names (`| <hash>:` per stage).
fn programs(text: &str) -> Vec<&str> {
    text.split(" | ")
        .skip(1)
        .filter_map(|stage| stage.split(':').next())
        .filter(|h| h.len() == 16 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .collect()
}

/// A small buffer's sample: its words as floats, then in hex.
fn words(bytes: &[u8], len: u64, at: &Result<u64, &'static str>) -> String {
    let at = match at {
        Ok(at) => *at as usize,
        Err(why) => return format!("not sampled: {why}"),
    };
    let Some(b) = bytes.get(at..at + len as usize) else {
        return "not read back".to_owned();
    };
    let words: Vec<u32> = b
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&w| u32::from_le_bytes(w))
        .collect();
    let floats: Vec<String> = words
        .iter()
        .map(|&w| format!("{}", f32::from_bits(w)))
        .collect();
    let hex: Vec<String> = words.iter().map(|w| format!("{w:08x}")).collect();
    format!("[{}] ({})", floats.join(", "), hex.join(" "))
}

/// The report of `lists` (in submission order), their samples in `bytes`.
fn report_text(frame: u64, lists: &[Recording], bytes: &[u8]) -> String {
    let passes: usize = lists.iter().map(|l| l.passes.len()).sum();
    let ops: usize = lists
        .iter()
        .flat_map(|l| &l.passes)
        .map(|p| p.ops.len())
        .sum();
    let dropped: usize = lists
        .iter()
        .flat_map(|l| &l.passes)
        .flat_map(|p| &p.ops)
        .filter(|o| !o.ran)
        .count();
    let mut out = format!(
        "XTP pass dump of frame {frame}: {} lists, {passes} passes, {ops} ops ({dropped} dropped).\n\
         A pass is a run of ops on one list writing the same outputs; its outputs are sampled after \
         it ({ROWS} rows across each): \"lit\" is the share of texels with a channel other than 0, \
         then each channel's largest, mean and lowest finite value.\n",
        lists.len()
    );
    let after_text = |a: &After| {
        let desc = a
            .desc
            .map_or("?".to_owned(), |(w, h, f)| format!("{w}x{h} f{f}"));
        let what = match &a.sample {
            Ok(s) => stats(s, bytes).map_or("not read back".to_owned(), |s| s.text()),
            Err(why) => format!("not sampled: {why}"),
        };
        (desc, what)
    };
    let mut timeline: Vec<(usize, String, Vec<String>)> = Vec::new();
    out.push_str("\n== Passes\n");
    let mut n = 0;
    for (l, list) in lists.iter().enumerate() {
        for pass in &list.passes {
            n += 1;
            let ran = pass.ops.iter().filter(|o| o.ran).count();
            let mut hashes: Vec<&str> = pass.ops.iter().flat_map(|o| programs(&o.text)).collect();
            hashes.sort_unstable();
            hashes.dedup();
            let _ = writeln!(
                out,
                "P{n} L{} {}/{} ran, programs [{}]",
                l + 1,
                ran,
                pass.ops.len(),
                hashes.join(", ")
            );
            for a in &pass.after {
                let (desc, what) = after_text(a);
                let _ = writeln!(
                    out,
                    "    -> {} {:#x} {desc}: {what}",
                    a.how.name(),
                    a.resource
                );
                let line = format!("P{n} {what}");
                match timeline.iter_mut().find(|t| t.0 == a.resource) {
                    Some(t) => t.2.push(line),
                    None => timeline.push((a.resource, desc, vec![line])),
                }
            }
        }
    }
    out.push_str("\n== Outputs, pass by pass\n");
    for (resource, desc, lines) in &timeline {
        let _ = writeln!(out, "{resource:#x} {desc}");
        for line in lines {
            let _ = writeln!(out, "    {line}");
        }
    }
    out.push_str("\n== Ops\n");
    let mut n = 0;
    for (l, list) in lists.iter().enumerate() {
        for pass in &list.passes {
            n += 1;
            let _ = writeln!(out, "P{n} L{}", l + 1);
            for op in &pass.ops {
                let _ = writeln!(out, "  {}{}", if op.ran { "" } else { "DROPPED " }, op.text);
                for (address, len, at) in &op.buffers {
                    let _ = writeln!(
                        out,
                        "    buffer {address:#x}+{len} before it: {}",
                        words(bytes, *len, at)
                    );
                }
            }
        }
    }
    out
}

static DUMP: Mutex<Option<Dump>> = Mutex::new(None);

fn dump() -> MutexGuard<'static, Option<Dump>> {
    lock(&DUMP)
}

thread_local! {
    /// The list this thread last recorded an op, a binding or targets for.
    static CURRENT: Cell<usize> = const { Cell::new(0) };
}

/// `HostList::note`: an op about to be recorded in `list`, described by
/// `what`. Recorded when the frame is dumped; gives back what describes
/// it, for the device-removed report.
pub(crate) fn op<'a, F: FnOnce() -> String + 'a>(
    list: &HostList,
    what: F,
) -> Box<dyn FnOnce() -> String + 'a> {
    let program = pick_program();
    if !recording() && program.is_none() {
        return Box::new(what);
    }
    let text = what();
    if let Some(program) = program {
        note_op_outputs(list, &text, program);
    }
    if !recording() {
        return Box::new(move || text);
    }
    if let Some(host) = crate::host::host() {
        CURRENT.set(key(list));
        dump()
            .get_or_insert_with(|| Dump::new(BUDGET, dump_width()))
            .op(&host.device, list, &text);
    }
    Box::new(move || text)
}

/// `State::set_targets`: the render targets and depth of `list`'s next
/// draws (title resources; depth 0 for none).
pub(crate) fn targets(list: &HostList, targets: &[usize], depth: usize) {
    if pick_program().is_some()
        && let Some(o) = lock(&OUTPUTS).as_mut()
        && let Some((t, _)) = o.get_mut(&key(list))
    {
        t.clear();
    }
    CURRENT.set(key(list));
    for &t in targets.iter().chain((depth != 0).then_some(&depth)) {
        note_output(list, t as This, true);
    }
    if !recording() {
        return;
    }
    dump()
        .get_or_insert_with(|| Dump::new(BUDGET, dump_width()))
        .targets(list, targets, depth);
}

/// `readable`: a texture the op being prepared in `list` binds for
/// unordered access.
pub(crate) fn written(list: &HostList, resource: This) {
    CURRENT.set(key(list));
    note_output(list, resource, false);
    if !recording() {
        return;
    }
    dump()
        .get_or_insert_with(|| Dump::new(BUDGET, dump_width()))
        .written(list, resource as usize);
}

/// `note_unready`: a draw or dispatch of `pipeline`, recorded at `path`,
/// that does not run for `why`.
pub(crate) fn dropped(path: &str, pipeline: usize, why: &impl std::fmt::Debug) {
    let list = CURRENT.get();
    if pick_program().is_some()
        && let Some(o) = lock(&OUTPUTS).as_mut()
        && let Some((_, pending)) = o.get_mut(&list)
    {
        pending.clear();
    }
    if !recording() {
        return;
    }
    if let Some(d) = dump().as_mut() {
        d.dropped(list, format!("{path}: pipeline {pipeline:#x} {why:?}"));
    }
}

/// `HostList::close`: `list` is about to close.
pub(crate) fn closed(list: &HostList) {
    if !dump_on() {
        return;
    }
    if let (Some(d), Some(host)) = (dump().as_mut(), crate::host::host()) {
        d.closed(&host.device, list);
    }
}

/// `Host::submit`: a title submission of `runs`, in order.
pub(crate) fn submitted(runs: &[&HostList]) {
    if !dump_on() {
        return;
    }
    if let Some(d) = dump().as_mut() {
        d.submitted(runs);
    }
}

/// Writes the report of the lists submitted since the last one, at the
/// present of frame `frame`. Once neither this frame nor the next is
/// listed, what lists recorded but never submitted is forgotten and the
/// read-back memory starts again.
fn report(frame: u64) {
    if !dump_on() {
        return;
    }
    if dump().as_ref().is_some_and(|d| !d.submitted.is_empty())
        && let (Some(host), Some(folder)) = (crate::host::host(), crate::capture::folder())
    {
        host.drain();
        if let Some(text) = dump().as_mut().and_then(|d| d.report(frame)) {
            let path = folder.join(format!("passes-{frame}.txt"));
            let _ = std::fs::create_dir_all(folder);
            match std::fs::write(&path, text) {
                Ok(()) => tracing::info!(path = %path.display(), "pass dump written"),
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "pass dump not written")
                }
            }
        }
    }
    if !crate::capture::listed(frame)
        && !crate::capture::listed(frame + 1)
        && let Some(d) = dump().as_mut()
        && d.readback.used > 0
    {
        d.forget();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_parse() {
        assert_eq!(
            parse("1280x720:f62"),
            Some(Selector {
                pick: Pick::Written {
                    width: 1280,
                    height: 720,
                    format: Some(62),
                    n: 0,
                    program: None
                },
                scale: 1.0
            })
        );
        assert_eq!(
            parse(" 1920x1080:2:f10 *0.25"),
            Some(Selector {
                pick: Pick::Written {
                    width: 1920,
                    height: 1080,
                    format: Some(10),
                    n: 2,
                    program: None
                },
                scale: 0.25
            })
        );
        assert_eq!(
            parse("0x22d2636b5a0*4"),
            Some(Selector {
                pick: Pick::Address(0x22d2636b5a0),
                scale: 4.0
            })
        );
        assert_eq!(
            parse("1280x720:f62@2dc364df5d3278b8*2"),
            Some(Selector {
                pick: Pick::Written {
                    width: 1280,
                    height: 720,
                    format: Some(62),
                    n: 0,
                    program: Some(0x2dc3_64df_5d32_78b8)
                },
                scale: 2.0
            })
        );
        for bad in [
            "",
            "1280",
            "1280x",
            "0x",
            "0x0",
            "1280x720:fz",
            "1280x720*x",
            "1280x720*inf",
            "1280x720@12",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn typeless_formats_are_viewed_through_a_typed_member() {
        assert_eq!(view_of(27), Some((28, 0, 4)));
        assert_eq!(view_of(19), Some((21, 0, 1)));
        assert_eq!(view_of(44), Some((46, 0, 1)));
        assert_eq!(view_of(62), Some((62, 1, 1)));
        assert_eq!(view_of(10), Some((10, 0, 4)));
        assert_eq!(view_of(90), Some((87, 0, 4)));
        // Typed depth formats cannot be viewed; block formats are not shown.
        assert_eq!(view_of(20), None);
        assert_eq!(view_of(40), None);
        assert_eq!(view_of(71), None);
    }

    #[test]
    fn ops_are_told_apart_by_what_they_write() {
        assert_eq!(
            kind_of("dispatch 40x23x1 compute 1f55c45de66dc2c4 | 1f55c45de66dc2c4: u0=0x0"),
            Kind::Compute
        );
        assert_eq!(kind_of("indirect kind 2 max 1 at 0x1000"), Kind::Compute);
        assert_eq!(kind_of("indirect kind 10 max 4 at 0x1000"), Kind::Graphics);
        assert_eq!(kind_of("mesh 12"), Kind::Graphics);
        assert_eq!(kind_of("draw 3x1 vertex a | b"), Kind::Graphics);
        assert_eq!(
            kind_of("clear UAV Float([0.0, 0.0, 0.0, 0.0]) of 0x22a83108070"),
            Kind::Writes(0x22a83108070)
        );
        assert_eq!(
            kind_of(
                "FidelityFX bilinear upscale Upscale { list: 0x10, color: 0x20, output: 0x22a8310be50, render: [1280, 720] }"
            ),
            Kind::Writes(0x22a8310be50)
        );
        assert_eq!(kind_of("copy resource 0x1 to 0x2"), Kind::Other);
        assert_eq!(
            kind_of("write 0x1 (4 bytes) at 0x10 on the GPU"),
            Kind::Other
        );
        assert_eq!(
            programs(
                "dispatch 1x1x1 compute x | 1f55c45de66dc2c4: u0=0x0 [heap 3: a] | 0123456789abcdef: u1=0x4"
            ),
            vec!["1f55c45de66dc2c4", "0123456789abcdef"]
        );
        assert_eq!(
            small_buffers(
                r#"x tables ["a Srv s0+16: buffer Some((40221212656, 16))", "a Srv s0+40: buffer Some((41111749488, 384))", "a Uav s1+8: uav buffer Some((7, 64))", "b Srv s0+16: buffer Some((40221212656, 16))"]"#
            ),
            vec![(40221212656, 16), (7, 64)]
        );
    }

    #[test]
    fn texels_decode_as_their_formats() {
        // Half floats 1.0, 0.5, -2, 0.
        let h = [0x3c00u16, 0x3800, 0xc000, 0]
            .map(u16::to_le_bytes)
            .concat();
        assert_eq!(texel(10, &h), [1.0, 0.5, -2.0, 0.0]);
        // R11G11B10: 1.0 in each channel.
        let v: u32 = 0x3c0 | 0x3c0 << 11 | 0x1e0 << 22;
        assert_eq!(&texel(26, &v.to_le_bytes())[..3], &[1.0, 1.0, 1.0]);
        // R10G10B10A2: 1023, 0, 4 and alpha 3.
        let v: u32 = 1023 | 4 << 20 | 3 << 30;
        let t = texel(24, &v.to_le_bytes());
        assert_eq!((t[0], t[1], t[3]), (1.0, 0.0, 1.0));
        assert!((t[2] - 4.0 / 1023.0).abs() < 1e-6);
        assert_eq!(texel(62, &[200])[0], 200.0);
        assert_eq!(
            texel(87, &[1, 2, 3, 4])[..3],
            [3.0 / 255.0, 2.0 / 255.0, 1.0 / 255.0]
        );
        // R9G9B9E5: 1.0 in red (mantissa 256, exponent 16).
        let v: u32 = 256 | 16 << 27;
        assert_eq!(texel(67, &v.to_le_bytes())[0], 1.0);
    }

    #[test]
    fn stats_count_lit_texels_and_odd_values() {
        // Two rows of two R16_FLOAT texels: 0, 2 | NaN, 0.5.
        let bytes = [0u16, 0x4000, 0x7e00, 0x3800]
            .map(u16::to_le_bytes)
            .concat();
        let sample = Sample {
            width: 2,
            format: 54,
            rows: vec![(0, 0), (1, 4)],
        };
        let s = stats(&sample, &bytes).unwrap();
        assert_eq!((s.texels, s.lit, s.odd), (4, 3, 1));
        assert_eq!((s.min[0], s.max[0]), (0.0, 2.0));
        assert!((s.sum[0] / s.finite[0] as f64 - 2.5 / 3.0).abs() < 1e-9);
        assert!(
            s.text().starts_with("lit 75.0% max (2.0000)"),
            "{}",
            s.text()
        );
    }

    /// Clears a `width` x `height` render target of host format `format` to
    /// `color` on a title list, run.
    fn cleared(width: u64, height: u32, format: u32, color: [f32; 4]) -> This {
        use crate::object::slot;
        let null = std::ptr::null();
        let desc = crate::resource::tests_support::target(width, height, format);
        let target = crate::resource::tests_support::committed(&desc);
        let rtv = [0u64; 16];
        crate::descriptor::create_rtv(null, target, null.cast(), rtv.as_ptr() as u64);
        let list = crate::testgpu::TitleList::new();
        type Clear = extern "system" fn(This, u64, *const [f32; 4], u32, *const RECT);
        slot::<Clear>(list.this(), 0x1a0)(list.this(), rtv.as_ptr() as u64, &color, 0, null.cast());
        list.run();
        target
    }

    fn release(r: This) {
        crate::object::slot::<extern "system" fn(This) -> u32>(r, 0x10)(r);
    }

    /// A target shown: stretched over the image, values times the scale,
    /// integers over 255, one channel as grey.
    #[test]
    fn targets_are_shown_scaled_over_the_image() {
        let test = "passdump::tests::targets_are_shown_scaled_over_the_image";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let hdr = cleared(4, 2, 10, [2.0, 0.5, 0.25, 1.0]);
        let ids = cleared(2, 2, 62, [200.0, 0.0, 0.0, 0.0]);
        let mut show = Show::new(host, 8, 4).unwrap();
        let near = |texel: &[u8], want: [u8; 3]| {
            texel[..3]
                .iter()
                .zip(want)
                .all(|(&t, w)| t.abs_diff(w) <= 1)
        };
        for (target, scale, want) in [(hdr, 0.5, [255, 64, 32]), (ids, 1.0, [200, 200, 200])] {
            let copy = crate::resource::host_copy(target).unwrap();
            let image = show.draw(host, &copy, scale).unwrap();
            host.drain();
            let texels = crate::resource::tests_support::read_back(host, &image, 0x4);
            for (x, y) in [(0, 0), (7, 3), (3, 2)] {
                let t = &texels[y * 256 + x * 4..][..4];
                assert!(near(t, want), "{t:?} at {x},{y}, want {want:?}");
            }
        }
        assert_eq!(host.log_messages(), None);
        release(hdr);
        release(ids);
    }

    /// A pass dump of one list: a draw into a cleared target, a dispatch
    /// writing nothing; the target is sampled after the draw's pass, as the
    /// clear left it.
    #[test]
    fn passes_are_sampled_after_their_ops() {
        let test = "passdump::tests::passes_are_sampled_after_their_ops";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let target = cleared(8, 4, 10, [0.5, 0.25, 0.0, 1.0]);
        // A 16-byte buffer the draw reads, its host copy up to date.
        let exposure = crate::testgpu::TitleBuffer::new(256);
        exposure.set_words(|i| [0x3f80_0000, 0x4000_0000, 0, 0x3e80_0000][i % 4]);
        let mut dump = Dump::new(1 << 20, 0);
        let list = host.new_list().unwrap();
        exposure.shadow().refresh(&list, exposure.address(), 16);
        dump.targets(&list, &[target as usize], 0);
        let draw = format!(
            "draw 3x1 vertex x | 0123456789abcdef: u0=0x0 tables [\"0123456789abcdef Srv s0+16: buffer Some(({}, 16))\"]",
            exposure.address()
        );
        dump.op(&host.device, &list, &draw);
        dump.op(
            &host.device,
            &list,
            "draw 3x1 vertex x | 0123456789abcdef: u0=0x0",
        );
        dump.dropped(key(&list), "dispatch: pipeline 0x1 NoPipeline".into());
        dump.op(
            &host.device,
            &list,
            "dispatch 1x1x1 compute y | fedcba9876543210: u0=0x0",
        );
        dump.closed(&host.device, &list);
        list.close();
        host.execute(&[&list]);
        dump.submitted(&[&list]);
        host.drain();
        let text = dump.report(7).unwrap();
        assert!(
            text.starts_with("XTP pass dump of frame 7: 1 lists, 2 passes, 4 ops (1 dropped)."),
            "{text}"
        );
        // The dropped dispatch is listed in the pass it was recorded in.
        assert!(
            text.contains("P1 L1 2/3 ran, programs [0123456789abcdef]"),
            "{text}"
        );
        let after = format!(
            "-> rt {:#x} 8x4 f10: lit 100.0% max (0.5000, 0.2500, 0.0000, 1.0000)",
            target as usize
        );
        assert!(text.contains(&after), "{text}");
        assert!(
            text.contains("P2 L1 1/1 ran, programs [fedcba9876543210]"),
            "{text}"
        );
        assert!(
            text.contains("  DROPPED dispatch: pipeline 0x1 NoPipeline"),
            "{text}"
        );
        let sampled = format!(
            "    buffer {:#x}+16 before it: [1, 2, 0, 0.25] (3f800000 40000000 00000000 3e800000)",
            exposure.address()
        );
        assert!(text.contains(&sampled), "{text}");
        // Nothing is left: the read-back memory starts again.
        assert_eq!(dump.readback.used, 0);
        assert_eq!(dump.report(8), None);
        assert_eq!(host.log_messages(), None);
        release(target);
    }
}
