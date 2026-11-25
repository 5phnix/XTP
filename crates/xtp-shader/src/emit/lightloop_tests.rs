//! The light loop's preparation (11eeb27c2d15980d): an M0-indexed VGPR array
//! (`v_movrels_b32`, `v_movreld_b32`) whose registers past the elements the
//! program uses hold a descriptor's dwords, which the console compiler
//! copied there from a run of SGPRs and which are written again before any
//! other read (`Lower::unused_element`). M0 must be proven below such an
//! element wherever `v_movrels_b32` may read it (`Spans`): in that program
//! a loop counter clamped to 0..11 that leaves the loop past its end.
//! Synthetic programs of that pattern, lowered, validated and run on the
//! hardware adapter and WARP.

use super::host::{Dispatch, Host, Root, View};
use super::valu_tests::ONE_AT_A_TIME;
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const M0: u8 = 124;
const EXEC_LO: u8 = 126;
const DESCRIPTOR_WORD: &str = "a descriptor's address or format read as data";

/// SOP1 `s_mov_b32 sdst, src0`.
const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

/// SOP2 `op sdst, src0, src1`.
const fn sop2(op: u8, sdst: u8, src0: u8, src1: u8) -> u32 {
    0x8000_0000
        | (op as u32 & 0x7F) << 23
        | (sdst as u32 & 0x7F) << 16
        | (src1 as u32) << 8
        | src0 as u32
}

