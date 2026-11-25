//! Lane masks read as numbers (`lower/mask.rs`, `structure.rs`): the
//! console compiler's test for a value the whole wave holds, in console
//! wave64 mesh programs. Hand-assembled GFX10 code, as in the other tests
//! of this crate: what each target's translation calls or refuses, checked
//! by dxil.dll where it is installed.
//!
//! The test (`v_readfirstlane`, `v_cmp_eq`, `s_xor` with EXEC, `s_bcnt1`,
//! `s_cbranch_scc1` to the general path) stopped every such mesh program
//! as "a lane mask read as data": mesh programs read no mask as a number.
//! Its count's SCC is now "some lane differs", each lane's own bit, and on
//! a host without 64-lane waves its first-lane read is each lane's own
//! value where no group exchange can run.

use super::{HostShader, Target};
use crate::ShaderError;
use crate::native::{MeshIo, NativeProgram, StageIo};

const S_ENDPGM: u32 = 0xBF81_0000;
/// `exp prim, v9, off, off, off done`.
const EXP_PRIM: [u32; 2] = [0xF800_0941, 0x0000_0009];

const NVIDIA: Target = Target {
    lanes_min: 32,
    lanes_max: 32,
    stride_table: false,
};
const RDNA: Target = Target {
    lanes_min: 32,
    lanes_max: 64,
    stride_table: false,
};

/// The lanes below 40 of a console wave64 mesh group of 64 run the test on
/// their key, lane / 8 (the same in eight lanes at a time, so the wave's
/// lanes differ), then take the scalar path (key + 100 from the scalar) or
/// the general one (key + 100 per lane); `count` puts the count itself in
/// v9 on the scalar path instead.
fn uniform_test(count: bool) -> NativeProgram {
    let words = [
        0xD765_0000,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v0, -1, 0
        0xD766_0000,
        0x0002_00C1, // v_mbcnt_hi_u32_b32 v0, -1, v0
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
        0xBF88_000C, // s_cbranch_execz -> 76
        0x2C02_0083, // v_lshrrev_b32 v1, 3, v0
        0x7E00_0501, // v_readfirstlane_b32 s0, v1
        0x7D84_0200, // v_cmp_eq_u32 vcc, s0, v1
        0x8982_7E6A, // s_xor_b64 s[2:3], vcc, exec
        0xBE84_1002, // s_bcnt1_i32_b64 s4, s[2:3]
        0xBF85_0004, // s_cbranch_scc1 -> 68
        0x8005_FF00,
        0x0000_0064, // s_add_u32 s5, s0, lit(100)
        if count {
            0x7E12_0204 // v_mov_b32 v9, s4
        } else {
            0x7E12_0205 // v_mov_b32 v9, s5
        },
        0xBF82_0002, // s_branch -> 76
        0x4A12_02FF,
        0x0000_0064, // 68: v_add_nc_u32 v9, lit(100), v1
        0x88FE_0A7E, // 76: s_or_b64 exec, exec, s[10:11]
    ];
    let mut code = words.to_vec();
    code.extend(EXP_PRIM);
    code.push(S_ENDPGM);
    NativeProgram {
        stage: crate::HwStage::Ms,
        wave64: true,
        float_mode: 0,
        vgprs: 24,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Ms(MeshIo {
            outputs: Vec::new(),
            clip_cntl: 0,
            threads: 64,
            max_vertices: 64,
            max_primitives: 64,
            topology: 2,
            lds_bytes: 0,
        }),
        code: code.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

fn empty() -> crate::Container<'static> {
    crate::Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    }
}

/// `native` translated for `target`, and the `dx.op` operations its module
/// calls; checked by dxil.dll where it is.
fn translate(native: &NativeProgram, target: Target) -> Result<(HostShader, String), ShaderError> {
    let s = super::translate_program_for(&empty(), native, target).map_err(|(_, e)| e)?;
    let ops = super::last_module_summary();
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut s.dxil.clone())
            .unwrap_or_else(|e| panic!("validation: {e}\n{ops}"));
    }
    Ok((s, ops))
}

/// The hand-assembled program reads as written (a wrong word would test
/// another program).
#[test]
fn the_program_decodes_as_written() {
    let p = uniform_test(false);
    let text: Vec<String> = crate::gfx10::decode(&p.code)
        .unwrap()
        .iter()
        .map(|i| i.text(p.wave64))
        .collect();
    for (at, want) in [
        (2, "v_cmp_gt_u32 vcc, 40, v0"),
        (4, "s_cbranch_execz label_004C"),
        (5, "v_lshrrev_b32 v1, 3, v0"),
        (6, "v_readfirstlane_b32 s0, v1"),
        (7, "v_cmp_eq_u32 vcc, s0, v1"),
        (8, "s_xor_b64 s[2:3], vcc, exec"),
        (9, "s_bcnt1_i32_b64 s4, s[2:3]"),
        (10, "s_cbranch_scc1 label_0044"),
        (11, "s_add_u32 s5, s0, lit(0x00000064)"),
        (12, "v_mov_b32 v9, s5"),
        (13, "s_branch label_004C"),
        (14, "v_add_nc_u32 v9, lit(0x00000064), v1"),
        (15, "s_or_b64 exec, exec, s[10:11]"),
        (16, "exp prim, v9, off, off, off done"),
    ] {
        assert_eq!(text[at], want, "{text:#?}");
    }
}

/// The test translates for every host. Without 64-lane waves the
/// first-lane read is each lane's own value (lanes below 40 only reach it,
/// so no group exchange could run there): no wave operation, no barrier.
/// With them, the host wave's first lane as before.
#[test]
fn a_wave_uniform_test_translates_in_mesh_programs() {
    let p = uniform_test(false);
    let (_, ops) = translate(&p, NVIDIA).unwrap();
    assert!(!ops.contains("waveReadLaneFirst"), "{ops}");
    assert!(!ops.contains("barrier"), "{ops}");
    assert!(!ops.contains("waveActiveBallot"), "{ops}");
    for target in [RDNA, Target::ANY] {
        let (_, ops) = translate(&p, target).unwrap();
        assert!(!ops.contains("waveActiveBallot"), "{target:?}: {ops}");
    }
}

/// The count itself, read as a number, stays what it was: mesh programs
/// read no lane mask as data.
#[test]
fn the_count_read_as_a_number_stays_refused() {
    let p = uniform_test(true);
    for target in [NVIDIA, RDNA, Target::ANY] {
        match translate(&p, target) {
            Err(ShaderError::Unsupported { what, .. }) => {
                assert_eq!(what, "a lane mask read as data", "{target:?}")
            }
            Err(e) => panic!("{target:?}: {e}"),
            Ok((_, ops)) => panic!("{target:?}: translated: {ops}"),
        }
    }
}
