//! Parked scalars on hand-assembled GFX10 code: what comes back as moves,
//! what stays parked, and how the translation then lowers it.

use crate::ir::{Inst, PARKED, Program, Reg, build};

const S_ENDPGM: u32 = 0xBF81_0000;
/// `v_cmp_gt_u32 vcc, 20, v0`: lanes below 20.
const BELOW_20: u32 = 0x7D88_0094;
/// `s_and_saveexec_b64 s[10:11], vcc`.
const SAVEEXEC: u32 = 0xBE8A_246A;
/// `s_or_b64 exec, exec, s[10:11]`.
const RESTORE_EXEC: u32 = 0x88FE_0A7E;

/// `v_writelane_b32 v{vgpr}, s{sgpr}, lane`.
fn writelane(vgpr: u8, sgpr: u8, lane: u8) -> [u32; 2] {
    [
        0xD761_0000 | u32::from(vgpr),
        u32::from(sgpr) | (128 + u32::from(lane)) << 9,
    ]
}

/// `v_readlane_b32 s{sgpr}, v{vgpr}, lane`.
fn readlane(sgpr: u8, vgpr: u8, lane: u8) -> [u32; 2] {
    [
        0xD760_0000 | u32::from(sgpr),
        (256 + u32::from(vgpr)) | (128 + u32::from(lane)) << 9,
    ]
}

/// `s_mov_b32 s{dst}, k` for an inline integer `k` (0-64).
fn s_mov_k(dst: u8, k: u8) -> u32 {
    0xBE80_0380 | u32::from(dst) << 16 | u32::from(k)
}

/// `v_mov_b32 v{dst}, s{src}`.
fn v_mov_s(dst: u8, src: u8) -> u32 {
    0x7E00_0200 | u32::from(dst) << 17 | u32::from(src)
}

fn code(parts: &[&[u32]]) -> Vec<u8> {
    parts
        .concat()
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect()
}

fn ir(parts: &[&[u32]], wave64: bool) -> Program {
    build(&code(parts), wave64, 256).unwrap()
}

fn insts(p: &Program) -> Vec<&Inst> {
    p.blocks.iter().flat_map(|b| &b.insts).collect()
}

fn names(p: &Program) -> Vec<&'static str> {
    insts(p).iter().map(|i| i.name()).collect()
}

fn lane_ops(p: &Program) -> usize {
    names(p)
        .iter()
        .filter(|n| matches!(**n, "v_writelane_b32" | "v_readlane_b32"))
        .count()
}

/// The parked code `inst` writes or reads, if any.
fn parked_code(inst: &Inst) -> Option<u8> {
    inst.reads
        .iter()
        .chain(&inst.writes)
        .find_map(|r| match r.0 {
            Reg::S(c) if c >= PARKED => Some(c),
            _ => None,
        })
}

/// The moves to or from parked codes, by name.
fn parked_moves(p: &Program) -> Vec<&'static str> {
    insts(p)
        .into_iter()
        .filter(|i| parked_code(i).is_some())
        .map(|i| i.name())
        .collect()
}

/// A compute program of 64 threads, translated for a host whose waves are
/// `lanes` wide: the wave size it requires, and the form it is lowered in.
fn translate(parts: &[&[u32]], wave64: bool, lanes: (u32, u32)) -> (Option<u32>, String) {
    let native = crate::NativeProgram {
        stage: crate::HwStage::Cs,
        wave64,
        vgprs: 8,
        layout: vec![],
        used: vec![],
        io: crate::native::StageIo::Cs(crate::native::ComputeIo {
            rsrc2: 0,
            threads: [64, 1, 1],
        }),
        code: code(parts),
    };
    let c = crate::Container {
        digest: [0; 16],
        version: (1, 0),
        parts: vec![],
    };
    let target = crate::emit::Target {
        lanes_min: lanes.0,
        lanes_max: lanes.1,
    };
    let shader = crate::emit::translate_program_for(&c, &native, target)
        .unwrap_or_else(|(step, e)| panic!("{}: {e}", step.name()));
    (shader.wave_size(), crate::emit::last_module_summary())
}

const NVIDIA: (u32, u32) = (32, 32);
const ANY: (u32, u32) = (4, 128);