/// VOP1 `op vdst, src0` (`src0`: 0..105 an SGPR, 256..511 a VGPR).
const fn vop1(op: u16, vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// VOP2 `op vdst, src0, vsrc1`.
const fn vop2(op: u8, vdst: u8, src0: u16, vsrc1: u8) -> u32 {
    (op as u32 & 0x3F) << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// VOPC `op vcc/exec, src0, vsrc1`.
const fn vopc(op: u8, src0: u16, vsrc1: u8) -> u32 {
    0x7C00_0000 | (op as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// VOP3 `op vdst, src0, src1, src2` (a compare's `vdst` is its SGPR).
const fn vop3(op: u16, vdst: u8, src0: u16, src1: u16, src2: u16) -> [u32; 2] {
    [
        0xD400_0000 | (op as u32 & 0x3FF) << 16 | vdst as u32,
        (src0 as u32 & 0x1FF) | (src1 as u32 & 0x1FF) << 9 | (src2 as u32 & 0x1FF) << 18,
    ]
}

/// SOPP `op simm16`: a branch `off` dwords past the next instruction.
const fn sopp(op: u8, off: i16) -> u32 {
    0xBF80_0000 | (op as u32) << 16 | off as u16 as u32
}

/// An inline integer constant, 0..=64.
const fn int(k: u8) -> u16 {
    128 + k as u16
}

const fn sgpr(n: u8) -> u16 {
    n as u16
}

const fn vgpr(n: u8) -> u16 {
    256 + n as u16
}

const V_MOV_B32: u16 = 0x001;
const V_READFIRSTLANE_B32: u16 = 0x002;
const V_MOVRELD_B32: u16 = 0x042;
const V_MOVRELS_B32: u16 = 0x043;
const V_XOR_B32: u8 = 0x1D;
const V_ADD_NC_U32: u8 = 0x25;
const V_FMAC_F32: u8 = 0x2B;
const V_CMP_EQ_U32: u16 = 0x0C2;
const V_CMPX_GT_U32: u8 = 0xD4;
const V_CMPX_NE_U32: u8 = 0xD5;
const V_MED3_U32: u16 = 0x159;
const S_AND_B32: u8 = 0x0E;
const S_XOR_B32: u8 = 0x12;
const S_ANDN2_B32: u8 = 0x14;
const S_AND_SAVEEXEC_B32: u8 = 0x3C;
const S_BRANCH: u8 = 0x02;
const S_CBRANCH_SCC1: u8 = 0x05;
const S_CBRANCH_EXECNZ: u8 = 0x09;

/// SOP1 `op sdst, src0`.
const fn sop1(op: u8, sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | (op as u32) << 8 | src0 as u32
}

/// The array v5..v7 = {s0, s1, s8}: two elements and, past them, the first
/// dword of the UAV the program stores to (its address).
const FILL: [u32; 3] = [
    vop1(V_MOV_B32, 5, vgpr(1)),
    vop1(V_MOV_B32, 6, vgpr(2)),
    vop1(V_MOV_B32, 7, sgpr(8)),
];

/// A loop that sets the first 1 + (s3 & 1) elements to s2, one a turn:
/// the array's registers join at its header.
const UPDATE: [u32; 8] = [
    sop2(0x0E, 12, 3, 0x81),         // s_and_b32 s12, s3, 1
    sop2(0x00, 12, 12, 0x81),        // s_add_u32 s12, s12, 1
    s_mov_b32(13, 0x80),             // s_mov_b32 s13, 0
    s_mov_b32(M0, 13),               // loop: s_mov_b32 m0, s13
    vop1(V_MOVRELD_B32, 5, vgpr(3)), // v_movreld_b32 v5, v3
    sop2(0x00, 13, 13, 0x81),        // s_add_u32 s13, s13, 1
    0xBF0A_0C0D,                     // s_cmp_lt_u32 s13, s12
    0xBF85_FFFB,                     // s_cbranch_scc1 loop
];

/// v0 = element s0 & 1.
const READ: [u32; 2] = [
    sop2(0x0E, M0, 0, 0x81),         // s_and_b32 m0, s0, 1
    vop1(V_MOVRELS_B32, 0, vgpr(5)), // v_movrels_b32 v0, v5
];

/// The array's registers given to another value: v7 written, its old value
/// (the descriptor's dword) not read; v0 = v0 ^ v1.
const REUSE: [u32; 2] = [
    vop2(V_XOR_B32, 7, vgpr(0), 1), // v_xor_b32 v7, v0, v1
    vop1(V_MOV_B32, 0, vgpr(7)),    // v_mov_b32 v0, v7
];

/// The light loop's preparation in small, its end clamped to
/// `0..=end_max`: a counter goes from s2 to s3, both clamped by
/// `v_med3_u32` (start to 0..=1), and each turn adds element `counter` to
/// v0 and stores the sum there, both through a waterfall loop on the
/// counter's first lane (`v_readfirstlane_b32` into M0); the lanes past the
/// end leave (`v_cmpx_gt_u32`, the loop's lanes in s20), as at 0x47c. The
/// moves name v5..v11: the descriptor's dword at v7 (element 2), the
/// counter at v10 (element 5), so `v_movreld_b32` rewrites the counter.
fn counted(end_max: u8) -> Vec<u32> {
    let mut w = Vec::new();
    w.extend(vop3(V_MED3_U32, 8, vgpr(3), int(0), int(1))); // v8: start
    w.extend(vop3(V_MED3_U32, 9, vgpr(4), int(0), int(end_max))); // v9: end
    w.push(vop1(V_MOV_B32, 0, int(0))); // v_mov_b32 v0, 0
    w.push(s_mov_b32(21, EXEC_LO)); // s_mov_b32 s21, exec_lo
    w.push(s_mov_b32(20, EXEC_LO)); // s_mov_b32 s20, exec_lo
    w.push(vop1(V_MOV_B32, 10, vgpr(8))); // v_mov_b32 v10, v8
    let enter = w.len();
    w.push(0); // s_branch body
    let latch = w.len();
    w.push(sop2(S_AND_B32, EXEC_LO, 23, 20)); // latch: s_and_b32 exec_lo, s23, s20
    w[enter] = sopp(S_BRANCH, (w.len() - enter - 1) as i16);
    // body: v11 = element v10; v0 += v11; element v10 = v0.
    w.extend(waterfall(vop1(V_MOVRELS_B32, 11, vgpr(5))));
    w.push(vop2(V_ADD_NC_U32, 0, vgpr(0), 11)); // v_add_nc_u32 v0, v0, v11
    w.extend(waterfall(vop1(V_MOVRELD_B32, 5, vgpr(0))));
    w.push(vop2(V_ADD_NC_U32, 10, int(1), 10)); // v_add_nc_u32 v10, 1, v10
    w.push(s_mov_b32(23, EXEC_LO)); // s_mov_b32 s23, exec_lo
    w.push(vopc(V_CMPX_GT_U32, vgpr(10), 9)); // v_cmpx_gt_u32 v10, v9
    w.push(sop2(S_ANDN2_B32, 20, 20, EXEC_LO)); // s_andn2_b32 s20, s20, exec_lo
    let back = (latch as isize - w.len() as isize - 1) as i16;
    w.push(sopp(S_CBRANCH_SCC1, back)); // s_cbranch_scc1 latch
    w.push(s_mov_b32(EXEC_LO, 21)); // s_mov_b32 exec_lo, s21
    w
}

/// `mov` (M0 relative) once for each value of v10 among the lanes, M0 that
/// value, as the console compiler emits it; EXEC saved in s25.
fn waterfall(mov: u32) -> Vec<u32> {
    let mut w = vec![
        s_mov_b32(25, EXEC_LO),                  // s_mov_b32 s25, exec_lo
        vop1(V_READFIRSTLANE_B32, 22, vgpr(10)), // loop: v_readfirstlane_b32 s22, v10
    ];
    w.extend(vop3(V_CMP_EQ_U32, 24, sgpr(22), vgpr(10), 0)); // v_cmp_eq_u32 s24, s22, v10
    w.extend([
        sop1(S_AND_SAVEEXEC_B32, 24, 24), // s_and_saveexec_b32 s24, s24
        s_mov_b32(M0, 22),                // s_mov_b32 m0, s22
        mov,
        sop2(S_XOR_B32, EXEC_LO, EXEC_LO, 24), // s_xor_b32 exec_lo, exec_lo, s24
    ]);
    let back = 1 - w.len() as i16 - 1;
    w.push(sopp(S_CBRANCH_EXECNZ, back)); // s_cbranch_execnz loop
    w.push(s_mov_b32(EXEC_LO, 25)); // s_mov_b32 exec_lo, s25
    w
}

/// What [`counted`] leaves in v0 where `end_max` is 1, for root constants
/// `[s0, s1, start, end]`.
fn summed([a, b, start, end]: [u32; 4]) -> u32 {
    let mut elements = [a, b];
    let (mut k, end) = (start.min(1) as usize, end.min(1) as usize);
    let mut sum = 0u32;
    loop {
        sum = sum.wrapping_add(elements[k]);
        elements[k] = sum;
        k += 1;
        if k > end {
            return sum;
        }
    }
}

const LOOP_CASES: [[u32; 4]; 5] = [
    [10, 21, 0, 0],
    [10, 21, 0, 1],
    [10, 21, 1, 1],
    [10, 21, 1, 0],
    [10, 21, 7, 9],
];

/// Lowers a one-thread compute program: s0..s3 root constants, s[8:11] the
/// raw UAV at heap index 0 (its descriptor loaded from the table at s4),
/// v1..v4 = s0..s3, then `body`, then v0 stored to the UAV's dword 0.
fn lower_cs(body: &[u32]) -> Result<HostShader, ShaderError> {
    lower_with(body, 8)
}

/// [`lower_cs`] with `vgprs` VGPRs.
fn lower_with(body: &[u32], vgprs: u16) -> Result<HostShader, ShaderError> {
    let mut words = vec![
        s_mov_b32(5, 0x80), // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 4 >> 1,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    for k in 0..4u8 {
        words.push(vop1(V_MOV_B32, k + 1, sgpr(k)));
    }
    words.extend_from_slice(body);
    // buffer_store_dword v0, off, s[8:11], 0
    words.extend_from_slice(&[0x38 << 26 | 0x1C << 18, 0x80 << 24 | 2 << 16]);
    words.push(S_ENDPGM);
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        float_mode: 0,
        vgprs,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
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
    })?;
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        group_base: lowered.group_base,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        stride_table: lowered.stride_table,
        run_time_buffers: lowered.run_time_buffers,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    Ok(shader)
}

/// What the lowering of `parts` (concatenated) stops with, if anything.
fn refusal(parts: &[&[u32]]) -> Option<&'static str> {
    refusal_with(parts, 8)
}

/// [`refusal`] with `vgprs` VGPRs.
fn refusal_with(parts: &[&[u32]], vgprs: u16) -> Option<&'static str> {
    match lower_with(&parts.concat(), vgprs) {
        Ok(_) => None,
        Err(ShaderError::Unsupported { what, .. }) => Some(what),
        Err(e) => panic!("{e}"),
    }
}

