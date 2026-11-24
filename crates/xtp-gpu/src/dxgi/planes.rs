//! The planes of a present (after VIctory #211, by Yaekai).
//!
//! `PresentX(plane count, planes, parameters)` names one or more planes,
//! which the console's display composes, plane 0 at the bottom. Each
//! plane's parameters start with the frame token (u64), the resource count
//! (u32 at 8) and the resource array (at 0x10); planes are [`STRIDE`] bytes
//! apart. In Series S mode Grand Theft Auto VI presents a 1080p scene plane
//! and a 2160p interface plane (VIctory #211).
//!
//! Plane 0 is shown as before (`queue::present`). With more planes, each
//! one whose first resource is ours is drawn into a texture of ours, plane
//! 0 opaque and the others blended over it, and that texture is shown. The
//! stride is #211's, not checked against the SDK's headers: a plane read at
//! it counts only if its first resource is one of ours, which a wrong
//! stride does not give; the planes are logged whenever their number
//! changes.
//!
//! shortcut: every plane is stretched over the whole output and the planes
//! after the first are blended as premultiplied alpha (#211's choice); their
//! source and destination rectangles, colour space and flags are not read.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

use crate::host::{Host, HostList};
use crate::object::This;

/// Bytes between two planes' parameters (`D3D12XBOX_PRESENT_PLANE_PARAMETERS`,
/// as VIctory #211 has it).
pub(crate) const STRIDE: usize = 0x48;
/// Planes drawn at most.
const MAX: usize = 8;

/// Plane `i`'s frame token and first resource, read without trusting the
/// memory; `None` if it cannot be read or names no resource.
fn plane(planes: *const u8, i: usize) -> Option<(u64, This)> {
    let mut head = [0u8; 0x18];
    let at = planes.wrapping_add(i * STRIDE);
    if !crate::host::read_into(at, &mut head) {
        return None;
    }
    let word = |o: usize| u64::from_le_bytes(head[o..o + 8].try_into().unwrap_or([0; 8]));
    let (token, count, list) = (word(0), word(8) as u32, word(0x10));
    if count == 0 || list == 0 {
        return None;
    }
    let mut first = [0u8; 8];
    if !crate::host::read_into(list as *const u8, &mut first) {
        return None;
    }
    Some((token, u64::from_le_bytes(first) as This))
}

/// The number of planes last presented.
static LAST: AtomicU32 = AtomicU32::new(0);

/// The first resource of each plane that names one of ours, plane 0 first
/// (at most [`MAX`]). Logs the planes when their number changes.
pub(crate) fn resources(count: u32, planes: *const u8) -> Vec<This> {
    let count = count as usize;
    let mut found = Vec::new();
    let mut lines = Vec::new();
    for i in 0..count.min(MAX) {
        let read = plane(planes, i);
        let ours = read.filter(|&(_, r)| crate::resource::is_resource(r));
        match ours {
            Some((token, resource)) => {
                lines.push(format!("{i}: {} token {token:#x}", describe(resource)));
                found.push(resource);
            }
            None => lines.push(format!("{i}: no resource of ours ({:x?})", raw(planes, i))),
        }
    }
    if LAST.swap(count as u32, Ordering::Relaxed) != count as u32 {
        tracing::info!(count, planes = ?lines, "the title presents planes");
    }
    found
}

/// A resource's host copy in a few words.
fn describe(resource: This) -> String {
    crate::resource::host_resource(resource).map_or_else(
        || format!("{resource:?} without a host copy"),
        |t| {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            format!("{resource:?} {}x{} f{}", d.Width, d.Height, d.Format.0)
        },
    )
}

/// Plane `i`'s parameters as words, for the log.
fn raw(planes: *const u8, i: usize) -> Vec<u64> {
    let mut bytes = [0u8; STRIDE];
    if !crate::host::read_into(planes.wrapping_add(i * STRIDE), &mut bytes) {
        return Vec::new();
    }
    let (words, _) = bytes.as_chunks::<8>();
    words.iter().map(|&w| u64::from_le_bytes(w)).collect()
}