#[test]
fn parked_scalars_come_back_as_scalar_moves() {
    let p = ir(
        &[
            &[s_mov_k(9, 7)],
            &writelane(5, 9, 3),
            &[s_mov_k(9, 0)],
            &readlane(10, 5, 3),
            &[v_mov_s(2, 10), S_ENDPGM],
        ],
        false,
    );
    assert_eq!(
        names(&p),
        [
            "s_mov_b32",
            "s_mov_b32",
            "s_mov_b32",
            "s_mov_b32",
            "v_mov_b32",
            "s_endpgm"
        ]
    );
    let i = insts(&p);
    let slot = Reg::S(parked_code(i[1]).unwrap());
    // The save copies s9 as set before it; it reads neither EXEC nor v5.
    assert_eq!(i[1].reads, [(Reg::S(9), i[0].write(Reg::S(9)).unwrap())]);
    assert_eq!(i[3].read(slot), i[1].write(slot));
    assert_eq!(i[4].read(Reg::S(10)), i[3].write(Reg::S(10)));
    // What the program reads on entry: EXEC and the v2 lanes outside it.
    let inputs: Vec<Reg> = p.inputs.iter().map(|i| i.0).collect();
    assert_eq!(inputs, [Reg::S(crate::ir::EXEC), Reg::V(2)]);
}

/// `v_writelane` writes its lane whatever EXEC holds, as a move does. Lane
/// 40 is in the upper half of the console wave: on a 32-lane host the
/// lane read had to cross halves, the move needs no lane at all.
#[test]
fn a_save_under_a_partial_exec_is_the_same_move() {
    let parts: &[&[u32]] = &[
        &[BELOW_20, SAVEEXEC, s_mov_k(9, 7)],
        &writelane(5, 9, 40),
        &[RESTORE_EXEC],
        &readlane(12, 5, 40),
        &[v_mov_s(2, 12), S_ENDPGM],
    ];
    let p = ir(parts, true);
    assert_eq!(lane_ops(&p), 0, "{:?}", names(&p));
    let i = insts(&p);
    let (save, restore) = (i[3], i[5]);
    let slot = Reg::S(parked_code(save).unwrap());
    assert_eq!(restore.read(slot), save.write(slot));
    assert_eq!(save.read(Reg::S(crate::ir::EXEC)), None);
    let (wave, form) = translate(parts, true, NVIDIA);
    assert_eq!(wave, None, "{form}");
    assert!(form.ends_with("; per lane"), "{form}");
}

/// Saved only in a branch that lanes 20-63 skip, restored after it: the
/// console wave ran the branch, so every lane reads 7. Per lane, those
/// lanes would not have the save: the lane stays parked, and the program
/// keeps the console flow.
#[test]
fn a_scalar_saved_in_a_branch_some_lanes_skip_stays_parked() {
    let parts: &[&[u32]] = &[
        &[BELOW_20, SAVEEXEC, 0xBF88_0003], // s_cbranch_execz +3
        &[s_mov_k(9, 7)],
        &writelane(5, 9, 3),
        &[RESTORE_EXEC],
        &readlane(12, 5, 3),
        &[v_mov_s(2, 12), S_ENDPGM],
    ];
    let p = ir(parts, true);
    assert_eq!(lane_ops(&p), 2, "{:?}", names(&p));
    assert_eq!(parked_moves(&p), [] as [&str; 0]);
    let (_, form) = translate(parts, true, ANY);
    assert!(form.ends_with("; console flow"), "{form}");
}

/// A parked value read after a divergent join of two saves: 3 before the
/// branch, 7 in it. The console wave ran the branch: every lane reads 7.
/// Per lane, lanes 20-63 would read 3: the lane stays parked.
#[test]
fn a_parked_value_saved_on_both_sides_of_a_join_stays_parked() {
    let p = ir(
        &[
            &[s_mov_k(9, 3)],
            &writelane(5, 9, 3),
            &[BELOW_20, SAVEEXEC, 0xBF88_0003], // s_cbranch_execz +3
            &[s_mov_k(9, 7)],
            &writelane(5, 9, 3),
            &[RESTORE_EXEC],
            &readlane(12, 5, 3),
            &[v_mov_s(2, 12), S_ENDPGM],
        ],
        true,
    );
    assert_eq!(lane_ops(&p), 3, "{:?}", names(&p));
}

