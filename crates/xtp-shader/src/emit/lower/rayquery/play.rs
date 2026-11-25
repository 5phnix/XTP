//! The proof by play's machine (`docs/analysis/rt-dxr.md` section 5.3): a
//! GFX10 wave32 interpreter over the IR.
//!
//! It runs a program's blocks as the console runs one wave of it: a register
//! file (the scalar codes of `ir::Reg::S`, parked scalars included, SCC and
//! 32 lanes of each VGPR), the wave's LDS, and an [`Env`] for what lies
//! outside the wave: memory, the answers of `image_bvh_intersect_ray` and,
//! in a spliced program, the ray query's. Each instruction is decoded once
//! into an [`Op`]. One the machine has no model of fails the play where it
//! runs, not where it only stands, so that a proof never rests on a guess.
//! Floats follow the program's MODE: round to nearest even only; f32 and f16
//! denormals are kept or flushed as its denormal bits say.
//!
//! The semantics are the RDNA ISA reference's, instruction by instruction.
//! Out-of-bounds buffer reads, unaligned LDS addresses and the like are not
//! modelled: they fail the play.

use crate::gfx10::{Fields, NULL, Operand, Valu};
use crate::ir::{EXEC, Exit, Inst, Program, VCC};

use super::{Kind, Query, Why};

/// Lanes of a wave32 wave.
pub(super) const LANES: usize = 32;

/// Why a play stopped, and at which instruction (its byte offset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Fail {
    pub offset: usize,
    pub why: Why,
}

/// One lane's ray as `image_bvh_intersect_ray` takes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BvhRay {
    pub node: u32,
    pub t_max: f32,
    pub origin: [f32; 3],
    pub direction: [f32; 3],
    pub inverse: [f32; 3],
}

/// One lane's ray as a spliced ray query takes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct QueryRay {
    pub origin: [f32; 3],
    pub direction: [f32; 3],
    pub t_min: f32,
    pub t_max: f32,
    pub mask: u32,
    pub flags: u32,
}

/// What lies outside the wave.
pub(super) trait Env {
    /// The dword at byte address `at` (a multiple of 4), which `inst` reads.
    fn load(&mut self, inst: &Inst, at: u64) -> Result<u32, Why>;
    /// Lane `lane` of `inst` writes `value` at `at` (a multiple of 4).
    fn store(&mut self, inst: &Inst, lane: usize, at: u64, value: u32) -> Result<(), Why>;
    /// What `image_bvh_intersect_ray` returns (dmask 0xf, boxes sorted,
    /// triangles' i and j) for `ray` in the structure whose nodes start at
    /// `base`.
    fn intersect(&mut self, base: u64, ray: &BvhRay) -> Result<[u32; 4], Why>;
    /// What the host's query answers for lane `lane`: whether it committed
    /// a triangle hit, and the committed t (the ray's t-max on a miss).
    fn query(&mut self, lane: usize, ray: &QueryRay) -> Result<(bool, f32), Why>;
}

/// A wave's registers and LDS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Wave {
    /// Scalar codes as `ir::Reg::S` names them: SGPRs, VCC, M0, EXEC and,
    /// from `ir::PARKED` on, parked scalars.
    pub s: [u32; 256],
    pub scc: bool,
    pub v: Vec<[u32; LANES]>,
    /// The wave's LDS, in dwords.
    pub lds: Vec<u32>,
}

impl Wave {
    pub fn exec(&self) -> u32 {
        self.s[usize::from(EXEC)]
    }
}

/// The lanes of `mask`, lowest first.
pub(super) fn lanes(mask: u32) -> impl Iterator<Item = usize> {
    (0..LANES).filter(move |&l| mask >> l & 1 != 0)
}

type Int1 = fn(u32) -> u32;
type Int2 = fn(u32, u32) -> u32;
type Int3 = fn(u32, u32, u32) -> u32;
type Float1 = fn(f32) -> f32;
type Float2 = fn(f32, f32) -> f32;
type Float3 = fn(f32, f32, f32) -> f32;
/// A scalar operation: (sources, SCC) to (result, SCC if written).
type Scalar2 = fn(u32, u32, bool) -> (u32, Option<bool>);
type Scalar1 = fn(u32) -> (u32, Option<bool>);

/// An instruction as the machine runs it.
#[derive(Clone, Copy)]
enum Op {
    /// Nothing for the wave (`s_branch`: its block's exit says where).
    Nop,
    /// `s_cbranch_*`, decided at its block's end.
    Branch,
    Scalar2(Scalar2),
    Scalar1(Scalar1),
    /// `s_cmp_*`.
    ScalarCompare(fn(u32, u32) -> bool),
    /// `s_cmpk_*`, the immediate sign-extended or not.
    CompareK(fn(u32, u32) -> bool, bool),
    MoveK,
    /// `s_lshl_b64`, `s_lshr_b64`: a 64-bit source shifted.
    Shift64(fn(u64, u32) -> u64),
    Move64,
    /// `s_*_saveexec_b32`: (source, EXEC) to the new EXEC.
    SaveExec(fn(u32, u32) -> u32),
    ScalarLoad,
    Buffer {
        store: bool,
        format: bool,
    },
    Global {
        store: bool,
    },
    Lds(Lds),
    Bvh,
    Query,
    Int1(Int1),
    Int2(Int2),
    Int3(Int3),
    Float1(Float1),
    Float2(Float2),
    Float3(Float3),
    /// `v_fmac_f32`: the destination is the addend.
    Fmac,
    FmaMix,
    F32FromF16,
    IntCompare(fn(u32, u32) -> bool, bool),
    FloatCompare(fn(f32, f32) -> bool, bool),
    Cndmask,
    ReadFirstLane,
    ReadLane,
    WriteLane,
    MbcntLo,
    MbcntHi,
    AddCarry,
    Shl64,
    Shr64,
}

#[derive(Clone, Copy)]
enum Lds {
    Read,
    Read2,
    Write,
    Write2,
    Exchange,
}

fn int_compare(op: &str, signed: bool) -> Option<fn(u32, u32) -> bool> {
    Some(match (op, signed) {
        ("f", _) => |_, _| false,
        ("t", _) => |_, _| true,
        ("eq", _) => |a, b| a == b,
        ("ne" | "lg", _) => |a, b| a != b,
        ("lt", false) => |a, b| a < b,
        ("le", false) => |a, b| a <= b,
        ("gt", false) => |a, b| a > b,
        ("ge", false) => |a, b| a >= b,
        ("lt", true) => |a, b| (a as i32) < (b as i32),
        ("le", true) => |a, b| (a as i32) <= (b as i32),
        ("gt", true) => |a, b| (a as i32) > (b as i32),
        ("ge", true) => |a, b| (a as i32) >= (b as i32),
        _ => return None,
    })
}

fn float_compare(op: &str) -> Option<fn(f32, f32) -> bool> {
    Some(match op {
        "f" => |_, _| false,
        "tru" | "t" => |_, _| true,
        "lt" => |a, b| a < b,
        "eq" => |a, b| a == b,
        "le" => |a, b| a <= b,
        "gt" => |a, b| a > b,
        "lg" => |a, b| a < b || a > b,
        "ge" => |a, b| a >= b,
        "o" => |a, b| !a.is_nan() && !b.is_nan(),
        "u" => |a, b| a.is_nan() || b.is_nan(),
        "nge" => |a, b| !(a >= b),
        "nlg" => |a, b| !(a < b || a > b),
        "ngt" => |a, b| !(a > b),
        "nle" => |a, b| !(a <= b),
        "neq" => |a, b| a != b,
        "nlt" => |a, b| !(a < b),
        _ => return None,
    })
}

fn bfe_u(x: u32, offset: u32, width: u32) -> u32 {
    if width == 0 {
        0
    } else if width >= 32 {
        x >> offset
    } else {
        x >> offset & ((1 << width) - 1)
    }
}