/// Shows a present of several planes as one image, frame `frame`: its
/// first plane's resource and, in diagnostics builds, whether it was black.
/// `None` if there are fewer than two planes of ours, or they cannot be
/// composed (the caller shows plane 0 as before).
pub(crate) fn present(count: u32, planes: *const u8, frame: u64) -> Option<(This, Option<bool>)> {
    if count < 2 || planes.is_null() {
        if count == 1 && !planes.is_null() && LAST.load(Ordering::Relaxed) != 1 {
            // Logs the planes when they become one.
            resources(count, planes);
        }
        return None;
    }
    let resources = resources(count, planes);
    let host = crate::host::host()?;
    let sources: Vec<This> = resources
        .iter()
        .copied()
        .filter(|&r| crate::resource::host_resource(r).is_some())
        .collect();
    if sources.len() < 2 {
        return None;
    }
    let mut compositor = COMPOSITOR.lock().unwrap_or_else(|e| e.into_inner());
    if compositor.is_none() {
        match Compositor::new(host) {
            Ok(c) => *compositor = Some(c),
            Err(e) => {
                once(&format!("no plane compositor ({e}): plane 0 shown alone"));
                return None;
            }
        }
    }
    let image = compositor.as_mut()?.compose(host, &sources)?;
    const RENDER_TARGET: u32 = 0x4;
    let shown = host.present(&image, RENDER_TARGET, frame);
    Some((sources[0], shown))
}

/// Logs `text` the first time.
fn once(text: &str) {
    static SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if !said.iter().any(|s| s == text) {
        tracing::warn!("{text}");
        said.push(text.to_owned());
    }
}

