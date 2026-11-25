//! Vérifie le trajet complet des identifiants GFX10 sur le GPU de test.

use super::HostPipeline;
use crate::indirect::{self, Expanded};
use crate::pipeline::{Shader, Translated};
use std::sync::{Arc, OnceLock};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS;
use xtp_shader::native::{ComputeIo, StageIo};

fn counting(axis: usize) -> HostPipeline {
    let (container, template) = crate::shader_cache::wave64_program(false);
    // s0 désigne la table UAV, s1 contient l’unique composante de GroupID demandée.
    // Chaque groupe écrit son identifiant, puis incrémente son compteur de passages.
    let words: [u32; 14] = [
        0xBE82_0300,
        0xBE83_0380,
        0xF408_0101,
        0xFA00_0000,
        0x9308_FF01,
        8,           // s_mul_i32 s8, s1, 8
        0x7E06_0208, // v3 = adresse dans le tampon
        0x7E04_0201, // v2 = identifiant
        0xE070_1000,
        0x8001_0203, // store v2 à l’adresse v3
        0x7E04_0281, // v2 = 1
        0xE0C8_1004,
        0x8001_0203, // atomic_add au dword suivant
        0xBF81_0000,
    ];
    let native = xtp_shader::NativeProgram {
        wave64: false,
        float_mode: 0,
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 | 1 << (7 + axis),
            threads: [1, 1, 1],
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
        ..template
    };
    let shader = xtp_shader::emit::translate_program(&container, &native).expect("traduction");
    let shader = Arc::new(Shader {
        stage: "compute",
        hash: format!("group-id-{axis}"),
        container: Vec::new(),
        native: OnceLock::new(),
        translated: OnceLock::from(Some(Translated {
            host: shader,
            used: vec![],
            wave64: false,
        })),
    });
    HostPipeline::new(&[shader], None).expect("pipeline de comptage")
}

#[test]
fn every_group_runs_once_with_its_guest_id() {
    let test = "pipeline::host::indirect_dispatch_tests::every_group_runs_once_with_its_guest_id";
    let Some(gpu) = crate::testgpu::device(test) else {
        return;
    };
    let host = gpu.host();
    for (axis, n) in [(0, 70000u32), (1, 65536), (2, (1 << 20) + 1)] {
        let p = counting(axis);
        let base = p
            .group_base()
            .expect("base des groupes dans les constantes");
        let (param, dwords) = p.stages[0].user_data.unwrap();
        let user = vec![0u32; dwords as usize];
        let mut dims = [1; 3];
        dims[axis] = n;
        for indirect in [false, true] {
            let list = host.new_list().unwrap();
            let (output, data) = host.shared_buffer(u64::from(n) * 8).unwrap();
            let (args, mapped) = host.shared_buffer(16).unwrap();
            let mut held = None;
            // SAFETY: tampons mappés, descriptions bornées, objets vivants jusqu’à drain().
            unsafe {
                std::ptr::copy_nonoverlapping(dims.as_ptr(), mapped.cast(), 3);
                let heap: ID3D12DescriptorHeap = host
                    .device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: 1,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
                    .unwrap();
                let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            NumElements: 2 * n,
                            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                            ..Default::default()
                        },
                    },
                };
                host.device.CreateUnorderedAccessView(
                    &output,
                    None,
                    Some(&desc),
                    heap.GetCPUDescriptorHandleForHeapStart(),
                );
                if indirect {
                    let scratch = Expanded::reserve(&mut held, &list, 1).unwrap();
                    assert!(indirect::rewrite(
                        &list,
                        args.GetGPUVirtualAddress(),
                        (2, 12, 1),
                        None,
                        Some(scratch),
                        None
                    ));
                    indirect::ready(&list, &scratch.buffer);
                }
                let l = &list.list;
                l.SetDescriptorHeaps(&[Some(heap.clone())]);
                l.SetComputeRootSignature(&p.root);
                l.SetPipelineState(&p.pso);
                l.SetComputeRoot32BitConstants(param, dwords, user.as_ptr().cast(), 0);
                if let Some(scratch) = held.as_ref() {
                    let sig = indirect::signature(2, &p.root, Some(base)).unwrap();
                    list.keep(&sig);
                    l.ExecuteIndirect(
                        &sig,
                        scratch.capacity,
                        &scratch.buffer,
                        4,
                        &scratch.buffer,
                        0,
                    );
                } else {
                    indirect::direct(&list, dims, Some(base), false, |[x, y, z]| {
                        l.Dispatch(x, y, z)
                    });
                }
                list.close();
                host.execute(&[&list]);
                host.drain();
                let words = std::slice::from_raw_parts(data.cast::<u32>(), 2 * n as usize);
                for (id, &pair) in words.as_chunks::<2>().0.iter().enumerate() {
                    assert_eq!(
                        pair,
                        [id as u32, 1],
                        "axe {axis}, indirect {indirect}, groupe {id}"
                    );
                }
            }
        }
    }
}
