//! GFX10 machine code: instruction boundaries, encodings and opcodes.
//!
//! Instructions are found by following control flow from offset 0, not by a
//! linear sweep: compilers place data (constant tables) in the code region
//! after the end of the program. [`Instruction::fields`] decodes the operand
//! fields and [`Instruction::text`] prints them in AMD's SP3 syntax.
//!
//! Encodings by the top bits of the first dword:
//!
//! ```text
//! 0xxxxxxx  VOP2; 0111111 VOP1; 0111110 VOPC
//! 10        SOP2; 1011 SOPK; 101111101 SOP1; 101111110 SOPC; 101111111 SOPP
//! 110010 VINTRP  110011 VOP3P  110101 VOP3  110110 DS  110111 FLAT/GLOBAL/SCRATCH
//! 111000 MUBUF   111010 MTBUF  111100 MIMG  111101 SMEM  111110 EXP
//! ```

mod fields;
mod opcodes;
mod sp3;

pub use fields::{Cache, Dpp, Fields, NULL, Operand, Sdwa, Valu};

use std::collections::BTreeMap;

use crate::ShaderError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    Sop2,
    Sopk,
    Sop1,
    Sopc,
    Sopp,
    Smem,
    Vop2,
    Vop1,
    Vopc,
    Vop3,
    Vop3p,
    Vintrp,
    Ds,
    Mubuf,
    Mtbuf,
    Mimg,
    Exp,
    Flat,
    Global,
    Scratch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// Byte offset in the program.
    pub offset: usize,
    pub encoding: Encoding,
    pub opcode: u16,
    /// Every dword of the instruction, including literals and extra dwords.
    pub words: Vec<u32>,
}

impl Instruction {
    /// AMD's name for the opcode, if it is in our table.
    pub fn name(&self) -> Option<&'static str> {
        name(self.encoding, self.opcode)
    }
}

/// Decodes every instruction reachable from offset 0, sorted by offset.
pub fn decode(code: &[u8]) -> Result<Vec<Instruction>, ShaderError> {
    if !code.len().is_multiple_of(4) {
        return Err(isa(code.len(), "code size is not a multiple of 4"));
    }
    let mut found = BTreeMap::new();
    let mut pending = vec![0usize];
    while let Some(mut offset) = pending.pop() {
        while !found.contains_key(&offset) {
            let inst = instruction(code, offset)?;
            let next = offset + 4 * inst.words.len();
            let flow = control(&inst, code.len(), |at| {
                found.range(..at).next_back().map(|(_, i)| i)
            })?;
            found.insert(offset, inst);
            match flow {
                Flow::Next => offset = next,
                Flow::Stop => break,
                Flow::Jump(target) => offset = target,
                Flow::Branch(target) => {
                    pending.push(target);
                    offset = next;
                }
            }
        }
    }
    let list: Vec<Instruction> = found.into_values().collect();
    if let Some(w) = list
        .windows(2)
        .find(|w| w[0].offset + 4 * w[0].words.len() > w[1].offset)
    {
        return Err(isa(w[1].offset, "a branch lands inside an instruction"));
    }
    Ok(list)
}

/// Where execution goes after an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Next,
    /// End of the program, or an indirect jump that is not a far jump.
    Stop,
    Jump(usize),
    /// Conditional: to the target or the next instruction.
    Branch(usize),
}

/// Control flow after `inst`, resolving far jumps. `before(at)` returns the
/// decoded instruction with the highest offset below `at`, if any.
pub(crate) fn control<'a>(
    inst: &Instruction,
    len: usize,
    before: impl Fn(usize) -> Option<&'a Instruction>,
) -> Result<Flow, ShaderError> {
    Ok(match flow(inst, inst.offset + 4 * inst.words.len(), len)? {
        Flow::Stop if inst.name() == Some("s_setpc_b64") => {
            far_jump(before, inst, len)?.map_or(Flow::Stop, Flow::Jump)
        }
        flow => flow,
    })
}