fn bfe_i(x: u32, offset: u32, width: u32) -> u32 {
    if width == 0 {
        return 0;
    }
    let field = bfe_u(x, offset, width);
    if width >= 32 {
        field
    } else {
        ((field << (32 - width)) as i32 >> (32 - width)) as u32
    }
}

fn sext24(x: u32) -> i64 {
    i64::from((x << 8) as i32 >> 8)
}

fn med3<T: PartialOrd + Copy>(a: T, b: T, c: T) -> T {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    if c < lo {
        lo
    } else if c > hi {
        hi
    } else {
        c
    }
}

/// `inst`'s operation, or why the machine has none.
fn op(inst: &Inst) -> Result<Op, Why> {
    let name = inst.name();
    let none = "an instruction the proof by play has no model of";
    if Query::of(inst).is_some() {
        return Ok(Op::Query);
    }
    let op = match &inst.fields {
        Fields::Sopp { .. } => match name {
            "s_branch" | "s_endpgm" | "s_barrier" | "s_code_end" => Op::Nop,
            "s_cbranch_scc0" | "s_cbranch_scc1" | "s_cbranch_vccz" | "s_cbranch_vccnz"
            | "s_cbranch_execz" | "s_cbranch_execnz" => Op::Branch,
            _ => return Err(none),
        },
        Fields::Sop { imm: Some(_), .. } => match name {
            "s_movk_i32" => Op::MoveK,
            _ => {
                let (Some(rest), Some(signed)) =
                    (name.strip_prefix("s_cmpk_"), Some(name.ends_with("_i32")))
                else {
                    return Err(none);
                };
                let cmp = rest.split('_').next().unwrap_or("");
                Op::CompareK(int_compare(cmp, signed).ok_or(none)?, signed)
            }
        },
        Fields::Sop { .. } => scalar_op(name).ok_or(none)?,
        Fields::Smem { .. } => match name {
            "s_load_dword" | "s_load_dwordx2" | "s_load_dwordx4" | "s_load_dwordx8"
            | "s_load_dwordx16" => Op::ScalarLoad,
            _ => return Err(none),
        },
        Fields::Buffer { .. } => match name {
            "buffer_load_dword"
            | "buffer_load_dwordx2"
            | "buffer_load_dwordx3"
            | "buffer_load_dwordx4" => Op::Buffer {
                store: false,
                format: false,
            },
            "buffer_load_format_x" => Op::Buffer {
                store: false,
                format: true,
            },
            "buffer_store_dword"
            | "buffer_store_dwordx2"
            | "buffer_store_dwordx3"
            | "buffer_store_dwordx4" => Op::Buffer {
                store: true,
                format: false,
            },
            _ => return Err(none),
        },
        Fields::Flat { .. } => match name {
            "global_load_dword"
            | "global_load_dwordx2"
            | "global_load_dwordx3"
            | "global_load_dwordx4" => Op::Global { store: false },
            "global_store_dword"
            | "global_store_dwordx2"
            | "global_store_dwordx3"
            | "global_store_dwordx4" => Op::Global { store: true },
            _ => return Err(none),
        },
        Fields::Ds { .. } => Op::Lds(match name {
            "ds_read_b32" => Lds::Read,
            "ds_read2_b32" => Lds::Read2,
            "ds_write_b32" => Lds::Write,
            "ds_write2_b32" => Lds::Write2,
            "ds_wrxchg_rtn_b32" => Lds::Exchange,
            _ => return Err(none),
        }),
        Fields::Image { .. } if name == "image_bvh_intersect_ray" => Op::Bvh,
        Fields::Valu(_) => vector_op(name).ok_or(none)?,
        _ => return Err(none),
    };
    Ok(op)
}

/// A scalar result, and SCC set when it is not zero.
fn nz(r: u32) -> (u32, Option<bool>) {
    (r, Some(r != 0))
}

fn scalar_op(name: &str) -> Option<Op> {
    let two = |f: Scalar2| Some(Op::Scalar2(f));
    let one = |f: Scalar1| Some(Op::Scalar1(f));
    if let Some(rest) = name.strip_prefix("s_cmp_") {
        let mut parts = rest.split('_');
        let (cmp, ty) = (parts.next()?, parts.next()?);
        return Some(Op::ScalarCompare(int_compare(cmp, ty == "i32")?));
    }
    match name {
        "s_add_u32" => two(|a, b, _| {
            let (r, c) = a.overflowing_add(b);
            (r, Some(c))
        }),
        "s_addc_u32" => two(|a, b, c| {
            let r = u64::from(a) + u64::from(b) + u64::from(c);
            (r as u32, Some(r >> 32 != 0))
        }),
        "s_sub_u32" => two(|a, b, _| {
            let (r, c) = a.overflowing_sub(b);
            (r, Some(c))
        }),
        "s_subb_u32" => two(|a, b, c| {
            let r = i64::from(a) - i64::from(b) - i64::from(c);
            (r as u32, Some(r < 0))
        }),
        "s_add_i32" => two(|a, b, _| {
            let (r, o) = (a as i32).overflowing_add(b as i32);
            (r as u32, Some(o))
        }),
        "s_sub_i32" => two(|a, b, _| {
            let (r, o) = (a as i32).overflowing_sub(b as i32);
            (r as u32, Some(o))
        }),
        "s_mul_i32" => two(|a, b, _| (a.wrapping_mul(b), None)),
        "s_min_i32" => two(|a, b, _| {
            let lt = (a as i32) < (b as i32);
            (if lt { a } else { b }, Some(lt))
        }),
        "s_min_u32" => two(|a, b, _| (a.min(b), Some(a < b))),
        "s_max_i32" => two(|a, b, _| {
            let gt = (a as i32) > (b as i32);
            (if gt { a } else { b }, Some(gt))
        }),
        "s_max_u32" => two(|a, b, _| (a.max(b), Some(a > b))),
        "s_cselect_b32" => two(|a, b, scc| (if scc { a } else { b }, None)),
        "s_and_b32" => two(|a, b, _| nz(a & b)),
        "s_or_b32" => two(|a, b, _| nz(a | b)),
        "s_xor_b32" => two(|a, b, _| nz(a ^ b)),
        "s_andn2_b32" => two(|a, b, _| nz(a & !b)),
        "s_orn2_b32" => two(|a, b, _| nz(a | !b)),
        "s_nand_b32" => two(|a, b, _| nz(!(a & b))),
        "s_nor_b32" => two(|a, b, _| nz(!(a | b))),
        "s_xnor_b32" => two(|a, b, _| nz(!(a ^ b))),
        "s_lshl_b32" => two(|a, b, _| nz(a << (b & 31))),
        "s_lshr_b32" => two(|a, b, _| nz(a >> (b & 31))),
        "s_ashr_i32" => two(|a, b, _| nz(((a as i32) >> (b & 31)) as u32)),
        "s_bfm_b32" => two(|a, b, _| (bfe_u(u32::MAX, 0, a & 31) << (b & 31), None)),
        "s_bfe_u32" => two(|a, b, _| nz(bfe_u(a, b & 31, b >> 16 & 0x7F))),
        "s_bfe_i32" => two(|a, b, _| nz(bfe_i(a, b & 31, b >> 16 & 0x7F))),
        "s_lshl1_add_u32" | "s_lshl2_add_u32" | "s_lshl3_add_u32" | "s_lshl4_add_u32" => {
            let n = u32::from(name.as_bytes()[6] - b'0');
            match n {
                1 => two(|a, b, _| shl_add(a, b, 1)),
                2 => two(|a, b, _| shl_add(a, b, 2)),
                3 => two(|a, b, _| shl_add(a, b, 3)),
                _ => two(|a, b, _| shl_add(a, b, 4)),
            }
        }
        "s_mov_b32" => one(|a| (a, None)),
        "s_not_b32" => one(|a| nz(!a)),
        "s_brev_b32" => one(|a| (a.reverse_bits(), None)),
        "s_bcnt1_i32_b32" => one(|a| nz(a.count_ones())),
        "s_bcnt0_i32_b32" => one(|a| nz(a.count_zeros())),
        "s_ff1_i32_b32" => one(|a| (if a == 0 { u32::MAX } else { a.trailing_zeros() }, None)),
        "s_flbit_i32_b32" => one(|a| (if a == 0 { u32::MAX } else { a.leading_zeros() }, None)),
        "s_sext_i32_i8" => one(|a| (i32::from(a as i8) as u32, None)),
        "s_sext_i32_i16" => one(|a| (i32::from(a as i16) as u32, None)),
        "s_abs_i32" => one(|a| nz((a as i32).unsigned_abs())),
        "s_lshl_b64" => Some(Op::Shift64(|a, b| a << (b & 63))),
        "s_lshr_b64" => Some(Op::Shift64(|a, b| a >> (b & 63))),
        "s_mov_b64" => Some(Op::Move64),
        "s_and_saveexec_b32" => Some(Op::SaveExec(|s, e| s & e)),
        "s_or_saveexec_b32" => Some(Op::SaveExec(|s, e| s | e)),
        "s_andn2_saveexec_b32" => Some(Op::SaveExec(|s, e| s & !e)),
        "s_andn1_saveexec_b32" => Some(Op::SaveExec(|s, e| !s & e)),
        _ => None,
    }
}

