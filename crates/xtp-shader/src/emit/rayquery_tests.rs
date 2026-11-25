//! Inline ray queries in place of a traversal walk (RT phase 2b,
//! `lower/rayquery.rs`), on programs written for these tests, never the
//! title's code:
//!
//! * [`PROGRAM`], a compute program of family A's outline
//!   (docs/analysis/rt-dxr.md section 4.2) whose loop is a skeleton: the
//!   splice and its DXIL around a site described by hand (section 5.2), and
//!   the spliced program on WARP against an acceleration structure built
//!   here, which calibrates DXR's conventions (facing, t at its ends);
//! * [`family_a`], a whole walk of family A's shape: the structure's root
//!   at +0x30 behind an empty-structure check, a wave-wide top level over
//!   boxes and instances (mask 0xd5, culling and facing by instance flags,
//!   world-to-object transforms with a translation-only path), a per-lane
//!   BLAS walk with a short stack in LDS over boxes and triangle pairs, an
//!   iteration cap of 0xfffe, then t + 0.04 or FLT_MAX stored per thread.
//!   It is proven by play (section 5.3), its edge cases differ as
//!   documented, and its spliced translation binds and traces on WARP
//!   against a host TLAS of the play's world with a blank guest header.

use std::collections::HashMap;

use super::HostShader;
use super::lower::rayquery::play::QueryRay;
use super::lower::rayquery::proof::{self, Play, Prover};
use super::lower::rayquery::world::{self, Blas, Instance, Rng, Root, Tri, World};
use super::lower::rayquery::{
    self, Kind, Site, Source, TLAS_REGISTER, TlasDescriptor, asm, ray_flags, splice,
};
use crate::dxbc::{Container, FourCC};
use crate::ir::{EXEC, Program, Reg};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};

/// The table at `s0`: the ray buffer's descriptor at dword 0, the
/// structure's at dword 8 (0x20), the output's at dword 16 (0x40). Rays are
/// 32 bytes: origin xyz, t-max, direction xyz, padding. As in family A,
/// the structure's root is its header's dword at +0x30, and a root that is
/// not positive (an empty structure) skips the walk: a block just before
/// the exit records no hit.
const PROGRAM: &[u32] = &[
    0xBE85_0380, // 0x00 s_mov_b32 s5, 0
    0xBE84_0300, // 0x04 s_mov_b32 s4, s0
    0xF408_0202,
    0xFA00_0000, // 0x08 s_load_dwordx4 s[8:11], s[4:5], 0
    0xF408_0302,
    0xFA00_0040, // 0x10 s_load_dwordx4 s[12:15], s[4:5], 0x40
    0xF408_0402,
    0xFA00_0020, // 0x18 s_load_dwordx4 s[16:19], s[4:5], 0x20
    0xBF8C_C07F, // 0x20 s_waitcnt lgkmcnt(0)
    0x3402_0085, // 0x24 v_lshlrev_b32 v1, 5, v0
    0xE038_1000,
    0x8002_0201, // 0x28 buffer_load_dwordx4 v[2:5], v1, s[8:11], 0 offen
    0xE03C_1010,
    0x8002_0601, // 0x30 buffer_load_dwordx3 v[6:8], v1, s[8:11], 0 offen offset:16
    0x7E18_0280, // 0x38 v_mov_b32 v12, 0 (no hit)
    0x9094_8810, // 0x3c s_lshr_b64 s[20:21], s[16:17], 8 (the base, 256-byte units)
    0xBE96_0314, // 0x40 s_mov_b32 s22, s20
    0xBE97_0380, // 0x44 s_mov_b32 s23, 0
    0x8F96_8816, // 0x48 s_lshl_b64 s[22:23], s[22:23], 8
    0x8816_B016, // 0x4c s_or_b32 s22, s22, 48
    0xF400_074B,
    0xFA00_0000, // 0x50 s_load_dword s29, s[22:23], 0 (the root)
    0xBE9C_037E, // 0x58 s_mov_b32 s28, exec_lo
    0xBF8C_0070, // 0x5c s_waitcnt vmcnt(0) lgkmcnt(0)
    0xBF02_801D, // 0x60 s_cmp_gt_i32 s29, 0
    0xBF84_001A, // 0x64 s_cbranch_scc0 0xd0 (an empty structure)
    0x7E12_5506, // 0x68 v_rcp_f32 v9, v6
    0x7E14_5507, // 0x6c v_rcp_f32 v10, v7
    0x7E16_5508, // 0x70 v_rcp_f32 v11, v8
    0x7E1A_021D, // 0x74 v_mov_b32 v13, s29
    0xBE98_0314, // 0x78 s_mov_b32 s24, s20
    0xBE99_0B81, // 0x7c s_brev_b32 s25, 1
    0xBE9A_03C1, // 0x80 s_mov_b32 s26, -1
    0xBE9B_03FF,
    0x8900_03FF, // 0x84 s_mov_b32 s27, 0x890003ff
    // The walk (0x8c).
    0xF198_9F07,
    0x0006_0E0D,
    0x0403_0205,
    0x0908_0706,
    0x0000_0B0A, // 0x8c image_bvh_intersect_ray v[14:17], [v13, v5, v2, v3, v4, v6, v7, v8, v9, v10, v11], s[24:27]
    0xBF8C_3F70, // 0xa0 s_waitcnt vmcnt(0)
    0x7E24_550F, // 0xa4 v_rcp_f32 v18, v15
    0x1024_250E, // 0xa8 v_mul_f32 v18, v14, v18
    0x7C02_0B12, // 0xac v_cmp_lt_f32 vcc_lo, v18, v5
    0x020A_2505, // 0xb0 v_cndmask_b32 v5, v5, v18, vcc_lo
    0xD501_000C,
    0x01A9_030C, // 0xb4 v_cndmask_b32 v12, v12, 1, vcc_lo
    0x7E1A_0310, // 0xbc v_mov_b32 v13, v16
    0x7D02_1A80, // 0xc0 v_cmp_lt_i32 vcc_lo, 0, v13
    0x877E_6A7E, // 0xc4 s_and_b32 exec_lo, exec_lo, vcc_lo
    0xBF89_FFF0, // 0xc8 s_cbranch_execnz 0x8c
    0xBF82_0001, // 0xcc s_branch 0xd4
    // The skip path (0xd0).
    0x7E18_0280, // 0xd0 v_mov_b32 v12, 0
    // After the walk (0xd4).
    0xBEFE_031C, // 0xd4 s_mov_b32 exec_lo, s28
    0x0602_0AFF,
    0x3D23_D70A, // 0xd8 v_add_f32 v1, 0.04, v5
    0x7D8A_1880, // 0xe0 v_cmp_ne_u32 vcc_lo, 0, v12
    0x0202_02FF,
    0x7F7F_FFFF, // 0xe4 v_cndmask_b32 v1, FLT_MAX, v1, vcc_lo
    0x3426_0082, // 0xec v_lshlrev_b32 v19, 2, v0
    0xE070_1000,
    0x8003_0113, // 0xf0 buffer_store_dword v1, v19, s[12:15], 0 offen
    0xBF81_0000, // 0xf8 s_endpgm
];

/// What [`PROGRAM`] decodes to, in order.
const NAMES: &[&str] = &[
    "s_mov_b32",
    "s_mov_b32",
    "s_load_dwordx4",
    "s_load_dwordx4",
    "s_load_dwordx4",
    "s_waitcnt",
    "v_lshlrev_b32",
    "buffer_load_dwordx4",
    "buffer_load_dwordx3",
    "v_mov_b32",
    "s_lshr_b64",
    "s_mov_b32",
    "s_mov_b32",
    "s_lshl_b64",
    "s_or_b32",
    "s_load_dword",
    "s_mov_b32",
    "s_waitcnt",
    "s_cmp_gt_i32",
    "s_cbranch_scc0",
    "v_rcp_f32",
    "v_rcp_f32",
    "v_rcp_f32",
    "v_mov_b32",
    "s_mov_b32",
    "s_brev_b32",
    "s_mov_b32",
    "s_mov_b32",
    "image_bvh_intersect_ray",
    "s_waitcnt",
    "v_rcp_f32",
    "v_mul_f32",
    "v_cmp_lt_f32",
    "v_cndmask_b32",
    "v_cndmask_b32",
    "v_mov_b32",
    "v_cmp_lt_i32",
    "s_and_b32",
    "s_cbranch_execnz",
    "s_branch",
    "v_mov_b32",
    "s_mov_b32",
    "v_add_f32",
    "v_cmp_ne_u32",
    "v_cndmask_b32",
    "v_lshlrev_b32",
    "buffer_store_dword",
    "s_endpgm",
];

/// The walk's site, by hand: rays in v[2:5] (origin, t-max) and v[6:8];
/// the code after it reads t-max (`v5`) and whether a hit was recorded
/// (`v12`); the empty-structure check jumps to 0xd0.
const SITE: Site = Site {
    header: 0x8c,
    exit: 0xd4,
    skip: Some(0xd0),
    origin: [2, 3, 4],
    direction: [6, 7, 8],
    t_max: 5,
    t_min: 0,
    mask: 0xd5,
    flags: ray_flags::FORCE_OPAQUE | ray_flags::CULL_BACK_FACING_TRIANGLES,
    results: &[(5, Kind::T), (12, Kind::Hit)],
    kept: &[],
    cap: None,
    tlas: Source {
        user_sgpr: 0,
        offset: 0x20,
    },
};

/// The proof by play's own tests, which play the family A program below.
#[path = "rayquery_play_tests.rs"]
mod play;

/// Threads in the group: one wave of 32.
const THREADS: u32 = 32;

fn used(kind: ResourceType, table_offset_dwords: u32) -> UsedResource {
    UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 0,
        slot: 0,
    }
}

fn table() -> Vec<RootParam> {
    vec![RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data: 0,
        slot: 0,
    }]
}

fn native() -> NativeProgram {
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        float_mode: 0,
        vgprs: 24,
        layout: table(),
        used: vec![
            used(ResourceType::Srv, 0),
            used(ResourceType::Srv, 8),
            used(ResourceType::Uav, 16),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [THREADS, 1, 1],
        }),
        code: PROGRAM.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

fn ir(native: &NativeProgram) -> Program {
    crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap()
}

