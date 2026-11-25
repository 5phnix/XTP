//! Helper lanes in a pixel program's lane reads, drawn on WARP and on
//! every hardware adapter.
//!
//! A helper lane runs a pixel program for a quad's pixel the primitive
//! does not cover, so that its covered neighbours get derivatives. On the
//! console, whole-quad mode (`s_wqm`) keeps helper lanes in EXEC: every
//! lane read sees them, and they see every lane read. On the host, D3D12
//! leaves them out of wave operations unless the pixel shader declares
//! `WaveOpsIncludeHelperLanes` (SM 6.7): what `WaveReadLaneFirst`,
//! `WaveReadLaneAt` or a `WaveActive*` reduction reads from a helper lane,
//! or returns in one, is undefined. Quad reads (`QuadReadLaneAt`) are the
//! exception: they read helper lanes and return in them as in any lane.
//!
//! Each program below is hand-assembled GFX10 code. It runs under
//! `s_wqm`, as the console compiler's pixel programs that take
//! derivatives do, and gives each pixel its own key, x + 100 y of its
//! centre. One triangle crosses a 16 × 16 target, so many quads are only
//! partly covered: a covered pixel whose neighbour is not covered reads a
//! helper lane. The cases:
//!
//! * DPP quad permutations and a hand-made derivative (`quad_perm`
//!   [1,1,3,3] minus [0,0,2,2], which must be 1 everywhere);
//! * `ds_swizzle` in quad mode;
//! * a waterfall loop over `v_readfirstlane_b32`, bounded to 64 passes in
//!   the program itself so that a lane never served ends with -1 instead
//!   of running to the loop limit;
//! * a whole-wave section (the console's expansion of `WaveActiveMax`).

use super::container::{self, SigElement};
use super::host::{Draw, Host, adapters};
use super::{HostShader, validator};
use crate::dxbc::{self, Container, FourCC};
use crate::native::{HwStage, NativeProgram, StageIo};

/// `SPI_PS_INPUT_ADDR` bits: the pixel's position, x and y, as floats
/// (v0 and v1 when nothing else is enabled).
const POS_X_FLOAT: u32 = 1 << 8;
const POS_Y_FLOAT: u32 = 1 << 9;

const SIZE: usize = 16;
/// The waterfall loop's bound, in passes.
const BOUND: u32 = 64;

const EXEC_LO: u32 = 126;
const VCC_LO: u32 = 106;
/// Source operand codes: VGPR n is 256 + n; inline constants.
const V: u32 = 256;
const ZERO: u32 = 128;
const LITERAL: u32 = 255;
const ONE_F: u32 = 242;
const MINUS_ONE_F: u32 = 243;
const TWO_F: u32 = 244;

const S_ENDPGM: u32 = 0xBF81_0000;
/// `s_waitcnt lgkmcnt(0)`.
const WAIT_LGKM: u32 = 0xBF8C_C07F;

/// Quad permutations: lane n of each quad reads lane `perm >> 2n & 3`.
const ACROSS_X: u32 = 0xB1; // [1,0,3,2]
const ACROSS_Y: u32 = 0x4E; // [2,3,0,1]
const DIAGONAL: u32 = 0x1B; // [3,2,1,0]
const RIGHT_COLUMN: u32 = 0xF5; // [1,1,3,3]
const LEFT_COLUMN: u32 = 0xA0; // [0,0,2,2]

fn vop1(op: u32, vdst: u32, src0: u32) -> u32 {
    0x7E00_0000 | vdst << 17 | op << 9 | src0
}

fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
    op << 25 | vdst << 17 | vsrc1 << 9 | src0
}

fn vopc(op: u32, src0: u32, vsrc1: u32) -> u32 {
    0x7C00_0000 | op << 17 | vsrc1 << 9 | src0
}

fn sop1(op: u32, sdst: u32, src0: u32) -> u32 {
    0xBE80_0000 | sdst << 16 | op << 8 | src0
}

fn sop2(op: u32, sdst: u32, src0: u32, src1: u32) -> u32 {
    0x8000_0000 | op << 23 | sdst << 16 | src1 << 8 | src0
}

fn sopc(op: u32, src0: u32, src1: u32) -> u32 {
    0xBF00_0000 | op << 16 | src1 << 8 | src0
}