fn shl_add(a: u32, b: u32, n: u32) -> (u32, Option<bool>) {
    let r = (u64::from(a) << n) + u64::from(b);
    (r as u32, Some(r >> 32 != 0))
}

fn vector_op(name: &str) -> Option<Op> {
    if let Some(rest) = name
        .strip_prefix("v_cmpx_")
        .map(|r| (r, true))
        .or_else(|| name.strip_prefix("v_cmp_").map(|r| (r, false)))
    {
        let ((rest, x), mut parts) = (rest, rest.0.split('_'));
        let _ = rest;
        let (cmp, ty) = (parts.next()?, parts.next()?);
        return match ty {
            "u32" | "i32" => Some(Op::IntCompare(int_compare(cmp, ty == "i32")?, x)),
            "f32" => Some(Op::FloatCompare(float_compare(cmp)?, x)),
            _ => None,
        };
    }
    let op = match name {
        "v_mov_b32" => Op::Int1(|a| a),
        "v_not_b32" => Op::Int1(|a| !a),
        "v_bfrev_b32" => Op::Int1(|a| a.reverse_bits()),
        "v_cvt_f32_u32" => Op::Int1(|a| (a as f32).to_bits()),
        "v_cvt_f32_i32" => Op::Int1(|a| (a as i32 as f32).to_bits()),
        "v_cvt_f32_ubyte0" => Op::Int1(|a| f32::from(a as u8).to_bits()),
        "v_cvt_u32_f32" => Op::Int1(|a| f32::from_bits(a) as u32),
        "v_cvt_i32_f32" => Op::Int1(|a| f32::from_bits(a) as i32 as u32),
        "v_add_nc_u32" => Op::Int2(|a, b| a.wrapping_add(b)),
        "v_sub_nc_u32" => Op::Int2(|a, b| a.wrapping_sub(b)),
        "v_subrev_nc_u32" => Op::Int2(|a, b| b.wrapping_sub(a)),
        "v_and_b32" => Op::Int2(|a, b| a & b),
        "v_or_b32" => Op::Int2(|a, b| a | b),
        "v_xor_b32" => Op::Int2(|a, b| a ^ b),
        "v_lshlrev_b32" => Op::Int2(|a, b| b << (a & 31)),
        "v_lshrrev_b32" => Op::Int2(|a, b| b >> (a & 31)),
        "v_ashrrev_i32" => Op::Int2(|a, b| ((b as i32) >> (a & 31)) as u32),
        "v_min_u32" => Op::Int2(|a, b| a.min(b)),
        "v_max_u32" => Op::Int2(|a, b| a.max(b)),
        "v_min_i32" => Op::Int2(|a, b| (a as i32).min(b as i32) as u32),
        "v_max_i32" => Op::Int2(|a, b| (a as i32).max(b as i32) as u32),
        "v_mul_i32_i24" => Op::Int2(|a, b| (sext24(a) * sext24(b)) as u32),
        "v_mul_u32_u24" => Op::Int2(|a, b| (a & 0xFF_FFFF).wrapping_mul(b & 0xFF_FFFF)),
        "v_mul_lo_u32" => Op::Int2(|a, b| a.wrapping_mul(b)),
        "v_mul_hi_u32" => Op::Int2(|a, b| ((u64::from(a) * u64::from(b)) >> 32) as u32),
        "v_bcnt_u32_b32" => Op::Int2(|a, b| a.count_ones().wrapping_add(b)),
        "v_bfm_b32" => Op::Int2(|a, b| bfe_u(u32::MAX, 0, a & 31) << (b & 31)),
        "v_bfe_u32" => Op::Int3(|a, b, c| bfe_u(a, b & 31, c & 31)),
        "v_bfe_i32" => Op::Int3(|a, b, c| bfe_i(a, b & 31, c & 31)),
        "v_bfi_b32" => Op::Int3(|a, b, c| a & b | !a & c),
        "v_lshl_add_u32" => Op::Int3(|a, b, c| (a << (b & 31)).wrapping_add(c)),
        "v_add_lshl_u32" => Op::Int3(|a, b, c| a.wrapping_add(b) << (c & 31)),
        "v_lshl_or_b32" => Op::Int3(|a, b, c| a << (b & 31) | c),
        "v_and_or_b32" => Op::Int3(|a, b, c| a & b | c),
        "v_or3_b32" => Op::Int3(|a, b, c| a | b | c),
        "v_xor3_b32" => Op::Int3(|a, b, c| a ^ b ^ c),
        "v_add3_u32" => Op::Int3(|a, b, c| a.wrapping_add(b).wrapping_add(c)),
        "v_xad_u32" => Op::Int3(|a, b, c| (a ^ b).wrapping_add(c)),
        "v_min3_u32" => Op::Int3(|a, b, c| a.min(b).min(c)),
        "v_max3_u32" => Op::Int3(|a, b, c| a.max(b).max(c)),
        "v_med3_u32" => Op::Int3(med3),
        "v_min3_i32" => Op::Int3(|a, b, c| (a as i32).min(b as i32).min(c as i32) as u32),
        "v_max3_i32" => Op::Int3(|a, b, c| (a as i32).max(b as i32).max(c as i32) as u32),
        "v_med3_i32" => Op::Int3(|a, b, c| med3(a as i32, b as i32, c as i32) as u32),
        "v_mad_u32_u24" => {
            Op::Int3(|a, b, c| (a & 0xFF_FFFF).wrapping_mul(b & 0xFF_FFFF).wrapping_add(c))
        }
        "v_mad_i32_i24" => Op::Int3(|a, b, c| ((sext24(a) * sext24(b)) as u32).wrapping_add(c)),
        "v_alignbit_b32" => {
            Op::Int3(|a, b, c| ((u64::from(a) << 32 | u64::from(b)) >> (c & 31)) as u32)
        }
        "v_rcp_f32" => Op::Float1(|a| 1.0 / a),
        "v_rsq_f32" => Op::Float1(|a| (1.0 / f64::from(a).sqrt()) as f32),
        "v_sqrt_f32" => Op::Float1(f32::sqrt),
        "v_floor_f32" => Op::Float1(f32::floor),
        "v_ceil_f32" => Op::Float1(f32::ceil),
        "v_trunc_f32" => Op::Float1(f32::trunc),
        "v_rndne_f32" => Op::Float1(f32::round_ties_even),
        "v_fract_f32" => Op::Float1(|a| (a - a.floor()).min(f32::from_bits(0x3F7F_FFFF))),
        "v_add_f32" => Op::Float2(|a, b| a + b),
        "v_sub_f32" => Op::Float2(|a, b| a - b),
        "v_subrev_f32" => Op::Float2(|a, b| b - a),
        "v_mul_f32" => Op::Float2(|a, b| a * b),
        "v_mul_legacy_f32" => Op::Float2(|a, b| if a == 0.0 || b == 0.0 { 0.0 } else { a * b }),
        "v_min_f32" => Op::Float2(f32::min),
        "v_max_f32" => Op::Float2(f32::max),
        "v_fma_f32" => Op::Float3(f32::mul_add),
        "v_min3_f32" => Op::Float3(|a, b, c| a.min(b).min(c)),
        "v_max3_f32" => Op::Float3(|a, b, c| a.max(b).max(c)),
        "v_med3_f32" => Op::Float3(med3),
        "v_fmac_f32" => Op::Fmac,
        "v_fma_mix_f32" => Op::FmaMix,
        "v_cvt_f32_f16" => Op::F32FromF16,
        "v_cndmask_b32" => Op::Cndmask,
        "v_readfirstlane_b32" => Op::ReadFirstLane,
        "v_readlane_b32" => Op::ReadLane,
        "v_writelane_b32" => Op::WriteLane,
        "v_mbcnt_lo_u32_b32" => Op::MbcntLo,
        "v_mbcnt_hi_u32_b32" => Op::MbcntHi,
        "v_add_co_u32" | "v_add_co_ci_u32" => Op::AddCarry,
        "v_lshlrev_b64" => Op::Shl64,
        "v_lshrrev_b64" => Op::Shr64,
        _ => return None,
    };
    Some(op)
}