/// Runs `shader` for each `[s0, s1, s2, s3]` on the hardware adapter and
/// WARP, and checks what it stores.
fn run(shader: &HostShader, cases: &[[u32; 4]], want: impl Fn([u32; 4]) -> u32) {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        for &case in cases {
            let mut user = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                user[u.index as usize] = match (u.root, u.slot) {
                    (RootType::Constants, 0) => case[u.dword as usize],
                    (RootType::Table, 1) => 0,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(user))],
                        views: &[View::RawUav(1)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(out[0][0], want(case), "{label} {case:?}");
        }
        ran += 1;
    }
    assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available");
}

const CASES: [[u32; 4]; 4] = [
    [10, 21, 32, 0],
    [11, 21, 32, 0],
    [10, 21, 32, 1],
    [11, 21, 32, 1],
];

/// Element `a & 1` of the array after [`UPDATE`]: the first is s2, the
/// second s2 where s3 is odd, else s1.
fn element([a, b, x, n]: [u32; 4]) -> u32 {
    [x, if n & 1 == 1 { x } else { b }][a as usize & 1]
}

/// The pattern of the light loop's preparation: a descriptor's dword only
/// fills an element of the M0-indexed array past those the program reads,
/// reaches the array loop's joins, and its register is then written again
/// (its old value listed as a read of the write, not used). The program
/// translates, validates and reads the elements it names.
#[test]
fn a_descriptor_dword_only_in_an_unused_array_element_then_overwritten_translates() {
    assert_eq!(refusal(&[&FILL, &READ]), None, "without a join");
    let shader = lower_cs(&[&FILL[..], &UPDATE, &READ].concat()).expect("translated");
    run(&shader, &CASES, element);
    let reused = lower_cs(&[&FILL[..], &UPDATE, &READ, &REUSE].concat()).expect("translated");
    run(&reused, &CASES, |c| element(c) ^ c[0]);
    // v_movrels_b32 v7, v5: the destination is the element's own register,
    // read where M0 is 2; M0 is s0 & 1, and v7 then holds element 0 or 1.
    let onto = [vop1(V_MOVRELS_B32, 7, vgpr(5)), vop1(V_MOV_B32, 0, vgpr(7))];
    let shader = lower_cs(&[&FILL[..], &READ[..1], &onto].concat()).expect("translated");
    run(&shader, &CASES, |[a, b, ..]| [a, b][a as usize & 1]);
}

