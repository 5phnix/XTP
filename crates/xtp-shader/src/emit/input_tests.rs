//! Pixel inputs, interpolation and exports. Barycentric tests from VIctory #453.

use crate::dxbc::{self, Container, FourCC};
use crate::emit::container::{self, SigElement};
use crate::emit::validator::Validator;
use crate::emit::{HostShader, emit, signature};
use crate::native::{HwStage, NativeProgram, PsInput, StageIo, Usage};
use crate::{ShaderError, io, ir, resource, structure};

/// `SPI_PS_INPUT_ADDR` bits: PERSP_CENTER, PERSP_CENTROID,
/// LINEAR_CENTER.
const PERSP_CENTER: u32 = 1 << 1;
const PERSP_CENTROID: u32 = 1 << 2;
const LINEAR_CENTER: u32 = 1 << 5;
const S_ENDPGM: u32 = 0xBF81_0000;

/// `exp mrt0, v<r>, v<r>, v<r>, v<r> done vm`.
fn exp_mrt0(r: u32) -> [u32; 2] {
    [0xF800_180F, r * 0x0101_0101]
}

/// VOP2 `v_add_f32 v<dst>, v<a>, v<b>`.
fn v_add_f32(dst: u32, a: u32, b: u32) -> u32 {
    3 << 25 | dst << 17 | b << 9 | (256 + a)
}

fn element(name: &str, kind: u8, row: u32) -> SigElement {
    SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp: 0,
        row: Some(row),
        col: 0,
        cols: 4,
        usage: 0,
        half: false,
    }
}

/// Translates a pixel program with the input VGPRs `input_addr`
/// enables and one float4 TEXCOORD0 attribute (attr 0, row 0).
fn pixel(words: &[u32], input_addr: u32) -> Result<HostShader, ShaderError> {
    pixel_rsrc(words, input_addr, 0)
}

fn pixel_rsrc(words: &[u32], input_addr: u32, rsrc2: u32) -> Result<HostShader, ShaderError> {
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 8,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![PsInput {
                usage: Usage::Generic,
                row: 0,
                mask: 0b1111,
                attr: 0,
                flat: false,
                fp16: false,
                pack2: false,
                row2: 0,
            }],
            input_addr,
            rsrc2,
            db_shader_control: 0,
            patches: vec![],
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let mut target = element("SV_Target", 16, 0);
    target.usage = 15;
    let isg1 = container::signature(&[element("TEXCOORD", 0, 0)], true);
    let osg1 = container::signature(&[target], false);
    let bytes = dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
    emit(&c, &native, &p, &st, &io, &res)
}

/// The translation's `SFI0` features and `ISG1` rows.
fn parts(shader: &HostShader) -> (u64, Vec<SigElement>) {
    let c = Container::parse(&shader.dxil).unwrap();
    let sfi0 = c.part(FourCC::SFI0).unwrap().data.try_into().unwrap();
    (
        u64::from_le_bytes(sfi0),
        signature(c.part(FourCC::ISG1).unwrap().data).unwrap(),
    )
}

fn validate(shader: &HostShader) {
    if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone()).unwrap();
    }
}

/// The `PSV0` interpolation mode of TEXCOORD0 (row 0, x-w, float).
fn texcoord_mode(shader: &HostShader) -> u8 {
    let c = Container::parse(&shader.dxil).unwrap();
    let psv = c.part(FourCC::PSV0).unwrap().data;
    let row = [1, 0, 0x44, 0, 3];
    let at = psv.windows(5).position(|w| w == row).unwrap();
    psv[at + 5]
}

/// `v_mov_b32 v<dst>, v<src>`.
fn v_mov_b32(dst: u32, src: u32) -> u32 {
    0x7E00_0200 | dst << 17 | (256 + src)
}