/// Saved once before the branch and restored after its join: every path
/// has the one save, so every lane reads it either way.
#[test]
fn a_value_saved_before_a_branch_is_restored_after_it() {
    let parts: &[&[u32]] = &[
        &[s_mov_k(9, 3)],
        &writelane(5, 9, 3),
        &[BELOW_20, SAVEEXEC, 0xBF88_0001], // s_cbranch_execz +1
        &[s_mov_k(9, 7)],
        &[RESTORE_EXEC],
        &readlane(12, 5, 3),
        &[v_mov_s(2, 12), S_ENDPGM],
    ];
    let p = ir(parts, true);
    assert_eq!(lane_ops(&p), 0, "{:?}", names(&p));
    assert_eq!(parked_moves(&p), ["s_mov_b32", "s_mov_b32"]);
    let (wave, form) = translate(parts, true, NVIDIA);
    assert_eq!(wave, None, "{form}");
    assert!(form.ends_with("; per lane"), "{form}");
}

/// A scalar parked across iterations: saved before the loop and at the
/// end of each iteration, restored at its start. Every lane that goes
/// round ran the save of the iteration before.
const COUNTED_LOOP: &[u32] = &[
    0xBE89_0380, // s_mov_b32 s9, 0
    0xD761_0005,
    0x0001_0609, // v_writelane_b32 v5, s9, 3
    // 12: loop
    0xD760_0009,
    0x0001_0705, // v_readlane_b32 s9, v5, 3
    0x8009_8109, // s_add_u32 s9, s9, 1
    0xD761_0005,
    0x0001_0609, // v_writelane_b32 v5, s9, 3
    0xBF07_8409, // s_cmp_lg_u32 s9, 4
    0xBF85_FFF9, // s_cbranch_scc1 loop
];

#[test]
fn a_scalar_parked_across_iterations_is_unparked() {
    let p = ir(&[COUNTED_LOOP, &[v_mov_s(2, 9), S_ENDPGM]], true);
    assert_eq!(lane_ops(&p), 0, "{:?}", names(&p));
    let head = &p.blocks[1];
    let [phi] = &head.phis[..] else {
        panic!("{:?}", head.phis)
    };
    assert!(matches!(phi.reg, Reg::S(c) if c >= PARKED), "{phi:?}");
    assert_eq!(head.insts[0].read(phi.reg), Some(phi.value));
    assert_eq!(phi.args[1], head.insts[2].write(phi.reg).unwrap());
}

/// Restored after the loop, the value is the last iteration's for the
/// console wave, the lane's own last one per lane: it stays parked.
#[test]
fn a_value_a_loop_defines_restored_after_it_stays_parked() {
    let p = ir(
        &[
            COUNTED_LOOP,
            &readlane(12, 5, 3),
            &[v_mov_s(2, 12), S_ENDPGM],
        ],
        true,
    );
    assert_eq!(lane_ops(&p), 4, "{:?}", names(&p));
}

/// The saved EXEC of a wave64 branch, parked in lanes 31 and 32 (one in
/// each half of the console wave), saved and restored with other work
/// between the halves: it comes back as the mask it is, a pair.
fn mask_in_two_lanes(between_restores: u32) -> Vec<Vec<u32>> {
    vec![
        vec![BELOW_20, SAVEEXEC],
        writelane(5, 10, 31).to_vec(),
        vec![s_mov_k(9, 7)],
        writelane(5, 11, 32).to_vec(),
        vec![s_mov_k(10, 0), s_mov_k(11, 0), v_mov_s(2, 9)],
        readlane(10, 5, 31).to_vec(),
        vec![between_restores],
        readlane(11, 5, 32).to_vec(),
        vec![RESTORE_EXEC, v_mov_s(2, 9), S_ENDPGM],
    ]
}