fn flow(inst: &Instruction, next: usize, len: usize) -> Result<Flow, ShaderError> {
    let target = || {
        let delta = 4 * i64::from(inst.words[0] as u16 as i16);
        usize::try_from(next as i64 + delta)
            .ok()
            .filter(|&t| t < len)
            .ok_or(isa(inst.offset, "branch target outside the code"))
    };
    Ok(match (inst.encoding, inst.name().unwrap_or("")) {
        (
            Encoding::Sopp,
            "s_endpgm" | "s_endpgm_saved" | "s_endpgm_ordered_ps_done" | "s_code_end",
        ) => Flow::Stop,
        (Encoding::Sopp, "s_branch") => Flow::Jump(target()?),
        (Encoding::Sopp, name) if name.starts_with("s_cbranch_") => Flow::Branch(target()?),
        (Encoding::Sopk, "s_call_b64") => Flow::Branch(target()?),
        // Indirect jump; `decode` resolves the compiler's far-jump form.
        (Encoding::Sop1, "s_setpc_b64") => Flow::Stop,
        _ => Flow::Next,
    })
}

/// Resolves the far jump compilers emit beyond the reach of a branch offset:
/// `s_getpc_b64 s[n:n+1]`, `s_add_u32|s_sub_u32 sn, sn, literal`, a carry
/// into s(n+1), `s_setpc_b64 s[n:n+1]`, looking back over the straight-line
/// run just decoded. Any other `s_setpc_b64` (e.g. a handover to the next
/// merged stage through registers) gives `None`.
fn far_jump<'a>(
    before: impl Fn(usize) -> Option<&'a Instruction>,
    setpc: &Instruction,
    len: usize,
) -> Result<Option<usize>, ShaderError> {
    let n = setpc.words[0] & 0xFF;
    let mut delta = None;
    let mut at = setpc.offset;
    for _ in 0..4 {
        let Some(prev) = before(at) else {
            break;
        };
        if prev.offset + 4 * prev.words.len() != at {
            break;
        }
        let w = prev.words[0];
        let (sdst, ssrc0, ssrc1) = ((w >> 16) & 0x7F, w & 0xFF, (w >> 8) & 0xFF);
        match (prev.encoding, prev.name()) {
            (Encoding::Sop2, Some(op @ ("s_add_u32" | "s_sub_u32")))
                if sdst == n && ssrc0 == n && ssrc1 == 0xFF && delta.is_none() =>
            {
                // The low dword of a signed 64-bit offset: a backward jump
                // adds a negative one (its high dword is -1).
                let literal = i64::from(prev.words[1] as i32);
                delta = Some(if op == "s_add_u32" { literal } else { -literal });
            }
            (Encoding::Sop1, Some("s_getpc_b64")) if sdst == n => {
                let Some(delta) = delta else { break };
                // s_getpc_b64 yields the address of the instruction after it.
                return usize::try_from(prev.offset as i64 + 4 + delta)
                    .ok()
                    .filter(|&t| t < len)
                    .map(Some)
                    .ok_or(isa(setpc.offset, "far jump target outside the code"));
            }
            _ => {}
        }
        at = prev.offset;
    }
    Ok(None)
}

