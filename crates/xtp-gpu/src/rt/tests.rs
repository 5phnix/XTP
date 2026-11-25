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

/// A title TLAS build at `dst` of the one instance at `instances`.
fn tlas_desc(dst: u64, instances: &TitleBuffer) -> Build {
    Build {
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
    }
}

/// The triangle (-1, -1), (0, 1), (1, -1) at z = 0: the ray of thread 0
/// of `ray_test.hlsl` hits it at two units, the ray of thread 1 misses.
const POSITIONS: [f32; 6] = [-1., -1., 0., 1., 1., -1.];
/// What `ray_test.hlsl` writes when thread 0's ray hits at two units.
fn hit() -> [u32; 4] {
    [1, 2.0f32.to_bits(), 0, 0]
}
/// What it writes when both rays miss: an empty TLAS.
const MISSES: [u32; 4] = [0; 4];
/// What an output holds before any trace writes it.
const UNWRITTEN: [u32; 4] = [77; 4];

fn unwritten() -> TitleBuffer {
    let output = TitleBuffer::new(16);
    output.set_words(|_| 77);
    output
}

/// The host device for `test`, if it builds DXR structures; `rays` is
/// whether it also runs inline ray queries (DXR tier 1.1). A missing tier
/// is reported as a skip (a failure with `XTP_REQUIRE_GPU`), never silent.
fn dxr(test: &str) -> Option<(crate::testgpu::Gpu, bool)> {
    let gpu = crate::testgpu::device(test)?;
    let mut caps = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
    // SAFETY: the output is the options structure selected by the feature identifier.
    let tier = unsafe {
        gpu.host().device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS5,
            (&raw mut caps).cast(),
            size_of_val(&caps) as u32,
        )
    }
    .map_or(D3D12_RAYTRACING_TIER_NOT_SUPPORTED, |()| {
        caps.RaytracingTier
    });
    // The DXR device this module records with.
    let builds = device(gpu.host());
    if tier == D3D12_RAYTRACING_TIER_NOT_SUPPORTED || builds.is_none() {
        crate::testgpu::skip(test, "the host device has no DXR");
        return None;
    }
    let rays = tier.0 >= D3D12_RAYTRACING_TIER_1_1.0;
    if !rays {
        crate::testgpu::skip(
            test,
            "the traces: inline ray queries require DXR tier 1.1 (the builds still run)",
        );
    }
    Some((gpu, rays))
}

/// A one-instance TLAS of the test triangle's BLAS, built and submitted.
struct World {
    tlas: TitleBuffer,
    blas: TitleBuffer,
    _inputs: [TitleBuffer; 3],
}

impl World {
    fn new() -> World {
        let (vertices, indices) = (TitleBuffer::new(24), TitleBuffer::new(8));
        vertices.set_words(|i| POSITIONS[i].to_bits());
        indices.set_words(|i| [1 << 16, 2][i]);
        let blas = TitleBuffer::new(1024);
        let tlas = TitleBuffer::new(1024);
        let instances = TitleBuffer::new(64);
        instances.set_words(|i| match i {
            0 | 5 | 10 => 1f32.to_bits(),
            12 => 0xff00_0000,
            14 => blas.address() as u32,
            15 => (blas.address() >> 32) as u32,
            _ => 0,
        });
        let producer = TitleList::new();
        let geometry = geometry(&vertices, &indices);
        // SAFETY: the title list and all CPU arrays are live for each call.
        unsafe {
            assert!(Builder.build(
                producer.this(),
                &Build {
                    DestAccelerationStructureData: blas.address(),
                    Inputs: blas_input(&geometry),
                    ..Default::default()
                }
            ));
            assert!(Builder.build(producer.this(), &tlas_desc(tlas.address(), &instances)));
        }
        producer.run();
        World {
            tlas,
            blas,
            _inputs: [vertices, indices, instances],
        }
    }
}

/// A title descriptor slot of a raytracing SRV naming title `address`.
struct Descriptor([u64; 4]);

impl Descriptor {
    fn new(address: u64) -> Box<Descriptor> {
        let mut slot = Box::new(Descriptor([0; 4]));
        let mut srv = [0u32; 10];
        srv[1] = D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE.0 as u32;
        srv[4] = address as u32;
        srv[5] = (address >> 32) as u32;
        let handle = slot.0.as_mut_ptr() as u64;
        crate::descriptor::create_srv(std::ptr::null(), std::ptr::null(), &srv, handle);
        slot
    }

    fn handle(&self) -> u64 {
        self.0.as_ptr() as u64
    }
}