/// [`super::translate_program_for`]'s steps with the walk at `site`
/// spliced where `rayquery::prepared` splices the walks it knows (proven
/// by play first when `prove` says so).
fn translate_with(
    native: &NativeProgram,
    site: &Site,
    prove: bool,
) -> Result<HostShader, crate::ShaderError> {
    let c = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: vec![],
    };
    let p = ir(native);
    let p = if prove {
        rayquery::spliced(p, native, site)?
    } else {
        splice(p, site).map_err(|what| crate::ShaderError::Unsupported {
            offset: site.header,
            what,
        })?
    };
    let st = crate::structure::structure(&p);
    let res = crate::resource::track_native(&p, native, st.as_ref().ok())?;
    let io = crate::io::raise(&p, native)?;
    let st = st?;
    super::emit_for(&c, native, &p, &st, &io, &res, super::Target::ANY)
}

fn translate(native: &NativeProgram, site: &Site) -> Result<HostShader, crate::ShaderError> {
    translate_with(native, site, false)
}

#[test]
fn rayquery_synthetic_program_decodes_as_written() {
    let code = native().code;
    let names: Vec<&str> = crate::gfx10::decode(&code)
        .unwrap()
        .iter()
        .map(|i| i.name().unwrap_or("?"))
        .collect();
    assert_eq!(names, NAMES);
    // The walk's intersection reads the node, t-max, the origin, the
    // direction and its inverse, in that order.
    let p = ir(&native());
    let walk = p.blocks.iter().find(|b| b.offset == 0x8c).unwrap();
    let reads: Vec<Reg> = walk.insts[0].reads.iter().map(|r| r.0).collect();
    for (k, v) in [13, 5, 2, 3, 4, 6, 7, 8, 9, 10, 11].into_iter().enumerate() {
        assert_eq!(reads[k], Reg::V(v), "address {k}");
    }
}

#[test]
fn rayquery_splices_the_walk_of_the_synthetic_program() {
    let before = ir(&native());
    let p = splice(before.clone(), &SITE).unwrap();
    let insts = || p.blocks.iter().flat_map(|b| &b.insts);
    // The walk is gone with what only it read: its intersection, the
    // inverse direction, the structure's descriptor, its root and the
    // empty-structure check.
    assert!(insts().all(|i| i.name() != "image_bvh_intersect_ray"));
    for gone in [
        "v_rcp_f32",
        "s_lshr_b64",
        "s_lshl_b64",
        "s_brev_b32",
        "s_load_dword",
        "s_cmp_gt_i32",
        "s_cbranch_scc0",
    ] {
        assert!(insts().all(|i| i.name() != gone), "{gone}");
    }
    let loads = insts().filter(|i| i.name() == "s_load_dwordx4").count();
    assert_eq!(loads, 2, "the structure's descriptor is no longer loaded");
    // The check goes on to the walk's entry, then one block in the walk's
    // place, then the store.
    assert_eq!(p.blocks.len(), 4);
    assert_eq!(p.blocks[0].exit, crate::ir::Exit::Jump(1));
    assert_eq!(p.blocks[1].offset, 0x68);
    let spliced = &p.blocks[2];
    assert_eq!(spliced.offset, 0x8c);
    assert_eq!(spliced.preds, [1]);
    assert_eq!(spliced.exit, crate::ir::Exit::Jump(3));
    assert_eq!(p.blocks[3].offset, 0xd4);
    assert_eq!(p.blocks[3].preds, [2]);
    assert!(p.blocks[3].phis.is_empty());
    let [query] = &spliced.insts[..] else {
        panic!("{:?}", spliced.insts)
    };
    // It reads the ray where the walk started, and writes t and the hit.
    let entry = &p.blocks[0];
    let loaded = |reg| entry.insts.iter().find_map(|i| i.write(reg)).unwrap();
    for r in [2, 3, 4, 5, 6, 7, 8] {
        assert_eq!(query.read(Reg::V(r)), Some(loaded(Reg::V(r))), "v{r}");
    }
    assert_eq!(query.read(Reg::V(12)), Some(loaded(Reg::V(12))));
    let t = query.write(Reg::V(5)).unwrap();
    let hit = query.write(Reg::V(12)).unwrap();
    // The code after the walk reads them.
    let after = &p.blocks[3].insts;
    let add = after.iter().find(|i| i.name() == "v_add_f32").unwrap();
    assert_eq!(add.read(Reg::V(5)), Some(t));
    let cmp = after.iter().find(|i| i.name() == "v_cmp_ne_u32").unwrap();
    assert_eq!(cmp.read(Reg::V(12)), Some(hit));
}

#[test]
fn rayquery_refuses_sites_that_do_not_fit_the_walk() {
    let p = ir(&native());
    // A header that is no block.
    let off = Site {
        header: 0x90,
        ..SITE
    };
    assert!(splice(p.clone(), &off).is_err());
    // A walk running past the store: it is left to no block.
    let long = Site { exit: 0xd0, ..SITE };
    assert!(splice(p.clone(), &long).is_err());
    // Without the empty-structure check the skip path enters the walk's
    // range elsewhere than at its header.
    let unchecked = Site { skip: None, ..SITE };
    assert_eq!(
        splice(p.clone(), &unchecked).err(),
        Some("a ray-tracing walk entered other than at its header from one block")
    );
    // A skip path that no branch takes.
    let nowhere = Site {
        skip: Some(0xcc),
        ..SITE
    };
    assert!(splice(p.clone(), &nowhere).is_err());
    // The code after the walk reads whether it hit: without that result
    // the splice would drop what the walk left there.
    let no_hit = Site {
        results: &[(5, Kind::T)],
        ..SITE
    };
    assert_eq!(
        splice(p, &no_hit).err(),
        Some("the code after a ray-tracing walk reads a register the walk changes")
    );
}

