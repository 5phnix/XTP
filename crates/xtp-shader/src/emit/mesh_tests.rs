//! Console wave64 mesh programs on hosts without 64-lane waves: their
//! first-lane reads, barriers and LDS (`lower/mesh.rs`, `lower/uniform.rs`,
//! `docs/analysis/mesh-wave64.md`). Hand-assembled GFX10 code, as in the
//! other tests of this crate: what each target's translation calls or
//! refuses, then one program drawn on the test adapter (WARP on this PC:
//! waves of 4 lanes, so a console wave is 16 host waves) and checked against
//! what the console's lanes give, in both console waves of its group.

use super::host::{Draw, Host};
use super::{HostShader, Target};
use crate::ShaderError;
use crate::emit::container::SigElement;
use crate::native::{MeshIo, NativeProgram, PsInput, StageIo, Usage, VsOutput};

const S_ENDPGM: u32 = 0xBF81_0000;
/// `exp prim, v9, off, off, off done`.
const EXP_PRIM: [u32; 2] = [0xF800_0941, 0x0000_0009];
/// `v_mbcnt_lo_u32_b32 v0, -1, 0`: the lane number (below 32).
const LANE: [u32; 2] = [0xD765_0000, 0x0001_00C1];
const V_MOV_V9_S5: u32 = 0x7E12_0205; // v_mov_b32 v9, s5
const SAVE_EXEC: u32 = 0xBE8A_047E; // s_mov_b64 s[10:11], exec
/// `ds_write_b32 v0, v1`: the program uses its LDS.
const DS_WRITE: [u32; 2] = [0xD834_0000, 0x0000_0100];
const RESTORE_EXEC: u32 = 0xBEFE_040A; // s_mov_b64 exec, s[10:11]

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

