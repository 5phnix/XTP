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

fn trace(host: &Host, list: &HostList, tlas: u64, output: &TitleBuffer) {
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
        list.list.SetComputeRootShaderResourceView(0, tlas);
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
    let blas = TitleBuffer::new(1024);
    let cloning = TitleList::new();
    let compacted = TitleBuffer::new(1024);
    let cloned = TitleBuffer::new(1024);
    let first = TitleBuffer::new(1024);
    let second = TitleBuffer::new(1024);
    let instances = TitleBuffer::new(64);
    instances.set_words(|i| match i {
        0 | 5 | 10 => 1f32.to_bits(),
        12 => 0xff00_0000,
        _ => 0,
    });
    let address = TitleBuffer::new(8);
    address.set_words(|i| (cloned.address() >> (32 * i)) as u32);
    // This list writes the BLAS address on the GPU. The CPU instance still points to zero.
    instances.shadow();
    assert!(crate::shadow::copy(
        list.host(),
        instances.address() + 56,
        address.address(),
        8
    ));
    let tlas_desc = |dst| Build {
        DestAccelerationStructureData: dst,
        Inputs: Inputs {
            Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
            NumDescs: 1,
            Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                InstanceDescs: instances.address(),
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let displaced = TitleBuffer::new(24);
    displaced.set_words(|i| (positions[i] + 16.).to_bits());
    let changed = self::geometry(&displaced, &indices);
    let after_refit = TitleBuffer::new(16);
    after_refit.set_words(|_| u32::MAX);
    // SAFETY: the title list and all CPU arrays are live for each call. The recording
    // retains every GPU input, including both generations after their title resources go.
    unsafe {
        let producer = TitleList::new();
        let mut original = blas_input(&geometry);
        original.Flags |= D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_COMPACTION;
        assert!(build(
            producer.this(),
            &Build {
                DestAccelerationStructureData: blas.address(),
                Inputs: original,
                ..Default::default()
            }
        ));
        producer.run();
        let bytes = registry().structures[&blas.address()]
            .compacted_size
            .as_ref()
            .unwrap()
            .load(Ordering::Acquire);
        assert!(bytes > 0);
        assert!(copy(
            cloning.this(),
            compacted.address(),
            blas.address(),
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT.0
        ));
        assert_eq!(
            width(&registry().structures[&compacted.address()].result),
            bytes
        );
        assert!(copy(
            cloning.this(),
            cloned.address(),
            compacted.address(),
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE.0
        ));
        assert!(host_tlas(cloned.address()).is_none());
        assert!(build(list.this(), &tlas_desc(first.address())));
        let mut updated = blas_input(&changed);
        updated.Flags |= D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE
            | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS(1 << 24);
        assert!(build(
            list.this(),
            &Build {
                DestAccelerationStructureData: cloned.address(),
                SourceAccelerationStructureData: cloned.address(),
                Inputs: updated,
                ..Default::default()
            }
        ));
        assert!(build(list.this(), &tlas_desc(second.address())));
        let a = host_tlas_for_list(list.this(), first.address()).unwrap();
        let b = host_tlas_for_list(list.this(), second.address()).unwrap();
        assert_ne!(a, b);
        assert_eq!(host_tlas(first.address()), Some(a));
        assert_eq!(host_tlas(second.address()), Some(b));
        let mut table = [0u64; 20];
        for (offset, address, expected) in [(0x60, first.address(), a), (0x80, second.address(), b)]
        {
            let slot = table.as_mut_ptr().cast::<u8>().add(offset) as u64;
            let mut desc = [0u32; 10];
            desc[1] = D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE.0 as u32;
            desc[4] = address as u32;
            desc[5] = (address >> 32) as u32;
            crate::descriptor::create_srv(std::ptr::null(), std::ptr::null(), &desc, slot);
            assert_eq!(descriptor_tlas(list.this(), slot), Some(expected));
        }
        assert_eq!(
            descriptor_tlas(list.this(), table.as_ptr() as u64 + 0x60),
            Some(a)
        );
        let released = first.address();
        drop((blas, cloned, first, second));
        assert_eq!(host_tlas(released), None);
        trace(host, list.host(), a, &output);
        trace(host, list.host(), b, &after_refit);
    }
    // The TLAS was recorded before this producer was submitted.
    cloning.run();
    list.run();
    assert_eq!(output.words(), [1, 2.0f32.to_bits(), 0, 0]);
    assert_eq!(after_refit.words(), [0, 0, 0, 0]);
}
