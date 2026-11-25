//! GFX10 literal dwords used as signed, unsigned, and bitwise 64-bit operands.
//! The MAD cases include the -100 addend from Marvisak/VIctory#2261 (Calvix2k)
//! and #2254 (BastienRodz). The shift and compare cases follow #447 (Dufftwt).

use super::valu_tests::{build_valu_cs, run_cases, sop2, vgpr, vop1, vop3};

const LITERAL: u16 = 255;
const LITERALS: [u32; 2] = [100, (-100i32) as u32];

#[test]
fn mad_literal_addend() {
    let cases = [
        [0, 0, 0, 0],
        [10, 10, 0, 0],
        [(-100_000i32) as u32, 100_000, 0, 0],
    ];
    for (op, signed) in [(0x177, true), (0x176, false)] {
        for literal in LITERALS {
            // v_mad_{i64_i32,u64_u32} v[6:7], s20, v1, v2, literal
            let [w0, w1] = vop3(op, 6, vgpr(1), vgpr(2), LITERAL);
            for word in 0..2 {
                let shader =
                    build_valu_cs(&[w0 | 20 << 8, w1, literal, vop1(1, 0, vgpr(6 + word))]);
                run_cases(&shader, &cases, |host, _, [a, b, ..], got| {
                    let wide = |x: u32| {
                        if signed {
                            i64::from(x as i32) as u64
                        } else {
                            u64::from(x)
                        }
                    };
                    let sum = wide(a).wrapping_mul(wide(b)).wrapping_add(wide(literal));
                    assert_eq!(
                        got,
                        (sum >> (word * 32)) as u32,
                        "{host}: {op:#x}, {a} * {b} + {literal:#x}, word {word}"
                    );
                });
            }
        }
    }
}

#[test]
fn mad_carry_is_bit_64_of_the_full_sum() {
    let cases = [
        [10, 10, u32::MAX, u32::MAX],
        [(-3i32) as u32, 5, 0, 0],
        [(-3i32) as u32, 5, 25, 0],
        [(-3i32) as u32, 5, 0, 0x8000_0000],
    ];
    for (op, signed) in [(0x177, true), (0x176, false)] {
        let [w0, w1] = vop3(op, 6, vgpr(1), vgpr(2), vgpr(3));
        let [sel0, sel1] = vop3(0x101, 0, 128, 129, 20);
        let shader = build_valu_cs(&[w0 | 20 << 8, w1, sel0, sel1]);
        run_cases(&shader, &cases, |host, _, [a, b, lo, hi], got| {
            let acc = u64::from(hi) << 32 | u64::from(lo);
            let sum = if signed {
                i128::from(a as i32) * i128::from(b as i32) + i128::from(acc as i64)
            } else {
                i128::from(a) * i128::from(b) + i128::from(acc)
            };
            assert_eq!(got, ((sum >> 64) & 1) as u32, "{host}: {op:#x}, sum {sum}");
        });
    }
}

#[test]
fn shift_literal_source() {
    let cases = [[0, 0, 0, 0], [4, 0, 0, 0], [32, 0, 0, 0], [63, 0, 0, 0]];
    for (vop, sop) in [(0x2ff, 0x1f), (0x300, 0x21), (0x301, 0x23)] {
        for literal in LITERALS {
            let [w0, w1] = vop3(vop, 6, vgpr(1), LITERAL, 0);
            for word in 0..2 {
                let vector = [w0, w1, literal, vop1(1, 0, vgpr(6 + word))];
                let scalar = [
                    sop2(sop, 6, LITERAL as u8, 0),
                    literal,
                    vop1(1, 0, u16::from(6 + word)),
                ];
                for body in [&vector[..], &scalar[..]] {
                    let shader = build_valu_cs(body);
                    run_cases(&shader, &cases, |host, _, [n, ..], got| {
                        let result = match vop {
                            0x2ff => u64::from(literal) << n,
                            0x300 => u64::from(literal) >> n,
                            _ => (i64::from(literal as i32) >> n) as u64,
                        };
                        assert_eq!(
                            got,
                            (result >> (word * 32)) as u32,
                            "{host}: {body:x?}, shift {n}, word {word}"
                        );
                    });
                }
            }
        }
    }
}

#[test]
fn compare_literal_source() {
    let literal = (-100i32) as u32;
    let cases = [[literal, u32::MAX, 0, 0], [literal, 0, 0, 0]];
    for (op, signed) in [(0x0a2, true), (0x0e2, false)] {
        for (a, b) in [(LITERAL, vgpr(1)), (vgpr(1), LITERAL)] {
            // v_cmp_eq_{i64,u64} s20, a, b; v_cndmask_b32 v0, 0, 1, s20
            let [w0, w1] = vop3(op, 20, a, b, 0);
            let [sel0, sel1] = vop3(0x101, 0, 128, 129, 20);
            let shader = build_valu_cs(&[w0, w1, literal, sel0, sel1]);
            run_cases(&shader, &cases, |host, _, [_, hi, ..], got| {
                assert_eq!(
                    got,
                    u32::from((hi == u32::MAX) == signed),
                    "{host}: {op:#x}, high {hi:#x}"
                );
            });
        }
    }
}