fn sopp(op: u32, simm: i32) -> u32 {
    0xBF80_0000 | op << 16 | (simm as u32 & 0xFFFF)
}

/// `v_mov_b32_dpp v<dst>, v<src> quad_perm:<perm> row_mask:0xf bank_mask:0xf`.
fn quad_perm(dst: u32, src: u32, perm: u32) -> [u32; 2] {
    [vop1(0x01, dst, 250), 0xFF00_0000 | perm << 8 | src]
}

/// `ds_swizzle_b32 v<dst>, v<src> offset:swizzle(QUAD_PERM, ...)`.
fn swizzle_quad(dst: u32, src: u32, perm: u32) -> [u32; 2] {
    [0xD8D4_8000 | perm, dst << 24 | src]
}

/// `exp mrt0 v<a>, v<b>, v<c>, v<d> done vm`.
fn export(r: [u32; 4]) -> [u32; 2] {
    [0xF800_180F, r[0] | r[1] << 8 | r[2] << 16 | r[3] << 24]
}

/// Whole-quad mode, the pixel's key in v2 and 2.0 in v7.
fn prologue() -> Vec<u32> {
    vec![
        sop1(0x09, EXEC_LO, EXEC_LO), // s_wqm_b32 exec_lo, exec_lo
        vop2(0x08, 2, LITERAL, 1),    // v_mul_f32 v2, 100.0, v1
        100f32.to_bits(),
        vop2(0x03, 2, V, 2),  // v_add_f32 v2, v0, v2
        vop1(0x01, 7, TWO_F), // v_mov_b32 v7, 2.0
    ]
}

/// Quad permutations of the key: across x in v4, across y in v5, and in
/// v6 the right column's key minus the left column's (a derivative in x).
fn quad_perms() -> Vec<u32> {
    let mut w = prologue();
    w.extend(quad_perm(4, 2, ACROSS_X));
    w.extend(quad_perm(5, 2, ACROSS_Y));
    w.extend(quad_perm(8, 2, RIGHT_COLUMN));
    w.extend(quad_perm(9, 2, LEFT_COLUMN));
    w.push(vop2(0x04, 6, V + 8, 9)); // v_sub_f32 v6, v8, v9
    w.extend(export([4, 5, 6, 7]));
    w.push(S_ENDPGM);
    w
}

/// Quad-mode swizzles of the key: across x in v4, the diagonal in v5.
fn swizzles() -> Vec<u32> {
    let mut w = prologue();
    w.extend(swizzle_quad(4, 2, ACROSS_X));
    w.extend(swizzle_quad(5, 2, DIAGONAL));
    w.push(WAIT_LGKM);
    w.extend(export([4, 5, 2, 7]));
    w.push(S_ENDPGM);
    w
}

/// A waterfall loop over the key: each pass serves the lanes whose key is
/// the first lane's, v3 = the pass + 1 (-1 if never served); after
/// [`BOUND`] passes the loop stops. Then each lane reads its quad
/// neighbours' v3: across x in v4, across y in v5, the diagonal in v6.
fn waterfall() -> Vec<u32> {
    let mut w = prologue();
    w.push(vop1(0x01, 3, MINUS_ONE_F)); // v_mov_b32 v3, -1.0
    w.push(sop1(0x03, 10, ZERO)); // s_mov_b32 s10, 0
    w.push(sop1(0x03, 11, EXEC_LO)); // s_mov_b32 s11, exec_lo
    let top = w.len();
    w.push(vop1(0x02, 12, V + 2)); // v_readfirstlane_b32 s12, v2
    w.push(vopc(0xC2, 12, 2)); // v_cmp_eq_u32 vcc_lo, s12, v2
    w.push(sop1(0x3C, 13, VCC_LO)); // s_and_saveexec_b32 s13, vcc_lo
    w.push(vop1(0x06, 3, 10)); // v_cvt_f32_u32 v3, s10
    w.push(vop2(0x03, 3, ONE_F, 3)); // v_add_f32 v3, 1.0, v3
    w.push(sop2(0x12, EXEC_LO, EXEC_LO, 13)); // s_xor_b32 exec_lo, exec_lo, s13
    w.push(sop2(0x00, 10, 10, 129)); // s_add_u32 s10, s10, 1
    w.push(sopc(0x0A, 10, ZERO + BOUND)); // s_cmp_lt_u32 s10, BOUND
    let to_done = w.len();
    w.push(0); // s_cbranch_scc0 done
    let back = w.len();
    w.push(sopp(0x09, top as i32 - back as i32 - 1)); // s_cbranch_execnz top
    let done = w.len();
    w[to_done] = sopp(0x04, done as i32 - to_done as i32 - 1);
    w.push(sop1(0x03, EXEC_LO, 11)); // s_mov_b32 exec_lo, s11
    w.extend(quad_perm(4, 3, ACROSS_X));
    w.extend(quad_perm(5, 3, ACROSS_Y));
    w.extend(quad_perm(6, 3, DIAGONAL));
    w.extend(export([3, 4, 5, 6]));
    w.push(S_ENDPGM);
    w
}

