//! A lane the iteration limit stops, in a loop followed by a store and a
//! group barrier (the maintainers' private watch notes), and the budget of all
//! loops together.
//!
//! Lane t of a group of 64 runs its loops to small bounds, except lane
//! `long` (root constant s1), whose bounds are above a limit. After the
//! loops each lane stores its count at dword t, passes `s_barrier` and
//! stores t + 1000 at dword 64 + t. On the console every store happens,
//! lane `long`'s with its whole count. On the host lane `long` stops: it
//! must store nothing, and the others must still pass the barrier.
//!
//! * Through a conditional back edge (`s_cbranch_execnz`), a lane at the
//!   limit took the loop's exit and stored the count it had reached
//!   (Marvisak/VIctory#1483).
//! * A lane that leaves at the limit has returned; a group barrier after
//!   the loop would wait for it (VItoriaBr `808a209`). It now keeps
//!   passing barriers until the whole group has ended (`lower/flow.rs`,
//!   `Drain`). Each dispatch has a time limit, so a wait that never ends
//!   fails the test instead of hanging it. (WARP does not wait at a group
//!   barrier for a thread that has returned, so this form passed on WARP
//!   before too.)
//! * Loops inside loops multiply their limits; the budget
//!   ([`super::loop_budget`]) stops a lane once all of its loops together
//!   have gone back that many times (`Budget`).

use std::sync::mpsc;
use std::time::Duration;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, Target};
use crate::native::{ComputeIo, ResourceType, RootParam, RootType, StageIo, UsedResource};

/// The group: two console waves of 32.
const THREADS: u32 = 64;

/// The bound of every loop of every lane but the long one.
const SHORT: u32 = 3;

/// The inner loop's bound in the long lane.
const INNER: u32 = 1024;

/// A loop's back edge.
#[derive(Clone, Copy, Debug)]
enum Back {
    /// `s_cbranch_execnz loop` at the loop's end.
    Conditional,
    /// `s_cbranch_execz exit; s_branch loop`: a latch block of its own.
    Unconditional,
}

/// The loops between the start and the stores. v1 counts the passes of
/// the innermost loop.
#[derive(Clone, Copy, Debug)]
enum Loops {
    /// One loop, to the long lane's bound.
    One(Back),
    /// A loop to the long lane's bound around one to [`INNER`].
    Nested,
}