/// An f16's value.
pub(super) fn f16_to_f32(h: u16, keep_denormals: bool) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from(h >> 10 & 0x1F);
    let mantissa = f32::from(h & 0x3FF);
    match exponent {
        0 if keep_denormals => sign * mantissa * 2f32.powi(-24),
        0 => sign * 0.0,
        31 if mantissa == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        e => sign * (1.0 + mantissa / 1024.0) * 2f32.powi(e - 15),
    }
}

/// The machine for one program.
pub(super) struct Machine<'a> {
    p: &'a Program,
    ops: Vec<Vec<Result<Op, Why>>>,
    /// MODE's float bits: f32 denormals kept on input (0x10) and output
    /// (0x20), f16's (0x40, 0x80).
    mode: u8,
}

/// A float result's modifiers: clamp, and output modifiers refused.
fn modifiers_out(v: &Valu, x: f32) -> Result<f32, Why> {
    if v.omod != 0 {
        return Err("an output modifier in the play");
    }
    Ok(if v.clamp {
        if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
    } else {
        x
    })
}

impl<'a> Machine<'a> {
    /// The machine for `p`, whose MODE float bits are `float_mode`.
    pub fn new(p: &'a Program, float_mode: u8) -> Result<Self, Why> {
        if p.wave64 {
            return Err("a wave64 program in the play");
        }
        if float_mode & 0x0F != 0 {
            return Err("a float rounding mode the play has no model of");
        }
        Ok(Machine {
            p,
            ops: p
                .blocks
                .iter()
                .map(|b| b.insts.iter().map(op).collect())
                .collect(),
            mode: float_mode,
        })
    }

    /// Runs `w` from block `b` until it ends (`None`) or is about to enter
    /// a block for which `stop` holds (`Some(that block)`), within `budget`
    /// instructions.
    pub fn run(
        &self,
        w: &mut Wave,
        env: &mut impl Env,
        mut b: usize,
        stop: impl Fn(usize) -> bool,
        budget: &mut u64,
    ) -> Result<Option<usize>, Fail> {
        loop {
            let block = &self.p.blocks[b];
            for (i, inst) in block.insts.iter().enumerate() {
                let fail = |why| Fail {
                    offset: inst.machine.offset,
                    why,
                };
                *budget = budget
                    .checked_sub(1)
                    .ok_or_else(|| fail("a play that runs past its budget"))?;
                let op = self.ops[b][i].map_err(fail)?;
                self.step(w, env, inst, op).map_err(fail)?;
            }
            let next = match block.exit {
                Exit::Jump(t) => t,
                Exit::Branch { taken, not_taken } => {
                    let inst = block.insts.last().ok_or(Fail {
                        offset: block.offset,
                        why: "a branch without its instruction",
                    })?;
                    let exec = w.exec();
                    let vcc = w.s[usize::from(VCC)];
                    let take = match inst.name() {
                        "s_cbranch_scc0" => !w.scc,
                        "s_cbranch_scc1" => w.scc,
                        "s_cbranch_vccz" => vcc == 0,
                        "s_cbranch_vccnz" => vcc != 0,
                        "s_cbranch_execz" => exec == 0,
                        "s_cbranch_execnz" => exec != 0,
                        _ => {
                            return Err(Fail {
                                offset: inst.machine.offset,
                                why: "a branch the play has no model of",
                            });
                        }
                    };
                    if take { taken } else { not_taken }
                }
                Exit::End => return Ok(None),
                Exit::Indirect => {
                    return Err(Fail {
                        offset: block.offset,
                        why: "an indirect jump in the play",
                    });
                }
            };
            if stop(next) {
                return Ok(Some(next));
            }
            b = next;
        }
    }

    fn scalar(&self, w: &Wave, op: &Operand) -> Result<u32, Why> {
        Ok(match *op {
            Operand::Scalar { code: NULL, .. } => 0,
            Operand::Scalar { code, .. } => w.s[usize::from(code)],
            Operand::Const(c) => constant(w, c)?,
            Operand::Literal(x) => x,
            Operand::Vector { .. } | Operand::Attr { .. } => {
                return Err("a vector operand where a scalar one is read");
            }
        })
    }

    fn scalar64(&self, w: &Wave, op: &Operand) -> Result<u64, Why> {
        Ok(match *op {
            Operand::Scalar { code: NULL, .. } => 0,
            Operand::Scalar { code, dwords: 2 } if code < 255 => {
                u64::from(w.s[usize::from(code)]) | u64::from(w.s[usize::from(code) + 1]) << 32
            }
            Operand::Const(c @ 128..=208) => i64::from(constant(w, c)? as i32) as u64,
            _ => return Err("a 64-bit scalar operand the play has no model of"),
        })
    }

    fn lane(&self, w: &Wave, op: &Operand, l: usize) -> Result<u32, Why> {
        match *op {
            Operand::Vector { reg, .. } => Ok(w.v[usize::from(reg)][l]),
            _ => self.scalar(w, op),
        }
    }

    fn lane64(&self, w: &Wave, op: &Operand, l: usize) -> Result<u64, Why> {
        match *op {
            Operand::Vector { reg, dwords: 2 } => {
                let r = usize::from(reg);
                Ok(u64::from(w.v[r][l]) | u64::from(w.v[r + 1][l]) << 32)
            }
            _ => self.scalar64(w, op),
        }
    }

    fn write_scalar(w: &mut Wave, op: &Operand, value: u32) -> Result<(), Why> {
        match *op {
            Operand::Scalar { code: NULL, .. } => Ok(()),
            Operand::Scalar { code, .. } => {
                w.s[usize::from(code)] = value;
                Ok(())
            }
            _ => Err("a scalar result to a non-scalar operand"),
        }
    }

    fn keep_f32(&self, input: bool) -> bool {
        self.mode & if input { 0x10 } else { 0x20 } != 0
    }

    fn flush(&self, x: f32, input: bool) -> f32 {
        if x.is_subnormal() && !self.keep_f32(input) {
            0.0f32.copysign(x)
        } else {
            x
        }
    }