/// The light loop's own bound: M0 is a counter whose start and end are
/// clamped to 0..=1, and only the lanes that have not passed the end go
/// round again, so it never names element 2 (the descriptor's dword), though
/// `v_movreld_b32` rewrites the counter's register. The program translates
/// and sums the elements the counter names.
#[test]
fn a_counter_bounded_by_its_loop_exit_keeps_m0_below_the_descriptor_dword() {
    let shader = lower_with(&[&FILL[..], &counted(1)].concat(), 12).expect("translated");
    run(&shader, &LOOP_CASES, summed);
}

/// Where M0 may name the element, the guest reads the descriptor's dword
/// there, which the translation would replace with 0: the program stops as
/// before (the counter-example of the review of 440d7e4, and the ways a
/// bound can fail).
#[test]
fn m0_that_may_name_the_descriptor_dword_stops_the_program() {
    let why = Some(DESCRIPTOR_WORD);
    let m0_is_2 = s_mov_b32(M0, int(2) as u8); // s_mov_b32 m0, 2
    // M0 = 2: v_movrels_b32 v0, v5 reads v7.
    assert_eq!(refusal(&[&FILL, &[m0_is_2, READ[1]]]), why);
    // M0 = s0 & 3, which may be 2.
    let and_3 = sop2(S_AND_B32, M0, 0, int(3) as u8); // s_and_b32 m0, s0, 3
    assert_eq!(refusal(&[&FILL, &[and_3, READ[1]]]), why);
    // A join where one predecessor leaves M0 at 2, the other at s0 & 1.
    let joined = [
        READ[0],                 // s_and_b32 m0, s0, 1
        0xBF06_8001,             // s_cmp_eq_u32 s1, 0
        sopp(S_CBRANCH_SCC1, 1), // s_cbranch_scc1 read
        m0_is_2,                 // s_mov_b32 m0, 2
        READ[1],                 // read: v_movrels_b32 v0, v5
    ];
    assert_eq!(refusal(&[&FILL, &joined]), why);
    // v_movrels_b32 v7, v5 with M0 = 2: the dword copied onto itself.
    let onto = [
        m0_is_2,
        vop1(V_MOVRELS_B32, 7, vgpr(5)),
        vop1(V_MOV_B32, 0, vgpr(7)),
    ];
    assert_eq!(refusal(&[&FILL, &onto]), why);
    // The light loop with its end clamped to 0..=2: the counter reaches 2.
    assert_eq!(refusal_with(&[&FILL, &counted(2)[..]], 12), why);
}