enum Scene {
    /// A TLAS host address, bound directly.
    Address(u64),
    /// A title descriptor slot, bound in submission order.
    Descriptor(u64),
    /// The same, with the root signature a host pipeline makes for a
    /// RayQuery splice: the TLAS at t240 among bounded root SRVs.
    Spliced(u64),
    /// A descriptor slot, with group counts at a title address the GPU
    /// wrote (and the command count at another, if set).
    Indirect(u64, u64, Option<u64>),
}

/// `ray_test.hlsl` compiled by the Windows SDK's DXC, with `defines`.
fn ray_program(defines: &[&str]) -> Vec<u8> {
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
        .args(defines.iter().flat_map(|d| ["-D", d]))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/rt/ray_test.hlsl"))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    std::fs::read(binary).unwrap()
}

/// The root signature of a spliced program reading the TLAS, with its
/// output at `u0`: the TLAS parameter is the host pipeline's own
/// ([`crate::pipeline::host::root_view`]). Its index and the signature.
fn spliced_root(host: &Host) -> (u32, ID3D12RootSignature) {
    use crate::pipeline::host::{bounded_root_srvs, root_srv_in_table, root_view};
    use xtp_shader::emit::{ROOT_CBV_SPACE, TLAS_REGISTER};
    // ray_test.hlsl names them: t240, space 1.
    assert_eq!((TLAS_REGISTER, ROOT_CBV_SPACE), (240, 1));
    assert!(bounded_root_srvs(), "XTP_ROOT_SRV_DIRECT is not set");
    assert!(root_srv_in_table(3) && !root_srv_in_table(TLAS_REGISTER));
    let mut ranges = Vec::new();
    let all = D3D12_SHADER_VISIBILITY_ALL;
    let other = root_view(D3D12_ROOT_PARAMETER_TYPE_SRV, 3, all, &mut ranges);
    assert_eq!(
        other.ParameterType,
        D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE
    );
    let tlas = root_view(
        D3D12_ROOT_PARAMETER_TYPE_SRV,
        TLAS_REGISTER,
        all,
        &mut ranges,
    );
    assert_eq!(tlas.ParameterType, D3D12_ROOT_PARAMETER_TYPE_SRV);
    let output = D3D12_ROOT_PARAMETER1 {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
        Anonymous: D3D12_ROOT_PARAMETER1_0 {
            Descriptor: D3D12_ROOT_DESCRIPTOR1::default(),
        },
        ShaderVisibility: all,
    };
    let (root, _) =
        crate::pipeline::host::root_signature(&host.device, &[tlas, output], &[], true).unwrap();
    (0, root)
}