/// [`PROGRAM`]-like `native` with its words from byte offset `at` replaced
/// by `words`.
fn patched(native: &NativeProgram, at: usize, words: &[u32]) -> NativeProgram {
    let mut n = native.clone();
    for (k, w) in words.iter().enumerate() {
        n.code[at + 4 * k..at + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
    }
    n
}

/// A walk with a path that does not go on to its exit is refused (Codex
/// review of c83e996, point 1): the query would go on to it.
#[test]
fn rayquery_refuses_walks_that_do_not_always_reach_their_exit() {
    // The skip path ends the program (0xd0: s_endpgm).
    let ends = patched(&native(), 0xd0, &[0xBF81_0000]);
    assert_eq!(
        splice(ir(&ends), &SITE).err(),
        Some("a ray-tracing walk with a path that ends the program")
    );
    // The loop's way out goes back to the header (0xcc: s_branch 0x8c),
    // and the empty-structure check to the exit itself (0x64:
    // s_cbranch_scc0 0xd4): only that check reaches the exit.
    let never = patched(&native(), 0xcc, &[0xBF82_FFEF]);
    let never = patched(&never, 0x64, &[0xBF84_001B]);
    assert_eq!(
        splice(ir(&never), &Site { skip: None, ..SITE }).err(),
        Some("a ray-tracing walk whose header never reaches its exit")
    );
}

/// A walk with an effect the query does not have is refused (Codex review
/// of c83e996, point 2); a stack in LDS is no such effect while nothing
/// outside the walk touches LDS.
#[test]
fn rayquery_refuses_walks_with_effects_the_query_lacks() {
    let effect = Some("a ray-tracing walk with a memory effect the query does not have");
    // A store in the loop (0xb4: buffer_store_dword v1, v19, s[12:15], 0
    // offen, in place of the hit's v_cndmask_b32).
    let store = patched(&native(), 0xb4, &[0xE070_1000, 0x8003_0113]);
    assert_eq!(splice(ir(&store), &SITE).err(), effect);
    // A push on a stack in LDS (0xb4: ds_write_b32 v13, v14).
    let stack = patched(&native(), 0xb4, &[0xD834_0000, 0x0000_0E0D]);
    assert!(splice(ir(&stack), &SITE).is_ok());
    // ... which the code after the walk reads (0xd8: ds_read_b32 v1, v0).
    let read = patched(&stack, 0xd8, &[0xD8D8_0000, 0x0100_0000]);
    assert_eq!(splice(ir(&read), &SITE).err(), effect);
    // ... or reads through a VALU operand (Codex review of eb7d7d8, point
    // 4; 0xd8: v_mov_b32 v1, src_lds_direct, then s_nop 0).
    let direct = patched(&stack, 0xd8, &[0x7E02_02FE, 0xBF80_0000]);
    assert_eq!(splice(ir(&direct), &SITE).err(), effect);
}

/// What the proof cannot check is refused before it plays (Codex review of
/// eb7d7d8): ray flags the host's answer does not model, FORCE_OPAQUE
/// missing among them (point 1: the lowering commits no candidate, so a
/// non-opaque triangle would be missed), and a kept register other than
/// EXEC, which the play does not compare (point 6).
#[test]
fn rayquery_refuses_sites_the_proof_cannot_check() {
    let p = ir(&native());
    let flags = Some("a ray query with ray flags the proof has no model of");
    let without_opaque = Site {
        flags: ray_flags::CULL_BACK_FACING_TRIANGLES,
        ..SITE
    };
    assert_eq!(splice(p.clone(), &without_opaque).err(), flags);
    // RAY_FLAG_ACCEPT_FIRST_HIT_AND_END_SEARCH.
    let first_hit = Site {
        flags: ray_flags::FORCE_OPAQUE | 0x04,
        ..SITE
    };
    assert_eq!(splice(p.clone(), &first_hit).err(), flags);
    let kept = Site {
        kept: &[Reg::V(1)],
        ..SITE
    };
    assert_eq!(
        splice(p, &kept).err(),
        Some("a ray-tracing walk that keeps a register other than EXEC")
    );
    // The review's counterexample: family A's site without FORCE_OPAQUE.
    let (native, site) = family_a();
    let site = Site {
        flags: ray_flags::CULL_BACK_FACING_TRIANGLES,
        ..site
    };
    assert_eq!(
        proof::prove(&ir(&native), &native, &site)
            .map(|_| ())
            .map_err(|f| f.why),
        Err("a ray query with ray flags the proof has no model of")
    );
}

/// The host's answer sets a lane aside (an edge, or fragile) only where the
/// ambiguity can change it (Codex review of eb7d7d8, point 3): a triangle
/// at t-max behind a clear hit, or a grazed one past it, changes nothing;
/// alone at t-max, or grazed before the hit, it does.
#[test]
fn rayquery_host_answer_sets_aside_only_lanes_it_may_change() {
    let ray = |t_max: f32| QueryRay {
        origin: [1.0, 1.0, 0.0],
        direction: [0.0, 0.0, 1.0],
        t_min: 0.0,
        t_max,
        mask: 0xd5,
        flags: ray_flags::FORCE_OPAQUE | ray_flags::CULL_BACK_FACING_TRIANGLES,
    };
    let world = |tris: Vec<Tri>| {
        one_instance(Blas {
            leaves: tris.into_iter().map(|t| vec![t]).collect(),
            cycle: None,
        })
    };
    let answer = world(vec![a_at(3.0), a_at(5.0)]).query(&ray(5.0));
    assert!(answer.hit && !answer.edge && !answer.fragile, "{answer:?}");
    assert_eq!(answer.t, 3.0);
    let answer = world(vec![a_at(5.0)]).query(&ray(5.0));
    assert!(!answer.hit && answer.edge, "{answer:?}");
    // A's shape moved to x = 1: the ray runs on its edge from (1, 0).
    let grazed = |z: f32| -> Tri { [[1.0, 0.0, z], [1.0, 4.0, z], [5.0, 0.0, z]] };
    let answer = world(vec![a_at(3.0), grazed(7.0)]).query(&ray(100.0));
    assert!(answer.hit && !answer.fragile, "{answer:?}");
    assert_eq!(answer.t, 3.0);
    let answer = world(vec![a_at(3.0), grazed(2.0)]).query(&ray(100.0));
    assert!(answer.fragile, "{answer:?}");
}

/// Without a site the program is not spliced: the walk stays, and the
/// translation refuses it as before (no program changes until a walk is
/// proven).
#[test]
fn rayquery_leaves_programs_without_a_site_as_they_are() {
    let native = native();
    let p = ir(&native);
    let kept = super::lower::rayquery::prepared(p.clone(), &native).unwrap();
    assert_eq!(kept, p);
}

/// The skeleton's walk is no walk: the play refuses it (its loop goes to
/// the node its intersection returns, which a world's triangles are not).
#[test]
fn rayquery_proof_refuses_the_skeleton_walk() {
    let native = native();
    assert!(proof::prove(&ir(&native), &native, &SITE).is_err());
}

/// The `PSV0` resource bindings of a container: (type, space, lower and
/// upper register, kind, flags).
fn bindings(dxil: &[u8]) -> Vec<[u32; 6]> {
    let c = Container::parse(dxil).unwrap();
    let psv = c.part(FourCC::PSV0).unwrap().data;
    let at = |o: usize| crate::dxbc::u32_at(psv, o).unwrap();
    let info = at(0) as usize;
    let count = at(4 + info) as usize;
    let size = at(8 + info) as usize;
    (0..count)
        .map(|n| {
            let base = 12 + info + n * size;
            std::array::from_fn(|k| at(base + 4 * k))
        })
        .collect()
}

/// Checks `cs`'s ray-tracing parts and validates it with dxil.dll.
fn check_dxil(cs: &HostShader) {
    assert_eq!(cs.root_srvs, [TLAS_REGISTER]);
    let summary = super::last_module_summary();
    for op in [
        "allocateRayQuery",
        "rayQuery_TraceRayInline",
        "rayQuery_Proceed",
        "rayQuery_StateScalar",
    ] {
        assert!(summary.contains(op), "{summary}");
    }
    // Ray-tracing tier 1.1 (SFI0) and the structure's binding: a raw SRV
    // of kind 16 at t240, space 1, as DXC 10.0.26100 writes them.
    let c = Container::parse(&cs.dxil).unwrap();
    let sfi0 = c.part(FourCC::SFI0).unwrap().data;
    let features = u64::from_le_bytes(sfi0.try_into().unwrap());
    assert_ne!(features & 0x10_0000, 0, "SFI0 {features:#x}");
    assert!(
        bindings(&cs.dxil).contains(&[4, 1, 240, 240, 16, 0]),
        "{:?}",
        bindings(&cs.dxil)
    );
    let path = super::validator::Validator::default_path();
    let Ok(mut v) = super::validator::Validator::load(path.as_deref()) else {
        eprintln!("no dxil.dll; not validated");
        return;
    };
    eprintln!(
        "dxil.dll {} (validator {:?})",
        path.map(|p| p.display().to_string()).unwrap_or_default(),
        v.version()
    );
    v.validate(&mut cs.dxil.clone()).expect("DXIL validation");
}

#[test]
fn rayquery_spliced_program_validates() {
    check_dxil(&translate(&native(), &SITE).unwrap());
}

/// The structure the skeleton's WARP run traces: one BLAS of three
/// triangles, built five times over with instances of different masks,
/// flags and transforms. (D3D12's culling is in object space; a triangle is
/// front facing when its vertices turn clockwise seen from the ray's
/// origin, in D3D12's left-handed frame.)
mod skeleton_world {
    /// The BLAS: non-indexed triangles, three floats a vertex.
    pub const TRIANGLES: [[[f32; 3]; 3]; 3] = [
        // A: front facing for rays along +z, at z = 5.
        [[0.0, 0.0, 5.0], [0.0, 4.0, 5.0], [4.0, 0.0, 5.0]],
        // B: back facing for rays along +z, at z = 3.
        [[10.0, 0.0, 3.0], [14.0, 0.0, 3.0], [10.0, 4.0, 3.0]],
        // C: A again at z = 7, behind it.
        [[0.0, 0.0, 7.0], [0.0, 4.0, 7.0], [4.0, 0.0, 7.0]],
    ];

    use super::world::{CULL_DISABLE, FRONT_CCW};

    /// Instances: object-to-world 3×4 rows, mask, flags.
    pub type Instance = ([[f32; 4]; 3], u8, u8);

    const fn moved(x: f32, z_scale: f32) -> [[f32; 4]; 3] {
        [
            [1.0, 0.0, 0.0, x],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, z_scale, 0.0],
        ]
    }

    pub const INSTANCES: [Instance; 5] = [
        (moved(0.0, 1.0), 0x01, 0),
        // Outside the query's mask (0xd5).
        (moved(20.0, 1.0), 0x02, 0),
        (moved(40.0, 1.0), 0x04, CULL_DISABLE),
        (moved(60.0, 1.0), 0x10, FRONT_CCW),
        // Stretched along z: t is the world's.
        (moved(80.0, 2.0), 0x80, 0),
    ];

    /// A ray: origin, direction, t-max; what the query answers (the hit's
    /// t, none for a miss).
    pub type Ray = ([f32; 3], [f32; 3], f32, Option<f32>);

    const Z: [f32; 3] = [0.0, 0.0, 1.0];

    pub const RAYS: [Ray; 15] = [
        ([1.0, 1.0, 0.0], Z, 100.0, Some(5.0)),
        // t-max short of A.
        ([1.0, 1.0, 0.0], Z, 4.5, None),
        // B's back face, culled.
        ([11.0, 1.0, 0.0], Z, 100.0, None),
        // Outside the mask.
        ([21.0, 1.0, 0.0], Z, 100.0, None),
        ([41.0, 1.0, 0.0], Z, 100.0, Some(5.0)),
        // Culling disabled: B's back face, nearer than nothing else.
        ([51.0, 1.0, 0.0], Z, 100.0, Some(3.0)),
        // Facing flipped: A and C culled...
        ([61.0, 1.0, 0.0], Z, 100.0, None),
        // ... B hit.
        ([71.0, 1.0, 0.0], Z, 100.0, Some(3.0)),
        ([81.0, 1.0, 0.0], Z, 100.0, Some(10.0)),
        // A direction of length 2: t counts it.
        ([1.0, 1.0, -5.0], [0.0, 0.0, 2.0], 100.0, Some(5.0)),
        ([200.0, 200.0, 0.0], Z, 100.0, None),
        // Between A and C: A is behind the origin.
        ([1.0, 1.0, 6.0], Z, 100.0, Some(1.0)),
        // From behind C and A: their back faces, culled.
        ([1.0, 1.0, 10.0], [0.0, 0.0, -1.0], 100.0, None),
        // From A's plane: A at t = t-min (0). DXR leaves out a hit at
        // t-min (WARP: C at 2), where the console's walk takes it (t >= 0:
        // 0.04 there; `rayquery::Kind::T`).
        ([1.0, 1.0, 5.0], Z, 100.0, Some(2.0)),
        // A at t = t-max ([`AT_T_MAX`]): DXR's rule leaves it out
        // (t-min < t < t-max), as the console's walk does (t < t-max);
        // WARP keeps it (5.04 there, measured). Either is taken on WARP.
        ([1.0, 1.0, 0.0], Z, 5.0, Some(5.0)),
    ];

    /// The ray of [`RAYS`] whose hit is at its t-max.
    pub const AT_T_MAX: usize = 14;
}

/// D3D12 on WARP for these tests: acceleration structures built from a
/// scene, a translated program bound (alone, or with its views) and run.
#[cfg(windows)]
mod warp {
    use std::mem::ManuallyDrop;

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;

    use super::super::HostShader;
    use super::super::host::Host;
    use super::TLAS_REGISTER;

    fn err(e: windows::core::Error) -> String {
        e.to_string()
    }

    /// An instance: object-to-world rows, id, mask, hit-group
    /// contribution, flags, BLAS.
    pub type Placed = ([[f32; 4]; 3], u32, u8, u32, u8, usize);

    /// BLASes of non-indexed triangles, and instances over them.
    pub struct Scene {
        pub blases: Vec<Vec<[[f32; 3]; 3]>>,
        pub instances: Vec<Placed>,
    }

    /// A view in the heap, by slot: raw bytes, or the raw output of `n`
    /// dwords.
    pub enum View {
        Srv(Vec<u8>),
        Out(u32),
    }