/// Every other read of the dword stays a read of the descriptor's address,
/// which stops the program as before.
#[test]
fn a_descriptor_dword_an_array_element_uses_still_stops_the_program() {
    let why = Some(DESCRIPTOR_WORD);
    // v_xor_b32 v0, v0, v7: read as an operand.
    let xor_v7 = [vop2(V_XOR_B32, 0, vgpr(0), 7)];
    assert_eq!(refusal(&[&FILL, &READ, &xor_v7]), why);
    assert_eq!(refusal(&[&FILL, &UPDATE, &READ, &xor_v7]), why);
    // v_fmac_f32 v7, v1, v2 adds to the old value.
    let fmac = [vop2(V_FMAC_F32, 7, vgpr(1), 2), REUSE[1]];
    assert_eq!(refusal(&[&FILL, &UPDATE, &READ, &fmac]), why);
    // v_movrels_b32 v0, v7: the array's first element.
    let m0 = &READ[..1];
    assert_eq!(
        refusal(&[&FILL, m0, &[vop1(V_MOVRELS_B32, 0, vgpr(7))]]),
        why
    );
    // v_movreld_b32 v7, v3; v_movrels_b32 v0, v6: the first element of the
    // array the store names.
    let first = [
        vop1(V_MOVRELD_B32, 7, vgpr(3)),
        vop1(V_MOVRELS_B32, 0, vgpr(6)),
    ];
    assert_eq!(refusal(&[&FILL, m0, &first]), why);
    // v_mov_b32 v4, s8; v_movreld_b32 v5, v4: the dword is an element's value.
    let stored = [vop1(V_MOV_B32, 4, sgpr(8)), vop1(V_MOVRELD_B32, 5, vgpr(4))];
    assert_eq!(refusal(&[&FILL, m0, &stored, &READ]), why);
    // The register written again under a guard (v_cmpx), which keeps the
    // dword in the lanes outside it, then read: v7 holds the address there.
    let guarded = [
        s_mov_b32(14, EXEC_LO),             // s_mov_b32 s14, exec_lo
        vopc(V_CMPX_NE_U32, sgpr(0x80), 1), // v_cmpx_ne_u32 exec_lo, 0, v1
        REUSE[0],                           // v_xor_b32 v7, v0, v1
        s_mov_b32(EXEC_LO, 14),             // s_mov_b32 exec_lo, s14
        REUSE[1],                           // v_mov_b32 v0, v7
    ];
    assert_eq!(refusal(&[&FILL, &UPDATE, &READ, &guarded]), why);
}
