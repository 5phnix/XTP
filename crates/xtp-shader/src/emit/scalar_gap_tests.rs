use super::valu_tests::{build_valu_cs_with_mode, run_cases};

// v_cvt_f32_f64 v0, v[1:2]; both words come from the root constants.
const CVT: u32 = 0x7e00_1f01;

fn check(body: &[u32], mode: u8, cases: &[(u64, u32)]) {
    let shader = build_valu_cs_with_mode(body, mode);
    let inputs: Vec<_> = cases
        .iter()
        .map(|&(bits, _)| [bits as u32, (bits >> 32) as u32, 0, 0])
        .collect();
    run_cases(&shader, &inputs, |host, i, _, result| {
        assert_eq!(
            result, cases[i].1,
            "{host}, MODE={mode:#04x}, source={:#018x}",
            cases[i].0
        );
    });
}

#[test]
fn double_to_float_exact_bits() {
    check(
        &[CVT],
        0xf0,
        &[
            (0, 0),
            (1 << 63, 0x8000_0000),
            (0x3ff0_0000_1000_0000, 0x3f80_0000),
            (0x3ff0_0000_1000_0001, 0x3f80_0001),
            (0x3ff0_0000_3000_0000, 0x3f80_0002),
            (0x3690_0000_0000_0000, 0),
            (0x3690_0000_0000_0001, 1),
            (0x36a0_0000_0000_0000, 1),
            (0xb6a0_0000_0000_0000, 0x8000_0001),
            (0x380f_fffe_0000_0000, 0x007f_fff8),
            (0x380f_ffff_e000_0000, 0x0080_0000),
            (0x47ef_ffff_efff_ffff, 0x7f7f_ffff),
            (0x47ef_ffff_f000_0000, 0x7f80_0000),
            (0xfff0_0000_0000_0000, 0xff80_0000),
            (0x7ff0_0000_0000_0001, 0x7fc0_0000),
            (0xfffa_bcde_e000_0000, 0xffd5_e6f7),
        ],
    );
}

#[test]
fn double_literal_and_source_modifiers() {
    // The literal gives the high word. The inline float constant uses its
    // double-precision representation directly.
    check(&[0x7e00_1eff, 0x400c_0000], 0xf0, &[(0, 3.5f32.to_bits())]);
    check(&[0x7e00_1ef8], 0xf0, &[(0, 0x3e22_f983)]);
    // VOP3, -abs(s[0:1]); the negation also acts on the signed zero.
    check(
        &[0xd58f_0100, 0x2000_0000],
        0xf0,
        &[(0, 0x8000_0000), (0xbff0_0000_0000_0000, 0xbf80_0000)],
    );
}

#[test]
fn double_to_float_obeys_mode_writes() {
    // Round toward +∞, then keep the f32 output denormals.
    check(
        &[0xbfa4_0001, 0xbfa5_000f, CVT],
        0,
        &[
            (1, 1),
            (1 << 63 | 1, 0x8000_0000),
            (0xbff0_0000_1000_0000, 0xbf80_0000),
            (0xffef_ffff_ffff_ffff, 0xff7f_ffff),
        ],
    );
    // MODE[1:0] = 2, written by a literal. Denormal outputs stay flushed to zero.
    check(
        &[0xba80_0801, 2, CVT],
        0xc0,
        &[
            (0xbff0_0000_1000_0000, 0xbf80_0001),
            (0xb6a0_0000_0000_0000, 0x8000_0000),
            (0x380f_ffff_e000_0001, 0),
        ],
    );
    check(&[CVT], 0x23, &[(0x7fef_ffff_ffff_ffff, 0x7f7f_ffff)]);
    // Even when rounding toward +∞, a denormal f64 input flushed to zero stays zero.
    check(&[CVT], 0x21, &[(1, 0)]);
}

#[test]
fn getpc_read_as_data() {
    // The preamble is seven words long. Both destinations are written first,
    // to catch a kept word as well as a PC shifted by four bytes.
    for (source, expected) in [(6, 40), (7, 0)] {
        check(
            &[0xbe86_03c1, 0xbe87_03c1, 0xbe86_1f00, 0x7e00_0200 | source],
            0,
            &[(0, expected)],
        );
    }
}

#[test]
fn conversion_uses_the_mode_at_a_control_flow_join() {
    // If s2 is zero, MODE at entry is kept. Otherwise s2 replaces the two
    // rounding bits, then both paths join at the conversion.
    let shader = build_valu_cs_with_mode(&[0xbf06_8002, 0xbf85_0001, 0xb982_0801, CVT], 0xf0);
    let cases = [
        [0x1000_0000, 0x3ff0_0000, 0, 0],
        [0x1000_0000, 0x3ff0_0000, 1, 0],
    ];
    run_cases(&shader, &cases, |_, i, _, result| {
        assert_eq!(result, 0x3f80_0000 + i as u32)
    });
}