fn trace(host: &Host, title_list: &TitleList, tlas: Scene, output: &TitleBuffer) {
    let list = title_list.host();
    let spliced = matches!(tlas, Scene::Spliced(_));
    let code = ray_program(if spliced { &["SPLICED"] } else { &[] });
    // The commands of an indirect trace come before its PSO and root views.
    let prepared = match tlas {
        // SAFETY: the test's live list and descriptor slot.
        Scene::Indirect(handle, arguments, count) => Some(
            unsafe { prepare_indirect_tlas(title_list.this(), handle, (arguments, 12, 1), count) }
                .expect("indirect trace commands"),
        ),
        _ => None,
    };
    let shadow = output.shadow();
    shadow.refresh(list, output.address(), 16);
    shadow.gpu_writes(list, output.address(), 16);
    // SAFETY: the root signature is embedded by DXC or made here. The list keeps
    // both objects and the output shadow, and the caller retains the TLAS through
    // completion.
    unsafe {
        let (param, root) = if spliced {
            crate::testgpu::bind_indexed_heaps(host, list);
            spliced_root(host)
        } else {
            (0, host.device.CreateRootSignature(0, &code).unwrap())
        };
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
            .SetComputeRootUnorderedAccessView(1, shadow.gpu(output.address()));
        match tlas {
            Scene::Address(address) => {
                list.list.SetComputeRootShaderResourceView(0, address);
                list.list.Dispatch(1, 1, 1);
            }
            Scene::Descriptor(handle) | Scene::Spliced(handle) => assert!(
                dispatch_descriptor_tlas(title_list.this(), handle, &root, param, [1, 1, 1])
            ),
            Scene::Indirect(..) => assert!(dispatch_indirect_tlas(
                title_list.this(),
                prepared.as_ref().unwrap(),
                &root,
                param
            )),
        }
        list.list.ResourceBarrier(&[uav_barrier()]);
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

#[test]
fn rt_ray_hits_the_cloned_blas_at_two_units() {
    let test = "rt_ray_hits_the_cloned_blas_at_two_units";
    let Some((gpu, rays)) = dxr(test) else {
        return;
    };
    let host = gpu.host();
    let list = TitleList::new();
    let (vertices, indices) = (TitleBuffer::new(24), TitleBuffer::new(8));
    vertices.set_words(|i| POSITIONS[i].to_bits());
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
    let future = TitleList::new();
    let too_early = TitleList::new();
    let delayed_hit = TitleBuffer::new(16);
    let missed = unwritten();
    let slot = Descriptor::new(first.address());
    if rays {
        trace(
            host,
            &future,
            Scene::Descriptor(slot.handle()),
            &delayed_hit,
        );
        trace(host, &too_early, Scene::Descriptor(slot.handle()), &missed);
        too_early.run();
        // Nothing published at its address in queue order: an empty TLAS, every ray missing.
        assert_eq!(missed.words(), MISSES);
    }

    let displaced = TitleBuffer::new(24);
    displaced.set_words(|i| (POSITIONS[i] + if i % 2 == 0 { 4. } else { 0. }).to_bits());
    let changed = self::geometry(&displaced, &indices);
    let after_refit = TitleBuffer::new(16);
    after_refit.set_words(|_| u32::MAX);
    // SAFETY: the title list and all CPU arrays are live for each call. The recording
    // retains every GPU input, including both generations after their title resources go.
    unsafe {
        // The TLAS is recorded before its BLAS address has a registry entry.
        assert!(Builder.build(list.this(), &tlas_desc(first.address(), &instances)));
        let producer = TitleList::new();
        let mut original = blas_input(&geometry);
        original.Flags |= D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_COMPACTION;
        assert!(Builder.build(
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
        let registry_source_capacity = registry().structures[&blas.address()].result.bytes;
        assert!(Builder.copy(
            cloning.this(),
            compacted.address(),
            blas.address(),
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT.0
        ));
        assert_eq!(
            registry().structures[&compacted.address()].result.bytes,
            registry_source_capacity
        );
        assert!(Builder.copy(
            cloning.this(),
            cloned.address(),
            compacted.address(),
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE.0
        ));
        assert!(host_tlas(list.this(), cloned.address()).is_none());
        let mut updated = blas_input(&changed);
        updated.Flags |= D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE
            | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS(1 << 24);
        assert!(Builder.build(
            list.this(),
            &Build {
                DestAccelerationStructureData: cloned.address(),
                SourceAccelerationStructureData: cloned.address(),
                Inputs: updated,
                ..Default::default()
            }
        ));
        assert!(Builder.build(list.this(), &tlas_desc(second.address(), &instances)));
        assert!(host_tlas(cloning.this(), first.address()).is_none());
        let a = host_tlas_for_list(list.this(), first.address()).unwrap();
        let b = host_tlas_for_list(list.this(), second.address()).unwrap();
        assert_ne!(a, b);
        assert_eq!(a % 256, 0);
        assert_eq!(b % 256, 0);
        assert!(
            a.abs_diff(b) < 65536,
            "small TLAS results share one physical page"
        );
        assert_eq!(host_tlas(list.this(), first.address()), Some(a));
        assert_eq!(host_tlas(list.this(), second.address()), Some(b));
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
            // The variant on the host recording pins the same structure.
            assert_eq!(descriptor_tlas_in(list.host(), slot), Some(expected));
        }
        assert_eq!(
            descriptor_tlas(list.this(), table.as_ptr() as u64 + 0x60),
            Some(a)
        );
        assert_eq!(
            descriptor_tlas_in(cloning.host(), table.as_ptr() as u64 + 0x60),
            None,
            "its producer is not submitted"
        );
        let released = first.address();
        drop((blas, first, second));
        assert_eq!(host_tlas(list.this(), released), None);
        if rays {
            trace(host, &list, Scene::Address(a), &output);
            trace(host, &list, Scene::Address(b), &after_refit);
        }
    }
    // The TLAS was recorded before this producer was submitted.
    cloning.run();
    list.run();
    future.run();
    if rays {
        assert_eq!(delayed_hit.words(), hit());
        assert_eq!(output.words(), hit());
        assert_eq!(after_refit.words(), [0, 0, 1, 2.0f32.to_bits()]);
    }
}

#[test]
fn rt_spliced_trace_binds_t240_as_a_root_srv_with_bounded_root_srvs() {
    let test = "rt_spliced_trace_binds_t240_as_a_root_srv_with_bounded_root_srvs";
    let Some((gpu, rays)) = dxr(test) else {
        return;
    };
    let host = gpu.host();
    // The command signature takes the parameter on any DXR device.
    let (param, root) = spliced_root(host);
    assert!(dispatch::signature(&root, param, 20).is_some());
    if !rays {
        return;
    }
    let world = World::new();
    let slot = Descriptor::new(world.tlas.address());
    let list = TitleList::new();
    let out = unwritten();
    trace(host, &list, Scene::Spliced(slot.handle()), &out);
    list.run();
    assert_eq!(out.words(), hit());
}

#[test]
fn rt_indirect_trace_takes_the_groups_the_gpu_wrote() {
    let test = "rt_indirect_trace_takes_the_groups_the_gpu_wrote";
    let Some((gpu, rays)) = dxr(test) else {
        return;
    };
    let host = gpu.host();
    let world = World::new();
    let slot = Descriptor::new(world.tlas.address());
    let list = TitleList::new();
    // The title's CPU leaves zero groups; the GPU writes one.
    let (written, args) = (TitleBuffer::new(16), TitleBuffer::new(16));
    written.set_words(|i| u32::from(i < 3));
    args.shadow();
    assert!(crate::shadow::copy(
        list.host(),
        args.address(),
        written.address(),
        12
    ));
    let none = TitleBuffer::new(4);
    // SAFETY: the test's live list and descriptor slot.
    unsafe {
        // Arguments must be dword-aligned title memory.
        assert!(
            prepare_indirect_tlas(
                list.this(),
                slot.handle(),
                (args.address() + 2, 12, 1),
                None
            )
            .is_none()
        );
    }
    if !rays {
        return;
    }
    let (traced, uncounted) = (unwritten(), unwritten());
    trace(
        host,
        &list,
        Scene::Indirect(slot.handle(), args.address(), None),
        &traced,
    );
    // A command count of zero runs no command.
    trace(
        host,
        &list,
        Scene::Indirect(slot.handle(), args.address(), Some(none.address())),
        &uncounted,
    );
    list.run();
    assert_eq!(traced.words(), hit());
    assert_eq!(uncounted.words(), UNWRITTEN);
}

#[test]
fn rt_missing_tlas_writes_every_ray_as_a_miss() {
    let test = "rt_missing_tlas_writes_every_ray_as_a_miss";
    let Some((gpu, rays)) = dxr(test) else {
        return;
    };
    if !rays {
        return;
    }
    let host = gpu.host();
    let world = World::new();
    // A title address where no TLAS was ever published, and a BLAS address,
    // which is no TLAS either.
    let nothing = TitleBuffer::new(256);
    let blas = world.blas.address();
    let (direct, spliced, indirect, not_a_tlas) =
        (unwritten(), unwritten(), unwritten(), unwritten());
    let empty = Descriptor::new(nothing.address());
    let wrong = Descriptor::new(blas);
    let args = TitleBuffer::new(12);
    args.set_words(|_| 1);
    let list = TitleList::new();
    trace(host, &list, Scene::Descriptor(empty.handle()), &direct);
    trace(host, &list, Scene::Spliced(empty.handle()), &spliced);
    trace(
        host,
        &list,
        Scene::Indirect(empty.handle(), args.address(), None),
        &indirect,
    );
    trace(host, &list, Scene::Descriptor(wrong.handle()), &not_a_tlas);
    list.run();
    for out in [&direct, &spliced, &indirect, &not_a_tlas] {
        assert_eq!(out.words(), MISSES, "the program ran and wrote its misses");
    }
}

#[test]
fn rt_retirement_cancels_overlapping_publications_and_keeps_adjacent_ranges() {
    let mut table = Registry::default();
    let base = 0x4000;
    for offset in [0, 256, 512, 768] {
        table.begin(base + offset, Some(256));
    }
    let old = table.ranges[&(base + 256)].generation;
    let (new, _) = table.begin(base + 128, Some(384));
    assert!(!table.finish(base + 256, old, None));
    assert_eq!(
        table.ranges.keys().copied().collect::<Vec<_>>(),
        [base + 128, base + 512, base + 768]
    );
    table.retire(base + 180, base + 181, Some(None));
    assert!(!table.finish(base + 128, new.generation, None));
    assert_eq!(table.ranges.len(), 2);
}

#[test]
fn rt_owner_survives_more_than_64_intervening_buffers() {
    let base = 0x9876_0000_0000;
    crate::shadow::register(1, base, 1024);
    for i in 1..=100 {
        crate::shadow::register(i + 1, base + 2 * i as u64, 1);
    }
    assert_eq!(
        crate::shadow::buffer_owner(base + 255, 1),
        Some((1, base + 1024))
    );
    assert_eq!(
        crate::shadow::buffer_owner(base + 200, 1),
        Some((101, base + 201))
    );
    assert_eq!(
        crate::shadow::buffer_owner(base + 200, 16),
        Some((1, base + 1024))
    );
    registry().begin(base + 255, Some(128));
    crate::shadow::unregister(1, base);
    assert!(!registry().ranges.contains_key(&(base + 255)));
    for i in 1..=100 {
        crate::shadow::unregister(i + 1, base + 2 * i as u64);
    }
}
