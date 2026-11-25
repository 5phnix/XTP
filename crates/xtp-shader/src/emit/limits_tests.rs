//! Silent cuts made explicit (the limits inventory): hand-assembled GFX10
//! programs (our own code).
//!
//! - Float MODE writes (`ir::float_modes`): what is refused, by cause, and
//!   what passes (writes that change nothing the program computes, a 16-bit
//!   rounding toward zero, which is counted).
//! - Cube arrays (`Lower::cube_view`): a cube sample whose face operand is a
//!   face id plus 8 × a slice declares a cube array, one of a face id a
//!   cube; any other face operand is refused. The handle kinds come from
//!   dxc's disassembly (the Windows SDK's `dxc.exe`, else not checked), and
//!   `dxil.dll` validates every translation where it is found.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{HostShader, emit};
use crate::dxbc::{self, Container};
use crate::ir::{self, Reg};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const S_WAITCNT_0: u32 = 0xBF8C_0000;
const ZERO: u32 = 0x80;
const LITERAL: u32 = 0xFF;
const EIGHT: u32 = 0x4100_0000;

/// `s_setreg_imm32_b32 hwreg(HW_REG_MODE, offset, size), value`.
fn setreg_imm(offset: u32, size: u32, value: u32) -> [u32; 2] {
    [0xBA80_0000 | 1 | offset << 6 | (size - 1) << 11, value]
}

/// `s_setreg_b32 hwreg(HW_REG_MODE, offset, size), s<src>`.
fn setreg(offset: u32, size: u32, src: u8) -> u32 {
    0xB980_0000 | u32::from(src) << 16 | 1 | offset << 6 | (size - 1) << 11
}

/// `s_round_mode imm` and `s_denorm_mode imm`.
fn round_mode(imm: u32) -> u32 {
    0xBFA4_0000 | imm
}
fn denorm_mode(imm: u32) -> u32 {
    0xBFA5_0000 | imm
}

/// VOP2 `op v<d>, v<a>, v<b>`.
fn vop2(op: u32, d: u8, a: u8, b: u8) -> u32 {
    op << 25 | u32::from(d) << 17 | u32::from(b) << 9 | (256 + u32::from(a))
}
const V_ADD_F32: u32 = 0x03;
const V_MUL_F32: u32 = 0x08;
const V_FMAC_F32: u32 = 0x2B;
const V_ADD_F16: u32 = 0x32;

/// VOP2 `op v<d>, lit(x), v<b>`.
fn vop2_literal(op: u32, d: u8, x: u32, b: u8) -> [u32; 2] {
    [
        op << 25 | u32::from(d) << 17 | u32::from(b) << 9 | LITERAL,
        x,
    ]
}

/// `v_cvt_i32_f32 v<d>, v<a>` (truncates whatever the rounding mode).
fn cvt_i32_f32(d: u8, a: u8) -> u32 {
    0x7E00_0000 | u32::from(d) << 17 | 0x08 << 9 | (256 + u32::from(a))
}

/// `v_cvt_f32_f64 v0, v[1:2]`.
const CVT_F32_F64: u32 = 0x7E00_1F01;

/// `v_cubeid_f32 v<d>, v<x>, v<y>, v<z>`.
fn cubeid(d: u8, x: u8, y: u8, z: u8) -> [u32; 2] {
    [
        0x35 << 26 | 0x144 << 16 | u32::from(d),
        (256 + u32::from(x)) | (256 + u32::from(y)) << 9 | (256 + u32::from(z)) << 18,
    ]
}

/// The program `words` as the IR builds it.
fn build(words: &[u32]) -> Result<ir::Program, ShaderError> {
    let code: Vec<u8> = words
        .iter()
        .chain(&[S_ENDPGM])
        .flat_map(|w| w.to_le_bytes())
        .collect();
    ir::build(&code, false, 16)
}

/// Why the IR refuses `words`.
fn refusal(words: &[u32]) -> &'static str {
    match build(words) {
        Err(ShaderError::Unsupported { what, .. }) => what,
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("built: {words:#x?}"),
    }
}

