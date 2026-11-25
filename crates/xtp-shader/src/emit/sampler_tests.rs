//! Synthetic program adapted from VIctory #1477 (2286e038, geiyiliyabumo).

use super::container::{SigElement, signature};
use super::{HostShader, StaticSampler, translate_program};
use crate::dxbc::{self, Container, FourCC};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};

const SAMPLE: u32 = 0xF080_0108;
const SAMPLE_C: u32 = 0xF0A0_0108;
const SAMPLE_C_LZ: u32 = 0xF0BC_0108;
const LESS_EQUAL: u32 = 3 << 12;

fn mixed_sampler(compute: bool, comparison_first: bool, word0: u32) -> HostShader {
    let mut words = Vec::new();
    if compute {
        // Keep 16 × the thread id in v7 for the final write.
        words.push(0x340E_0084); // v_lshlrev_b32 v7, 4, v0
    }
    words.push(0xBE81_0380); // s_mov_b32 s1, 0
    if compute {
        words.extend([0xF408_0300, 0xFA00_0020]); // s_load_dwordx4 s[12:15], s[0:1], 32
    }
    words.extend([
        0xF40C_0000,
        0xFA00_0000, // s_load_dwordx8 s[0:7], s[0:1], 0
        0xBE88_03FF,
        word0, // s_mov_b32 s8, word0
        0xBE89_0380,
        0xBE8A_0380,
        0xBE8B_0380, // s[9:11] = 0
        0x7E00_02F0,
        0x7E02_02F0,
        0x7E04_02F0, // v0 (reference), v1 and v2 (UV) = 0.5
        0xBF8C_0070,
    ]);
    let plain = [SAMPLE, 0x0040_0301];
    let compare = [SAMPLE_C, 0x0040_0400];
    if comparison_first {
        words.extend(compare);
        words.extend(plain);
    } else {
        words.extend(plain);
        words.extend(compare);
    }
    words.extend([
        0x7E00_02F2, // v0 = 1, second reference
        SAMPLE_C_LZ,
        0x0040_0500, // v5
        plain[0],
        0x0040_0601, // v6
        0xBF8C_3F70,
    ]);
    let io = if compute {
        words.extend([0xE078_1000, 0x8003_0307]); // buffer_store_dwordx4 v[3:6], v7, s[12:15]
        StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [4, 1, 1],
        })
    } else {
        words.extend([0xF800_180F, 0x0605_0403]); // exp mrt0, v3, v4, v5, v6
        StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            rsrc2: 0,
            db_shader_control: 0,
            patches: vec![],
        }
    };
    words.push(0xBF81_0000);
    let table_entry = |kind, table_offset_dwords, dwords| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 0,
        dwords,
        user_data: 0,
        slot: 0,
    };
    let mut used = vec![table_entry(ResourceType::Srv, 0, 8)];
    if compute {
        used.push(table_entry(ResourceType::Uav, 8, 4));
    }
    let native = NativeProgram {
        stage: if compute { HwStage::Cs } else { HwStage::Ps },
        wave64: false,
        float_mode: 0,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used,
        io,
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let bytes = if compute {
        dxbc::build(&[])
    } else {
        let output = SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 15,
            half: false,
        };
        dxbc::build(&[(FourCC::OSG1, &signature(&[output], false))])
    };
    translate_program(&Container::parse(&bytes).unwrap(), &native).unwrap()
}

#[test]
fn immediate_sampler_slots_distinguish_comparison_uses() {
    for reduction in [0, 1, 2] {
        let words = [LESS_EQUAL | reduction << 29, 0, 0, 0];
        let plain = StaticSampler::from_words(words);
        let comparison = StaticSampler {
            filter: 0x80,
            ..plain
        };
        for comparison_first in [false, true] {
            let shader = mixed_sampler(false, comparison_first, words[0]);
            assert_eq!(shader.samplers, [words; 2]);
            assert_eq!(
                shader.static_samplers,
                if comparison_first {
                    [comparison, plain]
                } else {
                    [plain, comparison]
                }
            );
            #[cfg(windows)]
            if let Some(path) = super::validator::Validator::default_path() {
                super::validator::Validator::load(Some(&path))
                    .unwrap()
                    .validate(&mut shader.dxil.clone())
                    .unwrap();
            }
        }
    }
}

#[cfg(windows)]
#[test]
fn immediate_sampler_reads_depth_and_compares() {
    use super::host::{Dispatch, Host, Root, View};

    let _lock = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let host = match Host::new(false) {
        Ok(host) => host,
        Err(e) => {
            assert!(std::env::var_os("XTP_REQUIRE_GPU").is_none(), "{e}");
            eprintln!("skipped: {e}");
            return;
        }
    };
    for comparison_first in [false, true] {
        let shader = mixed_sampler(true, comparison_first, LESS_EQUAL);
        let (root, pso) = host.compute_pipeline(&shader).unwrap();
        let result = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(vec![0]))],
                    views: &[View::Depth(1, 1, &[0.75]), View::RawUav(16)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        // Each thread reads the texel, compares 0.5 then 1 with its depth, and reads it again.
        let expected = [0.75f32, 1.0, 0.0, 0.75].map(f32::to_bits).repeat(4);
        assert_eq!(result, [expected], "comparison_first={comparison_first}");
    }
}