    /// Source `i` of VALU `v` in lane `l`, its SDWA selection applied.
    fn src(&self, w: &Wave, v: &Valu, i: usize, l: usize) -> Result<u32, Why> {
        let op = v.src.get(i).ok_or("a missing source")?;
        let x = self.lane(w, op, l)?;
        Ok(match (&v.sdwa, i) {
            (Some(s), 0 | 1) => {
                let (sel, sext) = (s.src_sel[i], s.sext >> i & 1 != 0);
                let (shift, bits) = match sel {
                    0..=3 => (8 * u32::from(sel), 8),
                    4 | 5 => (16 * u32::from(sel - 4), 16),
                    _ => (0, 32),
                };
                if sext {
                    bfe_i(x, shift, bits)
                } else {
                    bfe_u(x, shift, bits)
                }
            }
            _ => x,
        })
    }

    /// Float source `i`: denormals per MODE, then abs and neg.
    fn fsrc(&self, w: &Wave, v: &Valu, i: usize, l: usize) -> Result<f32, Why> {
        let mut x = self.flush(f32::from_bits(self.src(w, v, i, l)?), true);
        if v.abs >> i & 1 != 0 {
            x = x.abs();
        }
        if v.neg >> i & 1 != 0 {
            x = -x;
        }
        Ok(x)
    }

    fn fout(&self, v: &Valu, x: f32) -> Result<u32, Why> {
        Ok(self.flush(modifiers_out(v, x)?, false).to_bits())
    }

    /// Writes lane `l` of a VGPR destination.
    fn write_lane(w: &mut Wave, v: &Valu, l: usize, value: u64) -> Result<(), Why> {
        match v.dst {
            Some(Operand::Vector { reg, dwords }) => {
                let r = usize::from(reg);
                w.v[r][l] = value as u32;
                if dwords == 2 {
                    w.v[r + 1][l] = (value >> 32) as u32;
                }
                Ok(())
            }
            _ => Err("a vector result without a VGPR destination"),
        }
    }

    fn step(&self, w: &mut Wave, env: &mut impl Env, inst: &Inst, op: Op) -> Result<(), Why> {
        match (&inst.fields, op) {
            (_, Op::Nop | Op::Branch) => Ok(()),
            (Fields::Sop { dst, src, imm }, _) => self.salu(w, op, dst.as_ref(), src, *imm),
            (Fields::Valu(v), Op::Query) => self.query(w, env, inst, v),
            (Fields::Valu(v), _) => self.valu(w, v, op),
            (
                Fields::Smem {
                    data,
                    base,
                    offset,
                    soffset,
                    ..
                },
                Op::ScalarLoad,
            ) => {
                let Some(Operand::Scalar { code, dwords }) = *data else {
                    return Err("a scalar load without a destination");
                };
                let at = (self.scalar64(w, base)? & 0xFFFF_FFFF_FFFF)
                    .wrapping_add(i64::from(*offset) as u64)
                    .wrapping_add(u64::from(self.scalar(w, soffset)?))
                    & !3;
                for k in 0..dwords {
                    let x = env.load(inst, at + 4 * u64::from(k))?;
                    w.s[usize::from(code + k)] = x;
                }
                Ok(())
            }
            (Fields::Buffer { .. }, Op::Buffer { store, format }) => {
                self.buffer(w, env, inst, store, format)
            }
            (
                Fields::Flat {
                    dst,
                    addr,
                    data,
                    saddr,
                    offset,
                    lds: false,
                    ..
                },
                Op::Global { store },
            ) => {
                let regs = if store { data } else { dst };
                let Some(Operand::Vector { reg, dwords }) = *regs else {
                    return Err("a global access without its data registers");
                };
                let Some(Operand::Vector { reg: a, .. }) = *addr else {
                    return Err("a global access without an address");
                };
                for l in lanes(w.exec()) {
                    let base = match saddr {
                        Some(op @ Operand::Scalar { code, .. }) if *code != NULL => self
                            .scalar64(w, op)?
                            .wrapping_add(u64::from(w.v[usize::from(a)][l])),
                        _ => {
                            let a = usize::from(a);
                            u64::from(w.v[a][l]) | u64::from(w.v[a + 1][l]) << 32
                        }
                    };
                    let at = base.wrapping_add(i64::from(*offset) as u64);
                    if at % 4 != 0 {
                        return Err("an unaligned global access in the play");
                    }
                    for k in 0..dwords {
                        let r = usize::from(reg + k);
                        let at = at + 4 * u64::from(k);
                        if store {
                            env.store(inst, l, at, w.v[r][l])?;
                        } else {
                            w.v[r][l] = env.load(inst, at)?;
                        }
                    }
                }
                Ok(())
            }
            (Fields::Ds { .. }, Op::Lds(kind)) => self.lds(w, inst, kind),
            (Fields::Image { .. }, Op::Bvh) => self.bvh(w, env, inst),
            _ => Err("an instruction the proof by play has no model of"),
        }
    }

    fn salu(
        &self,
        w: &mut Wave,
        op: Op,
        dst: Option<&Operand>,
        src: &[Operand],
        imm: Option<u16>,
    ) -> Result<(), Why> {
        let s = |k: usize| -> Result<u32, Why> {
            self.scalar(w, src.get(k).ok_or("a missing scalar source")?)
        };
        let put = |w: &mut Wave, value: u32| match dst {
            Some(d) => Self::write_scalar(w, d, value),
            None => Err("a scalar result without a destination"),
        };
        match op {
            Op::Scalar2(f) => {
                let (r, scc) = f(s(0)?, s(1)?, w.scc);
                put(w, r)?;
                if let Some(scc) = scc {
                    w.scc = scc;
                }
            }
            Op::Scalar1(f) => {
                let (r, scc) = f(s(0)?);
                put(w, r)?;
                if let Some(scc) = scc {
                    w.scc = scc;
                }
            }
            Op::ScalarCompare(f) => w.scc = f(s(0)?, s(1)?),
            Op::CompareK(f, signed) => {
                let imm = imm.ok_or("a missing immediate")?;
                let imm = if signed {
                    i32::from(imm as i16) as u32
                } else {
                    u32::from(imm)
                };
                w.scc = f(s(0)?, imm);
            }
            Op::MoveK => {
                let imm = imm.ok_or("a missing immediate")?;
                put(w, i32::from(imm as i16) as u32)?;
            }
            Op::Shift64(f) => {
                let a = self.scalar64(w, src.first().ok_or("a missing scalar source")?)?;
                let r = f(a, s(1)?);
                Self::write64(w, dst, r)?;
                w.scc = r != 0;
            }
            Op::Move64 => {
                let a = self.scalar64(w, src.first().ok_or("a missing scalar source")?)?;
                Self::write64(w, dst, a)?;
            }
            Op::SaveExec(f) => {
                let old = w.exec();
                let new = f(s(0)?, old);
                put(w, old)?;
                w.s[usize::from(EXEC)] = new;
                w.scc = new != 0;
            }
            _ => return Err("an instruction the proof by play has no model of"),
        }
        Ok(())
    }

    fn write64(w: &mut Wave, dst: Option<&Operand>, value: u64) -> Result<(), Why> {
        match dst {
            Some(&Operand::Scalar { code, dwords: 2 }) if code < 255 => {
                w.s[usize::from(code)] = value as u32;
                w.s[usize::from(code) + 1] = (value >> 32) as u32;
                Ok(())
            }
            _ => Err("a 64-bit scalar result the play has no model of"),
        }
    }