/// The console's expansion of `WaveActiveMax(key)` in a wave32: the whole
/// wave on, inactive lanes at 0, the maximum across each row of 16
/// (`row_xmask`), then across the rows (`v_permlanex16`), read from lane 0
/// into s4. v3 = s4; v4 = v3 across x.
fn section_max() -> Vec<u32> {
    let mut w = prologue();
    w.push(0xBE86_3DC1); // s_or_saveexec_b32 s6, -1
    w.extend([0xD501_0005, 0x001A_0480]); // v_cndmask_b32 v5, 0, v2, s6
    for distance in [1, 2, 4, 8] {
        // v_mov_b32_dpp v6, v5 row_xmask:<distance>
        w.extend([vop1(0x01, 6, 250), 0xFF01_6005 | distance << 8]);
        w.push(vop2(0x14, 5, V + 6, 5)); // v_max_u32 v5, v6, v5
    }
    w.extend([0xD778_0006, 0x0201_0105]); // v_permlanex16_b32 v6, v5, 0, 0
    w.push(vop2(0x14, 5, V + 6, 5)); // v_max_u32 v5, v6, v5
    w.extend([0xD760_0004, 0x0001_0105]); // v_readlane_b32 s4, v5, 0
    w.push(sop1(0x03, EXEC_LO, 6)); // s_mov_b32 exec_lo, s6
    w.push(vop1(0x01, 3, 4)); // v_mov_b32 v3, s4
    w.extend(quad_perm(4, 3, ACROSS_X));
    w.extend(export([3, 4, 2, 7]));
    w.push(S_ENDPGM);
    w
}

fn native(words: &[u32]) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        float_mode: 0,
        vgprs: 16,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Ps {
            inputs: Vec::new(),
            input_addr: POS_X_FLOAT | POS_Y_FLOAT,
            rsrc2: 0,
            db_shader_control: 0,
            patches: Vec::new(),
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// The program translated, checked by dxil.dll where it is installed, and
/// the `dx.op` operations its module calls.
fn translate(words: &[u32]) -> (HostShader, String) {
    let element = |name: &str, kind, interp, usage| SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp,
        row: Some(0),
        col: 0,
        cols: 4,
        usage,
        half: false,
    };
    // SV_Position, linear without perspective; SV_Target 0.
    let position = element("SV_Position", 3, 4, 0);
    let target = element("SV_Target", 16, 0, 0xF);
    let isg1 = container::signature(&[position], true);
    let osg1 = container::signature(&[target], false);
    let bytes = dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let ps = super::translate_program(&c, &native(words))
        .unwrap_or_else(|(step, e)| panic!("{step:?}: {e}"));
    let ops = super::last_module_summary();
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut ps.dxil.clone())
            .unwrap_or_else(|e| panic!("validation: {e}\n{ops}"));
    }
    (ps, ops)
}

