//! The geometry stage of the console's rectangle pipelines whose vertex
//! shader has no per-vertex input (it makes the corners from the vertex
//! id): [`xtp_shader::emit::rect_gs`] behind the translated vertex shader.
//! Both draw paths run such a rectangle as the triangle of its three
//! vertices (direct: `draw::rectangle_vertices`; indirect: the rewrite pass
//! of [`crate::rectangles`]), and the stage adds the triangle with the
//! fourth corner. A pipeline of the console's rectangle type gets it at its
//! creation ([`crate::pipeline::host`]); one the driver refuses with it is
//! made without it, as before (logged there).

use std::sync::Mutex;

use crate::pipeline::host::Input;

/// The geometry shader (a DXIL container) that a pipeline of the console's
/// rectangle type with input elements `inputs` runs behind its translated
/// vertex shader `vs`, with its pixel shader `ps` (none: no pixel shader),
/// to make each rectangle's fourth corner. None when it has a per-vertex
/// input (its rectangles are expanded from their vertices instead:
/// [`crate::primitive::expand_rects`], [`crate::rectangles`]), or when the
/// stage cannot serve its shaders (logged once per reason).
pub(crate) fn geometry_stage(inputs: &[Input], vs: &[u8], ps: Option<&[u8]>) -> Option<Vec<u8>> {
    if inputs.iter().any(|e| !e.per_instance) {
        return None;
    }
    match xtp_shader::emit::rect_gs::rectangle_gs(vs, ps) {
        Ok(gs) => Some(gs),
        Err(e) => {
            static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
            let why = e.to_string();
            let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.contains(&why) {
                tracing::warn!(
                    reason = %why,
                    "rectangles without vertex inputs: no geometry stage, so no fourth corner \
                     (logged once per reason)"
                );
                seen.push(why);
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use xtp_shader::emit::rect_gs::probe;

    use super::*;
    use crate::host::{Host, HostList};
    use crate::shadow::Shadow;
    use crate::testgpu::{TitleBuffer, TitleList};

    /// The target: one cell of `CELL` × `CELL` pixels a case, side by
    /// side, four floats a pixel.
    const CELL: u32 = 16;
    const CELLS: u32 = 5;
    const W: u32 = CELL * CELLS;
    const H: u32 = CELL;
    /// A target row in the readback buffer (`W` × 16 bytes, 256-aligned).
    const PITCH: u32 = (W * 16).next_multiple_of(256);

    /// A triangle the test draws: its vertices in pixels (x, y) and their
    /// w, what it should cover, and whether its float output is checked.
    struct Case {
        what: &'static str,
        corners: [(f32, f32); 3],
        /// w of each vertex: position (x w, y w) in clip space for pixel
        /// (x, y) at w 1.
        w: [f32; 3],
        /// Pixel x, y of what it covers: four corners for a rectangle, three
        /// for a triangle.
        covers: Vec<(f32, f32)>,
        /// Whether the interpolated output is the affine [`value`] (with
        /// w 1).
        affine: bool,
    }

    /// Clip space of pixel (`x`, `y`) at w `w` (that pixel once divided).
    fn clip(x: f32, y: f32, w: f32) -> [f32; 4] {
        let (cx, cy) = (x / W as f32 * 2.0 - 1.0, 1.0 - y / H as f32 * 2.0);
        [cx * w, cy * w, 0.0, w]
    }

    /// The float output the probe carries, at pixel (`x`, `y`): affine, so
    /// the fourth corner's (`a + b − corner`) is its value there.
    fn value(x: f32, y: f32) -> [f32; 4] {
        [x / W as f32, y / H as f32, (x + 2.0 * y) / 128.0, 1.0]
    }

    /// The flat output of vertex `k` of case `n`.
    fn flat(n: usize, k: usize) -> u32 {
        (100 * n + k + 1) as u32
    }

    /// The five cases, cell by cell: a rectangle whose corner is v0 (the
    /// tiled lighting's order: top left, bottom left, top right), one whose
    /// corner is v1, one whose corner is v2, three vertices with no corner,
    /// and a corner whose third vertex has another w (its clip x and y are
    /// a corner's; once divided it is not one).
    fn cases() -> Vec<Case> {
        let cell = |n: u32| {
            let x0 = (n * CELL) as f32;
            let (x1, y1) = (x0 + CELL as f32, H as f32);
            (x0, x1, y1)
        };
        let rect = |n: u32, what, order: [usize; 3]| {
            let (x0, x1, y1) = cell(n);
            // Top left, bottom left, top right, bottom right.
            let at = [(x0, 0.0), (x0, y1), (x1, 0.0), (x1, y1)];
            Case {
                what,
                corners: order.map(|k| at[k]),
                w: [1.0; 3],
                covers: vec![at[0], at[1], at[3], at[2]],
                affine: true,
            }
        };
        let (x3, _, y3) = cell(3);
        let (x4, _, y4) = cell(4);
        // At w 2, pixel y 4 has clip y 1, v0's: v0 shares x with v1 and
        // clip y with v2, a corner in clip space, but not once divided.
        let far = (x4 + 12.0, 4.0);
        assert_eq!(clip(far.0, far.1, 2.0)[1], clip(x4, 0.0, 1.0)[1]);
        vec![
            rect(0, "corner v0 (top left, bottom left, top right)", [0, 1, 2]),
            rect(1, "corner v1 (top right, top left, bottom left)", [2, 0, 1]),
            rect(2, "corner v2 (bottom left, top right, top left)", [1, 2, 0]),
            Case {
                what: "no corner",
                corners: [(x3, 0.0), (x3, y3), (x3 + CELL as f32, 4.0)],
                w: [1.0; 3],
                covers: vec![(x3, 0.0), (x3, y3), (x3 + CELL as f32, 4.0)],
                affine: true,
            },
            Case {
                what: "a corner whose w differ",
                corners: [(x4, 0.0), (x4, y4), far],
                w: [1.0, 1.0, 2.0],
                covers: vec![(x4, 0.0), (x4, y4), far],
                affine: false,
            },
        ]
    }

    /// The probe vertex shader for `cases`: three vertex ids a case.
    fn vertex_shader(cases: &[Case]) -> Vec<u8> {
        let (mut positions, mut values, mut flats) = (Vec::new(), Vec::new(), Vec::new());
        for (n, c) in cases.iter().enumerate() {
            for (k, (&(x, y), &w)) in c.corners.iter().zip(&c.w).enumerate() {
                let p = clip(x, y, 1.0);
                positions.push([p[0] * w, p[1] * w, 0.0, w]);
                values.push(value(x, y));
                flats.push(flat(n, k));
            }
        }
        probe::vertex(&positions, &values, &flats)
    }

    /// Whether pixel point `p` is inside the convex polygon `poly` by more
    /// than `margin` pixels (`Some(true)`), outside by more (`Some(false)`),
    /// or on its edge (`None`).
    fn inside(poly: &[(f32, f32)], p: (f32, f32), margin: f32) -> Option<bool> {
        let mut sign = 0.0f32;
        let mut near = false;
        for (i, &a) in poly.iter().enumerate() {
            let b = poly[(i + 1) % poly.len()];
            let (ex, ey) = (b.0 - a.0, b.1 - a.1);
            let d = (ex * (p.1 - a.1) - ey * (p.0 - a.0)) / (ex * ex + ey * ey).sqrt();
            if d.abs() <= margin {
                near = true;
                continue;
            }
            if sign == 0.0 {
                sign = d.signum();
            } else if d.signum() != sign {
                return Some(false);
            }
        }
        if near { None } else { Some(true) }
    }

    /// A root signature with nothing in it.
    fn empty_root(device: &ID3D12Device) -> ID3D12RootSignature {
        let mut blob: Option<ID3DBlob> = None;
        // SAFETY: an empty description; the out pointer is ours.
        unsafe {
            D3D12SerializeRootSignature(
                &D3D12_ROOT_SIGNATURE_DESC::default(),
                D3D_ROOT_SIGNATURE_VERSION_1,
                &mut blob,
                None,
            )
            .expect("serialized");
            let blob = blob.expect("a blob");
            let bytes = std::slice::from_raw_parts(
                blob.GetBufferPointer().cast::<u8>(),
                blob.GetBufferSize(),
            );
            device
                .CreateRootSignature(0, bytes)
                .expect("a root signature")
        }
    }

    fn bytecode(code: &[u8]) -> D3D12_SHADER_BYTECODE {
        D3D12_SHADER_BYTECODE {
            pShaderBytecode: code.as_ptr().cast(),
            BytecodeLength: code.len(),
        }
    }

    /// A pipeline of `vs`, `gs` and `ps` drawing triangles into a four-float
    /// target, culling `cull`.
    fn pipeline(
        device: &ID3D12Device,
        root: &ID3D12RootSignature,
        (vs, gs, ps): (&[u8], Option<&[u8]>, &[u8]),
        cull: D3D12_CULL_MODE,
    ) -> ID3D12PipelineState {
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        formats[0] = DXGI_FORMAT_R32G32B32A32_FLOAT;
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(root) },
            VS: bytecode(vs),
            GS: gs.map_or_else(D3D12_SHADER_BYTECODE::default, bytecode),
            PS: bytecode(ps),
            BlendState: blend,
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: cull,
                DepthClipEnable: true.into(),
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
        // SAFETY: the description and the code it points at live through
        // the call.
        unsafe { device.CreateGraphicsPipelineState(&desc) }.expect("the driver takes it")
    }

    /// A four-float render target and its view.
    struct Target {
        resource: ID3D12Resource,
        views: ID3D12DescriptorHeap,
    }

    impl Target {
        fn new(device: &ID3D12Device) -> Target {
            let mut resource: Option<ID3D12Resource> = None;
            // SAFETY: plain creation from descriptions that live through
            // the calls.
            unsafe {
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                            Width: u64::from(W),
                            Height: H,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                            ..Default::default()
                        },
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        None,
                        &mut resource,
                    )
                    .expect("a render target");
                let resource = resource.expect("a render target");
                let views: ID3D12DescriptorHeap = device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                        NumDescriptors: 1,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                        NodeMask: 0,
                    })
                    .expect("a view heap");
                device.CreateRenderTargetView(
                    &resource,
                    None,
                    views.GetCPUDescriptorHandleForHeapStart(),
                );
                Target { resource, views }
            }
        }

        /// Clears it and binds it with `pso` and `root`, triangles listed.
        fn bind(&self, list: &HostList, root: &ID3D12RootSignature, pso: &ID3D12PipelineState) {
            // SAFETY: an open list; our live objects, alive until it has run.
            unsafe {
                let rtv = self.views.GetCPUDescriptorHandleForHeapStart();
                let l = &list.list;
                l.ClearRenderTargetView(rtv, &[0.0f32; 4], None);
                l.SetGraphicsRootSignature(root);
                l.SetPipelineState(pso);
                l.OMSetRenderTargets(1, Some(&rtv), false, None);
                l.RSSetViewports(&[D3D12_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: W as f32,
                    Height: H as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]);
                l.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: W as i32,
                    bottom: H as i32,
                }]);
                l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            }
        }

        /// Runs `list` and reads the target back: four floats a pixel, rows
        /// top first.
        fn read(&self, host: &Host, list: &TitleList) -> Vec<[f32; 4]> {
            let (buffer, data) = host.shared_buffer(u64::from(PITCH * H)).expect("readback");
            let l = list.host();
            // SAFETY: an open list; our live target and buffer, the
            // footprint inside the buffer.
            unsafe {
                l.list.ResourceBarrier(&[crate::host::transition(
                    &self.resource,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&buffer),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                                Width: W,
                                Height: H,
                                Depth: 1,
                                RowPitch: PITCH,
                            },
                        },
                    },
                };
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&self.resource),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: 0,
                    },
                };
                l.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            }
            list.run();
            (0..W * H)
                .map(|i| {
                    let at = (i / W * PITCH + i % W * 16) as usize;
                    // SAFETY: the list has run; inside the mapped buffer.
                    unsafe { data.add(at).cast::<[f32; 4]>().read_unaligned() }
                })
                .collect()
        }
    }

    /// The shaders of the tests: the probe vertex shader for [`cases`], the
    /// stage made for it (as a pipeline of no vertex input gets it), and
    /// the probe pixel shader.
    fn shaders(cases: &[Case]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let (vs, ps) = (vertex_shader(cases), probe::pixel());
        let gs = geometry_stage(&[], &vs, Some(&ps)).expect("a geometry stage");
        (vs, gs, ps)
    }

    /// Draws [`cases`] with `pso`, `draw` recording the draw itself.
    fn render(
        host: &Host,
        root: &ID3D12RootSignature,
        pso: &ID3D12PipelineState,
        draw: impl FnOnce(&HostList),
    ) -> Vec<[f32; 4]> {
        let target = Target::new(&host.device);
        let list = TitleList::new();
        target.bind(list.host(), root, pso);
        draw(list.host());
        target.read(host, &list)
    }

    /// Checks `pixels` against `cases`: each pixel well inside a case's
    /// cover is drawn, with the case's v0 flat value and, for an affine
    /// case, the float output at its centre; each pixel well outside every
    /// cover is not. Returns, by case, the pixels drawn.
    fn check(pixels: &[[f32; 4]], cases: &[Case], what: &str) -> Vec<usize> {
        let mut drawn = vec![0; cases.len()];
        for y in 0..H {
            for x in 0..W {
                let px = pixels[(y * W + x) as usize];
                let centre = (x as f32 + 0.5, y as f32 + 0.5);
                let n = (x / CELL) as usize;
                let c = &cases[n];
                let at = format!("{what}, {}: pixel ({x}, {y})", c.what);
                match inside(&c.covers, centre, 0.01) {
                    Some(true) => {
                        assert_eq!(px[3], flat(n, 0) as f32, "{at}: v0's flat value");
                        if c.affine {
                            let want = value(centre.0, centre.1);
                            for k in 0..3 {
                                assert!(
                                    (px[k] - want[k]).abs() < 1e-4,
                                    "{at}: output {k} {} for {}",
                                    px[k],
                                    want[k]
                                );
                            }
                        }
                    }
                    Some(false) => assert_eq!(px, [0.0; 4], "{at}: not drawn"),
                    None => {}
                }
                if px[3] != 0.0 {
                    drawn[n] += 1;
                }
            }
        }
        drawn
    }

    /// The stage is made for a pipeline without per-vertex inputs (per
    /// instance ones are no vertex input), not for one with them, nor for a
    /// pixel shader that reads the primitive id.
    #[test]
    fn the_stage_is_for_pipelines_without_vertex_inputs() {
        let input = |per_instance| Input {
            slot: 0,
            offset: 0,
            format: 2,
            per_instance,
        };
        let cases = cases();
        let (vs, ps) = (vertex_shader(&cases), probe::pixel());
        assert!(geometry_stage(&[], &vs, Some(&ps)).is_some());
        assert!(geometry_stage(&[input(true)], &vs, None).is_some());
        assert!(geometry_stage(&[input(true), input(false)], &vs, Some(&ps)).is_none());
        let reads_id = xtp_shader::emit::rect_gs::rectangle_gs(&vs, Some(&ps)).is_ok();
        assert!(reads_id, "the probe's pixel shader reads no primitive id");
    }

    /// A rectangle list without vertex inputs, drawn as the direct path
    /// draws it (three vertices a rectangle): each rectangle is whole, with
    /// the fourth corner `a + b − corner` whichever vertex is the corner
    /// (the float output is the affine function's value at every pixel of
    /// both triangles), and v0's flat value over all of it; three vertices
    /// with no corner, or with another w, stay one triangle. Drawn culling
    /// back faces, then front faces: each case is drawn whole by exactly one
    /// of them (both triangles turn the same way).
    #[test]
    fn rectangles_without_vertex_inputs_get_their_fourth_corner() {
        let test =
            "rectangles::geometry::tests::rectangles_without_vertex_inputs_get_their_fourth_corner";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let cases = cases();
        let (vs, gs, ps) = shaders(&cases);
        let root = empty_root(&host.device);
        let vertices = 3 * cases.len() as u32;
        let mut passes = Vec::new();
        for cull in [D3D12_CULL_MODE_BACK, D3D12_CULL_MODE_FRONT] {
            let pso = pipeline(&host.device, &root, (&vs, Some(&gs), &ps), cull);
            // SAFETY: an open list with everything bound.
            let pixels = render(host, &root, &pso, |l| unsafe {
                l.list.DrawInstanced(vertices, 1, 0, 0)
            });
            passes.push(pixels);
        }
        let mut both = passes[0].clone();
        for (b, f) in both.iter_mut().zip(&passes[1]) {
            if f[3] != 0.0 {
                assert_eq!(*b, [0.0; 4], "a pixel drawn by both cull modes");
                *b = *f;
            }
        }
        let total = check(&both, &cases, "both cull modes");
        for (n, c) in cases.iter().enumerate() {
            let count = |pixels: &[[f32; 4]]| {
                (0..W * H)
                    .filter(|&i| (i % W) / CELL == n as u32 && pixels[i as usize][3] != 0.0)
                    .count()
            };
            let (b, f) = (count(&passes[0]), count(&passes[1]));
            assert!(
                (b == 0) != (f == 0),
                "{}: drawn whole by one cull mode ({b} and {f} pixels)",
                c.what
            );
            assert_eq!(b + f, total[n]);
        }
        for n in 0..3 {
            assert_eq!(total[n], (CELL * CELL) as usize, "{}: whole", cases[n].what);
        }
    }

    /// Without the stage the same draw lights each rectangle's first
    /// triangle only: what the test above sees is the stage's doing.
    #[test]
    fn without_the_stage_rectangles_keep_one_triangle() {
        let test = "rectangles::geometry::tests::without_the_stage_rectangles_keep_one_triangle";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let cases = cases();
        let (vs, _, ps) = shaders(&cases);
        let root = empty_root(&host.device);
        let pso = pipeline(&host.device, &root, (&vs, None, &ps), D3D12_CULL_MODE_NONE);
        let vertices = 3 * cases.len() as u32;
        // SAFETY: an open list with everything bound.
        let pixels = render(host, &root, &pso, |l| unsafe {
            l.list.DrawInstanced(vertices, 1, 0, 0)
        });
        for (n, c) in cases.iter().take(3).enumerate() {
            let triangle: Vec<(f32, f32)> = c.corners.to_vec();
            let mut lit = 0;
            for y in 0..H {
                for x in n as u32 * CELL..(n as u32 + 1) * CELL {
                    let px = pixels[(y * W + x) as usize];
                    let centre = (x as f32 + 0.5, y as f32 + 0.5);
                    match inside(&triangle, centre, 0.01) {
                        Some(true) => lit += 1,
                        Some(false) => assert_eq!(px, [0.0; 4], "{}: ({x}, {y})", c.what),
                        None => {}
                    }
                }
            }
            assert!(lit > 0, "{}: its first triangle", c.what);
        }
    }

    /// The indirect path: one record of the five cases (3 vertices each),
    /// rewritten by [`crate::rectangles::expand`] for a pipeline without
    /// per-vertex inputs (the record as the title wrote it), then run by
    /// `ExecuteIndirect` with the stage: the same picture as the direct
    /// draw.
    #[test]
    fn indirect_rectangles_without_vertex_inputs_get_their_fourth_corner() {
        let _pool = crate::rectangles::POOL_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let test = "rectangles::geometry::tests::indirect_rectangles_without_vertex_inputs_get_their_fourth_corner";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let cases = cases();
        let (vs, gs, ps) = shaders(&cases);
        let root = empty_root(&host.device);
        let pso = pipeline(
            &host.device,
            &root,
            (&vs, Some(&gs), &ps),
            D3D12_CULL_MODE_NONE,
        );
        let record = [3 * cases.len() as u32, 1, 0, 0];
        let args = TitleBuffer::new(256);
        args.set_words(|i| record.get(i).copied().unwrap_or(0));
        let args_copy: Arc<Shadow> = args.shadow();
        let signature = draw_signature(&host.device);
        let target = Target::new(&host.device);
        let list = TitleList::new();
        let mut held = None;
        let prepared = crate::rectangles::expand(
            list.host(),
            &mut held,
            &[],
            &[(0, 0, 0); 32],
            (&args_copy, args.address(), 16, 1),
            None,
        )
        .expect("expanded");
        target.bind(list.host(), &root, &pso);
        prepared.bind_vertices(list.host());
        // SAFETY: an open list with everything bound; the arguments and
        // their count are the list's own, in the indirect argument state.
        unsafe {
            list.host().list.ExecuteIndirect(
                &signature,
                prepared.max,
                &prepared.args,
                prepared.offset,
                &prepared.args,
                prepared.count_offset,
            );
        }
        let pixels = target.read(host, &list);
        if let Some(s) = held.take() {
            crate::rectangles::give_back(s);
        }
        let total = check(&pixels, &cases, "indirect");
        for n in 0..3 {
            assert_eq!(total[n], (CELL * CELL) as usize, "{}: whole", cases[n].what);
        }
    }

    /// A command signature of draw arguments 16 bytes apart.
    fn draw_signature(device: &ID3D12Device) -> ID3D12CommandSignature {
        let argument = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DRAW,
            ..Default::default()
        };
        let desc = D3D12_COMMAND_SIGNATURE_DESC {
            ByteStride: 16,
            NumArgumentDescs: 1,
            pArgumentDescs: &argument,
            NodeMask: 0,
        };
        let mut signature: Option<ID3D12CommandSignature> = None;
        // SAFETY: a description that lives through the call; no root
        // arguments, so no root signature.
        unsafe {
            device.CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut signature)
        }
        .expect("a command signature");
        signature.expect("a command signature")
    }
}