    fn valu(&self, w: &mut Wave, v: &Valu, op: Op) -> Result<(), Why> {
        if v.dpp.is_some() {
            return Err("a DPP operand in the play");
        }
        if v.sdwa.is_some_and(|s| s.dst_sel != 6) {
            return Err("an SDWA destination selection in the play");
        }
        let float = matches!(
            op,
            Op::Float1(_)
                | Op::Float2(_)
                | Op::Float3(_)
                | Op::Fmac
                | Op::FmaMix
                | Op::F32FromF16
                | Op::FloatCompare(..)
        );
        if !float && (v.neg != 0 || v.abs != 0 || v.clamp || v.omod != 0) {
            return Err("a float modifier on an integer operation in the play");
        }
        if v.opsel != 0 && !matches!(op, Op::FmaMix | Op::F32FromF16) {
            return Err("an operand half selection in the play");
        }
        let exec = w.exec();
        match op {
            Op::Int1(f) => self.each(w, v, |m, w, l| Ok(u64::from(f(m.src(w, v, 0, l)?)))),
            Op::Int2(f) => self.each(w, v, |m, w, l| {
                Ok(u64::from(f(m.src(w, v, 0, l)?, m.src(w, v, 1, l)?)))
            }),
            Op::Int3(f) => self.each(w, v, |m, w, l| {
                let (a, b, c) = (m.src(w, v, 0, l)?, m.src(w, v, 1, l)?, m.src(w, v, 2, l)?);
                Ok(u64::from(f(a, b, c)))
            }),
            Op::Float1(f) => self.each(w, v, |m, w, l| {
                Ok(u64::from(m.fout(v, f(m.fsrc(w, v, 0, l)?))?))
            }),
            Op::Float2(f) => self.each(w, v, |m, w, l| {
                let (a, b) = (m.fsrc(w, v, 0, l)?, m.fsrc(w, v, 1, l)?);
                Ok(u64::from(m.fout(v, f(a, b))?))
            }),
            Op::Float3(f) => self.each(w, v, |m, w, l| {
                let (a, b, c) = (
                    m.fsrc(w, v, 0, l)?,
                    m.fsrc(w, v, 1, l)?,
                    m.fsrc(w, v, 2, l)?,
                );
                Ok(u64::from(m.fout(v, f(a, b, c))?))
            }),
            Op::Fmac => self.each(w, v, |m, w, l| {
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("an FMAC without a VGPR destination");
                };
                let c = m.flush(f32::from_bits(w.v[usize::from(reg)][l]), true);
                let (a, b) = (m.fsrc(w, v, 0, l)?, m.fsrc(w, v, 1, l)?);
                Ok(u64::from(m.fout(v, a.mul_add(b, c))?))
            }),
            Op::FmaMix => self.each(w, v, |m, w, l| {
                let mut x = [0f32; 3];
                for (i, x) in x.iter_mut().enumerate() {
                    let op = v.src.get(i).ok_or("a missing source")?;
                    let raw = m.lane(w, op, l)?;
                    *x = if v.opsel_hi >> i & 1 != 0 {
                        if matches!(op, Operand::Const(c) if *c != 128) {
                            return Err("an f16 constant in the play");
                        }
                        let half = if v.opsel >> i & 1 != 0 {
                            raw >> 16
                        } else {
                            raw
                        };
                        f16_to_f32(half as u16, m.mode & 0x40 != 0)
                    } else {
                        m.flush(f32::from_bits(raw), true)
                    };
                    if v.neg_hi >> i & 1 != 0 {
                        *x = x.abs();
                    }
                    if v.neg >> i & 1 != 0 {
                        *x = -*x;
                    }
                }
                Ok(u64::from(m.fout(v, x[0].mul_add(x[1], x[2]))?))
            }),
            Op::F32FromF16 => self.each(w, v, |m, w, l| {
                let raw = m.src(w, v, 0, l)?;
                let half = if v.opsel & 1 != 0 { raw >> 16 } else { raw };
                let mut x = f16_to_f32(half as u16, m.mode & 0x40 != 0);
                if v.abs & 1 != 0 {
                    x = x.abs();
                }
                if v.neg & 1 != 0 {
                    x = -x;
                }
                Ok(u64::from(m.fout(v, x)?))
            }),
            Op::IntCompare(f, x) => {
                let mut mask = 0;
                for l in lanes(exec) {
                    mask |= u32::from(f(self.src(w, v, 0, l)?, self.src(w, v, 1, l)?)) << l;
                }
                self.compare(w, v, mask, x)
            }
            Op::FloatCompare(f, x) => {
                let mut mask = 0;
                for l in lanes(exec) {
                    mask |= u32::from(f(self.fsrc(w, v, 0, l)?, self.fsrc(w, v, 1, l)?)) << l;
                }
                self.compare(w, v, mask, x)
            }
            Op::Cndmask => self.each(w, v, |m, w, l| {
                let mask = m.lane(w, v.src.get(2).ok_or("a missing mask")?, l)?;
                let k = if mask >> l & 1 != 0 { 1 } else { 0 };
                if v.neg != 0 || v.abs != 0 {
                    return Err("a float modifier on a lane select in the play");
                }
                Ok(u64::from(m.src(w, v, k, l)?))
            }),
            Op::ReadFirstLane => {
                let l = lanes(exec).next().unwrap_or(0);
                let x = self.src(w, v, 0, l)?;
                Self::write_scalar(w, v.dst.as_ref().ok_or("a missing destination")?, x)
            }
            Op::ReadLane => {
                let l = (self.scalar(w, v.src.get(1).ok_or("a missing lane")?)? & 31) as usize;
                let x = self.src(w, v, 0, l)?;
                Self::write_scalar(w, v.dst.as_ref().ok_or("a missing destination")?, x)
            }
            Op::WriteLane => {
                let x = self.scalar(w, v.src.first().ok_or("a missing source")?)?;
                let l = (self.scalar(w, v.src.get(1).ok_or("a missing lane")?)? & 31) as usize;
                Self::write_lane(w, v, l, u64::from(x))
            }
            Op::MbcntLo => self.each(w, v, |m, w, l| {
                let below = (1u64 << l) as u32 - 1;
                let x = (m.src(w, v, 0, l)? & below).count_ones();
                Ok(u64::from(x.wrapping_add(m.src(w, v, 1, l)?)))
            }),
            // Wave32 lanes are all in the low half.
            Op::MbcntHi => self.each(w, v, |m, w, l| Ok(u64::from(m.src(w, v, 1, l)?))),
            Op::AddCarry => {
                let mut carries = 0;
                for l in lanes(exec) {
                    let carry_in = match v.src.get(2) {
                        Some(mask) => self.lane(w, mask, l)? >> l & 1,
                        None => 0,
                    };
                    let sum = u64::from(self.src(w, v, 0, l)?)
                        + u64::from(self.src(w, v, 1, l)?)
                        + u64::from(carry_in);
                    Self::write_lane(w, v, l, sum & 0xFFFF_FFFF)?;
                    carries |= ((sum >> 32) as u32 & 1) << l;
                }
                Self::write_scalar(w, v.carry.as_ref().ok_or("a missing carry")?, carries)
            }
            Op::Shl64 | Op::Shr64 => self.each(w, v, |m, w, l| {
                let shift = m.src(w, v, 0, l)? & 63;
                let x = m.lane64(w, v.src.get(1).ok_or("a missing source")?, l)?;
                Ok(if matches!(op, Op::Shl64) {
                    x << shift
                } else {
                    x >> shift
                })
            }),
            _ => Err("an instruction the proof by play has no model of"),
        }
    }

    /// Runs `f` in each lane of EXEC and writes its result there.
    fn each(
        &self,
        w: &mut Wave,
        v: &Valu,
        f: impl Fn(&Self, &Wave, usize) -> Result<u64, Why>,
    ) -> Result<(), Why> {
        let mut out = [0u64; LANES];
        let exec = w.exec();
        for l in lanes(exec) {
            out[l] = f(self, w, l)?;
        }
        for l in lanes(exec) {
            Self::write_lane(w, v, l, out[l])?;
        }
        Ok(())
    }

    /// A compare's lane mask: to its destination, and to EXEC for `v_cmpx`.
    fn compare(&self, w: &mut Wave, v: &Valu, mask: u32, x: bool) -> Result<(), Why> {
        Self::write_scalar(w, v.dst.as_ref().ok_or("a compare without a mask")?, mask)?;
        if x {
            w.s[usize::from(EXEC)] = mask;
        }
        Ok(())
    }

    fn buffer(
        &self,
        w: &mut Wave,
        env: &mut impl Env,
        inst: &Inst,
        store: bool,
        format: bool,
    ) -> Result<(), Why> {
        let Fields::Buffer {
            data: Some(Operand::Vector { reg, dwords }),
            addr: Operand::Vector { reg: a, .. },
            rsrc: Operand::Scalar { code, .. },
            soffset,
            offset,
            offen,
            idxen,
            format: None,
            lds: false,
            tfe: false,
            ..
        } = inst.fields
        else {
            return Err("a buffer access the play has no model of");
        };
        let d: [u32; 4] = std::array::from_fn(|k| w.s[usize::from(code) + k]);
        let base = u64::from(d[0]) | u64::from(d[1] & 0xFFFF) << 32;
        let stride = u64::from(d[1] >> 16 & 0x3FFF);
        if d[1] >> 30 != 0 || d[3] >> 23 & 1 != 0 || d[3] >> 30 != 0 {
            return Err("a buffer descriptor the play has no model of");
        }
        if format && (!matches!(d[3] >> 12 & 0x7F, 20..=22) || d[3] & 7 != 4) {
            return Err("a buffer format the play has no model of");
        }
        let records = u64::from(d[2]);
        let soffset = u64::from(self.scalar(w, &soffset)?);
        for l in lanes(w.exec()) {
            let a = usize::from(a);
            let index = if idxen { u64::from(w.v[a][l]) } else { 0 };
            let voffset = match (idxen, offen) {
                (true, true) => u64::from(w.v[a + 1][l]),
                (false, true) => u64::from(w.v[a][l]),
                _ => 0,
            };
            let offset = voffset + soffset + u64::from(offset);
            let inside = if stride != 0 && idxen {
                index < records
            } else {
                offset + 4 * u64::from(dwords) <= records
            };
            if !inside {
                return Err("an out-of-bounds buffer access in the play");
            }
            let at = base + index * stride + offset;
            if at % 4 != 0 {
                return Err("an unaligned buffer access in the play");
            }
            for k in 0..dwords {
                let r = usize::from(reg + k);
                let at = at + 4 * u64::from(k);
                if store {
                    env.store(inst, l, at, w.v[r][l])?;
                } else {
                    w.v[r][l] = env.load(inst, at)?;
                }
            }
        }
        Ok(())
    }

    fn lds(&self, w: &mut Wave, inst: &Inst, kind: Lds) -> Result<(), Why> {
        let Fields::Ds {
            dst,
            addr: Some(Operand::Vector { reg: a, .. }),
            ref data,
            offset0,
            offset1,
            gds: false,
        } = inst.fields
        else {
            return Err("an LDS access the play has no model of");
        };
        let size = w.lds.len();
        let at = |address: u32, add: u32| -> Result<usize, Why> {
            let at = address.wrapping_add(add) as usize;
            if at % 4 != 0 || at / 4 >= size {
                return Err("an LDS access out of bounds or unaligned in the play");
            }
            Ok(at / 4)
        };
        let reg = |op: Option<&Operand>| match op {
            Some(&Operand::Vector { reg, .. }) => Ok(usize::from(reg)),
            _ => Err("an LDS access without its registers"),
        };
        let single = u32::from(offset0) | u32::from(offset1) << 8;
        let (o0, o1) = (4 * u32::from(offset0), 4 * u32::from(offset1));
        for l in lanes(w.exec()) {
            let address = w.v[usize::from(a)][l];
            match kind {
                Lds::Read => {
                    let x = w.lds[at(address, single)?];
                    w.v[reg(dst.as_ref())?][l] = x;
                }
                Lds::Read2 => {
                    let (x, y) = (w.lds[at(address, o0)?], w.lds[at(address, o1)?]);
                    let r = reg(dst.as_ref())?;
                    w.v[r][l] = x;
                    w.v[r + 1][l] = y;
                }
                Lds::Write => {
                    let x = w.v[reg(data.first())?][l];
                    w.lds[at(address, single)?] = x;
                }
                Lds::Write2 => {
                    let (x, y) = (w.v[reg(data.first())?][l], w.v[reg(data.get(1))?][l]);
                    w.lds[at(address, o0)?] = x;
                    w.lds[at(address, o1)?] = y;
                }
                Lds::Exchange => {
                    let x = w.v[reg(data.first())?][l];
                    let k = at(address, single)?;
                    let old = std::mem::replace(&mut w.lds[k], x);
                    w.v[reg(dst.as_ref())?][l] = old;
                }
            }
        }
        Ok(())
    }

    fn bvh(&self, w: &mut Wave, env: &mut impl Env, inst: &Inst) -> Result<(), Why> {
        let Fields::Image {
            data: Operand::Vector { reg: d, .. },
            ref addr,
            rsrc: Operand::Scalar { code, .. },
            dmask: 0xF,
            r128: true,
            a16: false,
            d16: false,
            tfe: false,
            lwe: false,
            ..
        } = inst.fields
        else {
            return Err("a BVH intersection the play has no model of");
        };
        let regs: Vec<usize> = match addr[..] {
            [Operand::Vector { reg, dwords: 11 }] => {
                (0..11).map(|k| usize::from(reg) + k).collect()
            }
            _ => addr
                .iter()
                .map(|a| match *a {
                    Operand::Vector { reg, dwords: 1 } => Ok(usize::from(reg)),
                    _ => Err("a BVH address the play has no model of"),
                })
                .collect::<Result<_, _>>()?,
        };
        if regs.len() != 11 {
            return Err("a BVH address the play has no model of");
        }
        let r: [u32; 4] = std::array::from_fn(|k| w.s[usize::from(code) + k]);
        // Type 8 (BVH), triangles return i and j, boxes sorted.
        if r[3] >> 28 != 8 || r[3] >> 24 & 1 == 0 || r[1] >> 31 == 0 {
            return Err("a BVH descriptor the play has no model of");
        }
        let base = (u64::from(r[0]) | u64::from(r[1] & 0xFF) << 32) << 8;
        for l in lanes(w.exec()) {
            let f = |k: usize| f32::from_bits(w.v[regs[k]][l]);
            let ray = BvhRay {
                node: w.v[regs[0]][l],
                t_max: f(1),
                origin: [f(2), f(3), f(4)],
                direction: [f(5), f(6), f(7)],
                inverse: [f(8), f(9), f(10)],
            };
            let out = env.intersect(base, &ray)?;
            for (k, x) in out.into_iter().enumerate() {
                w.v[usize::from(d) + k][l] = x;
            }
        }
        Ok(())
    }

    /// A spliced walk's ray query: the host's answer in each lane of EXEC,
    /// each result by its kind (`Kind::Count` reads 0, as the lowering
    /// gives it).
    fn query(&self, w: &mut Wave, env: &mut impl Env, inst: &Inst, _: &Valu) -> Result<(), Why> {
        let q = Query::of(inst).ok_or("a ray query without its words")?;
        for l in lanes(w.exec()) {
            let f = |r: u8| f32::from_bits(w.v[usize::from(r)][l]);
            let ray = QueryRay {
                origin: q.origin.map(f),
                direction: q.direction.map(f),
                t_min: f32::from_bits(q.t_min),
                t_max: f(q.t_max),
                mask: q.mask,
                flags: q.flags,
            };
            let (hit, t) = env.query(l, &ray)?;
            for &(r, kind) in &q.results {
                w.v[usize::from(r)][l] = match kind {
                    Kind::Hit => u32::from(hit),
                    Kind::T => t.to_bits(),
                    Kind::Count => 0,
                };
            }
        }
        Ok(())
    }
}