/// A vertex shader writing what the pixel shader reads: its position, for
/// a triangle whose edges cross the target's quads.
fn vertex_shader(ps: &HostShader) -> HostShader {
    let c = Container::parse(&ps.dxil).unwrap();
    let mut outputs = super::signature(c.part(FourCC::ISG1).unwrap().data).unwrap();
    for e in &mut outputs {
        e.usage = 0xF;
    }
    let positions = [
        [-0.9f32, -0.9, 0.5, 1.0],
        [-0.9, 0.75, 0.5, 1.0],
        [0.65, -0.9, 0.5, 1.0],
    ];
    let names: Vec<String> = outputs.iter().map(|e| e.name.clone()).collect();
    let dxil = super::rect_gs::probe::with_outputs(outputs, 3, |v, n, c| {
        assert_eq!(
            names[n], "SV_Position",
            "the pixel shader reads only its position"
        );
        positions[v][c].to_bits()
    });
    HostShader {
        stage: HwStage::Vs,
        dxil,
        user_data: Vec::new(),
        group_base: None,
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: Vec::new(),
        root_srvs: Vec::new(),
        root_uavs: Vec::new(),
        stride_table: false,
        run_time_buffers: 0,
    }
}

/// The devices to draw on: WARP, then each hardware adapter (none where
/// `XTP_TEST_ADAPTER` keeps the tests off hardware).
fn hosts() -> Vec<(String, Host)> {
    let mut out = Vec::new();
    match Host::new(true) {
        Ok(h) => out.push(("WARP".to_string(), h)),
        Err(e) => eprintln!("WARP: no SM 6.6 device ({e}); not run there"),
    }
    for name in adapters().unwrap_or_default() {
        match Host::on(&name) {
            Ok(h) => out.push((name, h)),
            Err(e) => eprintln!("{name}: {e}; not run there"),
        }
    }
    out
}

/// A pixel's key: x + 100 y of its centre.
fn key(x: usize, y: usize) -> f32 {
    x as f32 + 0.5 + 100.0 * (y as f32 + 0.5)
}

/// One drawn target: the pixels the program wrote (channel `marker` not
/// 0) and their values.
struct Drawn {
    texels: Vec<[f32; 4]>,
    marker: usize,
}

impl Drawn {
    fn at(&self, x: usize, y: usize) -> [f32; 4] {
        self.texels[y * SIZE + x]
    }

    fn covered(&self, x: usize, y: usize) -> bool {
        self.at(x, y)[self.marker] != 0.0
    }

    /// The covered pixels.
    fn pixels(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for y in 0..SIZE {
            for x in 0..SIZE {
                if self.covered(x, y) {
                    out.push((x, y));
                }
            }
        }
        out
    }
}

/// Draws `ps` on every host and runs `check` on each target, which returns
/// its failures (empty: right). Panics with every host's failures.
fn draw_everywhere(
    words: &[u32],
    marker: usize,
    check: impl Fn(&Drawn) -> Vec<String>,
) -> Vec<(String, Drawn)> {
    let (ps, _) = translate(words);
    let vs = vertex_shader(&ps);
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut failures = Vec::new();
    let mut drawn = Vec::new();
    for (name, host) in hosts() {
        let (root, pso) = host
            .pipeline(&vs, &ps)
            .unwrap_or_else(|e| panic!("{name}: pipeline: {e}"));
        let draw = Draw {
            roots: Vec::new(),
            textures: &[],
            samplers: &[],
            vertices: &[],
            stride: 0,
            size: (SIZE as u32, SIZE as u32),
            mesh_groups: 0,
        };
        let texels = host
            .draw(&root, &pso, &draw)
            .unwrap_or_else(|e| panic!("{name}: draw: {e}"));
        let t = Drawn { texels, marker };
        let helpers = t
            .pixels()
            .iter()
            .filter(|&&(x, y)| !t.covered(x ^ 1, y) || !t.covered(x, y ^ 1))
            .count();
        assert!(
            helpers > 0,
            "{name}: no covered pixel has a helper neighbour: the test sees nothing"
        );
        let wrong = check(&t);
        eprintln!(
            "{name}: {} covered pixels, {helpers} beside a helper lane, {} wrong",
            t.pixels().len(),
            wrong.len()
        );
        failures.extend(wrong.into_iter().map(|w| format!("{name}: {w}")));
        drawn.push((name, t));
    }
    assert!(failures.is_empty(), "{failures:#?}");
    drawn
}

/// Whether `(x, y)`'s neighbour `(nx, ny)` is a helper lane, as text.
fn lane(t: &Drawn, nx: usize, ny: usize) -> &'static str {
    if t.covered(nx, ny) {
        "covered"
    } else {
        "helper"
    }
}