/// [`ir::float_modes_of`] of `words`, which the IR builds.
fn modes(words: &[u32]) -> ir::FloatModes {
    ir::float_modes_of(&build(words).unwrap_or_else(|e| panic!("{e}")))
}

/// An f32 rounding other than to nearest even that reaches an f32
/// instruction is refused; restored first, or set to nearest even, it
/// passes.
#[test]
fn an_f32_rounding_mode_is_refused_where_it_reaches_f32_arithmetic() {
    let add = vop2(V_ADD_F32, 0, 1, 2);
    let toward_zero = setreg_imm(0, 2, 3);
    assert_eq!(
        refusal(&[toward_zero[0], toward_zero[1], add]),
        ir::F32_ROUNDING
    );
    assert_eq!(refusal(&[round_mode(1), add]), ir::F32_ROUNDING);
    // To nearest even, written: no change.
    assert_eq!(modes(&[round_mode(0), add]), ir::FloatModes::default());
    // Toward zero around a truncating conversion only, then back.
    modes(&[round_mode(3), cvt_i32_f32(3, 1), round_mode(0), add]);
}

/// BTP 6's pixel programs set the 16- and 64-bit rounding toward zero
/// before their 16-bit interpolation: not refused, counted; a 16-bit
/// instruction of a program that writes MODE reads it, one of a program
/// that does not, not.
#[test]
fn a_16_bit_rounding_toward_zero_is_counted_and_read_by_16_bit_instructions() {
    let add16 = vop2(V_ADD_F16, 0, 1, 2);
    let rtz = setreg_imm(2, 2, 3);
    let found = modes(&[rtz[0], rtz[1], add16]);
    assert!(found.half_rounding);
    // Set back to nearest even before the 16-bit instruction.
    let rne = setreg_imm(2, 2, 0);
    assert!(!modes(&[rtz[0], rtz[1], rne[0], rne[1], add16]).half_rounding);
    // f32 arithmetic is not under the 16-bit rounding.
    let add = vop2(V_ADD_F32, 0, 1, 2);
    assert!(!modes(&[rtz[0], rtz[1], add]).half_rounding);
    let reads_mode = |words: &[u32]| {
        let p = build(words).unwrap();
        let insts = p.blocks.iter().flat_map(|b| &b.insts);
        insts
            .filter(|i| i.name() == "v_add_f16")
            .all(|i| i.read(Reg::FloatMode).is_some())
    };
    assert!(reads_mode(&[rtz[0], rtz[1], add16]));
    assert!(!reads_mode(&[add16]));
}

/// A mode the program computes, from a register no constant moves into,
/// is refused where float instructions run under it; from a constant moved
/// into the register, it is that constant.
#[test]
fn a_mode_computed_at_run_time_is_refused() {
    let add = vop2(V_ADD_F32, 0, 1, 2);
    assert_eq!(refusal(&[setreg(0, 2, 2), add]), ir::RUN_TIME_MODE);
    // s_mov_b32 s2, 0
    let s_mov_s2_0 = 0xBE82_0300 | ZERO;
    modes(&[s_mov_s2_0, setreg(0, 2, 2), add]);
}

/// One denormal mode per precision for the whole program: set at the start
/// it passes (the mode DXIL would declare), changed between two f32
/// instructions it is refused.
#[test]
fn a_denormal_mode_that_changes_is_refused() {
    let add = vop2(V_ADD_F32, 0, 1, 2);
    let mul = vop2(V_MUL_F32, 3, 1, 2);
    modes(&[denorm_mode(0xF), add, mul]);
    assert_eq!(refusal(&[add, denorm_mode(0xF), mul]), ir::DENORMALS_CHANGE);
}

/// MODE's DX10_CLAMP, IEEE and FP16_OVFL bits change float results, and the
/// translation does not follow them.
#[test]
fn a_write_to_the_other_float_bits_of_mode_is_refused() {
    let ieee = setreg_imm(8, 2, 0);
    assert_eq!(refusal(&[ieee[0], ieee[1]]), ir::OTHER_MODE_BITS);
}