/// The program, the long lane's (outer) bound `bound`. s0: the table of
/// the UAV. The loops step by a register, so none is a counted loop: each
/// has the general limit.
fn program(loops: Loops, bound: u32) -> crate::NativeProgram {
    let mut words = vec![
        0xBE82_0300, // s_mov_b32 s2, s0
        0xBE83_0380, // s_mov_b32 s3, 0
        0xF408_0101,
        0xFA00_0000,         // s_load_dwordx4 s[4:7], s[2:3], null
        0xBF8C_C07F,         // s_waitcnt lgkmcnt(0)
        0x7E02_0280,         // v_mov_b32 v1, 0
        0x7E0A_0281,         // v_mov_b32 v5, 1
        0x7E08_0280 | SHORT, // v_mov_b32 v4, SHORT
        0x7E0C_02FF,
        bound,       // v_mov_b32 v6, bound
        0x7D84_0001, // v_cmp_eq_u32 vcc_lo, s1, v0
        0x0208_0D04, // v_cndmask_b32 v4, v4, v6, vcc_lo
        0x3406_0082, // v_lshlrev_b32 v3, 2, v0
        0xBE88_0380, // s_mov_b32 s8, 0
    ];
    match loops {
        Loops::One(back) => {
            words.extend([
                // loop:
                0x4A02_0305, // v_add_nc_u32 v1, v5, v1
                0x7D8C_0901, // v_cmp_ge_u32 vcc_lo, v1, v4
                0x8808_086A, // s_or_b32 s8, vcc_lo, s8
                0x8A7E_087E, // s_andn2_b32 exec_lo, exec_lo, s8
            ]);
            match back {
                Back::Conditional => words.push(0xBF89_FFFB), // s_cbranch_execnz loop
                Back::Unconditional => words.extend([
                    0xBF88_0001, // s_cbranch_execz exit
                    0xBF82_FFFA, // s_branch loop
                ]),
            }
        }
        Loops::Nested => words.extend([
            0x7E14_0280 | SHORT, // v_mov_b32 v10, SHORT
            0x7E16_02FF,
            INNER,       // v_mov_b32 v11, INNER
            0x0214_170A, // v_cndmask_b32 v10, v10, v11, vcc_lo
            0x7E04_0280, // v_mov_b32 v2, 0
            // outer:
            0x7E0E_0280, // v_mov_b32 v7, 0
            0xBE89_0380, // s_mov_b32 s9, 0
            // inner:
            0x4A0E_0F05, // v_add_nc_u32 v7, v5, v7
            0x4A02_0305, // v_add_nc_u32 v1, v5, v1
            0x7D8C_1507, // v_cmp_ge_u32 vcc_lo, v7, v10
            0x8809_096A, // s_or_b32 s9, vcc_lo, s9
            0x8A7E_097E, // s_andn2_b32 exec_lo, exec_lo, s9
            0xBF89_FFFA, // s_cbranch_execnz inner
            0x887E_097E, // s_or_b32 exec_lo, exec_lo, s9
            0x4A04_0505, // v_add_nc_u32 v2, v5, v2
            0x7D8C_0902, // v_cmp_ge_u32 vcc_lo, v2, v4
            0x8808_086A, // s_or_b32 s8, vcc_lo, s8
            0x8A7E_087E, // s_andn2_b32 exec_lo, exec_lo, s8
            0xBF89_FFF2, // s_cbranch_execnz outer
        ]),
    }
    words.extend([
        // exit:
        0x887E_087E, // s_or_b32 exec_lo, exec_lo, s8
        0xE070_1000,
        0x8001_0103, // buffer_store_dword v1, v3, s[4:7], 0 offen
        0xBF8A_0000, // s_barrier
        0x4A0E_00FF,
        1000, // v_add_nc_u32 v7, 1000, v0
        0xE070_1100,
        0x8001_0703, // buffer_store_dword v7, v3, s[4:7], 0 offen offset:256
        0xBF81_0000, // s_endpgm
    ]);
    let param = |root, user_data| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot: user_data,
    };
    crate::NativeProgram {
        stage: crate::HwStage::Cs,
        wave64: false,
        vgprs: 16,
        layout: vec![param(RootType::Table, 0), param(RootType::Constants, 1)],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1, // s0, s1
            threads: [THREADS, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// A bound above the general loop limit.
fn above_the_limit() -> u32 {
    super::loop_limit().saturating_add(16)
}

fn emit(loops: Loops, bound: u32, target: Target) -> HostShader {
    super::tests::emit_for(&program(loops, bound), target)
        .unwrap_or_else(|e| panic!("{loops:?}, {target:?}: {e}"))
}

/// What WARP stores with lane `long` the long one (`u32::MAX`: none), or
/// none without a WARP device. Panics if the dispatch does not finish in
/// time.
fn run(cs: &HostShader, long: u32) -> Option<Vec<u32>> {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let cs = cs.clone();
    let (tx, rx) = mpsc::channel();
    // The device lives on a thread of its own: a dispatch that never ends
    // stays there, and the test fails.
    std::thread::spawn(move || {
        let out = Host::new(true).ok().map(|host| {
            let (root, pso) = host.compute_pipeline(&cs).expect("pipeline");
            let mut user = vec![0; (cs.user_data_dwords() as usize).max(2)];
            user[1] = long;
            host.dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(user))],
                    views: &[View::RawUav(2 * THREADS)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("dispatch")
            .remove(0)
        });
        let _ = tx.send(out);
    });
    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(out) => out,
        Err(_) => panic!(
            "lane {long} long: the dispatch did not finish in 120 s (a group barrier waits for a lane that left?)"
        ),
    }
}

/// What the host must store: every lane's count `short` and t + 1000; lane
/// `long`'s count `long_count`, or nothing at all where it stops.
fn want(long: u32, short: u32, long_count: Option<u32>) -> Vec<u32> {
    let count = |t| if t == long { long_count } else { Some(short) };
    let counts = (0..THREADS).map(|t| count(t).unwrap_or(0));
    let after = (0..THREADS).map(|t| count(t).map_or(0, |_| t + 1000));
    counts.chain(after).collect()
}

/// Every form translates for any host, a host of 32-lane waves (NVIDIA)
/// and one of 32 and 64 (AMD RDNA), and the validator, where one is
/// installed, takes them. No device needed.
#[test]
fn the_programs_translate_and_validate() {
    let mut validator =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
            .ok();
    let forms = [
        Loops::One(Back::Conditional),
        Loops::One(Back::Unconditional),
        Loops::Nested,
    ];
    for loops in forms {
        for (lanes_min, lanes_max) in [(4, 128), (32, 32), (32, 64)] {
            let target = Target {
                lanes_min,
                lanes_max,
            };
            let mut cs = emit(loops, above_the_limit(), target);
            if let Some(v) = validator.as_mut() {
                v.validate(&mut cs.dxil)
                    .unwrap_or_else(|e| panic!("{loops:?}, {target:?}: validation: {e}"));
            }
        }
    }
}