/// A console wave64 mesh program of `threads` lanes with `lds_bytes` of
/// LDS: `words`, then the primitive export of v9.
fn mesh(words: &[u32], threads: u32, lds_bytes: u32) -> NativeProgram {
    let mut code = words.to_vec();
    code.extend(EXP_PRIM);
    code.push(S_ENDPGM);
    NativeProgram {
        stage: crate::HwStage::Ms,
        wave64: true,
        vgprs: 24,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Ms(MeshIo {
            outputs: Vec::new(),
            clip_cntl: 0,
            threads,
            max_vertices: 128,
            max_primitives: 128,
            topology: 2,
            lds_bytes,
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

/// `native` translated for `target` (in a container of `parts`), and the
/// `dx.op` operations its module calls; checked by dxil.dll where it is.
fn translate(
    native: &NativeProgram,
    parts: &crate::Container,
    target: Target,
) -> Result<(HostShader, String), ShaderError> {
    let s = super::translate_program_for(parts, native, target).map_err(|(_, e)| e)?;
    let ops = super::last_module_summary();
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut s.dxil.clone())
            .unwrap_or_else(|e| panic!("validation: {e}\n{ops}"));
    }
    Ok((s, ops))
}

/// What the reasons of `lower/mesh.rs` say.
const FIRST_LANE: &str = "a first-lane read of a value lanes may hold differently";
const BARRIER: &str = "a group barrier lanes of a wave64 mesh group reach differently";
const LDS_IN_ONE_WAVE: &str = "LDS shared within one wave64 mesh wave";

fn refusal(native: &NativeProgram, target: Target) -> Option<&'static str> {
    match translate(native, &empty(), target) {
        Ok(_) => None,
        Err(ShaderError::Unsupported { what, .. }) => Some(what),
        Err(e) => panic!("{e}"),
    }
}

/// A value the whole group holds is read as each lane's own, on any host:
/// no wave operation, no barrier.
#[test]
fn uniform_first_lane_reads_are_the_lanes_own() {
    let p = mesh(
        &[
            0xBE84_0387, // s_mov_b32 s4, 7
            0x7E02_0204, // v_mov_b32 v1, s4
            0x7E0A_0501, // v_readfirstlane_b32 s5, v1
            V_MOV_V9_S5,
        ],
        128,
        0,
    );
    for target in [NVIDIA, RDNA, Target::ANY] {
        let (_, ops) = translate(&p, &empty(), target).unwrap();
        assert!(!ops.contains("waveReadLaneFirst"), "{target:?}: {ops}");
        assert!(!ops.contains("barrier"), "{target:?}: {ops}");
    }
}

/// A lane's own value read from the first lane: on a host without 64-lane
/// waves, through group-shared memory between group barriers (EXEC full
/// and narrowed without a branch); with 64-lane waves, the host wave's
/// first lane as before.
#[test]
fn other_first_lane_reads_go_through_group_shared_memory() {
    let full_words = [
        LANE[0],
        LANE[1],
        0x7E0A_0500, // v_readfirstlane_b32 s5, v0
        V_MOV_V9_S5,
    ];
    let full = mesh(&full_words, 128, 0);
    let narrowed = mesh(
        &[
            LANE[0],
            LANE[1],
            SAVE_EXEC,
            0x7DA8_0094, // v_cmpx_gt_u32 exec, 20, v0
            0x7E0A_0500, // v_readfirstlane_b32 s5, v0
            RESTORE_EXEC,
            V_MOV_V9_S5,
        ],
        128,
        0,
    );
    for p in [&full, &narrowed] {
        let (_, ops) = translate(p, &empty(), NVIDIA).unwrap();
        assert!(ops.contains("barrier"), "{ops}");
        assert!(!ops.contains("waveReadLaneFirst"), "{ops}");
        let (_, ops) = translate(p, &empty(), RDNA).unwrap();
        assert!(ops.contains("waveReadLaneFirst"), "{ops}");
        assert!(!ops.contains("barrier"), "{ops}");
    }
    // Its slots fit next to LDS of the program's own, not in place of LDS
    // it never allocated.
    let with_lds = mesh(&[&DS_WRITE[..], &full_words].concat(), 128, 256);
    assert_eq!(refusal(&with_lds, NVIDIA), None);
}

/// Where lanes of the group branch apart, a first-lane read of a lane's
/// value has no form on a host without 64-lane waves (its exchange needs
/// every lane at a group barrier): refused with its reason; with 64-lane
/// waves it translates as before.
#[test]
fn first_lane_reads_where_lanes_branch_apart_are_refused() {
    let p = mesh(
        &[
            LANE[0],
            LANE[1],
            0x7D88_0094, // v_cmp_gt_u32 vcc, 20, v0
            0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
            0xBF88_0002, // s_cbranch_execz +2
            0x7E0A_0500, // v_readfirstlane_b32 s5, v0
            V_MOV_V9_S5,
            0x88FE_0A7E, // s_or_b64 exec, exec, s[10:11]
        ],
        128,
        0,
    );
    assert!(refusal(&p, NVIDIA).is_some_and(|w| w.starts_with(FIRST_LANE)));
    assert_eq!(refusal(&p, RDNA), None);
}

/// A program with LDS keeps its barriers: one in a block only some lanes
/// enter would hang a host wave that never gets there, and is refused on a
/// host without 64-lane waves; one where the lanes have met again is kept.
/// A group of one console wave that shares LDS is refused there (its
/// compiler left out the barriers between the halves).
#[test]
fn barriers_and_lds_of_wave64_mesh_groups() {
    let branch = [
        LANE[0],
        LANE[1],
        0x7D88_0094, // v_cmp_gt_u32 vcc, 20, v0
        0xBE8A_246A, // s_and_saveexec_b64 s[10:11], vcc
        0xBF88_0001, // s_cbranch_execz +1
    ];
    let inside = [&branch[..], &[0xBF8A_0000, 0x88FE_0A7E]].concat(); // s_barrier; s_or_b64 exec, exec, s[10:11]
    let after = [&branch[..], &[0x7E12_0280, 0x88FE_0A7E, 0xBF8A_0000]].concat(); // v_mov_b32 v9, 0; join; s_barrier
    let inside = mesh(&[&DS_WRITE[..], &inside].concat(), 128, 256);
    let after = mesh(&[&DS_WRITE[..], &after].concat(), 128, 256);
    assert!(refusal(&inside, NVIDIA).is_some_and(|w| w.starts_with(BARRIER)));
    assert_eq!(refusal(&inside, RDNA), None);
    let (_, ops) = translate(&after, &empty(), NVIDIA).unwrap();
    assert!(ops.contains("barrier"), "{ops}");
    let one_wave = mesh(&[DS_WRITE[0], DS_WRITE[1], 0x7E12_0280], 64, 256); // v_mov_b32 v9, 0
    assert!(refusal(&one_wave, NVIDIA).is_some_and(|w| w.starts_with(LDS_IN_ONE_WAVE)));
    assert_eq!(refusal(&one_wave, RDNA), None);
}

/// The program [`wave64_mesh_reads_draw_the_console_waves_values`] draws.
/// Thread t of the group (console wave w = t / 64, lane l = t % 64)
/// exports vertex t at x = t / 2 of 63 columns, at the top when t is even
/// and the bottom when odd, and triangle (t, t + 1, t + 2) below 126: a
/// strip in which texel (k, row r) shows vertex 2k + r (the provoking
/// vertex). Its attribute is what four first-lane reads give:
/// * 7, a value of the group's (read as the lane's own);
/// * w, the console wave's index (the wave's);
/// * l + 100 from the console wave's first lane: 100 (an exchange, EXEC
///   full);
/// * 3 l + 7 with EXEC holding lanes 10 to 63 only: 37 (an exchange, EXEC
///   narrowed; VIctory issue #1132's case, where the host wave's own first
///   lane gave lanes 32-63 the wrong value).
fn drawn_mesh() -> NativeProgram {
    let words = [
        0xD765_0000,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v0, -1, 0
        0xD766_0000,
        0x0002_00C1, // v_mbcnt_hi_u32_b32 v0, -1, v0
        0x7E06_0300, // v_mov_b32 v3, v0 (the lane)
        0x9382_FF03,
        0x0004_0018, // s_bfe_u32 s2, s3, lit(0x00040018) (the wave)
        0xD746_0000,
        0x0401_0C02, // v_lshl_add_u32 v0, s2, 6, v0 (the thread)
        0xBE84_0387, // s_mov_b32 s4, 7
        0x7E02_0204, // v_mov_b32 v1, s4
        0x7E0A_0501, // v_readfirstlane_b32 s5, v1
        0x7E02_0202, // v_mov_b32 v1, s2
        0x7E0C_0501, // v_readfirstlane_b32 s6, v1
        0x4A04_06FF,
        0x0000_0064, // v_add_nc_u32 v2, lit(100), v3
        0x7E0E_0502, // v_readfirstlane_b32 s7, v2
        0xD543_0004,
        0x021D_0703, // v_mad_u32_u24 v4, v3, 3, 7
        SAVE_EXEC,
        0x7DA2_0689, // v_cmpx_lt_u32 exec, 9, v3
        0x7E10_0504, // v_readfirstlane_b32 s8, v4
        RESTORE_EXEC,
        0x7E14_0C05, // v_cvt_f32_u32 v10, s5
        0x7E16_0C06, // v_cvt_f32_u32 v11, s6
        0x7E18_0C07, // v_cvt_f32_u32 v12, s7
        0x7E1A_0C08, // v_cvt_f32_u32 v13, s8
        0x2C0A_0081, // v_lshrrev_b32 v5, 1, v0
        0x7E0A_0D05, // v_cvt_f32_u32 v5, v5
        0x100A_0AFF,
        0x3D02_0821, // v_mul_f32 v5, lit(2 / 63), v5
        0x060A_0AF3, // v_add_f32 v5, -1.0, v5
        0x360C_0081, // v_and_b32 v6, 1, v0
        0x7E0C_0D06, // v_cvt_f32_u32 v6, v6
        0x100C_0CF5, // v_mul_f32 v6, -2.0, v6
        0x060C_0CF2, // v_add_f32 v6, 1.0, v6
        0x7E0E_02F0, // v_mov_b32 v7, 0.5
        0x7E10_02F2, // v_mov_b32 v8, 1.0
        0xF800_08CF,
        0x0807_0605, // exp pos0, v5, v6, v7, v8 done
        0xF800_020F,
        0x0D0C_0B0A, // exp param0, v10, v11, v12, v13
        0x4A1C_0081, // v_add_nc_u32 v14, 1, v0
        0x4A1E_0082, // v_add_nc_u32 v15, 2, v0
        0xD76F_0009,
        0x0401_150E, // v_lshl_or_b32 v9, v14, 10, v0
        0xD76F_0009,
        0x0425_290F, // v_lshl_or_b32 v9, v15, 20, v9
        0x7D82_00FF,
        0x0000_007D, // v_cmp_lt_u32 vcc, lit(125), v0
        0x7E20_7081, // v_bfrev_b32 v16, 1 (the null primitive)
        0x0212_2109, // v_cndmask_b32 v9, v9, v16, vcc
    ];
    let mut p = mesh(&words, 128, 0);
    let StageIo::Ms(ms) = &mut p.io else {
        unreachable!()
    };
    ms.outputs = vec![VsOutput {
        usage: Usage::Generic,
        row: 1,
        param: 0,
        fp16: false,
        pack2: false,
    }];
    p
}

/// A pixel program that writes its attribute 0 as the console's flat
/// interpolation gives it (the provoking vertex's).
fn flat_pixel() -> NativeProgram {
    let words = [
        0xC802_0002, // v_interp_mov_f32 v0, p0, attr0.x
        0xC806_0102, // v_interp_mov_f32 v1, p0, attr0.y
        0xC80A_0202, // v_interp_mov_f32 v2, p0, attr0.z
        0xC80E_0302, // v_interp_mov_f32 v3, p0, attr0.w
        0xF800_180F,
        0x0302_0100, // exp mrt0, v0, v1, v2, v3 done vm
        S_ENDPGM,
    ];
    NativeProgram {
        stage: crate::HwStage::Ps,
        wave64: false,
        vgprs: 4,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Ps {
            inputs: vec![PsInput {
                usage: Usage::Generic,
                row: 1,
                mask: 0b1111,
                attr: 0,
                flat: true,
                fp16: false,
                pack2: false,
                row2: 0,
            }],
            input_addr: 0,
            db_shader_control: 0,
            patches: Vec::new(),
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

fn element(name: &str, kind: u8, row: u32, usage: u8) -> SigElement {
    SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp: 0,
        row: Some(row),
        col: 0,
        cols: 4,
        usage,
        half: false,
    }
}

/// The hand-assembled programs read as written (a wrong word would test
/// another program).
#[test]
fn the_programs_decode_as_written() {
    let text = |p: &NativeProgram| -> Vec<String> {
        crate::gfx10::decode(&p.code)
            .unwrap()
            .iter()
            .map(|i| i.text(p.wave64))
            .collect()
    };
    let ms = text(&drawn_mesh());
    for (at, want) in [
        (0, "v_mbcnt_lo_u32_b32 v0, -1, 0"),
        (3, "s_bfe_u32 s2, s3, lit(0x00040018)"),
        (4, "v_lshl_add_u32 v0, s2, 6, v0"),
        (12, "v_mad_u32_u24 v4, v3, 3, 7"),
        (14, "v_cmpx_lt_u32 exec, 9, v3"),
        (15, "v_readfirstlane_b32 s8, v4"),
        (31, "exp pos0, v5, v6, v7, v8 done"),
        (32, "exp param0, v10, v11, v12, v13"),
        (35, "v_lshl_or_b32 v9, v14, 10, v0"),
        (37, "v_cmp_lt_u32 vcc, lit(0x0000007d), v0"),
        (39, "v_cndmask_b32 v9, v9, v16, vcc"),
        (40, "exp prim, v9, off, off, off done"),
    ] {
        assert_eq!(ms[at], want, "{ms:#?}");
    }
    let ps = text(&flat_pixel());
    assert_eq!(ps[3], "v_interp_mov_f32 v3, p0, attr0.w");
    assert_eq!(ps[4], "exp mrt0, v0, v1, v2, v3 done vm");
}

/// [`drawn_mesh`] drawn into 63 x 2 texels on the test adapter, for the
/// adapter's waves: every texel has the console waves' values.
#[test]
fn wave64_mesh_reads_draw_the_console_waves_values() {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 device; skipped");
        return;
    };
    let target = host.target();
    let ms_sig = crate::dxbc::build(&[(
        crate::FourCC::OSG1,
        &super::container::signature(
            &[
                element("SV_Position", 3, 0, 15),
                element("TEXCOORD", 0, 1, 15),
            ],
            false,
        ),
    )]);
    let ps_sig = crate::dxbc::build(&[
        (
            crate::FourCC::ISG1,
            &super::container::signature(
                &[
                    element("SV_Position", 3, 0, 0),
                    element("TEXCOORD", 0, 1, 15),
                ],
                true,
            ),
        ),
        (
            crate::FourCC::OSG1,
            &super::container::signature(&[element("SV_Target", 16, 0, 15)], false),
        ),
    ]);
    let ms_c = crate::Container::parse(&ms_sig).unwrap();
    let ps_c = crate::Container::parse(&ps_sig).unwrap();
    let (ms, ops) = translate(&drawn_mesh(), &ms_c, target).unwrap();
    if !target.runs(64) {
        assert!(ops.contains("barrier"), "exchanges on {target:?}: {ops}");
    }
    let (ps, _) = translate(&flat_pixel(), &ps_c, target).unwrap();
    let (root, pso) = host.mesh_pipeline(&ms, &ps).unwrap();
    let texels = host
        .draw(
            &root,
            &pso,
            &Draw {
                roots: Vec::new(),
                textures: &[],
                samplers: &[],
                vertices: &[],
                stride: 0,
                size: (63, 2),
                mesh_groups: 1,
            },
        )
        .unwrap();
    eprintln!("{target:?}: {} texels drawn", texels.len());
    let mut wrong = Vec::new();
    for (n, t) in texels.iter().enumerate() {
        let (k, row) = (n % 63, n / 63);
        let v = 2 * k + row;
        let want = [7.0, (v / 64) as f32, 100.0, 37.0];
        if *t != want {
            wrong.push(format!(
                "texel ({k}, {row}), vertex {v}: {t:?}, not {want:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} texels wrong on {target:?}:\n{}",
        wrong.len(),
        texels.len(),
        wrong[..wrong.len().min(16)].join("\n")
    );
}