fn instruction(code: &[u8], offset: usize) -> Result<Instruction, ShaderError> {
    let word = |i: usize| {
        code.get(offset + 4 * i..offset + 4 * i + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
            .ok_or(isa(offset, "instruction runs past the end of the code"))
    };
    let w = word(0)?;
    let (encoding, opcode, len) = if w >> 31 == 0 {
        let src0 = w & 0x1FF;
        // Literal constant, SDWA, DPP16, DPP8, DPP8 with fetch-inactive.
        let extra = usize::from(matches!(src0, 0xFF | 0xF9 | 0xFA | 0xE9 | 0xEA));
        match w >> 25 {
            0x3F => (Encoding::Vop1, (w >> 9) & 0xFF, 1 + extra),
            0x3E => (Encoding::Vopc, (w >> 17) & 0xFF, 1 + extra),
            op => {
                let mk_ak = name(Encoding::Vop2, op as u16).is_some_and(|n| {
                    matches!(
                        n,
                        "v_madmk_f32"
                            | "v_madak_f32"
                            | "v_fmamk_f32"
                            | "v_fmaak_f32"
                            | "v_madmk_f16"
                            | "v_madak_f16"
                            | "v_fmamk_f16"
                            | "v_fmaak_f16"
                    )
                });
                (Encoding::Vop2, op, 1 + extra.max(usize::from(mk_ak)))
            }
        }
    } else if w >> 30 == 0b10 {
        let literal = |srcs: &[u32]| 1 + usize::from(srcs.contains(&0xFF));
        match w >> 23 {
            0x17D => (Encoding::Sop1, (w >> 8) & 0xFF, literal(&[w & 0xFF])),
            0x17E => (
                Encoding::Sopc,
                (w >> 16) & 0x7F,
                literal(&[w & 0xFF, (w >> 8) & 0xFF]),
            ),
            0x17F => (Encoding::Sopp, (w >> 16) & 0x7F, 1),
            _ if w >> 28 == 0b1011 => {
                let op = (w >> 23) & 0x1F;
                let imm32 = name(Encoding::Sopk, op as u16) == Some("s_setreg_imm32_b32");
                (Encoding::Sopk, op, 1 + usize::from(imm32))
            }
            _ => (
                Encoding::Sop2,
                (w >> 23) & 0x7F,
                literal(&[w & 0xFF, (w >> 8) & 0xFF]),
            ),
        }
    } else {
        let vop3 =
            |w1: u32| 2 + usize::from([w1, w1 >> 9, w1 >> 18].iter().any(|s| s & 0x1FF == 0xFF));
        match w >> 26 {
            0x32 => (Encoding::Vintrp, (w >> 16) & 0x3, 1),
            0x33 => (Encoding::Vop3p, (w >> 16) & 0x7F, vop3(word(1)?)),
            0x35 => {
                let op = (w >> 16) & 0x3FF;
                // The e64 interpolation forms cannot take a literal. Their
                // attribute field is 0xFF for attr63.w.
                let len = if matches!(op, 0x200..=0x202) {
                    2
                } else {
                    vop3(word(1)?)
                };
                (Encoding::Vop3, op, len)
            }
            0x36 => (Encoding::Ds, (w >> 18) & 0xFF, 2),
            0x37 => {
                let encoding = match (w >> 14) & 3 {
                    0 => Encoding::Flat,
                    1 => Encoding::Scratch,
                    2 => Encoding::Global,
                    _ => return Err(isa(offset, "reserved FLAT segment")),
                };
                (encoding, (w >> 18) & 0x7F, 2)
            }
            0x38 => (Encoding::Mubuf, (w >> 18) & 0x7F | (w >> 25 & 1) << 7, 2),
            0x3A => (
                Encoding::Mtbuf,
                (w >> 16) & 0x7 | (word(1)? >> 21 & 1) << 3,
                2,
            ),
            // Bits 2:1 count extra dwords of non-sequential address registers.
            0x3C => (
                Encoding::Mimg,
                (w >> 18) & 0x7F | (w & 1) << 7,
                2 + (w as usize >> 1 & 3),
            ),
            0x3D => (Encoding::Smem, (w >> 18) & 0xFF, 2),
            0x3E => (Encoding::Exp, 0, 2),
            _ => return Err(isa(offset, "unknown encoding")),
        }
    };
    Ok(Instruction {
        offset,
        encoding,
        opcode: opcode as u16,
        words: (0..len).map(word).collect::<Result<_, _>>()?,
    })
}

fn name(encoding: Encoding, opcode: u16) -> Option<&'static str> {
    use opcodes as t;
    let (table, op) = match encoding {
        Encoding::Sop2 => (t::SOP2, opcode),
        Encoding::Sopk => (t::SOPK, opcode),
        Encoding::Sop1 => (t::SOP1, opcode),
        Encoding::Sopc => (t::SOPC, opcode),
        Encoding::Sopp => (t::SOPP, opcode),
        Encoding::Smem => (t::SMEM, opcode),
        Encoding::Vop2 => (t::VOP2, opcode),
        Encoding::Vop1 => (t::VOP1, opcode),
        Encoding::Vopc => (t::VOPC, opcode),
        // VOP3 also encodes the VOPC, VOP2, VOP1 and VINTRP opcodes at fixed bases.
        Encoding::Vop3 => match opcode {
            0x000..0x100 => (t::VOPC, opcode),
            0x100..0x140 => (t::VOP2, opcode - 0x100),
            0x180..0x200 => (t::VOP1, opcode - 0x180),
            0x200..=0x202 => (t::VINTRP, opcode - 0x200),
            _ => (t::VOP3, opcode),
        },
        Encoding::Vop3p => (t::VOP3P, opcode),
        Encoding::Vintrp => (t::VINTRP, opcode),
        Encoding::Ds => (t::DS, opcode),
        Encoding::Mubuf => (t::MUBUF, opcode),
        Encoding::Mtbuf => (t::MTBUF, opcode),
        Encoding::Mimg => (t::MIMG, opcode),
        Encoding::Exp => return Some("exp"),
        Encoding::Flat => (t::FLAT, opcode),
        Encoding::Global => (t::GLOBAL, opcode),
        Encoding::Scratch => (t::SCRATCH, opcode),
    };
    table
        .binary_search_by_key(&op, |&(o, _)| o)
        .ok()
        .map(|i| table[i].1)
}

