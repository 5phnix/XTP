//! End-to-end tests of descriptors whose dwords the code also moves or
//! joins as numbers (task meshdata): the synthetic GFX10 programs of
//! `resource::tests` lowered to DXIL, validated by `dxil.dll` when present,
//! and run on the host GPU and WARP.

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, Target, lower, validator};
use crate::ShaderError;
use crate::native::{
    ComputeIo, HwStage, MeshIo, NativeProgram, ResourceType, RootParam, RootType, StageIo,
    UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// Lowers a wave32 compute program of 32 threads with the table of views at
/// s0: structured buffers of dwords at entries 0 and 1, a raw UAV at entry
/// 2. `uniform` handles only, every access the whole wave's.
fn build(words: &[u32]) -> HostShader {
    let range = |kind, table_offset_dwords, stride| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride,
        dwords: 4,
        user_data: 0,
        slot: 0,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![
            range(ResourceType::Srv, 0, 4),
            range(ResourceType::Uav, 16, 0),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [32, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        target: super::Target::ANY,
        console_cf: false,
        per_lane_only: false,
    })
    .expect("lower");
    assert_eq!(lowered.nonuniform_handles, 0);
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        group_base: lowered.group_base,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        stride_table: lowered.stride_table,
        run_time_buffers: lowered.run_time_buffers,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    shader
}

/// Runs `shader` with `views` at heap indices 0.. (the UAV last) on the
/// hardware GPU and on WARP (each skipped without a device) and returns,
/// per one that ran, the UAV's 32 dwords.
fn run(shader: &HostShader, views: &[View]) -> Vec<(&'static str, Vec<u32>)> {
    let mut out = Vec::new();
    // One GPU test at a time (see `valu_tests::ONE_AT_A_TIME`).
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = match host.compute_pipeline(shader) {
            Ok(p) => p,
            Err(e) => panic!("{label}: compute_pipeline failed: {e}"),
        };
        // The table starts at heap index 0.
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let words = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        out.push((label, words[0].clone()));
    }
    out
}

/// `resource::tests::kept_per_lane`: the buffer both arms loaded from table
/// entry 0 and kept in VGPRs, read through a uniform handle in one pass of
/// the waterfall loop. Lanes below 24 (both arms) load and store dword
/// `lane` of the first buffer; the others hold numbers there, take no pass
/// and store nothing.
#[test]
fn lanes_read_the_buffer_both_arms_kept_in_vgprs() {
    use crate::resource::tests::{SecondArm, kept_per_lane};
    let shader = build(&kept_per_lane(SecondArm::SameEntry));
    let first: Vec<u32> = (0..32).map(|i| 100 + i).collect();
    let second: Vec<u32> = (0..32).map(|i| 200 + i).collect();
    let views = [
        View::Buffer {
            data: &first,
            stride: Stride::Structured(4),
        },
        View::Buffer {
            data: &second,
            stride: Stride::Structured(4),
        },
        View::RawUav(32),
    ];
    let results = run(&shader, &views);
    assert!(!results.is_empty());
    for (label, got) in results {
        let want: Vec<u32> = (0..32).map(|l| if l < 24 { 100 + l } else { 0 }).collect();
        assert_eq!(got, want, "{label}");
    }
}

/// What puts a number in s3 in the arm of [`mask_joined_with_a_number`]:
/// `s_add_i32 s3, s<sgpr>, 1`, from the root constant (s8 in a mesh
/// program, s0 in compute).
fn computed(sgpr: u32) -> u32 {
    0x8103_8100 | sgpr
}
const FIVE: u32 = 0xBE83_0385; // s_mov_b32 s3, 5

