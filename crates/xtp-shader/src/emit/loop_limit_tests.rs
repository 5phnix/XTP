//! A lane the iteration limit stops, in a loop followed by a store and a
//! group barrier (docs/upstream/watch.md, item 111).
//!
//! Lane t of a group of 64 counts to its bound, 3, except lane `long`
//! (root constant s1), whose bound is above the limit. After the loop each
//! lane stores its count at dword t, passes `s_barrier` and stores t + 1000
//! at dword 64 + t. On the console every store happens, lane `long`'s with
//! its whole count. On the host lane `long` stops at the limit: it must
//! store nothing, and the others must still pass the barrier.
//!
//! * Through a conditional back edge (`s_cbranch_execnz`), a lane at the
//!   limit took the loop's exit and stored the count it had reached
//!   (Marvisak/VIctory#1483).
//! * A lane that leaves at the limit has returned; a group barrier after
//!   the loop would wait for it (VItoriaBr `808a209`). It now keeps
//!   passing barriers until the whole group has ended (`lower/flow.rs`,
//!   `Drain`). Each dispatch has a time limit, so a wait that never ends
//!   fails the test instead of hanging it.

use std::sync::mpsc;
use std::time::Duration;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, Target};
use crate::native::{ComputeIo, ResourceType, RootParam, RootType, StageIo, UsedResource};

/// The group: two console waves of 32.
const THREADS: u32 = 64;

/// The count every lane but the long one reaches.
const SHORT: u32 = 3;

/// The loop's back edge.
#[derive(Clone, Copy, Debug)]
enum Back {
    /// `s_cbranch_execnz loop` at the loop's end.
    Conditional,
    /// `s_cbranch_execz exit; s_branch loop`: a latch block of its own.
    Unconditional,
}

/// The program, lane `long`'s bound `bound`. s0: the table of the UAV.
fn program(back: Back, bound: u32) -> crate::NativeProgram {
    let mut words = vec![
        0xBE82_0300, // s_mov_b32 s2, s0
        0xBE83_0380, // s_mov_b32 s3, 0
        0xF408_0101,
        0xFA00_0000,         // s_load_dwordx4 s[4:7], s[2:3], null
        0xBF8C_C07F,         // s_waitcnt lgkmcnt(0)
        0x7E02_0280,         // v_mov_b32 v1, 0
        0x7E0A_0281,         // v_mov_b32 v5, 1 (a register: no counted loop)
        0x7E08_0280 | SHORT, // v_mov_b32 v4, SHORT
        0x7E0C_02FF,
        bound,       // v_mov_b32 v6, bound
        0x7D84_0001, // v_cmp_eq_u32 vcc_lo, s1, v0
        0x0208_0D04, // v_cndmask_b32 v4, v4, v6, vcc_lo
        0x3406_0082, // v_lshlrev_b32 v3, 2, v0
        0xBE88_0380, // s_mov_b32 s8, 0
        // loop:
        0x4A02_0305, // v_add_nc_u32 v1, v5, v1
        0x7D8C_0901, // v_cmp_ge_u32 vcc_lo, v1, v4
        0x8808_086A, // s_or_b32 s8, vcc_lo, s8
        0x8A7E_087E, // s_andn2_b32 exec_lo, exec_lo, s8
    ];
    match back {
        Back::Conditional => words.push(0xBF89_FFFB), // s_cbranch_execnz loop
        Back::Unconditional => words.extend([
            0xBF88_0001, // s_cbranch_execz exit
            0xBF82_FFFA, // s_branch loop
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
        vgprs: 8,
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

/// A bound above the limit the loop gets (the general one: its step is a
/// register, so it is no counted loop).
fn above_the_limit() -> u32 {
    super::loop_limit().saturating_add(16)
}

fn emit(back: Back, target: Target) -> HostShader {
    super::tests::emit_for(&program(back, above_the_limit()), target)
        .unwrap_or_else(|e| panic!("{back:?}, {target:?}: {e}"))
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

/// What the host must store: every lane's count and t + 1000, nothing for
/// lane `long`.
fn want(long: u32) -> Vec<u32> {
    let count = (0..THREADS).map(|t| if t == long { 0 } else { SHORT });
    let after = (0..THREADS).map(|t| if t == long { 0 } else { t + 1000 });
    count.chain(after).collect()
}

/// Both forms translate for any host, a host of 32-lane waves (NVIDIA) and
/// one of 32 and 64 (AMD RDNA), and the validator, where one is installed,
/// takes them. No device needed.
#[test]
fn the_programs_translate_and_validate() {
    let mut validator =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
            .ok();
    for back in [Back::Conditional, Back::Unconditional] {
        for (lanes_min, lanes_max) in [(4, 128), (32, 32), (32, 64)] {
            let target = Target {
                lanes_min,
                lanes_max,
            };
            let mut cs = emit(back, target);
            if let Some(v) = validator.as_mut() {
                v.validate(&mut cs.dxil)
                    .unwrap_or_else(|e| panic!("{back:?}, {target:?}: validation: {e}"));
            }
        }
    }
}

/// A lane stopped at the limit through a conditional back edge stores
/// nothing: neither the count it had reached (it stored the limit before)
/// nor anything after the loop. The others store theirs.
#[test]
fn a_lane_stopped_through_a_conditional_back_edge_stores_nothing() {
    let cs = emit(Back::Conditional, Target::ANY);
    for long in [5, 37] {
        let Some(out) = run(&cs, long) else {
            eprintln!("no WARP device; skipped");
            return;
        };
        assert_eq!(out, want(long), "lane {long} long");
    }
}

/// With either back edge, the lanes that did not stop pass the group
/// barrier after the loop and store after it, whichever wave the stopped
/// lane is in, and with no lane stopped.
#[test]
fn the_group_barrier_after_a_stopped_lane_is_passed() {
    for back in [Back::Conditional, Back::Unconditional] {
        let cs = emit(back, Target::ANY);
        for long in [u32::MAX, 0, 31, 32, 63] {
            let Some(out) = run(&cs, long) else {
                eprintln!("no WARP device; skipped");
                return;
            };
            assert_eq!(out, want(long), "{back:?}, lane {long} long");
        }
    }
}