fn isa(offset: usize, reason: &'static str) -> ShaderError {
    ShaderError::Isa { offset, reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S_ENDPGM: u32 = 0xBF81_0000;
    /// s_branch / s_cbranch_scc0 with a dword offset relative to the next instruction.
    const fn sopp(op: u32, simm16: i16) -> u32 {
        0xBF80_0000 | op << 16 | simm16 as u16 as u32
    }

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn summary(code: &[u32]) -> Vec<(usize, usize, &'static str)> {
        decode(&bytes(code))
            .unwrap()
            .iter()
            .map(|i| (i.offset, i.words.len(), i.name().unwrap_or("?")))
            .collect()
    }

    #[test]
    fn tables_are_sorted() {
        for t in [
            opcodes::SOP2,
            opcodes::SOPK,
            opcodes::SOP1,
            opcodes::SOPC,
            opcodes::SOPP,
            opcodes::SMEM,
            opcodes::VOP2,
            opcodes::VOP1,
            opcodes::VOPC,
            opcodes::VOP3,
            opcodes::VOP3P,
            opcodes::VINTRP,
            opcodes::DS,
            opcodes::MUBUF,
            opcodes::MTBUF,
            opcodes::MIMG,
            opcodes::FLAT,
            opcodes::GLOBAL,
            opcodes::SCRATCH,
        ] {
            assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
        }
    }

    #[test]
    fn lengths_and_names() {
        let code = [
            0xBE80_03FF,
            0x3F80_0000, // s_mov_b32 s0, literal
            0x0600_0000, // v_add_f32 v0, s0, v0
            0xD54B_0000,
            0x03FE_0000,
            0x4000_0000, // v_fma_f32 with a literal src2
            0xF080_0F0A,
            0,
            0, // image_sample (2D) with one extra address dword
            0xF800_080F,
            0, // exp
            S_ENDPGM,
        ];
        assert_eq!(
            summary(&code),
            [
                (0, 2, "s_mov_b32"),
                (8, 1, "v_add_f32"),
                (12, 3, "v_fma_f32"),
                (24, 3, "image_sample"),
                (36, 2, "exp"),
                (44, 1, "s_endpgm"),
            ]
        );
    }

    // Encoding fixtures from llvm-project llvmorg-19.1.7, Apache-2.0 WITH
    // LLVM-exception. The interpolation modifier combination is synthetic.
    #[test]
    fn vop3_interpolation_operands_and_modifiers() {
        // llvm/test/MC/AMDGPU/gfx10_asm_vop3.s
        let vgpr = |reg| Operand::Vector { reg, dwords: 1 };
        let attr = Operand::Attr {
            index: 0,
            chan: 0,
            high: false,
        };
        for (op, name, w1, src) in [
            (0x200, "v_interp_p1_f32", 0x0002_0400, vgpr(2)),
            (0x201, "v_interp_p2_f32", 0x0002_0400, vgpr(2)),
            (0x202, "v_interp_mov_f32", 0, Operand::Const(0)),
            (0x202, "v_interp_mov_f32", 0x400, Operand::Const(2)),
        ] {
            let words = [0xD400_0005 | op << 16, w1];
            let inst = instruction(&bytes(&words), 0).unwrap();
            assert_eq!(inst.words, words);
            assert_eq!((inst.encoding, inst.opcode), (Encoding::Vop3, op as u16));
            assert_eq!(inst.name(), Some(name));
            assert_eq!(
                inst.fields(false),
                Fields::Valu(Valu {
                    dst: Some(vgpr(5)),
                    src: vec![attr, src],
                    ..Valu::default()
                })
            );
            if src == Operand::Const(2) {
                assert_eq!(inst.text(false), "v_interp_mov_f32 v5, p0, attr0.x");
            }
        }

        // attr63.w occupies the same field value as a generic VOP3 literal.
        let words = [0xD600_8205, 0x4802_04FF, S_ENDPGM];
        let decoded = decode(&bytes(&words)).unwrap();
        assert_eq!(decoded[1].offset, 8);
        assert_eq!(
            decoded[0].fields(false),
            Fields::Valu(Valu {
                dst: Some(vgpr(5)),
                src: vec![
                    Operand::Attr {
                        index: 63,
                        chan: 3,
                        high: false,
                    },
                    vgpr(2),
                ],
                neg: 0b010,
                abs: 0b010,
                clamp: true,
                omod: 1,
                ..Valu::default()
            })
        );
    }

    #[test]
    fn mimg_gather4h_operands() {
        // llvm/test/MC/AMDGPU/gfx10_asm_mimg.s
        let words = [0xF184_021A, 0x0021_4020, 0x0000_2221];
        let inst = instruction(&bytes(&words), 0).unwrap();
        assert_eq!(inst.words, words);
        assert_eq!((inst.encoding, inst.opcode), (Encoding::Mimg, 0x061));
        assert_eq!(inst.name(), Some("image_gather4h"));
        assert_eq!(
            inst.fields(false),
            Fields::Image {
                data: Operand::Vector { reg: 64, dwords: 4 },
                addr: (32..=34)
                    .map(|reg| Operand::Vector { reg, dwords: 1 })
                    .collect(),
                rsrc: Operand::Scalar { code: 4, dwords: 8 },
                sampler: Some(Operand::Scalar { code: 4, dwords: 4 }),
                dmask: 2,
                dim: 3,
                unorm: false,
                r128: false,
                a16: false,
                d16: false,
                tfe: false,
                lwe: false,
                cache: Cache::default(),
            }
        );
    }

    #[test]
    fn mimg_coarse_g16_sample_operands() {
        // llvm/test/MC/AMDGPU/gfx10_asm_mimg.s has 1D sequential and 2D NSA
        // encodings for each form. The comparison and clamp each add one dword.
        let cases: [(&str, u32, &[u32], &[u8]); 4] = [
            (
                "image_sample_cd_g16",
                0xF1A0_0F01,
                &[0xF1A0_0F0B, 0x0040_0000, 0x0005_0402],
                &[0, 2, 4, 5],
            ),
            (
                "image_sample_cd_cl_g16",
                0xF1A4_0F01,
                &[0xF1A4_0F0B, 0x0040_0000, 0x0605_0402],
                &[0, 2, 4, 5, 6],
            ),
            (
                "image_sample_c_cd_g16",
                0xF1A8_0F01,
                &[0xF1A8_0F0B, 0x0040_0000, 0x0605_0301],
                &[0, 1, 3, 5, 6],
            ),
            (
                "image_sample_c_cd_cl_g16",
                0xF1AC_0F01,
                &[0xF1AC_0F0D, 0x0040_0000, 0x0605_0301, 7],
                &[0, 1, 3, 5, 6, 7],
            ),
        ];
        for (name, w0, nsa, regs) in cases {
            let sequential = [w0, 0x0040_0000];
            for (words, dim, addr) in [
                (
                    &sequential[..],
                    0,
                    vec![Operand::Vector {
                        reg: 0,
                        dwords: regs.len() as u8 - 1,
                    }],
                ),
                (
                    nsa,
                    1,
                    regs.iter()
                        .map(|&reg| Operand::Vector { reg, dwords: 1 })
                        .collect(),
                ),
            ] {
                let inst = instruction(&bytes(words), 0).unwrap();
                assert_eq!(inst.words, words);
                assert_eq!(inst.encoding, Encoding::Mimg);
                assert_eq!(inst.name(), Some(name));
                assert_eq!(
                    inst.fields(false),
                    Fields::Image {
                        data: Operand::Vector { reg: 0, dwords: 4 },
                        addr,
                        rsrc: Operand::Scalar { code: 0, dwords: 8 },
                        sampler: Some(Operand::Scalar { code: 8, dwords: 4 }),
                        dmask: 0xF,
                        dim,
                        unorm: false,
                        r128: false,
                        a16: false,
                        d16: false,
                        tfe: false,
                        lwe: false,
                        cache: Cache::default(),
                    },
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn vop3p_signed_dot8_operands() {
        // llvm/test/MC/AMDGPU/gfx1011_dlops.s
        let words = [0xCC18_4000, 0x1C0E_0501];
        let inst = instruction(&bytes(&words), 0).unwrap();
        let vgpr = |reg| Operand::Vector { reg, dwords: 1 };
        assert_eq!(inst.words, words);
        assert_eq!((inst.encoding, inst.opcode), (Encoding::Vop3p, 0x018));
        assert_eq!(inst.name(), Some("v_dot8_i32_i4"));
        assert_eq!(
            inst.fields(false),
            Fields::Valu(Valu {
                dst: Some(vgpr(0)),
                src: vec![vgpr(1), vgpr(2), vgpr(3)],
                opsel_hi: 0b111,
                ..Valu::default()
            })
        );
    }

    #[test]
    fn image_address_dwords_by_form() {
        // 2D: offset, bias and reference a dword each; 16-bit derivatives
        // (g16 opcodes) two per dword, each direction its own; a16 packs
        // the coordinates and the level, not the derivatives (the counts
        // with a16 are those of LLVM's gfx10_asm_mimg.s).
        for (op, a16, n) in [
            (0x22u32, false, 6), // image_sample_d
            (0x22, true, 5),     // image_sample_d, a16
            (0x23, true, 6),     // image_sample_d_cl, a16
            (0xA2, false, 4),    // image_sample_d_g16
            (0xA2, true, 3),     // image_sample_d_g16, a16
            (0xBB, false, 7),    // image_sample_c_d_cl_o_g16
            (0x33, true, 7),     // image_sample_d_cl_o, a16
            (0xE8, true, 3),     // image_sample_cd_g16, a16
            (0xE9, true, 4),     // image_sample_cd_cl_g16, a16
            (0xEA, true, 4),     // image_sample_c_cd_g16, a16
            (0xEB, true, 5),     // image_sample_c_cd_cl_g16, a16
        ] {
            let w0 = 0xF000_0000 | (op & 0x7F) << 18 | op >> 7 | 1 << 3 | 1 << 8;
            let d = decode(&bytes(&[w0, u32::from(a16) << 30, S_ENDPGM])).unwrap();
            let Fields::Image { addr, .. } = d[0].fields(false) else {
                panic!("{:?}", d[0].name())
            };
            assert_eq!(
                addr,
                [Operand::Vector { reg: 0, dwords: n }],
                "{:?}",
                d[0].name()
            );
        }
    }

    #[test]
    fn follows_branches_and_skips_data() {
        let code = [
            sopp(0x04, 1), // s_cbranch_scc0 -> 8
            sopp(0x02, 1), // s_branch -> 12
            S_ENDPGM,
            S_ENDPGM,
            S_ENDPGM, // data follows the program end
            0xFFFF_FFFF,
        ];
        let found: Vec<_> = summary(&code).iter().map(|i| i.0).collect();
        assert_eq!(found, [0, 4, 8, 12]);
    }

    #[test]
    fn follows_far_jumps() {
        let code = [
            0xBE88_1F00, // s_getpc_b64 s[8:9]
            0x8008_FF08,
            20,          // s_add_u32 s8, s8, 20  -> 4 + 20 = 24
            0x8209_8009, // s_addc_u32 s9, s9, 0
            0xBE80_2008, // s_setpc_b64 s[8:9]
            0xFFFF_FFFF, // not code
            S_ENDPGM,
        ];
        let found: Vec<_> = summary(&code).iter().map(|i| (i.0, i.2)).collect();
        assert_eq!(
            found,
            [
                (0, "s_getpc_b64"),
                (4, "s_add_u32"),
                (12, "s_addc_u32"),
                (16, "s_setpc_b64"),
                (24, "s_endpgm"),
            ]
        );
        // Through other registers (a handover to another stage): the path ends.
        assert_eq!(summary(&[0xBE80_2018, S_ENDPGM]).len(), 1);
    }

    #[test]
    fn follows_far_jumps_backward() {
        // The compiler's backward far jump adds a negative offset: the low
        // dword as a 32-bit literal, the high dword as -1 with the carry.
        let code = [
            sopp(0x02, 1), // s_branch -> 8, past the jump target
            S_ENDPGM,      // the far jump's target: only it reaches here
            0xBE88_1F00,   // s_getpc_b64 s[8:9]
            0x8008_FF08,
            0xFFFF_FFF8, // s_add_u32 s8, s8, -8 -> 12 - 8 = 4
            0x8209_C109, // s_addc_u32 s9, s9, -1
            0xBE80_2008, // s_setpc_b64 s[8:9]
        ];
        let found: Vec<_> = summary(&code).iter().map(|i| (i.0, i.2)).collect();
        assert_eq!(
            found,
            [
                (0, "s_branch"),
                (4, "s_endpgm"),
                (8, "s_getpc_b64"),
                (12, "s_add_u32"),
                (20, "s_addc_u32"),
                (24, "s_setpc_b64"),
            ]
        );
    }

    #[test]
    fn rejects_bad_code() {
        assert!(decode(&[0; 3]).is_err(), "size not a multiple of 4");
        assert!(
            decode(&bytes(&[sopp(0x02, 100)])).is_err(),
            "branch out of range"
        );
        assert!(
            decode(&bytes(&[0xBE80_03FF])).is_err(),
            "literal past the end"
        );
        assert!(
            decode(&bytes(&[0xE400_0000, 0])).is_err(),
            "unknown encoding"
        );
        // s_cbranch_scc0 into the literal of the s_mov_b32 at 4.
        assert!(decode(&bytes(&[sopp(0x04, 1), 0xBE80_03FF, S_ENDPGM, S_ENDPGM])).is_err());
    }
}