/// A MODE write in a block the lanes outside a compare skip: the console
/// runs it for the whole wave, the translation per lane for those inside,
/// so a `v_cvt_f32_f64` after the join would read each lane's own mode.
/// Refused; after a branch on a scalar compare (every lane together, as
/// `scalar_gap_tests` runs it) it is not.
#[test]
fn a_mode_that_lanes_may_hold_apart_is_refused_at_a_conversion() {
    let by_lane = [
        0x7D82_008F, // v_cmp_lt_u32 vcc_lo, 15, v0
        0xBE8A_3C6A, // s_and_saveexec_b32 s10, vcc_lo
        0xBF88_0002, // s_cbranch_execz join
        setreg_imm(0, 2, 2)[0],
        2,           // s_setreg_imm32_b32 hwreg(HW_REG_MODE, 0, 2), 2
        0x887E_0A7E, // join: s_or_b32 exec_lo, exec_lo, s10
        CVT_F32_F64,
    ];
    assert_eq!(refusal(&by_lane), ir::MODE_BY_PATH);
    let by_wave = [
        0xBF06_8002, // s_cmp_eq_u32 s2, 0
        0xBF85_0002, // s_cbranch_scc1 join
        setreg_imm(0, 2, 2)[0],
        2,
        CVT_F32_F64, // join
    ];
    build(&by_wave).unwrap();
}

/// The [`ir::float_kinds`] of the instructions the checks lean on.
#[test]
fn float_kinds_name_the_rounded_result_and_the_precisions_taken() {
    let k = ir::float_kinds;
    let (f32, half) = (1, 2);
    assert_eq!(
        (k("v_add_f32").rounds, k("v_add_f32").denormals),
        (f32, f32)
    );
    assert_eq!(
        (k("v_cvt_f16_f32").rounds, k("v_cvt_f16_f32").denormals),
        (half, f32 | half)
    );
    assert_eq!(k("v_interp_p1ll_f16").rounds, f32);
    assert_eq!(k("v_interp_p2_f16").rounds, half);
    assert_eq!(k("v_fma_mix_f32").denormals, f32 | half);
    for exact in [
        "v_cvt_i32_f32",
        "v_cvt_pkrtz_f16_f32",
        "v_max_f32",
        "v_min3_f32",
        "v_floor_f32",
        "v_cmp_lt_f32",
        "v_pk_max_f16",
        "v_mov_b32",
    ] {
        assert_eq!(k(exact).rounds, 0, "{exact}");
    }
    assert_eq!(k("v_cvt_f32_f64"), ir::FloatKinds::default());
    assert_eq!(k("s_add_u32"), ir::FloatKinds::default());
}

// Cube arrays.

/// A compute program of one 32-lane wave: user data s0 the SRV table (the
/// cube at heap index 0, an address buffer at 1), s1 the sampler table, s2
/// the UAV table (index 2). Each lane loads 16 dwords into v2..v17, `face`
/// then computes the face operand into v4, `inst` samples with addresses
/// v[2:4] into v[20:23], which are stored at 16 × the lane.
fn cube_program(face: &[u32], inst: [u32; 2]) -> Vec<u32> {
    let s_mov = |d: u32, src: u32| 0xBE80_0300 | d << 16 | src;
    let s_load = |op: u32, d: u32, base: u32, offset: u32| {
        [
            0xF400_0000 | op << 18 | d << 6 | base / 2,
            0xFA00_0000 | offset,
        ]
    };
    let mubuf = |op: u32, data: u32, addr: u32, rsrc: u32, offset: u32| {
        [
            0xE000_0000 | op << 18 | 1 << 12 | offset,
            addr | data << 8 | (rsrc / 4) << 16 | ZERO << 24,
        ]
    };
    let v_lshl = |d: u32, by: u32, v: u32| 0x1A << 25 | d << 17 | v << 9 | (ZERO + by);
    let mut w = vec![
        s_mov(4, 0),
        s_mov(5, ZERO),
        s_mov(6, 1),
        s_mov(7, ZERO),
        s_mov(20, 2),
        s_mov(21, ZERO),
    ];
    w.extend(s_load(3, 8, 4, 0));
    w.extend(s_load(2, 32, 4, 0x20));
    w.extend(s_load(2, 16, 6, 0));
    w.extend(s_load(3, 24, 20, 0));
    w.push(S_WAITCNT_0);
    for r in 20..24 {
        w.push(0x7E00_0000 | r << 17 | 1 << 9 | ZERO); // v_mov_b32 v<r>, 0
    }
    w.push(v_lshl(1, 6, 0));
    for k in 0..4 {
        w.extend(mubuf(0x0E, 2 + 4 * k, 1, 32, 16 * k));
    }
    w.push(S_WAITCNT_0);
    w.extend(face);
    w.extend(inst);
    w.push(S_WAITCNT_0);
    w.push(v_lshl(24, 4, 0));
    w.extend(mubuf(0x1E, 20, 24, 24, 0));
    w.push(S_ENDPGM);
    w
}