/// Custom interpolation reads I and J (v0, v1 with PERSP_CENTER) as
/// plain floats: they are channels y and z of `SV_Barycentrics`, the
/// weights of vertices 1 and 2. I alone reads y, J alone z (the sum
/// of both would read the same with the two swapped).
#[test]
fn barycentrics_read_as_values_are_sv_barycentrics() {
    let [e0, e1] = exp_mrt0(2);
    for (value, read) in [
        (v_mov_b32(2, 0), 0b010),
        (v_mov_b32(2, 1), 0b100),
        (v_add_f32(2, 0, 1), 0b110),
    ] {
        let words = [value, e0, e1, S_ENDPGM];
        let shader = pixel(&words, PERSP_CENTER).unwrap();
        let (features, inputs) = parts(&shader);
        assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
        let bary: Vec<_> = inputs.iter().filter(|e| e.kind == 28).collect();
        assert_eq!(bary.len(), 1, "{inputs:?}");
        assert_eq!((bary[0].index, bary[0].col, bary[0].cols), (0, 0, 3));
        // Not packed: the validator wants packing location -1.
        assert_eq!(bary[0].row, None);
        // The channels read: the element's read mask.
        let k = inputs.iter().position(|e| e.kind == 28).unwrap();
        let isg1 = Container::parse(&shader.dxil).unwrap();
        let isg1 = isg1.part(FourCC::ISG1).unwrap().data;
        assert_eq!(isg1[8 + 32 * k + 25], read, "{inputs:?}");
        validate(&shader);
    }
}

/// Two kinds of barycentrics read directly: one element per mode.
#[test]
fn two_barycentric_modes_get_two_elements() {
    // v0-v1 perspective i, j; v2-v3 linear i, j.
    let [e0, e1] = exp_mrt0(4);
    let words = [v_add_f32(4, 0, 3), e0, e1, S_ENDPGM];
    let shader = pixel(&words, PERSP_CENTER | LINEAR_CENTER).unwrap();
    let (_, inputs) = parts(&shader);
    let mut indices: Vec<u32> = inputs
        .iter()
        .filter(|e| e.kind == 28)
        .map(|e| e.index)
        .collect();
    indices.sort();
    assert_eq!(indices, [0, 1], "{inputs:?}");
    validate(&shader);
}

/// Center and centroid perspective I/J both read as values: DXIL
/// allows one perspective `SV_Barycentrics` element (the validator:
/// "There can only be up to two input attributes of SV_Barycentrics
/// with different perspective interpolation mode"), so the program
/// is refused rather than given two, or center weights for centroid
/// ones.
#[test]
fn center_and_centroid_barycentrics_are_refused() {
    // v0-v1 perspective center i, j; v2-v3 perspective centroid i, j.
    let [e0, e1] = exp_mrt0(4);
    let words = [v_add_f32(4, 0, 3), e0, e1, S_ENDPGM];
    match pixel(&words, PERSP_CENTER | PERSP_CENTROID) {
        Err(ShaderError::Unsupported { what, .. }) => {
            assert!(what.contains("two perspective modes"), "{what}")
        }
        Err(e) => panic!("{e:?}"),
        Ok(shader) => panic!("translated: {:?}", parts(&shader).1),
    }
    // Centroid alone (v2, v3) still translates, with one element.
    let words = [v_add_f32(4, 2, 3), e0, e1, S_ENDPGM];
    let shader = pixel(&words, PERSP_CENTER | PERSP_CENTROID).unwrap();
    let (_, inputs) = parts(&shader);
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    validate(&shader);
}

/// I and J only fed to `v_interp_*` stay host interpolation: no
/// barycentrics are declared.
#[test]
fn interpolation_alone_declares_no_barycentrics() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_eq!(features & 0x2_0000, 0);
    assert!(inputs.iter().all(|e| e.kind != 28), "{inputs:?}");
    validate(&shader);
}