/// An inline constant's or special source's value.
fn constant(w: &Wave, c: u8) -> Result<u32, Why> {
    Ok(match c {
        128..=192 => u32::from(c - 128),
        193..=208 => (-i32::from(c - 192)) as u32,
        240 => 0.5f32.to_bits(),
        241 => (-0.5f32).to_bits(),
        242 => 1.0f32.to_bits(),
        243 => (-1.0f32).to_bits(),
        244 => 2.0f32.to_bits(),
        245 => (-2.0f32).to_bits(),
        246 => 4.0f32.to_bits(),
        247 => (-4.0f32).to_bits(),
        248 => 0x3E22_F983,
        251 => u32::from(w.s[usize::from(VCC)] == 0),
        252 => u32::from(w.exec() == 0),
        253 => u32::from(w.scc),
        _ => return Err("a special source the play has no model of"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10::Fields;

    struct NoEnv;

    impl Env for NoEnv {
        fn load(&mut self, _: &Inst, _: u64) -> Result<u32, Why> {
            Err("no memory")
        }
        fn store(&mut self, _: &Inst, _: usize, _: u64, _: u32) -> Result<(), Why> {
            Err("no memory")
        }
        fn intersect(&mut self, _: u64, _: &BvhRay) -> Result<[u32; 4], Why> {
            Err("no structure")
        }
        fn query(&mut self, _: usize, _: &QueryRay) -> Result<(bool, f32), Why> {
            Err("no structure")
        }
    }

    fn wave() -> Wave {
        Wave {
            s: [0; 256],
            scc: false,
            v: vec![[0; LANES]; 64],
            lds: vec![0; 512],
        }
    }

    /// Runs `src`, straight-line code, on `w` with MODE float bits `mode`.
    fn run(w: &mut Wave, src: &str, mode: u8) -> Result<(), Why> {
        let (words, _) = super::super::asm::assemble(&format!("{src}\ns_endpgm"));
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, false, 64).map_err(|_| "no IR")?;
        let m = Machine::new(&p, mode)?;
        let mut budget = 1000;
        m.run(w, &mut NoEnv, 0, |_| false, &mut budget)
            .map(|_| ())
            .map_err(|f| f.why)
    }

    #[test]
    fn rayquery_play_scalar_semantics() {
        let mut w = wave();
        w.s[0] = 0xF0;
        // Bits 4..7 of 0xF0, sign-extended.
        run(&mut w, "s_bfe_i32 s1, s0, lit(0x00040004)", 0).unwrap();
        assert_eq!(w.s[1], u32::MAX);
        assert!(w.scc);
        // A carry out, then in.
        run(&mut w, "s_add_u32 s2, s0, -1\ns_addc_u32 s3, 0, 0", 0).unwrap();
        assert_eq!((w.s[2], w.s[3]), (0xEF, 1));
        // EXEC 0xff becomes 0xf0, s4 the old one.
        w.s[usize::from(EXEC)] = 0xFF;
        run(&mut w, "s_and_saveexec_b32 s4, s0", 0).unwrap();
        assert_eq!((w.s[4], w.exec(), w.scc), (0xFF, 0xF0, true));
        // SCC says the first source won.
        run(&mut w, "s_max_i32 s5, s0, -1", 0).unwrap();
        assert_eq!((w.s[5], w.scc), (0xF0, true));
        run(&mut w, "s_bcnt1_i32_b32 s6, s0\ns_brev_b32 s7, 1", 0).unwrap();
        assert_eq!((w.s[6], w.s[7]), (4, 0x8000_0000));
        run(&mut w, "s_movk_i32 s8, 0xff80\ns_cmpk_gt_i32 s0, 0x0100", 0).unwrap();
        assert_eq!((w.s[8], w.scc), ((-128i32) as u32, false));
        // A 64-bit shift.
        w.s[10] = 0x8000_0001;
        w.s[11] = 0;
        run(&mut w, "s_lshl_b64 s[12:13], s[10:11], 8", 0).unwrap();
        assert_eq!((w.s[12], w.s[13]), (0x100, 0x80));
        run(&mut w, "s_cselect_b32 s14, 1, -1", 0).unwrap();
        assert_eq!(w.s[14], 1);
    }

    #[test]
    fn rayquery_play_vector_semantics() {
        let mut w = wave();
        w.s[usize::from(EXEC)] = 0b1011;
        for l in 0..LANES {
            w.v[0][l] = l as u32;
            // f16 2.0 high, 1.0 low.
            w.v[1][l] = 0x4000_3C00;
        }
        // The lane's index, in EXEC only.
        run(&mut w, "v_mbcnt_lo_u32_b32 v2, -1, 0", 0).unwrap();
        assert_eq!(&w.v[2][..5], &[0, 1, 0, 3, 0]);
        run(&mut w, "v_cvt_f32_f16 v3, v1 src0_sel:WORD_1", 0).unwrap();
        assert_eq!(f32::from_bits(w.v[3][0]), 2.0);
        run(&mut w, "v_mul_i32_i24 v4, sext(v1), v0 src0_sel:WORD_1", 0).unwrap();
        assert_eq!(w.v[4][3], 0x4000 * 3);
        // 2.0 * 1.0 + 0 from the halves.
        run(&mut w, "v_fma_mix_f32 v7, sel_hi(v1), sel_lo(v1), 0", 0).unwrap();
        assert_eq!(f32::from_bits(w.v[7][1]), 2.0);
        // Lane 0 leaves EXEC.
        run(&mut w, "v_cmpx_ne_u32 exec_lo, 0, v0", 0).unwrap();
        assert_eq!(w.exec(), 0b1010);
        // 0 times infinity is 0.
        w.v[6] = [f32::INFINITY.to_bits(); LANES];
        run(&mut w, "v_mul_legacy_f32 v5, 0, v6", 0).unwrap();
        assert_eq!(w.v[5][1], 0);
        run(&mut w, "v_readfirstlane_b32 s9, v0", 0).unwrap();
        assert_eq!(w.s[9], 1);
        // A lane of v8 by index, ignoring EXEC.
        w.s[10] = 7;
        run(
            &mut w,
            "v_writelane_b32 v8, s10, s10\nv_readlane_b32 s11, v8, s10",
            0,
        )
        .unwrap();
        assert_eq!((w.v[8][7], w.s[11]), (7, 7));
        // A carry across lanes' halves.
        w.v[9] = [u32::MAX; LANES];
        run(
            &mut w,
            "v_add_co_u32 v10, s12, v9, 1\nv_add_co_ci_u32 v11, s13, 0, 0, s12",
            0,
        )
        .unwrap();
        assert_eq!((w.v[10][1], w.v[11][1], w.s[12]), (0, 1, 0b1010));
        run(
            &mut w,
            "v_med3_u32 v12, 5, v0, 2\nv_bfe_i32 v13, -1, 4, 3",
            0,
        )
        .unwrap();
        assert_eq!((w.v[12][1], w.v[12][3], w.v[13][1]), (2, 3, u32::MAX));
    }

    #[test]
    fn rayquery_play_flushes_f32_denormals_as_mode_says() {
        let mut w = wave();
        w.s[usize::from(EXEC)] = 1;
        w.v[0][0] = 1;
        run(&mut w, "v_add_f32 v1, 0, v0", 0).unwrap();
        assert_eq!(w.v[1][0], 0);
        run(&mut w, "v_add_f32 v1, 0, v0", 0x30).unwrap();
        assert_eq!(w.v[1][0], 1);
    }

    #[test]
    fn rayquery_play_refuses_what_it_has_no_model_of() {
        let mut w = wave();
        w.s[usize::from(EXEC)] = 1;
        assert_eq!(
            run(&mut w, "v_exp_f32 v1, v0", 0),
            Err("an instruction the proof by play has no model of")
        );
        assert!(matches!(Fields::Sopp { imm: 0 }, Fields::Sopp { .. }));
    }

    #[test]
    fn rayquery_play_converts_f16() {
        assert_eq!(f16_to_f32(0x3C00, true), 1.0);
        assert_eq!(f16_to_f32(0xC000, true), -2.0);
        assert_eq!(f16_to_f32(0x7BFF, true), 65504.0);
        assert_eq!(f16_to_f32(0x0001, true), 2f32.powi(-24));
        assert_eq!(f16_to_f32(0x0001, false), 0.0);
        assert!(f16_to_f32(0x7C00, true).is_infinite());
    }
}