/// The MIMG instruction `name` on a cube image: addresses from v2, data
/// v20.., the image s[8:15], the sampler s[16:19].
fn cube_mimg(name: &str, dmask: u32) -> [u32; 2] {
    let words = |op: u32| {
        [
            0xF000_0000 | (op & 0x7F) << 18 | op >> 7 | 3 << 3 | dmask << 8,
            2 | 20 << 8 | (8 / 4) << 16 | (16 / 4) << 21,
        ]
    };
    let op = (0..256)
        .find(|&op| {
            let code: Vec<u8> = words(op)
                .iter()
                .chain(&[S_ENDPGM])
                .flat_map(|w| w.to_le_bytes())
                .collect();
            crate::gfx10::decode(&code).is_ok_and(|d| d[0].name() == Some(name))
        })
        .unwrap_or_else(|| panic!("{name}"));
    words(op)
}

fn cube_native(words: &[u32]) -> NativeProgram {
    let table = |user_data, slot| RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot,
    };
    let used = |kind, user_data, slot, dwords| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords,
        user_data,
        slot,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        float_mode: 0xC0,
        vgprs: 32,
        layout: vec![table(0, 1), table(1, 2), table(2, 3)],
        used: vec![
            used(ResourceType::Srv, 0, 1, 8),
            used(ResourceType::Sampler, 1, 2, 4),
            used(ResourceType::Uav, 2, 3, 8),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 3 << 1,
            threads: [32, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `words` translated, and validated where `dxil.dll` is found.
fn translate(words: &[u32]) -> Result<HostShader, ShaderError> {
    let n = cube_native(words);
    let p = ir::build(&n.code, n.wave64, n.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &n.layout, n.stage, Some(&st))?;
    let io = io::raise(&p, &n)?;
    let bytes = dxbc::build(&[]);
    let c = Container::parse(&bytes)?;
    let s = emit(&c, &n, &p, &st, &io, &res)?;
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut s.dxil.clone())
            .unwrap_or_else(|e| panic!("validation: {e}"));
    }
    Ok(s)
}

/// The kinds of the view-heap handles of `shader` (`annotateHandle`'s first
/// property, 5 a cube, 9 a cube array), from dxc's disassembly; None where
/// the Windows SDK's `dxc.exe` is not there.
fn heap_kinds(shader: &HostShader) -> Option<Vec<u32>> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dxc = match std::env::var_os("XTP_PC_DXC") {
        Some(p) => PathBuf::from(p),
        None => super::validator::Validator::default_path()?.with_file_name("dxc.exe"),
    };
    if !dxc.exists() {
        eprintln!("no dxc.exe; handle kinds not checked");
        return None;
    }
    let file = std::env::temp_dir().join(format!(
        "xtp-limits-{}-{}.dxil",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&file, &shader.dxil).unwrap();
    let out = std::process::Command::new(&dxc)
        .arg("-dumpbin")
        .arg(&file)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listing = String::from_utf8_lossy(&out.stdout);
    let mut heap = std::collections::HashSet::new();
    let mut kinds = Vec::new();
    for line in listing.lines() {
        let Some((name, call)) = line.trim().split_once(" = ") else {
            continue;
        };
        if let Some(args) = call.split_once("@dx.op.createHandleFromHeap(") {
            if args.1.split(", ").nth(2) == Some("i1 false") {
                heap.insert(name.to_owned());
            }
            continue;
        }
        let Some((_, args)) = call.split_once("@dx.op.annotateHandle(i32 216, %dx.types.Handle ")
        else {
            continue;
        };
        let (handle, props) = args.split_once(", ").unwrap();
        if heap.contains(handle) {
            let kind = props.split("{ i32 ").nth(1).unwrap();
            kinds.push(kind.split(',').next().unwrap().trim().parse().unwrap());
        }
    }
    Some(kinds)
}