/// Both: the attribute is interpolated and J is used as a value.
#[test]
fn interpolation_and_a_barycentric_value_together() {
    let [e0, e1] = exp_mrt0(3);
    let words = [
        0xC808_0000,        // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001,        // v_interp_p2_f32 v2, v1, attr0.x
        v_add_f32(3, 2, 1), // v3 = attribute + j
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_ne!(features & 0x2_0000, 0);
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    validate(&shader);
}

/// An interpolation whose i is computed (`v_mul_f32 v3, 0.5, v0`)
/// reads the attribute's vertices: TEXCOORD0 becomes constant, read
/// with `attributeAtVertex`; i and j are `SV_Barycentrics` values.
#[test]
fn computed_barycentrics_read_the_vertices() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0x1006_00F0, // v_mul_f32 v3, 0.5, v0
        0xC808_0003, // v_interp_p1_f32 v2, v3, attr0.x
        0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
}

/// Weights from constants alone: the vertices, no `SV_Barycentrics`.
#[test]
fn constant_weights_need_no_barycentrics() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0x7E06_02F0, // v_mov_b32 v3, 0.5
        0xC808_0003, // v_interp_p1_f32 v2, v3, attr0.x
        0xC809_0003, // v_interp_p2_f32 v2, v3, attr0.x
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
    assert!(inputs.iter().all(|e| e.kind != 28), "{inputs:?}");
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
}

/// The same attribute interpolated with the loaded i and j and with
/// a computed j: both from its vertices, the first weighed with
/// `SV_Barycentrics`.
#[test]
fn computed_and_loaded_barycentrics_on_one_attribute() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0xC810_0100, // v_interp_p1_f32 v4, v0, attr0.y
        0xC811_0101, // v_interp_p2_f32 v4, v1, attr0.y
        0x1006_02F0, // v_mul_f32 v3, 0.5, v1
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0003, // v_interp_p2_f32 v2, v3, attr0.x
        v_add_f32(2, 2, 4),
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (_, inputs) = parts(&shader);
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
}

/// P10 of an attribute the program interpolates is P1 - P0.
#[test]
fn p10_of_an_interpolated_attribute() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
        0xC812_0000, // v_interp_mov_f32 v4, p10, attr0.x
        v_add_f32(2, 2, 4),
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
    // Alone, what the hardware holds is not known.
    let [e0, e1] = exp_mrt0(4);
    let words = [0xC812_0000, e0, e1, S_ENDPGM];
    assert!(pixel(&words, PERSP_CENTER).is_err());
}

#[test]
fn a_first_half_export_reads_only_i_and_the_first_two_vertices() {
    let [exp, data] = exp_mrt0(2);
    let out = pixel(&[0xc808_0000, exp, data, S_ENDPGM], PERSP_CENTER).unwrap();
    let (_, inputs) = parts(&out);
    let bary = inputs.iter().position(|e| e.kind == 28).unwrap();
    let c = Container::parse(&out.dxil).unwrap();
    assert_eq!(
        c.part(FourCC::ISG1).unwrap().data[8 + 32 * bary + 25],
        0b010
    );
    assert_eq!(texcoord_mode(&out), 1);
    validate(&out);
}

#[test]
fn pixel_temporaries_begin_above_the_primitive_mask() {
    let [exp, data] = exp_mrt0(2);
    let read = |sgpr: u32| pixel_rsrc(&[0x7e04_0200 | sgpr, exp, data, S_ENDPGM], 0, 2);
    // One user SGPR (s0), then the primitive mask (s1).
    assert!(read(0).is_err());
    assert!(read(1).is_err());
    validate(&read(2).unwrap());
}

#[test]
fn conditional_pixel_export_keeps_the_lane_for_later_exports() {
    let [exp, data] = exp_mrt0(1);
    let words = [
        exp & !0x1800,
        data,        // initialize the target
        0x7d84_0080, // v_cmp_eq_u32 vcc, 0, v0
        0xbe8a_3c6a, // s_and_saveexec_b32 s10, vcc_lo
        exp & !0x1800,
        0,           // overwrite only in the selected lanes
        0xbefe_030a, // s_mov_b32 exec_lo, s10
        0xf800_1890,
        0, // exp null done vm
        S_ENDPGM,
    ];
    let out = pixel(&words, PERSP_CENTER).unwrap();
    assert!(!crate::emit::last_module_summary().contains("discard"));
    validate(&out);
}
