use super::*;
use crate::testgpu::{TitleBuffer, TitleList};

fn geometry(vertices: &TitleBuffer, indices: &TitleBuffer) -> Geometry {
    Geometry {
        Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
        Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE,
        Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
            Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                Transform3x4: 0,
                IndexFormat: DXGI_FORMAT_R16_TYPELESS,
                VertexFormat: DXGI_FORMAT_R32G32_FLOAT,
                IndexCount: 3,
                VertexCount: 3,
                IndexBuffer: indices.address(),
                VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                    StartAddress: vertices.address(),
                    StrideInBytes: 8,
                },
            },
        },
    }
}

fn blas_input(geometry: &Geometry) -> Inputs {
    Inputs {
        Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
        Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
        NumDescs: 1,
        DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
        Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
            pGeometryDescs: geometry,
        },
    }
}

#[test]
fn rt_triangle_translation_requires_the_whole_input_range() {
    let Some(_gpu) =
        crate::testgpu::device("rt_triangle_translation_requires_the_whole_input_range")
    else {
        return;
    };
    let list = TitleList::new();
    let (vertices, indices) = (TitleBuffer::new(24), TitleBuffer::new(6));
    let mut geometry = geometry(&vertices, &indices);
    // SAFETY: the test owns the descriptor and all referenced buffers.
    unsafe {
        let translated = triangles(list.host(), &blas_input(&geometry)).unwrap();
        let t = translated[0].Anonymous.Triangles;
        assert_eq!(t.IndexFormat, DXGI_FORMAT_R16_UINT);
        assert_eq!(t.IndexBuffer, indices.shadow().gpu(indices.address()));
        assert_eq!(
            t.VertexBuffer.StartAddress,
            vertices.shadow().gpu(vertices.address())
        );
        geometry.Anonymous.Triangles.IndexCount = 4;
        assert!(triangles(list.host(), &blas_input(&geometry)).is_none());
        geometry.Anonymous.Triangles.IndexCount = 3;
        geometry.Anonymous.Triangles.VertexBuffer.StartAddress += 4;
        assert!(triangles(list.host(), &blas_input(&geometry)).is_none());
    }
}

fn trace(host: &Host, list: &HostList, tlas: &ID3D12Resource, output: &TitleBuffer) {
    let dxc = std::env::var_os("XTP_PC_DXC")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            xtp_shader::emit::validator::Validator::default_path()
                .map(|p| p.with_file_name("dxc.exe"))
        })
        .expect("Windows SDK DXC for the inline ray test");
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("ray.dxil");
    let compiled = std::process::Command::new(dxc)
        .args(["-T", "cs_6_5", "-E", "main", "-Fo"])
        .arg(&binary)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/rt/ray_test.hlsl"))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let code = std::fs::read(binary).unwrap();
    let shadow = output.shadow();
    shadow.refresh(list, output.address(), 16);
    shadow.gpu_writes(list, output.address(), 16);
    // SAFETY: the root signature is embedded by DXC. The list keeps both objects
    // and the output shadow, and the caller retains the TLAS through completion.
    unsafe {
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).unwrap();
        let pso: ID3D12PipelineState = host
            .device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })
            .unwrap();
        list.keep(&root);
        list.keep(&pso);
        list.list.SetPipelineState(&pso);
        list.list.SetComputeRootSignature(&root);
        list.list
            .SetComputeRootShaderResourceView(0, tlas.GetGPUVirtualAddress());
        list.list
            .SetComputeRootUnorderedAccessView(1, shadow.gpu(output.address()));
        list.list.Dispatch(1, 1, 1);
        list.list.ResourceBarrier(&[uav_barrier()]);
    }
}

#[test]
fn rt_ray_hits_the_cloned_blas_at_two_units() {
    let test = "rt_ray_hits_the_cloned_blas_at_two_units";
    let Some(gpu) = crate::testgpu::device(test) else {
        return;
    };
    let host = gpu.host();
    let mut caps = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
    // SAFETY: the output is the options structure selected by the feature identifier.
    if unsafe {
        host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS5,
            (&raw mut caps).cast(),
            size_of_val(&caps) as u32,
        )
    }
    .is_err()
        || caps.RaytracingTier.0 < D3D12_RAYTRACING_TIER_1_1.0
    {
        crate::testgpu::skip(test, "inline ray queries require DXR tier 1.1");
        return;
    }
    let list = TitleList::new();
    let (vertices, indices) = (TitleBuffer::new(24), TitleBuffer::new(8));
    let positions: [f32; 6] = [-1., -1., 0., 1., 1., -1.];
    vertices.set_words(|i| positions[i].to_bits());
    indices.set_words(|i| [1 << 16, 2][i]);
    let output = TitleBuffer::new(16);
    output.set_words(|_| u32::MAX);
    let geometry = geometry(&vertices, &indices);
    let builder = Builder::default();
    // SAFETY: a real title list and live CPU geometry descriptions.
    unsafe {
        assert!(builder.build(
            list.this(),
            &Build {
                DestAccelerationStructureData: 0x1000,
                Inputs: blas_input(&geometry),
                ..Default::default()
            }
        ));
        assert!(builder.copy(
            list.this(),
            0x2000,
            0x1000,
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE.0
        ));
    }
    let table = builder.blas.lock().unwrap();
    let (source, clone) = (&table[&0x1000], &table[&0x2000]);
    // SAFETY: the upload holds one aligned instance, and its referenced clone
    // and the TLAS are live until the title queue has executed both rays.
    let tlas = unsafe {
        let clone_address = clone.GetGPUVirtualAddress();
        assert_ne!(source.GetGPUVirtualAddress(), clone_address);
        let (address, ptr) = list.host().scratch(64).unwrap();
        ptr.cast::<D3D12_RAYTRACING_INSTANCE_DESC>()
            .write(D3D12_RAYTRACING_INSTANCE_DESC {
                Transform: [1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0.],
                _bitfield1: 0xff000000,
                _bitfield2: 0,
                AccelerationStructure: clone_address,
            });
        Recorder::new(list.host())
            .unwrap()
            .build(&Inputs {
                Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
                NumDescs: 1,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                    InstanceDescs: address,
                },
                ..Default::default()
            })
            .unwrap()
    };
    drop(table);
    drop(builder);
    trace(host, list.host(), &tlas, &output);
    list.run();
    assert_eq!(output.words(), [1, 2.0f32.to_bits(), 0, 0]);
}