    /// A buffer of `size` bytes on `heap`, in `state`.
    fn buffer(
        dev: &ID3D12Device5,
        heap: D3D12_HEAP_TYPE,
        size: u64,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> Result<ID3D12Resource, String> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(256),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        };
        let mut r: Option<ID3D12Resource> = None;
        // SAFETY: descriptions that outlive the call.
        unsafe {
            dev.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: heap,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                state,
                None,
                &mut r,
            )
        }
        .map_err(err)?;
        r.ok_or_else(|| "no resource".into())
    }

    /// An upload buffer holding `bytes`.
    fn upload(dev: &ID3D12Device5, bytes: &[u8]) -> Result<ID3D12Resource, String> {
        let b = buffer(
            dev,
            D3D12_HEAP_TYPE_UPLOAD,
            bytes.len() as u64,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )?;
        // SAFETY: the mapping is at least `bytes.len()` long.
        unsafe {
            let mut p = std::ptr::null_mut();
            b.Map(0, None, Some(&mut p)).map_err(err)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
            b.Unmap(0, None);
        }
        Ok(b)
    }

    fn uav_barrier(r: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: ManuallyDrop::new(Some(r.clone())),
                }),
            },
        }
    }

    fn transition(
        r: &ID3D12Resource,
        before: D3D12_RESOURCE_STATES,
        after: D3D12_RESOURCE_STATES,
    ) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: ManuallyDrop::new(Some(r.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        }
    }

    /// Releases what a barrier holds.
    fn drop_barrier(b: D3D12_RESOURCE_BARRIER) {
        // SAFETY: the variant `Type` names is the one written.
        unsafe {
            match b.Type {
                D3D12_RESOURCE_BARRIER_TYPE_UAV => {
                    let mut uav = ManuallyDrop::into_inner(b.Anonymous.UAV);
                    ManuallyDrop::drop(&mut uav.pResource);
                }
                _ => {
                    let mut t = ManuallyDrop::into_inner(b.Anonymous.Transition);
                    ManuallyDrop::drop(&mut t.pResource);
                }
            }
        }
    }

    fn barrier(list: &ID3D12GraphicsCommandList4, b: D3D12_RESOURCE_BARRIER) {
        // SAFETY: an open list; the barrier's resource is alive.
        unsafe { list.ResourceBarrier(std::slice::from_ref(&b)) };
        drop_barrier(b);
    }

    /// An acceleration structure buffer and scratch for `inputs`.
    fn structure(
        dev: &ID3D12Device5,
        inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    ) -> Result<(ID3D12Resource, ID3D12Resource), String> {
        let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        // SAFETY: the inputs point at live descriptions.
        unsafe { dev.GetRaytracingAccelerationStructurePrebuildInfo(inputs, &mut info) };
        let data = buffer(
            dev,
            D3D12_HEAP_TYPE_DEFAULT,
            info.ResultDataMaxSizeInBytes,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
        )?;
        let scratch = buffer(
            dev,
            D3D12_HEAP_TYPE_DEFAULT,
            info.ScratchDataSizeInBytes,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )?;
        Ok((data, scratch))
    }

    /// The device, if it has ray-tracing tier 1.1.
    fn device(host: &Host) -> Result<Option<ID3D12Device5>, String> {
        let dev: ID3D12Device5 = host.device.cast().map_err(err)?;
        let mut o5 = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
        // SAFETY: the query writes the structure of the given size.
        unsafe {
            dev.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS5,
                (&raw mut o5).cast(),
                size_of_val(&o5) as u32,
            )
        }
        .map_err(err)?;
        Ok((o5.RaytracingTier.0 >= D3D12_RAYTRACING_TIER_1_1.0).then_some(dev))
    }

    /// Records the builds of `scene`'s BLASes and TLAS on `list`; the TLAS
    /// and what must live until the list has run.
    fn build(
        dev: &ID3D12Device5,
        list: &ID3D12GraphicsCommandList4,
        scene: &Scene,
    ) -> Result<(ID3D12Resource, Vec<ID3D12Resource>), String> {
        let mut keep = Vec::new();
        let mut blases = Vec::new();
        for tris in &scene.blases {
            let vertices: Vec<u8> = tris
                .iter()
                .flatten()
                .flatten()
                .flat_map(|f| f.to_le_bytes())
                .collect();
            let vb = upload(dev, &vertices)?;
            // SAFETY: a live buffer.
            let address = unsafe { vb.GetGPUVirtualAddress() };
            let geometry = D3D12_RAYTRACING_GEOMETRY_DESC {
                Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
                // Not opaque: the queries' FORCE_OPAQUE makes them so.
                Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_NONE,
                Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                    Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                        Transform3x4: 0,
                        IndexFormat: DXGI_FORMAT_UNKNOWN,
                        VertexFormat: DXGI_FORMAT_R32G32B32_FLOAT,
                        IndexCount: 0,
                        VertexCount: 3 * tris.len() as u32,
                        IndexBuffer: 0,
                        VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                            StartAddress: address,
                            StrideInBytes: 12,
                        },
                    },
                },
            };
            let inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
                Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
                NumDescs: 1,
                DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                    pGeometryDescs: &geometry,
                },
            };
            let (blas, scratch) = structure(dev, &inputs)?;
            // SAFETY: an open list; the inputs and buffers are alive.
            unsafe {
                list.BuildRaytracingAccelerationStructure(
                    &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                        DestAccelerationStructureData: blas.GetGPUVirtualAddress(),
                        Inputs: inputs,
                        SourceAccelerationStructureData: 0,
                        ScratchAccelerationStructureData: scratch.GetGPUVirtualAddress(),
                    },
                    None,
                );
            }
            barrier(list, uav_barrier(&blas));
            blases.push(blas.clone());
            keep.extend([vb, blas, scratch]);
        }
        // The TLAS: D3D12_RAYTRACING_INSTANCE_DESC, 64 bytes each.
        let mut instances = Vec::new();
        for (rows, id, mask, contribution, flags, blas) in &scene.instances {
            for f in rows.iter().flatten() {
                instances.extend(f.to_le_bytes());
            }
            instances.extend((id & 0xFF_FFFF | u32::from(*mask) << 24).to_le_bytes());
            instances.extend((contribution & 0xFF_FFFF | u32::from(*flags) << 24).to_le_bytes());
            // SAFETY: a live structure.
            instances.extend(unsafe { blases[*blas].GetGPUVirtualAddress() }.to_le_bytes());
        }
        let ib = upload(dev, &instances)?;
        // SAFETY: a live buffer.
        let address = unsafe { ib.GetGPUVirtualAddress() };
        let inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
            Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
            Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
            NumDescs: scene.instances.len() as u32,
            DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
            Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                InstanceDescs: address,
            },
        };
        let (tlas, scratch) = structure(dev, &inputs)?;
        // SAFETY: an open list; the inputs and buffers are alive.
        unsafe {
            list.BuildRaytracingAccelerationStructure(
                &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                    DestAccelerationStructureData: tlas.GetGPUVirtualAddress(),
                    Inputs: inputs,
                    SourceAccelerationStructureData: 0,
                    ScratchAccelerationStructureData: scratch.GetGPUVirtualAddress(),
                },
                None,
            );
        }
        barrier(list, uav_barrier(&tlas));
        keep.extend([ib, scratch, tlas.clone()]);
        Ok((tlas, keep))
    }

    /// The root parameter of `cs`'s acceleration structure in
    /// `Host::root_signature_for`'s layout: the user data first, then the
    /// root views by (register, type).
    pub fn tlas_parameter(cs: &HostShader) -> u32 {
        let mut views: Vec<(u8, i32)> = cs
            .root_cbvs
            .iter()
            .map(|&r| (r, D3D12_ROOT_PARAMETER_TYPE_CBV.0))
            .chain(
                cs.root_srvs
                    .iter()
                    .map(|&r| (r, D3D12_ROOT_PARAMETER_TYPE_SRV.0)),
            )
            .chain(
                cs.root_uavs
                    .iter()
                    .map(|&r| (r, D3D12_ROOT_PARAMETER_TYPE_UAV.0)),
            )
            .collect();
        views.sort_unstable();
        views.dedup();
        let at = views
            .iter()
            .position(|&v| v == (TLAS_REGISTER, D3D12_ROOT_PARAMETER_TYPE_SRV.0))
            .expect("the structure's root SRV");
        1 + at as u32
    }

    /// Runs a recorded list and waits for it.
    fn execute(
        dev: &ID3D12Device5,
        queue: &ID3D12CommandQueue,
        list: &ID3D12GraphicsCommandList4,
    ) -> Result<(), String> {
        // SAFETY: a closed list and live objects; the wait polls the fence.
        unsafe {
            list.Close().map_err(err)?;
            queue.ExecuteCommandLists(&[Some(list.cast().map_err(err)?)]);
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(err)?;
            queue.Signal(&fence, 1).map_err(err)?;
            while fence.GetCompletedValue() < 1 {
                if let Err(e) = dev.GetDeviceRemovedReason() {
                    return Err(format!("device removed: {}", err(e)));
                }
                std::thread::yield_now();
            }
            dev.GetDeviceRemovedReason()
                .map_err(|e| format!("device removed: {}", err(e)))
        }
    }

    /// Runs `cs` for one group with `views` in heap slots 0.. and `scene`'s
    /// TLAS at the reserved root SRV; with `dispatch` false, binds it all
    /// and runs the list without dispatching (the bind alone). Returns the
    /// output view's dwords; `Ok(None)` where the device has no ray-tracing
    /// tier 1.1.
    pub fn run(
        host: &Host,
        cs: &HostShader,
        scene: &Scene,
        views: &[View],
        dispatch: bool,
    ) -> Result<Option<Vec<u32>>, String> {
        let Some(dev) = device(host)? else {
            return Ok(None);
        };
        let root = host
            .root_signature_for(&[(cs, D3D12_SHADER_VISIBILITY_ALL)])
            .map_err(|e| format!("root signature: {e}"))?;
        let pso = host.compute_pipeline_with(&root, &cs.dxil).map_err(|e| {
            format!(
                "pipeline (wave size {:?}): {e}
{}",
                cs.wave_size(),
                super::super::last_module_summary()
            )
        })?;
        // SAFETY: D3D12 calls with descriptions that outlive them;
        // resources stay alive until the fence says the GPU is done.
        unsafe {
            let queue: ID3D12CommandQueue = dev
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .map_err(err)?;
            let alloc: ID3D12CommandAllocator = dev
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .map_err(err)?;
            let list: ID3D12GraphicsCommandList4 = dev
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso)
                .map_err(err)?;
            let (tlas, mut keep) = build(&dev, &list, scene)?;
            let heap: ID3D12DescriptorHeap = dev
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: views.len().max(1) as u32,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    ..Default::default()
                })
                .map_err(err)?;
            let size = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let slot = |n: usize| {
                let mut at = heap.GetCPUDescriptorHandleForHeapStart();
                at.ptr += n * size;
                at
            };
            let mut out = None;
            for (n, view) in views.iter().enumerate() {
                match view {
                    View::Srv(bytes) => {
                        let b = upload(&dev, bytes)?;
                        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                            Format: DXGI_FORMAT_R32_TYPELESS,
                            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                                Buffer: D3D12_BUFFER_SRV {
                                    FirstElement: 0,
                                    NumElements: bytes.len().div_ceil(4) as u32,
                                    StructureByteStride: 0,
                                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                                },
                            },
                        };
                        dev.CreateShaderResourceView(&b, Some(&desc), slot(n));
                        keep.push(b);
                    }
                    View::Out(dwords) => {
                        let b = buffer(
                            &dev,
                            D3D12_HEAP_TYPE_DEFAULT,
                            u64::from(*dwords) * 4,
                            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                            D3D12_RESOURCE_STATE_COMMON,
                        )?;
                        dev.CreateUnorderedAccessView(
                            &b,
                            None,
                            Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                                Format: DXGI_FORMAT_R32_TYPELESS,
                                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                                    Buffer: D3D12_BUFFER_UAV {
                                        FirstElement: 0,
                                        NumElements: *dwords,
                                        StructureByteStride: 0,
                                        CounterOffsetInBytes: 0,
                                        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                                    },
                                },
                            }),
                            slot(n),
                        );
                        out = Some((b.clone(), *dwords));
                        keep.push(b);
                    }
                }
            }
            list.SetDescriptorHeaps(&[Some(heap.clone())]);
            list.SetComputeRootSignature(&root);
            // User data: the table starts at heap slot 0 (byte offset 0).
            let user = vec![0u32; cs.user_data_dwords().max(1) as usize];
            list.SetComputeRoot32BitConstants(0, user.len() as u32, user.as_ptr().cast(), 0);
            list.SetComputeRootShaderResourceView(tlas_parameter(cs), tlas.GetGPUVirtualAddress());
            if !dispatch {
                execute(&dev, &queue, &list)?;
                drop((keep, heap, tlas));
                return Ok(Some(Vec::new()));
            }
            list.Dispatch(1, 1, 1);
            let (out, dwords) = out.ok_or("no output view")?;
            barrier(
                &list,
                transition(
                    &out,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
            );
            let readback = buffer(
                &dev,
                D3D12_HEAP_TYPE_READBACK,
                u64::from(dwords) * 4,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )?;
            list.CopyBufferRegion(&readback, 0, &out, 0, u64::from(dwords) * 4);
            execute(&dev, &queue, &list)?;
            let mut p = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut p)).map_err(err)?;
            let words = std::slice::from_raw_parts(p.cast::<u32>(), dwords as usize).to_vec();
            readback.Unmap(0, None);
            drop((keep, heap, tlas, out, readback));
            Ok(Some(words))
        }
    }
}