/// The skinned and shadow mesh programs' join (235e56869c1ea883 at 0x1980),
/// as a wave64 program with a root constant: s[2:3] is a lane mask
/// (lanes below 5, the EXEC of an arm); the arm sets s3 to `number` and a
/// flag in v1. After the arm, s3 joins the mask's high dword (the edge
/// that skips the arm) with that number, and only the flagged lanes, which
/// ran the arm, read it, into v9. `end`: what reads v9 at the end.
fn mask_joined_with_a_number(number: u32, end: &[u32]) -> Vec<u32> {
    let mut words = vec![
        0xD765_0000,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v0, -1, 0
        0x7E12_0280, // v_mov_b32 v9, 0
        0x7E02_0280, // v_mov_b32 v1, 0
        0x7D88_0085, // v_cmp_gt_u32 vcc, 5, v0
        0xBE82_046A, // s_mov_b64 s[2:3], vcc
        0xBE94_2402, // s_and_saveexec_b64 s[20:21], s[2:3]
        0xBF88_0002, // s_cbranch_execz -> the join
        number,
        0x7E02_0281, // v_mov_b32 v1, 1
        0xBEFE_0414, // the join: s_mov_b64 exec, s[20:21]
        0x7D84_0281, // v_cmp_eq_u32 vcc, 1, v1
        0xBE96_246A, // s_and_saveexec_b64 s[22:23], vcc
        0xBF88_0001, // s_cbranch_execz -> the end
        0x7E12_0203, // v_mov_b32 v9, s3
        0xBEFE_0416, // the end: s_mov_b64 exec, s[22:23]
    ];
    words.extend(end);
    words.push(S_ENDPGM);
    words
}

/// The root constant of [`mask_joined_with_a_number`] (user data 0).
fn constant() -> RootParam {
    RootParam {
        root: RootType::Constants,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data: 0,
        slot: 0,
    }
}

/// `words` as a wave64 mesh program of 64 lanes, translated for 32-lane
/// hosts, and checked by dxil.dll where it is; why not, if refused.
fn mesh(words: &[u32]) -> Result<(), &'static str> {
    let mut code = words.to_vec();
    // exp prim, v9, off, off, off done; then s_endpgm again.
    code.splice(code.len() - 1.., [0xF800_0941, 0x0000_0009, S_ENDPGM]);
    let native = NativeProgram {
        stage: HwStage::Ms,
        wave64: true,
        vgprs: 24,
        layout: vec![constant()],
        used: Vec::new(),
        io: StageIo::Ms(MeshIo {
            outputs: Vec::new(),
            clip_cntl: 0,
            threads: 64,
            max_vertices: 128,
            max_primitives: 128,
            topology: 2,
            lds_bytes: 0,
        }),
        code: code.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let parts = crate::Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let target = Target {
        lanes_min: 32,
        lanes_max: 32,
        stride_table: false,
    };
    match super::translate_program_for(&parts, &native, target) {
        Ok(s) => {
            if let Ok(mut v) =
                validator::Validator::load(validator::Validator::default_path().as_deref())
            {
                v.validate(&mut s.dxil.clone())
                    .unwrap_or_else(|e| panic!("validation: {e}"));
            }
            Ok(())
        }
        Err((_, ShaderError::Unsupported { what, .. })) => Err(what),
        Err((_, e)) => panic!("{e}"),
    }
}

/// `words` as a wave64 compute program of 64 lanes with 512 bytes of LDS,
/// lowered for any host; why not, if refused.
fn compute(words: &[u32]) -> Result<(), &'static str> {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 24,
        layout: vec![constant()],
        used: Vec::new(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 15 | 1 << 1,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    match super::tests::emit_for(&native, Target::ANY) {
        Ok(_) => Ok(()),
        Err(ShaderError::Unsupported { what, .. }) => Err(what),
        Err(e) => panic!("{e}"),
    }
}

const MASK_AS_DATA: &str = "a lane mask read as data";

/// The mask's dword on the edge that skips the arm is no number the
/// program reads: in a mesh program, where no ballot gives a mask as data,
/// the join takes an undefined value there, and the flagged lanes read the
/// arm's number. Not in compute (ballots are exact there, and the program
/// may read the mask's side), nor where the number is a constant (the
/// shape of `cond ? ballot : 5`): refused as before.
#[test]
fn a_mask_joined_with_a_computed_number_is_dead_outside_compute() {
    assert_eq!(mesh(&mask_joined_with_a_number(computed(8), &[])), Ok(()));
    assert_eq!(
        mesh(&mask_joined_with_a_number(FIVE, &[])),
        Err(MASK_AS_DATA)
    );
    // ds_write_b32 v0, v9 (by dword: v0 * 4 first)
    let store = [0x3400_0082, 0xD834_0000, 0x0000_0900];
    for number in [computed(0), FIVE] {
        assert_eq!(
            compute(&mask_joined_with_a_number(number, &store)),
            Err(MASK_AS_DATA)
        );
    }
}