#[test]
fn a_wave64_mask_parked_in_two_lanes_comes_back_as_a_mask() {
    let parts = mask_in_two_lanes(s_mov_k(9, 1));
    let parts: Vec<&[u32]> = parts.iter().map(Vec::as_slice).collect();
    let p = ir(&parts, true);
    assert_eq!(parked_moves(&p), ["s_mov_b64", "s_mov_b64"]);
    assert_eq!(lane_ops(&p), 0, "{:?}", names(&p));
    let i = insts(&p);
    // Saved where its second half was, restored where its first half was.
    let (save, restore) = (i[3], i[7]);
    assert_eq!((save.name(), restore.name()), ("s_mov_b64", "s_mov_b64"));
    let st = crate::structure::structure(&p).unwrap();
    let restored = restore.write(Reg::S(10)).unwrap();
    assert!(st.masks.contains_key(&restored));
    assert_eq!(
        st.highs.get(&restore.write(Reg::S(11)).unwrap()),
        Some(&restored)
    );
    let (wave, form) = translate(&parts, true, NVIDIA);
    assert_eq!(wave, None, "{form}");
    assert!(form.ends_with("; per lane"), "{form}");
}

/// The register restored late is read between the restores: restored
/// early as a pair, it would be read too soon. The halves move alone.
#[test]
fn halves_restored_apart_move_alone_when_the_second_is_read_between() {
    let parts = mask_in_two_lanes(v_mov_s(3, 11));
    let parts: Vec<&[u32]> = parts.iter().map(Vec::as_slice).collect();
    let p = ir(&parts, true);
    assert_eq!(parked_moves(&p), ["s_mov_b32"; 4]);
    let i = insts(&p);
    let early = i
        .iter()
        .find(|i| i.name() == "v_mov_b32" && i.read(Reg::S(11)).is_some());
    // It reads the 0 written before the restores, not the restored half.
    assert_eq!(early.unwrap().read(Reg::S(11)), i[6].write(Reg::S(11)));
}

/// A wave32 lane mask is one dword: neighbouring lanes are no pair.
#[test]
fn wave32_lanes_move_alone() {
    let p = ir(
        &[
            &writelane(5, 10, 0),
            &writelane(5, 11, 1),
            &readlane(10, 5, 0),
            &readlane(11, 5, 1),
            &[v_mov_s(2, 10), v_mov_s(3, 11), S_ENDPGM],
        ],
        false,
    );
    assert_eq!(parked_moves(&p), ["s_mov_b32"; 4]);
}

/// A lane select is taken modulo the wave size: 35 is lane 3 of a wave32.
#[test]
fn lane_selects_wrap_at_the_wave_size() {
    let p = ir(
        &[
            &writelane(5, 9, 35),
            &readlane(10, 5, 3),
            &[v_mov_s(2, 10), S_ENDPGM],
        ],
        false,
    );
    let i = insts(&p);
    let slot = Reg::S(parked_code(i[0]).unwrap());
    assert_eq!(i[1].read(slot), i[0].write(slot));
}

#[test]
fn a_vgpr_with_other_uses_is_no_parking() {
    let p = ir(
        &[
            &writelane(5, 9, 3),
            &readlane(10, 5, 3),
            &[0x7E04_0305, S_ENDPGM], // v_mov_b32 v2, v5
        ],
        false,
    );
    assert_eq!(lane_ops(&p), 2, "{:?}", names(&p));
}

/// A lane of a VGPR the program came in with (a thread id) is read, not
/// restored: it stays a lane read.
#[test]
fn a_lane_the_program_came_in_with_stays_a_lane_read() {
    let p = ir(&[&readlane(0, 0, 5), &[v_mov_s(1, 0), S_ENDPGM]], true);
    assert_eq!(names(&p), ["v_readlane_b32", "v_mov_b32", "s_endpgm"]);
}

/// Past the last parked code, lanes stay parked.
#[test]
fn lanes_past_the_last_code_stay_parked() {
    let lanes: Vec<(u8, u8)> = (0..130u8).map(|n| (1 + n / 32, n % 32)).collect();
    let mut words = Vec::new();
    for &(vgpr, lane) in &lanes {
        words.extend(writelane(vgpr, lane, lane));
    }
    for &(vgpr, lane) in &lanes {
        words.extend(readlane(lane, vgpr, lane));
    }
    words.push(S_ENDPGM);
    let p = ir(&[&words], false);
    assert_eq!(parked_moves(&p).len(), 2 * 127);
    assert_eq!(lane_ops(&p), 2 * 3);
}
