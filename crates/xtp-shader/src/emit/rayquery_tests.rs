//! Inline ray queries in place of a traversal walk (RT phase 2b,
//! `lower/rayquery.rs`), on a synthetic program written for these tests
//! (never the title's code): a compute program of family A's outline
//! (docs/analysis/rt-dxr.md section 4.2). It reads its descriptors from a
//! user-data table, a ray per thread (origin, t-max, direction), loops
//! around `image_bvh_intersect_ray` over the structure's descriptor, then
//! stores t + 0.04 where the walk recorded a hit and FLT_MAX elsewhere, as
//! the local lights' traced shadows do.
//!
//! Its loop is a skeleton, not a full walk: these tests check the splice
//! and the DXIL around a site described by hand (section 5.2), and the
//! spliced program's answers on WARP against an acceleration structure
//! built here with D3D12. Proving a walk by play (section 5.3) needs a full
//! walk and is the next step of the phase.

use super::HostShader;
use super::lower::rayquery::{Kind, Site, TLAS_REGISTER, ray_flags, splice};
use crate::dxbc::{Container, FourCC};
use crate::ir::{Program, Reg};
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
};

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

fn native() -> NativeProgram {
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        float_mode: 0,
        vgprs: 24,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
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
/// spliced where `rayquery::prepared` splices the walks it knows.
fn translate(native: &NativeProgram, site: &Site) -> Result<HostShader, crate::ShaderError> {
    let c = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: vec![],
    };
    let p = splice(ir(native), site).map_err(|what| crate::ShaderError::Unsupported {
        offset: site.header,
        what,
    })?;
    let st = crate::structure::structure(&p);
    let res = crate::resource::track_native(&p, native, st.as_ref().ok())?;
    let io = crate::io::raise(&p, native)?;
    let st = st?;
    super::emit_for(&c, native, &p, &st, &io, &res, super::Target::ANY)
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

/// [`PROGRAM`] with its words from byte offset `at` replaced by `words`.
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
    // The loop's way out ends the program (0xcc: s_endpgm).
    let ends = patched(&native(), 0xcc, &[0xBF81_0000]);
    assert_eq!(
        splice(ir(&ends), &SITE).err(),
        Some("a ray-tracing walk with a path that ends the program")
    );
    // The loop never leaves (0xc8: s_branch 0x8c): only the skip path
    // reaches the exit.
    let never = patched(&native(), 0xc8, &[0xBF82_FFF0]);
    assert_eq!(
        splice(ir(&never), &SITE).err(),
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

#[test]
fn rayquery_spliced_program_validates() {
    let cs = translate(&native(), &SITE).unwrap();
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

/// The structure the WARP run traces: one BLAS of three triangles, built
/// five times over with instances of different masks, flags and
/// transforms. (D3D12's culling is in object space; a triangle is front
/// facing when its vertices turn clockwise seen from the ray's origin, in
/// D3D12's left-handed frame.)
mod world {
    /// The BLAS: non-indexed triangles, three floats a vertex.
    pub const TRIANGLES: [[f32; 3]; 9] = [
        // A: front facing for rays along +z, at z = 5.
        [0.0, 0.0, 5.0],
        [0.0, 4.0, 5.0],
        [4.0, 0.0, 5.0],
        // B: back facing for rays along +z, at z = 3.
        [10.0, 0.0, 3.0],
        [14.0, 0.0, 3.0],
        [10.0, 4.0, 3.0],
        // C: A again at z = 7, behind it.
        [0.0, 0.0, 7.0],
        [0.0, 4.0, 7.0],
        [4.0, 0.0, 7.0],
    ];

    /// D3D12_RAYTRACING_INSTANCE_FLAG_*.
    pub const CULL_DISABLE: u8 = 1;
    pub const FRONT_COUNTERCLOCKWISE: u8 = 2;

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
        (moved(60.0, 1.0), 0x10, FRONT_COUNTERCLOCKWISE),
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
        // WARP keeps it (5.04 there). Either is taken.
        ([1.0, 1.0, 0.0], Z, 5.0, Some(5.0)),
    ];

    /// The ray of [`RAYS`] whose hit is at its t-max.
    pub const AT_T_MAX: usize = 14;
}

#[cfg(windows)]
mod warp {
    use std::mem::ManuallyDrop;

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;

    use super::super::HostShader;
    use super::super::host::Host;

    fn err(e: windows::core::Error) -> String {
        e.to_string()
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

    /// Builds [`super::world`]'s structure on `host`, runs `cs` with one
    /// ray per thread in heap slot 0, the structure at the reserved root
    /// SRV and a raw output in slot 2, and returns the output's dwords.
    /// `Ok(None)` where the device has no ray-tracing tier 1.1.
    pub fn run(host: &Host, cs: &HostShader, rays: &[u8]) -> Result<Option<Vec<u32>>, String> {
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
        if o5.RaytracingTier.0 < D3D12_RAYTRACING_TIER_1_1.0 {
            return Ok(None);
        }
        let (root, pso) = host.compute_pipeline(cs)?;
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
            // The BLAS.
            let vertices: Vec<u8> = super::world::TRIANGLES
                .iter()
                .flatten()
                .flat_map(|f| f.to_le_bytes())
                .collect();
            let vb = upload(&dev, &vertices)?;
            let geometry = D3D12_RAYTRACING_GEOMETRY_DESC {
                Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
                // Not opaque: the query's FORCE_OPAQUE makes it so.
                Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_NONE,
                Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                    Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                        Transform3x4: 0,
                        IndexFormat: DXGI_FORMAT_UNKNOWN,
                        VertexFormat: DXGI_FORMAT_R32G32B32_FLOAT,
                        IndexCount: 0,
                        VertexCount: super::world::TRIANGLES.len() as u32,
                        IndexBuffer: 0,
                        VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                            StartAddress: vb.GetGPUVirtualAddress(),
                            StrideInBytes: 12,
                        },
                    },
                },
            };
            let blas_inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
                Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
                NumDescs: 1,
                DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                    pGeometryDescs: &geometry,
                },
            };
            let (blas, blas_scratch) = structure(&dev, &blas_inputs)?;
            list.BuildRaytracingAccelerationStructure(
                &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                    DestAccelerationStructureData: blas.GetGPUVirtualAddress(),
                    Inputs: blas_inputs,
                    SourceAccelerationStructureData: 0,
                    ScratchAccelerationStructureData: blas_scratch.GetGPUVirtualAddress(),
                },
                None,
            );
            let barrier = uav_barrier(&blas);
            list.ResourceBarrier(std::slice::from_ref(&barrier));
            drop_barrier(barrier);
            // The TLAS: D3D12_RAYTRACING_INSTANCE_DESC, 64 bytes each.
            let mut instances = Vec::new();
            for (n, (rows, mask, flags)) in super::world::INSTANCES.iter().enumerate() {
                for f in rows.iter().flatten() {
                    instances.extend(f.to_le_bytes());
                }
                instances.extend((n as u32 | u32::from(*mask) << 24).to_le_bytes());
                instances.extend((u32::from(*flags) << 24).to_le_bytes());
                instances.extend(blas.GetGPUVirtualAddress().to_le_bytes());
            }
            let ib = upload(&dev, &instances)?;
            let tlas_inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
                Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
                NumDescs: super::world::INSTANCES.len() as u32,
                DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                    InstanceDescs: ib.GetGPUVirtualAddress(),
                },
            };
            let (tlas, tlas_scratch) = structure(&dev, &tlas_inputs)?;
            list.BuildRaytracingAccelerationStructure(
                &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                    DestAccelerationStructureData: tlas.GetGPUVirtualAddress(),
                    Inputs: tlas_inputs,
                    SourceAccelerationStructureData: 0,
                    ScratchAccelerationStructureData: tlas_scratch.GetGPUVirtualAddress(),
                },
                None,
            );
            let barrier = uav_barrier(&tlas);
            list.ResourceBarrier(std::slice::from_ref(&barrier));
            drop_barrier(barrier);
            // The views: rays (raw) at 0, the title's view of its structure
            // at 1 (which the spliced program no longer reads), the output
            // (raw) at 2.
            let views: ID3D12DescriptorHeap = dev
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 3,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    ..Default::default()
                })
                .map_err(err)?;
            let size = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let slot = |n: usize| {
                let mut at = views.GetCPUDescriptorHandleForHeapStart();
                at.ptr += n * size;
                at
            };
            let raw_srv = |dwords: u32| D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: 0,
                        NumElements: dwords,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            let rb = upload(&dev, rays)?;
            dev.CreateShaderResourceView(&rb, Some(&raw_srv(rays.len() as u32 / 4)), slot(0));
            // The title's view of its structure: blank, as when the
            // console builder never runs (its root at +0x30 is 0).
            let blank = upload(&dev, &[0; 256])?;
            dev.CreateShaderResourceView(&blank, Some(&raw_srv(64)), slot(1));
            let threads = super::THREADS;
            let out = buffer(
                &dev,
                D3D12_HEAP_TYPE_DEFAULT,
                u64::from(threads) * 4,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            )?;
            dev.CreateUnorderedAccessView(
                &out,
                None,
                Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            FirstElement: 0,
                            NumElements: threads,
                            StructureByteStride: 0,
                            CounterOffsetInBytes: 0,
                            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                        },
                    },
                }),
                slot(2),
            );
            list.SetDescriptorHeaps(&[Some(views.clone())]);
            list.SetComputeRootSignature(&root);
            // User data: the table starts at heap slot 0 (byte offset 0).
            let user = vec![0u32; cs.user_data_dwords().max(1) as usize];
            list.SetComputeRoot32BitConstants(0, user.len() as u32, user.as_ptr().cast(), 0);
            // The reserved root SRV is the only root view: parameter 1.
            list.SetComputeRootShaderResourceView(1, tlas.GetGPUVirtualAddress());
            list.Dispatch(1, 1, 1);
            let barrier = transition(
                &out,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            list.ResourceBarrier(std::slice::from_ref(&barrier));
            drop_barrier(barrier);
            let readback = buffer(
                &dev,
                D3D12_HEAP_TYPE_READBACK,
                u64::from(threads) * 4,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )?;
            list.CopyBufferRegion(&readback, 0, &out, 0, u64::from(threads) * 4);
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
            let mut p = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut p)).map_err(err)?;
            let words = std::slice::from_raw_parts(p.cast::<u32>(), threads as usize).to_vec();
            readback.Unmap(0, None);
            // Kept alive until here.
            drop((
                vb,
                ib,
                blas,
                blas_scratch,
                tlas,
                tlas_scratch,
                rb,
                blank,
                out,
            ));
            Ok(Some(words))
        }
    }
}

/// The spliced program on WARP: each thread's value is its ray's closest
/// opaque hit's t plus 0.04 under the query's mask and back-face culling
/// (with the instances' culling flags), or FLT_MAX.
#[test]
fn rayquery_spliced_program_traces_on_warp() {
    let cs = translate(&native(), &SITE).unwrap();
    let Ok(host) = super::host::Host::new(true) else {
        eprintln!("no WARP device; skipped");
        return;
    };
    // Ray n for thread n; the first rays again for the rest of the wave.
    let mut rays = Vec::new();
    let mut expected = Vec::new();
    for n in 0..THREADS as usize {
        let (origin, direction, t_max, t) = world::RAYS[n % world::RAYS.len()];
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
    let out = match warp::run(&host, &cs, &rays) {
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
                _ if n % world::RAYS.len() == world::AT_T_MAX && missed => false,
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