/// The hand-assembled programs read as written (a wrong word would test
/// another program).
#[test]
fn the_helper_lane_programs_decode_as_written() {
    let text = |words: &[u32]| -> Vec<String> {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        crate::gfx10::decode(&code)
            .unwrap()
            .iter()
            .map(|i| i.text(false))
            .collect()
    };
    let mut wrong = Vec::new();
    let mut want = |words: &[u32], lines: &[(usize, &str)]| {
        let t = text(words);
        if lines
            .iter()
            .any(|&(at, line)| t.get(at).map(String::as_str) != Some(line))
        {
            wrong.push(t);
        }
    };
    let prologue = [
        (0, "s_wqm_b32 exec_lo, exec_lo"),
        (1, "v_mul_f32 v2, lit(0x42c80000), v1"),
        (2, "v_add_f32 v2, v0, v2"),
        (3, "v_mov_b32 v7, 2.0"),
    ];
    want(&quad_perms(), &prologue);
    want(
        &quad_perms(),
        &[
            (4, "v_mov_b32 v4, v2 quad_perm:[1,0,3,2]"),
            (6, "v_mov_b32 v8, v2 quad_perm:[1,1,3,3]"),
            (7, "v_mov_b32 v9, v2 quad_perm:[0,0,2,2]"),
            (8, "v_sub_f32 v6, v8, v9"),
            (9, "exp mrt0, v4, v5, v6, v7 done vm"),
        ],
    );
    want(
        &swizzles(),
        &[
            // Quad mode (bit 15), [1,0,3,2] and [3,2,1,0].
            (4, "ds_swizzle_b32 v4, v2 offset:32945"),
            (5, "ds_swizzle_b32 v5, v2 offset:32795"),
        ],
    );
    want(
        &waterfall(),
        &[
            (7, "v_readfirstlane_b32 s12, v2"),
            (8, "v_cmp_eq_u32 vcc_lo, s12, v2"),
            (9, "s_and_saveexec_b32 s13, vcc_lo"),
            (12, "s_xor_b32 exec_lo, exec_lo, s13"),
            (14, "s_cmp_lt_u32 s10, 64"),
            (17, "s_mov_b32 exec_lo, s11"),
        ],
    );
    want(
        &section_max(),
        &[
            (4, "s_or_saveexec_b32 s6, -1"),
            (5, "v_cndmask_b32 v5, 0, v2, s6"),
            (6, "v_mov_b32 v6, v5 row_xmask:1"),
            (7, "v_max_u32 v5, v6, v5"),
            (14, "v_permlanex16_b32 v6, v5, 0, 0"),
            (16, "v_readlane_b32 s4, v5, 0"),
        ],
    );
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// What each translation calls: a pixel's quad permutations and quad
/// swizzles are quad reads, never wave reads (undefined from and in helper
/// lanes); a first-lane read asks whether the lane is a helper lane.
#[test]
fn pixel_lane_reads_are_quad_reads_or_ask_for_helper_lanes() {
    for (name, words) in [("quad_perm", quad_perms()), ("swizzle", swizzles())] {
        let (_, ops) = translate(&words);
        assert!(
            ops.contains("quadReadLaneAt") || ops.contains("quadOp"),
            "{name}: {ops}"
        );
        assert!(!ops.contains("waveReadLaneAt"), "{name}: {ops}");
    }
    let (_, ops) = translate(&waterfall());
    assert!(ops.contains("isHelperLane"), "{ops}");
    assert!(ops.contains("waveReadLaneFirst"), "{ops}");
    assert!(!ops.contains("waveReadLaneAt"), "{ops}");
    let (_, ops) = translate(&section_max());
    assert!(ops.contains("waveActiveOp"), "{ops}");
    assert!(!ops.contains("isHelperLane"), "{ops}");
}

/// DPP quad permutations reach helper lanes: the neighbour's key across x
/// and y, and the hand-made derivative 1, at a triangle's edge too. (With
/// wave reads, a helper neighbour is undefined: 0 on AMD, where the
/// derivative was the whole key and a texture read that took its level
/// from it read the smallest one.)
#[test]
fn quad_permutations_read_helper_lanes() {
    draw_everywhere(&quad_perms(), 3, |t| {
        let mut wrong = Vec::new();
        for (x, y) in t.pixels() {
            let px = t.at(x, y);
            if px[0] != key(x ^ 1, y) {
                wrong.push(format!(
                    "({x}, {y}) across x ({}): {} for {}",
                    lane(t, x ^ 1, y),
                    px[0],
                    key(x ^ 1, y)
                ));
            }
            if px[1] != key(x, y ^ 1) {
                wrong.push(format!(
                    "({x}, {y}) across y ({}): {} for {}",
                    lane(t, x, y ^ 1),
                    px[1],
                    key(x, y ^ 1)
                ));
            }
            if px[2] != 1.0 {
                wrong.push(format!("({x}, {y}) derivative: {}", px[2]));
            }
        }
        wrong
    });
}

/// Quad-mode swizzles reach helper lanes too.
#[test]
fn quad_swizzles_read_helper_lanes() {
    draw_everywhere(&swizzles(), 3, |t| {
        let mut wrong = Vec::new();
        for (x, y) in t.pixels() {
            let px = t.at(x, y);
            if px[0] != key(x ^ 1, y) {
                wrong.push(format!(
                    "({x}, {y}) across x ({}): {} for {}",
                    lane(t, x ^ 1, y),
                    px[0],
                    key(x ^ 1, y)
                ));
            }
            if px[1] != key(x ^ 1, y ^ 1) {
                wrong.push(format!(
                    "({x}, {y}) diagonal ({}): {} for {}",
                    lane(t, x ^ 1, y ^ 1),
                    px[1],
                    key(x ^ 1, y ^ 1)
                ));
            }
        }
        wrong
    });
}

/// A waterfall loop over `v_readfirstlane_b32` serves helper lanes: each
/// covered pixel and each of its quad neighbours, helper lanes included,
/// is served within the bound. Without a helper lane reading its own
/// value, `WaveReadLaneFirst` reads no lane once only helper lanes are
/// left, none of them matches, and the loop runs to its limit in each
/// (the program's bound here, the translator's loop limit in a title).
#[test]
fn a_waterfall_loop_serves_helper_lanes() {
    draw_everywhere(&waterfall(), 0, |t| {
        let mut wrong = Vec::new();
        for (x, y) in t.pixels() {
            let px = t.at(x, y);
            let served = |v: f32| (1.0..=BOUND as f32).contains(&v);
            if !served(px[0]) {
                wrong.push(format!("({x}, {y}) itself: {}", px[0]));
            }
            for (n, (nx, ny), what) in [
                (1, (x ^ 1, y), "across x"),
                (2, (x, y ^ 1), "across y"),
                (3, (x ^ 1, y ^ 1), "diagonal"),
            ] {
                if !served(px[n]) {
                    wrong.push(format!(
                        "({x}, {y}) {what} ({}): {}",
                        lane(t, nx, ny),
                        px[n]
                    ));
                }
            }
        }
        wrong
    });
}

/// A whole-wave section in a pixel program is the host's `WaveActiveMax`,
/// which leaves helper lanes out (unlike the console, whose `s_wqm` puts
/// them in EXEC): each covered pixel gets the key of a covered pixel of its
/// host wave, its own or a greater one, never a helper lane's. What a
/// helper lane gets is undefined in D3D12 and only reported here.
#[test]
fn a_wave_reduction_leaves_helper_lanes_out() {
    let drawn = draw_everywhere(&section_max(), 3, |t| {
        let keys: Vec<u32> = t
            .pixels()
            .iter()
            .map(|&(x, y)| key(x, y).to_bits())
            .collect();
        let mut wrong = Vec::new();
        for (x, y) in t.pixels() {
            let max = t.at(x, y)[0];
            if max < key(x, y) || !keys.contains(&max.to_bits()) {
                wrong.push(format!(
                    "({x}, {y}) maximum {max}: not a covered key at least {}",
                    key(x, y)
                ));
            }
        }
        wrong
    });
    for (name, t) in drawn {
        let (mut same, mut other) = (0, Vec::new());
        for (x, y) in t.pixels() {
            if t.covered(x ^ 1, y) {
                continue;
            }
            let [mine, helper, ..] = t.at(x, y);
            if helper == mine {
                same += 1;
            } else {
                other.push(helper);
            }
        }
        eprintln!(
            "{name}: a helper lane's maximum is its covered neighbour's in {same} quads; otherwise {other:?}"
        );
    }
}