/// The kinds of the image handles (the cube's, not the buffers').
fn image_kinds(shader: &HostShader) -> Option<Vec<u32>> {
    let mut kinds: Vec<u32> = heap_kinds(shader)?
        .into_iter()
        .filter(|k| k & 0xFF < 10)
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    Some(kinds)
}

/// The encodings these tests write, as the decoder reads them.
#[test]
fn the_cube_encodings_read_back() {
    let face = [
        cubeid(4, 5, 6, 7),
        vop2_literal(V_FMAC_F32, 4, EIGHT, 8),
        vop2_literal(V_MUL_F32, 9, EIGHT, 8),
    ]
    .concat();
    let mut words = face.clone();
    words.push(vop2(V_ADD_F32, 4, 4, 9));
    words.extend(cube_mimg("image_sample_lz", 7));
    words.push(S_ENDPGM);
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let text: Vec<String> = crate::gfx10::decode(&code)
        .unwrap()
        .iter()
        .map(|i| i.text(false))
        .collect();
    assert_eq!(text[0], "v_cubeid_f32 v4, v5, v6, v7", "{text:#?}");
    assert!(text[1].starts_with("v_fmac_f32 v4,"), "{text:#?}");
    assert!(text[1].ends_with(", v8"), "{text:#?}");
    assert!(text[2].starts_with("v_mul_f32 v9,"), "{text:#?}");
    assert_eq!(text[3], "v_add_f32 v4, v4, v9", "{text:#?}");
    assert!(text[4].starts_with("image_sample_lz"), "{text:#?}");
    assert!(text[4].contains("CUBE"), "{text:#?}");
}

/// The face id alone is a cube; the face id plus 8 × a slice (a multiply-
/// add by 8.0, or an addition of a product by 8.0) a cube array; a face
/// operand the program loads is refused.
#[test]
fn a_cube_sample_declares_a_cube_or_a_cube_array_by_its_face_operand() {
    let sample = cube_mimg("image_sample_lz", 7);
    let cube = translate(&cube_program(&cubeid(4, 5, 6, 7), sample)).unwrap();
    let fmac = [cubeid(4, 5, 6, 7), vop2_literal(V_FMAC_F32, 4, EIGHT, 8)].concat();
    let array = translate(&cube_program(&fmac, sample)).unwrap();
    let mut add = [cubeid(4, 5, 6, 7), vop2_literal(V_MUL_F32, 9, EIGHT, 8)].concat();
    add.push(vop2(V_ADD_F32, 4, 4, 9));
    let added = translate(&cube_program(&add, sample)).unwrap();
    if let (Some(c), Some(a), Some(b)) =
        (image_kinds(&cube), image_kinds(&array), image_kinds(&added))
    {
        assert_eq!(c, [5]);
        assert_eq!(a, [9]);
        assert_eq!(b, [9]);
    }
    match translate(&cube_program(&[], sample)) {
        Err(ShaderError::Unsupported { what, .. }) => {
            assert!(
                what.starts_with("a cube sample whose face operand"),
                "{what}"
            );
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("a loaded face operand translated"),
    }
}

/// A size query of a cube asking for its layer count is a cube array's
/// (TextureCube's HLSL size has none): the host's cube count, × 6 as the
/// console counts layers. Without it, a cube's.
#[test]
fn a_cube_size_query_asking_for_layers_is_a_cube_arrays() {
    let resinfo = |dmask| cube_mimg("image_get_resinfo", dmask);
    let layers = translate(&cube_program(&[], resinfo(0xF))).unwrap();
    let size = translate(&cube_program(&[], resinfo(0x3))).unwrap();
    if let (Some(l), Some(s)) = (image_kinds(&layers), image_kinds(&size)) {
        assert_eq!(l, [9]);
        assert_eq!(s, [5]);
    }
}