/// A lane stopped at the limit through a conditional back edge stores
/// nothing: neither the count it had reached (it stored the limit before)
/// nor anything after the loop. The others store theirs.
#[test]
fn a_lane_stopped_through_a_conditional_back_edge_stores_nothing() {
    let cs = emit(
        Loops::One(Back::Conditional),
        above_the_limit(),
        Target::ANY,
    );
    for long in [5, 37] {
        let Some(out) = run(&cs, long) else {
            eprintln!("no WARP device; skipped");
            return;
        };
        assert_eq!(out, want(long, SHORT, None), "lane {long} long");
    }
}

/// With either back edge, the lanes that did not stop pass the group
/// barrier after the loop and store after it, whichever wave the stopped
/// lane is in, and with no lane stopped.
#[test]
fn the_group_barrier_after_a_stopped_lane_is_passed() {
    for back in [Back::Conditional, Back::Unconditional] {
        let cs = emit(Loops::One(back), above_the_limit(), Target::ANY);
        for long in [u32::MAX, 0, 31, 32, 63] {
            let Some(out) = run(&cs, long) else {
                eprintln!("no WARP device; skipped");
                return;
            };
            assert_eq!(out, want(long, SHORT, None), "{back:?}, lane {long} long");
        }
    }
}

/// Two nested loops, each below its own limit: 2048 passes of the outer
/// loop over 1024 of the inner one are 2,097,151 back edges, past the
/// budget of 2^20, so the long lane stops and stores nothing (it stored
/// 2^21 before). 512 passes over 1024 (524,287 back edges) run to their
/// end. Both follow the switches in force.
#[test]
fn nested_loops_stop_at_the_budget_of_all_loops_together() {
    let (limit, budget) = (super::loop_limit(), super::loop_budget());
    for outer in [2048, 512] {
        let back_edges = (outer - 1) + outer * (INNER - 1);
        let stops = outer > limit || INNER > limit || budget > 0 && back_edges >= budget;
        let cs = emit(Loops::Nested, outer, Target::ANY);
        for long in [9, 40] {
            let Some(out) = run(&cs, long) else {
                eprintln!("no WARP device; skipped");
                return;
            };
            let long_count = (!stops).then_some(outer * INNER);
            assert_eq!(
                out,
                want(long, SHORT * SHORT, long_count),
                "outer {outer}, lane {long} long"
            );
        }
    }
}

/// What the lowering makes of `native`, for any host.
fn lowered(native: &crate::NativeProgram) -> super::lower::Lowered {
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = crate::io::raise(&p, native).unwrap();
    super::lower::lower(&super::lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
        target: Target::ANY,
        console_cf: false,
        per_lane_only: false,
    })
    .unwrap()
}

/// A branch whose two arms are back edges: the inner loop's latch goes
/// back to the inner header, or falls into the outer one. Each edge is
/// counted against its loop's limit (only the taken one was, and the outer
/// header's count had no value from the other edge), and the validator
/// takes the result.
// Ported from Marvisak/VIctory#1540 (head 1ccae2d) by Texbirds.
#[test]
fn a_branch_whose_two_arms_are_back_edges_counts_both() {
    let words = [
        0xBE80_0380, // 0  entry: s_mov_b32 s0, 0
        0xBF82_0003, // 1         s_branch -> 5 (outer)
        0x8001_8101, // 2  inner: s_add_u32 s1, s1, 1
        0xBF06_8201, // 3         s_cmp_eq_u32 s1, 2
        0xBF84_FFFD, // 4         s_cbranch_scc0 -> 2 (inner); else on to 5 (outer)
        0x8000_8100, // 5  outer: s_add_u32 s0, s0, 1
        0xBF06_8200, // 6         s_cmp_eq_u32 s0, 2
        0xBF85_0002, // 7         s_cbranch_scc1 -> 10 (exit)
        0xBE81_0380, // 8  pre:   s_mov_b32 s1, 0
        0xBF82_FFF8, // 9         s_branch -> 2 (inner)
        0xBF81_0000, // 10 exit:  s_endpgm
    ];
    let mut l = lowered(&super::tests::compute(&words, false));
    assert_eq!(l.loop_limits.len(), 2, "limits {:?}", l.loop_limits);
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut l.dxil).expect("DXIL validation");
    }
}