/// The format a plane in `format` is read in, and the composed image's
/// format when it is plane 0 (one the swap chain takes): the plain (UNORM
/// or float) member of its family, so its bytes go through as they are.
pub(crate) fn plain(format: u32) -> Option<DXGI_FORMAT> {
    Some(match format {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

static COMPOSITOR: Mutex<Option<Compositor>> = Mutex::new(None);

const SOURCE: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL), " \
    "StaticSampler(s0, filter = FILTER_MIN_MAG_MIP_LINEAR, addressU = TEXTURE_ADDRESS_CLAMP, " \
    "addressV = TEXTURE_ADDRESS_CLAMP, visibility = SHADER_VISIBILITY_PIXEL)"
Texture2D plane : register(t0);
SamplerState linear_clamp : register(s0);
[RootSignature(RS)]
void vs(uint id : SV_VertexID, out float4 position : SV_Position, out float2 uv : TEXCOORD0) {
    uv = float2((id << 1) & 2, id & 2);
    position = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
[RootSignature(RS)]
float4 ps(float4 position : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    return plane.Sample(linear_clamp, uv);
}
"#;

/// What composes planes: a full-screen triangle per plane, its pipelines
/// per output format (opaque and blended), and the image it draws.
struct Compositor {
    root: ID3D12RootSignature,
    vs: ID3DBlob,
    ps: ID3DBlob,
    /// Output format, then the opaque and the blending pipeline.
    pipelines: Vec<(DXGI_FORMAT, ID3D12PipelineState, ID3D12PipelineState)>,
    views: ID3D12DescriptorHeap,
    view_size: usize,
    target_view: ID3D12DescriptorHeap,
    /// The composed image (in the render target state between presents),
    /// its width, height and format.
    image: Option<(ID3D12Resource, u32, u32, DXGI_FORMAT)>,
    list: HostList,
}

// SAFETY: D3D12 objects are free-threaded; the compositor is used under
// its lock only.
unsafe impl Send for Compositor {}

fn compile(entry: PCSTR, target: PCSTR) -> windows::core::Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("planes"),
            None,
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
            tracing::warn!("plane compositor: {}", String::from_utf8_lossy(text));
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

impl Compositor {
    fn new(host: &Host) -> windows::core::Result<Compositor> {
        let device = &host.device;
        let vs = compile(s!("vs"), s!("vs_5_1"))?;
        let ps = compile(s!("ps"), s!("ps_5_1"))?;
        // SAFETY: plain object creation; the root signature is in the shader.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes(&vs))?;
            let views: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: MAX as u32,
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
            let view_size = device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let list = host.new_list()?;
            list.close();
            Ok(Compositor {
                root,
                vs,
                ps,
                pipelines: Vec::new(),
                views,
                view_size,
                target_view,
                image: None,
                list,
            })
        }
    }

    /// The opaque and the blending pipeline drawing into `format`.
    fn pipelines(
        &mut self,
        device: &ID3D12Device,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<(ID3D12PipelineState, ID3D12PipelineState)> {
        if let Some((_, opaque, blend)) = self.pipelines.iter().find(|p| p.0 == format) {
            return Ok((opaque.clone(), blend.clone()));
        }
        let make = |blend: bool| -> windows::core::Result<ID3D12PipelineState> {
            let mut target = D3D12_RENDER_TARGET_BLEND_DESC {
                RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
                ..Default::default()
            };
            if blend {
                // Premultiplied alpha.
                target.BlendEnable = true.into();
                (target.SrcBlend, target.DestBlend) = (D3D12_BLEND_ONE, D3D12_BLEND_INV_SRC_ALPHA);
                (target.SrcBlendAlpha, target.DestBlendAlpha) =
                    (D3D12_BLEND_ONE, D3D12_BLEND_INV_SRC_ALPHA);
                (target.BlendOp, target.BlendOpAlpha) = (D3D12_BLEND_OP_ADD, D3D12_BLEND_OP_ADD);
            }
            let mut blend_state = D3D12_BLEND_DESC::default();
            blend_state.RenderTarget[0] = target;
            let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
            formats[0] = format;
            let (vs, ps) = (bytes(&self.vs), bytes(&self.ps));
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                // SAFETY: a borrowed pointer the description never releases.
                pRootSignature: unsafe { std::mem::transmute_copy(&self.root) },
                VS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: vs.as_ptr().cast(),
                    BytecodeLength: vs.len(),
                },
                PS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: ps.as_ptr().cast(),
                    BytecodeLength: ps.len(),
                },
                BlendState: blend_state,
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
            // SAFETY: a complete description; the shaders live through the call.
            unsafe { device.CreateGraphicsPipelineState(&desc) }
        };
        let (opaque, blend) = (make(false)?, make(true)?);
        self.pipelines.push((format, opaque.clone(), blend.clone()));
        Ok((opaque, blend))
    }

    /// The composed image, `width` by `height` in `format`, made or remade
    /// as needed (in the render target state).
    fn image(
        &mut self,
        device: &ID3D12Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<ID3D12Resource> {
        if let Some((image, w, h, f)) = &self.image
            && (*w, *h, *f) == (width, height, format)
        {
            return Ok(image.clone());
        }
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
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let mut image: Option<ID3D12Resource> = None;
        // SAFETY: plain creation; the out pointer is ours.
        unsafe {
            device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                None,
                &mut image,
            )?;
        }
        let image = image.ok_or_else(windows::core::Error::empty)?;
        crate::host::name(&image, || format!("composed planes {width}x{height}"));
        tracing::info!(width, height, format = format.0, "composing present planes");
        self.image = Some((image.clone(), width, height, format));
        Ok(image)
    }

    /// Draws `planes` (title resources with host copies, plane 0 first)
    /// into the image and runs it; the image, in the render target state.
    fn compose(&mut self, host: &Host, planes: &[This]) -> Option<ID3D12Resource> {
        let textures: Vec<(This, ID3D12Resource, DXGI_FORMAT)> = planes
            .iter()
            .filter_map(|&r| {
                let t = crate::resource::host_resource(r)?.clone();
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                match plain(d.Format.0 as u32) {
                    Some(f) => Some((r, t, f)),
                    None => {
                        once(&format!("a plane in format {} not composed", d.Format.0));
                        None
                    }
                }
            })
            .collect();
        let (_, _, format) = textures.first()?;
        let format = *format;
        let (mut width, mut height) = (1, 1);
        for (_, t, _) in &textures {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            (width, height) = (width.max(d.Width as u32), height.max(d.Height));
        }
        let made = self
            .image(&host.device, width, height, format)
            .and_then(|image| Ok((image, self.pipelines(&host.device, format)?)));
        let (image, (opaque, blend)) = match made {
            Ok(made) => made,
            Err(e) => {
                once(&format!("planes not composed ({e}): plane 0 shown alone"));
                return None;
            }
        };
        let list = &self.list;
        list.reset();
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        // SAFETY: our open list, heaps and image; live plane textures.
        unsafe {
            let first = self.views.GetCPUDescriptorHandleForHeapStart();
            let gpu = self.views.GetGPUDescriptorHandleForHeapStart();
            for (i, (resource, texture, format)) in textures.iter().enumerate() {
                crate::list::ensure_state(list, *resource, PIXEL_SHADER_RESOURCE);
                let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: *format,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_SRV {
                            MipLevels: 1,
                            ..Default::default()
                        },
                    },
                };
                let at = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: first.ptr + i * self.view_size,
                };
                host.device
                    .CreateShaderResourceView(texture, Some(&view), at);
            }
            let target = self.target_view.GetCPUDescriptorHandleForHeapStart();
            host.device.CreateRenderTargetView(&image, None, target);
            let l = &list.list;
            l.OMSetRenderTargets(1, Some(&target), false, None);
            l.SetDescriptorHeaps(&[Some(self.views.clone())]);
            l.SetGraphicsRootSignature(&self.root);
            l.RSSetViewports(&[D3D12_VIEWPORT {
                Width: width as f32,
                Height: height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            l.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            }]);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            for i in 0..textures.len() {
                l.SetPipelineState(if i == 0 { &opaque } else { &blend });
                l.SetGraphicsRootDescriptorTable(
                    0,
                    D3D12_GPU_DESCRIPTOR_HANDLE {
                        ptr: gpu.ptr + (i * self.view_size) as u64,
                    },
                );
                l.DrawInstanced(3, 1, 0, 0);
            }
        }
        list.close();
        host.execute(&[list]);
        Some(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Planes laid out as the title passes them: `STRIDE` bytes apart,
    /// each naming one resource.
    struct Planes {
        bytes: Vec<u8>,
        /// Each plane's resource list (one resource), which `bytes` points
        /// into.
        _lists: Vec<usize>,
    }

    fn planes(resources: &[usize], token: u64) -> Planes {
        let mut bytes = vec![0u8; STRIDE * resources.len()];
        let lists = resources.to_vec();
        for i in 0..resources.len() {
            let list = lists[i..].as_ptr() as u64;
            let at = i * STRIDE;
            bytes[at..at + 8].copy_from_slice(&token.to_le_bytes());
            bytes[at + 8..at + 12].copy_from_slice(&1u32.to_le_bytes());
            bytes[at + 0x10..at + 0x18].copy_from_slice(&list.to_le_bytes());
        }
        Planes {
            bytes,
            _lists: lists,
        }
    }

    #[test]
    fn plane_formats_read_through_their_plain_member() {
        assert_eq!(plain(29), Some(DXGI_FORMAT_R8G8B8A8_UNORM));
        assert_eq!(plain(27), Some(DXGI_FORMAT_R8G8B8A8_UNORM));
        assert_eq!(plain(91), Some(DXGI_FORMAT_B8G8R8A8_UNORM));
        assert_eq!(plain(24), Some(DXGI_FORMAT_R10G10B10A2_UNORM));
        assert_eq!(plain(10), Some(DXGI_FORMAT_R16G16B16A16_FLOAT));
        assert_eq!(plain(40), None);
    }

    /// Planes whose resource is not ours (a wrong stride, garbage) are
    /// left out without being followed further.
    #[test]
    fn only_planes_naming_our_resources_count() {
        let not_ours = Box::new([0u64; 4]);
        let p = planes(&[not_ours.as_ptr() as usize, 0x10], 7);
        assert!(resources(2, p.bytes.as_ptr()).is_empty());
        // A plane array that cannot be read at all.
        assert!(resources(3, 0x8 as *const u8).is_empty());
    }

    /// Two planes of ours are composed: plane 0 stretched opaque, plane 1
    /// blended over it as premultiplied alpha.
    #[test]
    fn two_planes_are_composed() {
        let test = "dxgi::planes::tests::two_planes_are_composed";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        use crate::object::slot;
        let null = std::ptr::null();
        let make = |w: u64, h: u32| {
            let desc = crate::resource::tests_support::target(w, h, 28);
            crate::resource::tests_support::committed(&desc)
        };
        // A 4x4 scene plane (red) under an 8x8 interface plane: clear
        // (transparent) but for its top-left quarter, 40 % covered green.
        let (scene, ui) = (make(4, 4), make(8, 8));
        let rtv = [[0u64; 16]; 2];
        crate::descriptor::create_rtv(null, scene, null.cast(), rtv[0].as_ptr() as u64);
        crate::descriptor::create_rtv(null, ui, null.cast(), rtv[1].as_ptr() as u64);
        let list = crate::testgpu::TitleList::new();
        type Clear = extern "system" fn(This, u64, *const [f32; 4], u32, *const RECT);
        let clear = slot::<Clear>(list.this(), 0x1a0);
        clear(
            list.this(),
            rtv[0].as_ptr() as u64,
            &[1.0, 0.0, 0.0, 1.0],
            0,
            null.cast(),
        );
        clear(
            list.this(),
            rtv[1].as_ptr() as u64,
            &[0.0; 4],
            0,
            null.cast(),
        );
        let corner = RECT {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        };
        clear(
            list.this(),
            rtv[1].as_ptr() as u64,
            &[0.0, 0.4, 0.0, 0.4],
            1,
            &corner,
        );
        list.run();

        let p = planes(&[scene as usize, ui as usize], 0x1234);
        assert_eq!(resources(2, p.bytes.as_ptr()), vec![scene, ui]);
        let mut compositor = Compositor::new(host).unwrap();
        let image = compositor.compose(host, &[scene, ui]).unwrap();
        // SAFETY: a live resource.
        let d = unsafe { image.GetDesc() };
        assert_eq!(
            (d.Width, d.Height, d.Format),
            (8, 8, DXGI_FORMAT_R8G8B8A8_UNORM)
        );
        let texels = crate::resource::tests_support::read_back(host, &image, 0x4);
        let at = |x: usize, y: usize| &texels[y * 256 + x * 4..][..4];
        let near = |texel: &[u8], want: [u8; 3]| {
            texel[..3]
                .iter()
                .zip(want)
                .all(|(&t, w)| t.abs_diff(w) <= 1)
        };
        // Inside the corner: red under 40 % green, premultiplied.
        assert!(near(at(1, 1), [153, 102, 0]), "{:?}", at(1, 1));
        // Elsewhere: the scene's red.
        assert!(near(at(6, 6), [255, 0, 0]), "{:?}", at(6, 6));
        assert_eq!(host.log_messages(), None);
        for r in [scene, ui] {
            slot::<extern "system" fn(This) -> u32>(r, 0x10)(r);
        }
    }
}