/// The skeleton's scene for WARP.
fn skeleton_scene() -> warp::Scene {
    warp::Scene {
        blases: vec![skeleton_world::TRIANGLES.to_vec()],
        instances: skeleton_world::INSTANCES
            .iter()
            .enumerate()
            .map(|(n, &(rows, mask, flags))| (rows, n as u32, mask, 0, flags, 0))
            .collect(),
    }
}

/// The spliced skeleton on WARP: each thread's value is its ray's closest
/// opaque hit's t plus 0.04 under the query's mask and back-face culling
/// (with the instances' culling flags), or FLT_MAX. Its rays calibrate the
/// play's host model ([`World::query`]): the same answers.
#[test]
fn rayquery_spliced_program_traces_on_warp() {
    let cs = translate(&native(), &SITE).unwrap();
    // The host model's answers for the same rays and world.
    let model = World {
        root: Root::Box,
        instances: skeleton_world::INSTANCES
            .iter()
            .enumerate()
            .map(|(n, &(rows, mask, flags))| {
                let inst = Instance {
                    to_object: rows,
                    translation_only: false,
                    mask,
                    flags,
                    id: n as u32,
                    contribution: 0,
                    blas: 0,
                };
                // The rows are object to world: invert them.
                Instance {
                    to_object: inst.to_world(),
                    ..inst
                }
            })
            .collect(),
        blases: vec![Blas {
            leaves: skeleton_world::TRIANGLES.iter().map(|t| vec![*t]).collect(),
            cycle: None,
        }],
    };
    for (n, &(origin, direction, t_max, t)) in skeleton_world::RAYS.iter().enumerate() {
        // The model follows DXR's rule at t-max, and calls it an edge.
        let t = if n == skeleton_world::AT_T_MAX {
            None
        } else {
            t
        };
        let a = model.query(&super::lower::rayquery::play::QueryRay {
            origin,
            direction,
            t_min: 0.0,
            t_max,
            mask: 0xd5,
            flags: SITE.flags,
        });
        assert_eq!(a.hit, t.is_some(), "{origin:?} {direction:?} {t_max}");
        if let Some(t) = t {
            assert!((a.t - t).abs() < 1e-5, "{a:?}, {t}");
        }
    }
    let Ok(host) = super::host::Host::new(true) else {
        eprintln!("no WARP device; skipped");
        return;
    };
    // Ray n for thread n; the first rays again for the rest of the wave.
    let mut rays = Vec::new();
    let mut expected = Vec::new();
    for n in 0..THREADS as usize {
        let (origin, direction, t_max, t) = skeleton_world::RAYS[n % skeleton_world::RAYS.len()];
        for f in origin
            .into_iter()
            .chain([t_max])
            .chain(direction)
            .chain([0.0])
        {
            rays.extend(f.to_le_bytes());
        }
        expected.push(t);
    }
    let views = [
        warp::View::Srv(rays),
        // The title's view of its structure: blank, as when the console
        // builder never runs (its root at +0x30 is 0).
        warp::View::Srv(vec![0; 256]),
        warp::View::Out(THREADS),
    ];
    let out = match warp::run(&host, &cs, &skeleton_scene(), &views, true) {
        Ok(Some(out)) => out,
        Ok(None) => {
            eprintln!("WARP has no ray-tracing tier 1.1; skipped");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let hits = out.iter().filter(|&&x| x != f32::MAX.to_bits()).count();
    eprintln!("WARP traced {} rays: {hits} hits", out.len());
    let wrong: Vec<String> = out
        .iter()
        .zip(&expected)
        .enumerate()
        .filter(|&(n, (&got, want))| {
            let missed = got == f32::MAX.to_bits();
            let g = f32::from_bits(got);
            match want {
                _ if n % skeleton_world::RAYS.len() == skeleton_world::AT_T_MAX && missed => false,
                None => !missed,
                // A NaN is no hit, whatever its distance.
                Some(t) => !g.is_finite() || (g - (t + 0.04)).abs() > 1e-4 * t.max(1.0),
            }
        })
        .map(|(n, (&got, want))| {
            let want = want.map_or(f32::MAX, |t| t + 0.04);
            format!("thread {n}: {}, expected {want}", f32::from_bits(got))
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The four children of a top-level box: the lanes that meet each are
/// pushed on the wave's stack with it.
fn push_children() -> String {
    let mut out = String::new();
    for k in 0..4 {
        let c = 36 + k;
        out += &format!(
            "
    s_cmp_eq_u32 s{c}, -1
    s_cbranch_scc1 @k{k}
    v_cmp_eq_u32 s45, s{c}, v15
    v_cmp_eq_u32 s46, s{c}, v16
    s_or_b32 s45, s45, s46
    v_cmp_eq_u32 s46, s{c}, v17
    s_or_b32 s45, s45, s46
    v_cmp_eq_u32 s46, s{c}, v18
    s_or_b32 s45, s45, s46
    s_cmp_eq_u32 s45, 0
    s_cbranch_scc1 @k{k}
    v_writelane_b32 v40, s{c}, s30
    v_writelane_b32 v41, s45, s30
    s_add_i32 s30, s30, 1
k{k}:"
        );
    }
    out
}

/// A box's far children on this lane's stack.
fn push_far() -> String {
    let mut out = String::new();
    for r in [18, 17, 16] {
        out += &format!(
            "
    v_cmp_ne_u32 vcc_lo, -1, v{r}
    s_and_saveexec_b32 s68, vcc_lo
    ds_write_b32 v29, v{r}
    v_add_nc_u32 v29, lit(0x00000080), v29
    s_mov_b32 exec_lo, s68"
        );
    }
    out
}

/// Family A's walk, written for these tests (see the module notes). The
/// table at `s0` holds the rays' descriptor at 0x00 (32-byte records:
/// origin, t-max, direction), the structure's at 0x20, the output's at
/// 0x40 and the tile masks' at 0x60; one group (no group id, which would
/// make the translation ask for 32-lane waves, which the farm's WARP does
/// not make pipelines for).
fn family_a_text() -> String {
    let prologue = "
    s_mov_b32 s3, 0
    s_mov_b32 s2, s0
    s_load_dwordx4 s[4:7], s[2:3], null
    s_load_dwordx4 s[8:11], s[2:3], 0x000040
    s_load_dwordx4 s[12:15], s[2:3], 0x000020
    s_load_dwordx4 s[16:19], s[2:3], 0x000060
    s_mov_b32 s28, exec_lo
    v_lshrrev_b32 v1, 5, v0
    v_lshlrev_b32 v1, 2, v1
    buffer_load_dword v1, v1, s[16:19], 0 offen
    v_lshlrev_b32 v2, v0, 1
    v_and_b32 v2, v1, v2
    v_mov_b32 v1, lit(0x7f7fffff)
    v_cmpx_ne_u32 exec_lo, 0, v2
    s_cbranch_execz @store
    v_lshlrev_b32 v14, 5, v0
    buffer_load_dwordx4 v[1:4], v14, s[4:7], 0 offen
    buffer_load_dwordx3 v[5:7], v14, s[4:7], 0 offen offset:16
    // The structure: its base over 256, its root at +0x30.
    s_lshr_b64 s[20:21], s[12:13], 8
    s_mov_b32 s22, s20
    s_mov_b32 s23, 0
    s_lshl_b64 s[22:23], s[22:23], 8
    s_or_b32 s24, s22, 48
    s_mov_b32 s25, s23
    s_load_dword s26, s[24:25], null
    v_mov_b32 v8, 0
    v_mov_b32 v9, 0
    s_cmp_gt_i32 s26, 0
    s_cbranch_scc0 @skip
    // The inverse direction, this lane's stack in LDS, the wave's stack
    // (nodes in v40's lanes, lane masks in v41's) with the root.
    v_rcp_f32 v10, v5
    v_rcp_f32 v11, v6
    v_rcp_f32 v12, v7
    v_mbcnt_lo_u32_b32 v13, -1, 0
    v_lshlrev_b32 v13, 2, v13
    s_mov_b32 s29, exec_lo
    v_writelane_b32 v40, s26, 0
    v_writelane_b32 v41, s29, 0
    s_mov_b32 s30, 1
walk:
    s_add_i32 s30, s30, -1
    v_readlane_b32 s31, v40, s30
    v_readlane_b32 s32, v41, s30
    s_and_b32 s32, s32, s29
    s_cmp_eq_u32 s32, 0
    s_cbranch_scc1 @next
    s_and_b32 s33, s31, 7
    s_cmp_eq_u32 s33, 6
    s_cbranch_scc1 @instance
    // A box: its children from memory, each lane's sorted hits.
    s_lshr_b32 s34, s31, 3
    s_lshl_b32 s34, s34, 6
    s_add_u32 s34, s34, s22
    s_addc_u32 s35, s23, 0
    s_load_dwordx4 s[36:39], s[34:35], null
    s_mov_b32 s40, s20
    s_brev_b32 s41, 1
    s_mov_b32 s42, -1
    s_mov_b32 s43, lit(0x890003ff)
    s_mov_b32 s44, exec_lo
    s_mov_b32 exec_lo, s32
    v_mov_b32 v14, s31
    image_bvh_intersect_ray v[15:18], [v14,v4,v1,v2,v3,v5,v6,v7,v10,v11,v12], s[40:43] dmask:0xf dim:SQ_RSRC_IMG_1D r128 unorm";
    let instance = "
    s_mov_b32 exec_lo, s44
    s_branch @next
instance:
    // An instance: mask, flags, BLAS, world-to-object transform.
    s_lshr_b32 s34, s31, 3
    s_lshl_b32 s34, s34, 6
    s_add_u32 s34, s34, s22
    s_addc_u32 s35, s23, 0
    s_load_dwordx4 s[36:39], s[34:35], 0x000030
    s_load_dwordx8 s[44:51], s[34:35], 0x000040
    s_load_dwordx4 s[52:55], s[34:35], 0x000060
    s_and_b32 s56, s36, lit(0xd5000000)
    s_cmp_eq_u32 s56, 0
    s_cbranch_scc1 @next
    // The facing factor: 0 without culling, -1 counter-clockwise, else 1.
    s_lshr_b32 s57, s37, 24
    s_bfe_u32 s58, s57, lit(0x00010000)
    s_bfe_u32 s59, s57, lit(0x00010001)
    s_cmp_eq_u32 s59, 0
    s_cselect_b32 s60, 1, -1
    s_cmp_eq_u32 s58, 0
    s_cselect_b32 s60, s60, 0
    s_brev_b32 s61, -4
    s_and_b32 s61, s38, s61
    s_mov_b32 s62, s39
    s_mov_b32 s63, 0
    s_lshl_b64 s[64:65], s[62:63], 8
    s_mov_b32 s66, exec_lo
    s_mov_b32 exec_lo, s32
    s_cmp_lt_i32 s38, 0
    s_cbranch_scc1 @translate
    v_fma_f32 v19, s47, v1, s44
    v_fmac_f32 v19, s48, v2
    v_fmac_f32 v19, s49, v3
    v_fma_f32 v20, s50, v1, s45
    v_fmac_f32 v20, s51, v2
    v_fmac_f32 v20, s52, v3
    v_fma_f32 v21, s53, v1, s46
    v_fmac_f32 v21, s54, v2
    v_fmac_f32 v21, s55, v3
    v_mul_f32 v22, s47, v5
    v_fmac_f32 v22, s48, v6
    v_fmac_f32 v22, s49, v7
    v_mul_f32 v23, s50, v5
    v_fmac_f32 v23, s51, v6
    v_fmac_f32 v23, s52, v7
    v_mul_f32 v24, s53, v5
    v_fmac_f32 v24, s54, v6
    v_fmac_f32 v24, s55, v7
    v_rcp_f32 v25, v22
    v_rcp_f32 v26, v23
    v_rcp_f32 v27, v24
    s_branch @blas
translate:
    v_add_f32 v19, s44, v1
    v_add_f32 v20, s45, v2
    v_add_f32 v21, s46, v3
    v_mov_b32 v22, v5
    v_mov_b32 v23, v6
    v_mov_b32 v24, v7
    v_mov_b32 v25, v10
    v_mov_b32 v26, v11
    v_mov_b32 v27, v12
blas:
    // Each lane walks the BLAS from its root.
    v_mov_b32 v28, s61
    v_mov_b32 v29, v13
    s_mov_b32 s40, s62
    s_brev_b32 s41, 1
    s_mov_b32 s42, -1
    s_mov_b32 s43, lit(0x890003ff)
blas_loop:
    v_add_nc_u32 v9, 1, v9
    image_bvh_intersect_ray v[15:18], [v28,v4,v19,v20,v21,v22,v23,v24,v25,v26,v27], s[40:43] dmask:0xf dim:SQ_RSRC_IMG_1D r128 unorm
    v_and_b32 v30, 7, v28
    v_mov_b32 v31, -1
    s_mov_b32 s67, exec_lo
    // A box: the nearest child next, the others on the stack.
    v_and_b32 v32, 6, v30
    v_cmpx_eq_u32 exec_lo, 4, v32
    s_cbranch_execz @tris
    v_mov_b32 v31, v15";
    let rest = "
tris:
    // A triangle: kept at 0 <= t < t-max, facing as the instance culls.
    s_mov_b32 exec_lo, s67
    v_cmpx_gt_u32 exec_lo, 2, v30
    s_cbranch_execz @pop
    v_rcp_f32 v33, v16
    v_mul_legacy_f32 v33, v33, v15
    v_mul_i32_i24 v34, sext(v16), s60 src0_sel:WORD_1
    v_cmp_le_f32 vcc_lo, 0, v33
    v_cmp_lt_f32 s69, v33, v4
    s_and_b32 vcc_lo, vcc_lo, s69
    v_cmp_lt_i32 s69, -1, v34
    s_and_b32 vcc_lo, vcc_lo, s69
    v_cndmask_b32 v4, v4, v33, vcc_lo
    v_cndmask_b32 v8, v8, v28, vcc_lo
    // The first triangle of a pair: the second next.
    v_cmpx_eq_u32 exec_lo, 0, v30
    s_cbranch_execz @pop
    v_lshrrev_b32 v35, 3, v28
    v_lshlrev_b32 v35, 6, v35
    v_add_co_u32 v35, s70, s64, v35
    v_add_co_ci_u32 v36, s70, s65, 0, s70
    global_load_dword v37, v[35:36], null inst_offset:48
    v_bfe_u32 v37, v37, 7, 1
    v_cmpx_ne_u32 exec_lo, 0, v37
    s_cbranch_execz @pop
    v_or_b32 v31, 1, v28
pop:
    // No next node: one from the stack, if any.
    s_mov_b32 exec_lo, s67
    v_cmp_eq_u32 vcc_lo, -1, v31
    s_and_saveexec_b32 s68, vcc_lo
    v_cmpx_ne_u32 exec_lo, v13, v29
    v_add_nc_u32 v29, lit(0xffffff80), v29
    ds_read_b32 v31, v29
    s_mov_b32 exec_lo, s68
    v_mov_b32 v28, v31
    // Lanes past the cap leave every walk; lanes with nothing left, this one.
    v_cmp_lt_u32 vcc_lo, lit(0x0000fffe), v9
    s_andn2_b32 s29, s29, vcc_lo
    v_cmp_ne_u32 vcc_lo, -1, v28
    s_and_b32 vcc_lo, vcc_lo, s29
    s_and_b32 exec_lo, exec_lo, vcc_lo
    s_cbranch_execnz @blas_loop
    s_mov_b32 exec_lo, s66
next:
    s_cmp_eq_u32 s30, 0
    s_cbranch_scc0 @walk
    s_branch @after
skip:
    v_mov_b32 v9, 0
after:
    // A hit, or the cap passed: t + 0.04; else FLT_MAX.
    v_cmp_ne_u32 vcc_lo, 0, v8
    v_cmp_lt_u32 s0, lit(0x0000fffe), v9
    s_or_b32 vcc_lo, s0, vcc_lo
    v_add_f32 v1, lit(0x3d23d70a), v4
    v_cndmask_b32 v1, lit(0x7f7fffff), v1, vcc_lo
store:
    s_mov_b32 exec_lo, s28
    v_lshlrev_b32 v2, 2, v0
    buffer_store_dword v1, v2, s[8:11], 0 offen
    s_endpgm";
    format!(
        "{prologue}{}{instance}{}{rest}",
        push_children(),
        push_far()
    )
}

/// The family A program and its site, from [`family_a_text`] with `edit`
/// applied to its text.
fn family_a_with(edit: impl Fn(String) -> String) -> (NativeProgram, Site) {
    let (words, labels) = asm::assemble(&edit(family_a_text()));
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        // f32 denormals flushed, f16 kept: the title's.
        float_mode: 0xC0,
        vgprs: 48,
        layout: table(),
        used: vec![
            used(ResourceType::Srv, 0),
            used(ResourceType::Srv, 8),
            used(ResourceType::Uav, 16),
            used(ResourceType::Srv, 24),
        ],
        io: StageIo::Cs(ComputeIo {
            // One user SGPR, 2 KiB of LDS.
            rsrc2: 1 << 1 | 4 << 15,
            threads: [THREADS, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let site = Site {
        header: labels["walk"],
        exit: labels["after"],
        skip: Some(labels["skip"]),
        origin: [1, 2, 3],
        direction: [5, 6, 7],
        t_max: 4,
        t_min: 0,
        mask: 0xd5,
        flags: ray_flags::FORCE_OPAQUE | ray_flags::CULL_BACK_FACING_TRIANGLES,
        results: &[(8, Kind::Hit), (4, Kind::T), (9, Kind::Count)],
        kept: &[Reg::S(EXEC)],
        cap: Some(0xfffe),
        tlas: Source {
            user_sgpr: 0,
            offset: 0x20,
        },
    };
    (native, site)
}

fn family_a() -> (NativeProgram, Site) {
    family_a_with(|t| t)
}

/// `native` without its LDS. Spliced, the family A program uses none (its
/// walk did); with an allocation, a one-wave group asks for 32-lane waves
/// (`WaveSize`, as `32ed072b9f845048` does), which the farm's WARP makes no
/// pipeline for. The WARP runs translate this copy; the proof and the
/// validation take the program as it is.
fn without_lds(native: &NativeProgram) -> NativeProgram {
    let mut n = native.clone();
    if let StageIo::Cs(io) = &mut n.io {
        io.rsrc2 &= !(0x1FF << 15);
    }
    n
}

#[test]
fn rayquery_family_a_program_builds_and_splices() {
    let (native, site) = family_a();
    let p = ir(&native);
    let names: Vec<&str> = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .map(|i| i.name())
        .collect();
    assert!(names.contains(&"image_bvh_intersect_ray"));
    let spliced = splice(p, &site).unwrap();
    let left: Vec<&str> = spliced
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .map(|i| i.name())
        .collect();
    // The walk, the root's read, the check and the walk's set-up are gone;
    // the rays' loads, the tile mask and the store stay.
    for gone in [
        "image_bvh_intersect_ray",
        "s_load_dword",
        "s_cmp_gt_i32",
        "v_rcp_f32",
        "v_writelane_b32",
        "ds_read_b32",
    ] {
        assert!(!left.contains(&gone), "{gone}: {left:?}");
    }
    for kept in [
        "buffer_load_dword",
        "buffer_load_dwordx4",
        "buffer_load_dwordx3",
        "v_cmpx_ne_u32",
        "buffer_store_dword",
    ] {
        assert!(left.contains(&kept), "{kept}: {left:?}");
    }
}

/// The proof by play holds for family A's walk.
#[test]
fn rayquery_proves_the_family_a_walk_by_play() {
    let (native, site) = family_a();
    let p = ir(&native);
    let report = proof::prove(&p, &native, &site).unwrap_or_else(|f| panic!("{f:?}"));
    eprintln!("{report:?}");
    assert_eq!(report.worlds, proof::WORLDS);
    assert!(report.lanes > 300, "{report:?}");
}

/// Sites that say something else than the walk does, and walks that do
/// something else than their site says, are refused.
#[test]
fn rayquery_proof_refuses_what_the_walk_does_not_do() {
    let (native, site) = family_a();
    let p = ir(&native);
    let refused = |site: &Site| {
        proof::prove(&p, &native, site)
            .map(|_| ())
            .map_err(|f| f.why)
    };
    // Another mask, no culling, another t register, EXEC not kept.
    assert!(refused(&Site { mask: 0xff, ..site }).is_err());
    assert!(
        refused(&Site {
            flags: ray_flags::FORCE_OPAQUE,
            ..site
        })
        .is_err()
    );
    assert!(
        refused(&Site {
            results: &[(8, Kind::Hit), (5, Kind::T), (9, Kind::Count)],
            ..site
        })
        .is_err()
    );
    assert_eq!(
        refused(&Site { kept: &[], ..site }),
        Err("the code after a ray-tracing walk reads a register the walk changes")
    );
    // The structure's descriptor elsewhere: the walk reads past its world.
    assert!(
        refused(&Site {
            tlas: Source {
                user_sgpr: 0,
                offset: 0x60,
            },
            ..site
        })
        .is_err()
    );
    // A walk that ignores masks, and one that never culls.
    for (from, to) in [
        ("lit(0xd5000000)", "lit(0xff000000)"),
        (
            "v_cmp_lt_i32 s69, -1, v34",
            "v_cmp_lt_i32 s69, lit(0x80000000), v34",
        ),
    ] {
        let (native, site) = family_a_with(|t| t.replace(from, to));
        let p = ir(&native);
        assert!(proof::prove(&p, &native, &site).is_err(), "{to}");
    }
}

/// Rays DXR leaves undefined (a device may hang on them: a zero or NaN
/// direction, a NaN origin, a t-max below t-min or NaN) miss, as the
/// console's walk and the proof's host do; the other lanes of the wave
/// trace as before. On WARP, family A's program over two triangles that
/// cover its rays' square at z = 5.
#[test]
fn rayquery_family_a_misses_rays_dxr_leaves_undefined_on_warp() {
    let (native, site) = family_a();
    let cs = translate_with(&without_lds(&native), &site, false).unwrap();
    let Ok(host) = super::host::Host::new(true) else {
        eprintln!("no WARP device; skipped");
        return;
    };
    let square = Blas {
        leaves: vec![
            vec![a_at(5.0)],
            vec![[[4.0, 4.0, 5.0], [4.0, 0.0, 5.0], [0.0, 4.0, 5.0]]],
        ],
        cycle: None,
    };
    let w = one_instance(square);
    let scene = warp::Scene {
        blases: w
            .blases
            .iter()
            .map(|b| b.triangles().copied().collect())
            .collect(),
        instances: w
            .instances
            .iter()
            .map(|i| (i.to_world(), i.id, i.mask, i.contribution, i.flags, i.blas))
            .collect(),
    };
    let undefined: [([f32; 3], [f32; 3], f32); 5] = [
        ([1.0, 0.5, 0.0], [0.0, 0.0, 0.0], 20.0),
        ([f32::NAN, 0.5, 0.0], [0.0, 0.0, 1.0], 20.0),
        ([1.0, 0.5, 0.0], [0.0, f32::NAN, 1.0], 20.0),
        ([1.0, 0.5, 0.0], [0.0, 0.0, 1.0], -1.0),
        ([1.0, 0.5, 0.0], [0.0, 0.0, 1.0], f32::NAN),
    ];
    let mut records = Vec::new();
    for l in 0..THREADS as usize {
        // Lanes off the square's diagonal, straight up to z = 5.
        let fine = (
            [
                0.25 + (l % 6) as f32 * 0.25,
                0.25 + (l / 6) as f32 * 0.1,
                0.0,
            ],
            [0.0, 0.0, 1.0],
            20.0,
        );
        let (origin, direction, t_max) = undefined.get(l % 8).copied().unwrap_or(fine);
        for x in origin
            .into_iter()
            .chain([t_max])
            .chain(direction)
            .chain([0.0])
        {
            records.extend(x.to_le_bytes());
        }
    }
    let views = [
        warp::View::Srv(records),
        warp::View::Srv(vec![0; 256]),
        warp::View::Out(THREADS),
        warp::View::Srv(u32::MAX.to_le_bytes().to_vec()),
    ];
    let out = match warp::run(&host, &cs, &scene, &views, true) {
        Ok(Some(out)) => out,
        Ok(None) => {
            eprintln!("WARP has no ray-tracing tier 1.1; skipped");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    for (l, &got) in out.iter().enumerate() {
        let got = f32::from_bits(got);
        if l % 8 < undefined.len() {
            assert_eq!(got, f32::MAX, "lane {l}: an undefined ray misses");
        } else {
            assert!((got - 5.04).abs() < 1e-4, "lane {l}: {got}");
        }
    }
}

/// The structure's descriptor is found where the dispatch looks: in the
/// table whose address the site's user SGPR holds, at the site's offset; a
/// site whose SGPR starts no table, or holds root constants, has none, and
/// its walk is not spliced.
#[test]
fn rayquery_finds_the_structure_s_table_or_refuses_the_site() {
    let (native, site) = family_a();
    assert_eq!(
        rayquery::table_of(&native, &site.tlas),
        Some(TlasDescriptor {
            slot: 0,
            offset: 0x20
        })
    );
    // A second table at user SGPR 1, root parameter 3, with two ranges.
    let mut two = native.clone();
    let range = |table_offset_dwords| RootParam {
        root: RootType::Table,
        table_offset_dwords,
        sgprs: 1,
        base_register: 0,
        user_data: 1,
        slot: 3,
    };
    two.layout.extend([range(0), range(8)]);
    let at_1 = Source {
        user_sgpr: 1,
        offset: 0x40,
    };
    assert_eq!(
        rayquery::table_of(&two, &at_1),
        Some(TlasDescriptor {
            slot: 3,
            offset: 0x40
        })
    );
    // No parameter at SGPR 2; root constants at SGPR 0.
    let at_2 = Source {
        user_sgpr: 2,
        offset: 0x20,
    };
    assert_eq!(rayquery::table_of(&two, &at_2), None);
    let mut constants = native.clone();
    constants.layout[0].root = RootType::Constants;
    assert_eq!(rayquery::table_of(&constants, &site.tlas), None);
    let refused =
        |native: &NativeProgram, site: &Site| match rayquery::spliced(ir(native), native, site) {
            Err(crate::ShaderError::Unsupported { what, .. }) => what,
            other => panic!("{:?}", other.map(|_| ())),
        };
    let why = "a ray-tracing walk whose structure is not in one descriptor table";
    assert_eq!(refused(&constants, &site), why);
    assert_eq!(refused(&native, &Site { tlas: at_2, ..site }), why);
}

/// The proof of the walks `rayquery::SITES` names, for a
/// container of the title's given by path (`XTP_RT_CONTAINER`; its bytes
/// never enter the repository): each report, or the failure with its world,
/// lane and values.
#[test]
#[ignore = "needs a container of the title's: XTP_RT_CONTAINER"]
fn rayquery_proves_a_title_container_by_play() {
    let Some(path) = std::env::var_os("XTP_RT_CONTAINER") else {
        return;
    };
    let data = std::fs::read(path).unwrap();
    let c = Container::parse(&data).unwrap();
    let part = c.part(FourCC::DXIL).unwrap();
    let mut proven = 0;
    for native in crate::native::native_programs(part.data).unwrap() {
        let Some(site) = super::lower::rayquery::site_of(&native.code) else {
            continue;
        };
        let started = std::time::Instant::now();
        let report = proof::prove(&ir(&native), &native, site).unwrap_or_else(|f| panic!("{f:?}"));
        eprintln!("{report:?} in {:?}", started.elapsed());
        // Where the dispatch finds the title's descriptor of the structure.
        let tlas = rayquery::tlas_descriptor(&native).expect("the structure's table");
        eprintln!("{tlas:?}");
        proven += 1;
    }
    assert!(proven > 0, "no site in this container");
    // As the runtime translates it: spliced and valid.
    let cs = super::translate(&data).unwrap_or_else(|e| panic!("{e}"));
    check_dxil(&cs);
    // Its DXIL, for a look at its loops (`dxc -dumpbin`), where
    // `XTP_RT_DXIL_OUT` says: outside the repository, as the container.
    if let Some(out) = std::env::var_os("XTP_RT_DXIL_OUT") {
        std::fs::write(out, &cs.dxil).unwrap();
    }
    // With `XTP_TEST_ADAPTER=hardware` (or unset), its compute pipeline on
    // the default hardware adapter, translated for that adapter's waves: a
    // driver's compiler that fails on it fails this process only (VIctory
    // #2309 saw one recurse until its stack overflowed).
    match super::host::Host::new(false) {
        Ok(host) => {
            let adapters = super::host::adapters().unwrap_or_default();
            let cs = super::translate_for(&data, host.target()).unwrap_or_else(|e| panic!("{e}"));
            let started = std::time::Instant::now();
            host.compute_pipeline(&cs)
                .unwrap_or_else(|e| panic!("{adapters:?}: {e}"));
            eprintln!(
                "compute pipeline made on {:?} in {:?}",
                adapters.first(),
                started.elapsed()
            );
        }
        Err(e) => eprintln!("no device for its pipeline: {e}"),
    }
}

/// Lane 0's ray, planted for group 0 with only lane 0 in the tile mask.
fn planted(origin: [f32; 3], direction: [f32; 3], t_max: f32) -> HashMap<u64, u32> {
    let mut m = HashMap::new();
    let rays = Play::buffer(0, 0x00);
    for (k, x) in origin
        .into_iter()
        .chain([t_max])
        .chain(direction)
        .enumerate()
    {
        m.insert(rays + 4 * k as u64, x.to_bits());
    }
    m.insert(Play::buffer(0, 0x60), 1);
    m
}

/// A world of one translation-only instance (at the origin) of `blas`.
fn one_instance(blas: Blas) -> World {
    World {
        root: Root::Box,
        instances: vec![Instance {
            to_object: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
            ],
            translation_only: true,
            mask: 0x01,
            flags: 0,
            id: 0,
            contribution: 0,
            blas: 0,
        }],
        blases: vec![blas],
    }
}

/// The skeleton's triangle A at depth `z` (front facing for rays along +z).
fn a_at(z: f32) -> Tri {
    [[0.0, 0.0, z], [0.0, 4.0, z], [4.0, 0.0, z]]
}

/// Lane 0's stored value in a play's stores.
fn lane0(stores: &[(usize, u64, u32)]) -> f32 {
    let &(_, _, x) = stores.iter().find(|s| s.0 == 0).expect("lane 0's store");
    f32::from_bits(x)
}

/// Where the console's walk and DXR differ, with distinct values (rt-dxr
/// section 9, `Kind::T`, `Kind::Count`):
///
/// * a triangle at t = t-min (0): the walk keeps it (0 <= t), DXR does not
///   (t-min < t);
/// * a triangle at t = t-max: both leave it out by their rules (WARP keeps
///   it: the lane is an edge, left out of proofs);
/// * a walk caught in a cycle passes its 0xfffe iterations and counts as a
///   hit at its t-max; the host answers the whole query.
#[test]
fn rayquery_edge_cases_differ_as_documented() {
    let (native, site) = family_a();
    let p = ir(&native);
    let spliced = splice(p.clone(), &site).unwrap();
    let prover = Prover::new(&site, &native, &p, &spliced).unwrap();
    let z = [0.0, 0.0, 1.0];
    let play = |seed, plant: HashMap<u64, u32>, world: World| {
        let played = prover
            .play(seed, 0, Root::Box, &plant, &mut |_, _| world.clone())
            .unwrap_or_else(|f| panic!("{f:?}"));
        let report = &mut proof::Report::default();
        prover
            .compare(seed, &played, report)
            .unwrap_or_else(|f| panic!("{f:?}"));
        (lane0(&played.guest), lane0(&played.host), *report)
    };
    let pair = Blas {
        leaves: vec![vec![a_at(5.0)], vec![a_at(7.0)]],
        cycle: None,
    };
    // From A's plane: the walk takes A at 0, DXR takes the next at 2.
    let (guest, host, report) = play(
        1,
        planted([1.0, 1.0, 5.0], z, 100.0),
        one_instance(pair.clone()),
    );
    assert_eq!(guest, 0.04);
    assert!((host - 2.04).abs() < 1e-6, "{host}");
    assert_eq!(report.edges, 1);
    // A at t-max: neither takes it; an edge all the same.
    let (guest, host, report) = play(2, planted([1.0, 1.0, 0.0], z, 5.0), one_instance(pair));
    assert_eq!((guest, host), (f32::MAX, f32::MAX));
    assert_eq!(report.edges, 1);
    // A cycle nearer than A at 3: the walk passes its cap and reports a
    // hit at its t-max (100); the host finds A.
    let cycle = Blas {
        leaves: vec![vec![a_at(3.0)]],
        cycle: Some([[0.0, 0.0, 0.5], [4.0, 4.0, 0.6]]),
    };
    let (guest, host, report) = play(3, planted([1.0, 1.0, 0.0], z, 100.0), one_instance(cycle));
    assert!((guest - 100.04).abs() < 1e-4, "{guest}");
    assert!((host - 3.04).abs() < 1e-6, "{host}");
    assert_eq!(report.capped, 1);
}

/// The family A program, proven, spliced and translated: it validates, and
/// its root signature with the structure's root SRV binds alone on WARP,
/// before any dispatch (xcl #2189's RayQuery differential failed there).
#[test]
fn rayquery_family_a_translates_validates_and_binds_alone() {
    let (native, site) = family_a();
    check_dxil(&translate_with(&native, &site, true).unwrap());
    let cs = translate_with(&without_lds(&native), &site, false).unwrap();
    assert_eq!(cs.wave_size(), None);
    let Ok(host) = super::host::Host::new(true) else {
        eprintln!("no WARP device; skipped");
        return;
    };
    match warp::run(&host, &cs, &skeleton_scene(), &[], false) {
        Ok(Some(_)) => {}
        Ok(None) => eprintln!("WARP has no ray-tracing tier 1.1; skipped"),
        Err(e) => panic!("{e}"),
    }
}

/// End to end on WARP: the family A program, proven by play and spliced,
/// traces a world of the play's against a host TLAS built from it, with the
/// title's own view of its structure blank (its root reads 0). Each thread
/// stores what the guest walk stored in the play, t + 0.04 or FLT_MAX.
#[test]
fn rayquery_family_a_end_to_end_on_warp() {
    let (native, site) = family_a();
    let p = ir(&native);
    let spliced = splice(p.clone(), &site).unwrap();
    let prover = Prover::new(&site, &native, &p, &spliced).unwrap();
    // 32 rays from a small square towards +z, every lane in the tile mask.
    let mut rng = Rng::new(7);
    let mut plant = HashMap::new();
    let mut records = Vec::new();
    for l in 0..THREADS as u64 {
        let origin = [rng.range(0.0, 4.0) as f32, rng.range(0.0, 4.0) as f32, 0.0];
        let direction = [
            rng.range(-0.2, 0.2) as f32,
            rng.range(-0.2, 0.2) as f32,
            1.0,
        ];
        let t_max = rng.range(5.0, 20.0) as f32;
        for (k, x) in origin
            .into_iter()
            .chain([t_max])
            .chain(direction)
            .chain([0.0])
            .enumerate()
        {
            plant.insert(Play::buffer(0, 0x00) + 32 * l + 4 * k as u64, x.to_bits());
            records.extend(x.to_le_bytes());
        }
    }
    plant.insert(Play::buffer(0, 0x60), u32::MAX);
    let played = prover
        .play(11, 0, Root::Box, &plant, &mut |rays, rng| {
            // A world of a few instances at least.
            loop {
                let w = world::around(rays, Root::Box, u32::from(site.mask), rng);
                if w.instances.len() >= 5 {
                    break w;
                }
            }
        })
        .unwrap_or_else(|f| panic!("{f:?}"));
    prover
        .compare(11, &played, &mut proof::Report::default())
        .unwrap_or_else(|f| panic!("{f:?}"));
    let cs = translate_with(&without_lds(&native), &site, false).unwrap();
    let Ok(host) = super::host::Host::new(true) else {
        eprintln!("no WARP device; skipped");
        return;
    };
    let w = &played.world;
    let scene = warp::Scene {
        blases: w
            .blases
            .iter()
            .map(|b| b.triangles().copied().collect())
            .collect(),
        instances: w
            .instances
            .iter()
            .map(|i| (i.to_world(), i.id, i.mask, i.contribution, i.flags, i.blas))
            .collect(),
    };
    let views = [
        warp::View::Srv(records),
        warp::View::Srv(vec![0; 256]),
        warp::View::Out(THREADS),
        warp::View::Srv(u32::MAX.to_le_bytes().to_vec()),
    ];
    let out = match warp::run(&host, &cs, &scene, &views, true) {
        Ok(Some(out)) => out,
        Ok(None) => {
            eprintln!("WARP has no ray-tracing tier 1.1; skipped");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let guest: HashMap<usize, u32> = played.guest.iter().map(|&(l, _, x)| (l, x)).collect();
    let mut hits = 0;
    let mut wrong = Vec::new();
    for (l, &got) in out.iter().enumerate() {
        let a = played.answers[l].expect("an answer for every lane");
        if a.edge || a.fragile {
            continue;
        }
        let want = guest[&l];
        hits += usize::from(want != f32::MAX.to_bits());
        let (g, w) = (f32::from_bits(got), f32::from_bits(want));
        let far = !g.is_finite() || (g - w).abs() > 1e-4 * w.abs().max(1.0);
        if got != want && far {
            wrong.push(format!("lane {l}: WARP {g}, walk {w}"));
        }
    }
    eprintln!(
        "WARP traced {} rays of a world of {} instances: {hits} hits",
        out.len(),
        w.instances.len()
    );
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert!(hits >= 6, "{hits} hits");
}
