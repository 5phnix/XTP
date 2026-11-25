//! A GFX10 assembler for tests: text as `Instruction::text` writes it (wave
//! 32), one instruction a line, into machine words.
//!
//! It knows no opcode table: for each line it tries the encodings the
//! mnemonic may take, finds the opcode by decoding, and keeps the first
//! encoding whose decoded text is the line itself. So every word it makes
//! is what the decoder, and so the IR and the play, read back. Branch
//! targets are written `@name` and defined by a line `name:`; comments start
//! with `//`.

use std::collections::HashMap;

use crate::gfx10::{Encoding, Instruction, decode};

#[derive(Debug, Clone, PartialEq)]
enum Opnd {
    /// Scalar code, dwords.
    S(u32, u32),
    V(u32, u32),
    /// Inline constant code.
    C(u32),
    Lit(u32),
    /// A hexadecimal immediate (offsets, SOPK).
    Imm(u32),
    Label(String),
    /// NSA addresses.
    List(Vec<u32>),
    Mod {
        inner: Box<Opnd>,
        abs: bool,
        neg: bool,
        sext: bool,
        /// `sel_lo` (false) or `sel_hi` (true).
        sel: Option<bool>,
    },
}

fn scalar_code(t: &str) -> Option<(u32, u32)> {
    let named = match t {
        "vcc_lo" => Some(106),
        "vcc_hi" => Some(107),
        "m0" => Some(124),
        "null" => Some(125),
        "exec_lo" => Some(126),
        "exec_hi" => Some(127),
        _ => None,
    };
    if let Some(c) = named {
        return Some((c, 1));
    }
    let rest = t.strip_prefix('s')?;
    if let Some(range) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        let (a, b) = range.split_once(':')?;
        let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
        return Some((a, b - a + 1));
    }
    Some((rest.parse().ok()?, 1))
}

fn vector_reg(t: &str) -> Option<(u32, u32)> {
    let rest = t.strip_prefix('v')?;
    if let Some(range) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        let (a, b) = range.split_once(':')?;
        let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
        return Some((a, b - a + 1));
    }
    Some((rest.parse().ok()?, 1))
}

fn operand(t: &str) -> Option<Opnd> {
    let wrap = |inner: Opnd, f: &dyn Fn(&mut Opnd)| {
        let mut m = match inner {
            m @ Opnd::Mod { .. } => m,
            inner => Opnd::Mod {
                inner: Box::new(inner),
                abs: false,
                neg: false,
                sext: false,
                sel: None,
            },
        };
        f(&mut m);
        Some(m)
    };
    for (prefix, kind) in [("abs(", 0), ("sext(", 1), ("sel_lo(", 2), ("sel_hi(", 3)] {
        if let Some(inner) = t.strip_prefix(prefix).and_then(|r| r.strip_suffix(')')) {
            return wrap(operand(inner)?, &|m| {
                if let Opnd::Mod { abs, sext, sel, .. } = m {
                    match kind {
                        0 => *abs = true,
                        1 => *sext = true,
                        2 => *sel = Some(false),
                        _ => *sel = Some(true),
                    }
                }
            });
        }
    }
    if let Some(x) = t.strip_prefix("lit(0x").and_then(|r| r.strip_suffix(')')) {
        return Some(Opnd::Lit(u32::from_str_radix(x, 16).ok()?));
    }
    if let Some(x) = t.strip_prefix("0x") {
        return Some(Opnd::Imm(u32::from_str_radix(x, 16).ok()?));
    }
    if let Some(l) = t.strip_prefix('@') {
        return Some(Opnd::Label(l.to_owned()));
    }
    if let Some(list) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']'))
        && list.starts_with('v')
        && list.contains(',')
    {
        return Some(Opnd::List(
            list.split(',')
                .map(|v| vector_reg(v).map(|r| r.0))
                .collect::<Option<_>>()?,
        ));
    }
    let floats = [
        ("0.5", 240),
        ("-0.5", 241),
        ("1.0", 242),
        ("-1.0", 243),
        ("2.0", 244),
        ("-2.0", 245),
        ("4.0", 246),
        ("-4.0", 247),
    ];
    if let Some(&(_, c)) = floats.iter().find(|f| f.0 == t) {
        return Some(Opnd::C(c));
    }
    if let Ok(n) = t.parse::<i32>() {
        return match n {
            0..=64 => Some(Opnd::C(128 + n as u32)),
            -16..=-1 => Some(Opnd::C((192 - n) as u32)),
            _ => None,
        };
    }
    if let Some(inner) = t.strip_prefix('-') {
        return wrap(operand(inner)?, &|m| {
            if let Opnd::Mod { neg, .. } = m {
                *neg = true;
            }
        });
    }
    if let Some((c, n)) = scalar_code(t) {
        return Some(Opnd::S(c, n));
    }
    vector_reg(t).map(|(r, n)| Opnd::V(r, n))
}

/// The bare operand and its modifiers.
fn parts(o: &Opnd) -> (&Opnd, bool, bool, bool, Option<bool>) {
    match o {
        Opnd::Mod {
            inner,
            abs,
            neg,
            sext,
            sel,
        } => (inner, *abs, *neg, *sext, *sel),
        o => (o, false, false, false, None),
    }
}

/// A 9-bit source field and the literal it takes.
fn src9(o: &Opnd) -> Option<(u32, Option<u32>)> {
    Some(match parts(o).0 {
        Opnd::S(c, _) if *c < 128 => (*c, None),
        Opnd::V(r, _) => (256 + r, None),
        Opnd::C(c) => (*c, None),
        Opnd::Lit(x) => (255, Some(*x)),
        _ => return None,
    })
}

fn sdst(o: &Opnd) -> Option<u32> {
    match o {
        Opnd::S(c, _) if *c < 128 => Some(*c),
        _ => None,
    }
}

fn vreg(o: &Opnd) -> Option<u32> {
    match o {
        Opnd::V(r, _) => Some(*r),
        _ => None,
    }
}

#[derive(Default)]
struct Flags {
    idxen: bool,
    offen: bool,
    offset: u32,
    offset0: u32,
    offset1: u32,
    sel: [Option<u32>; 2],
    clamp: bool,
}

fn flags(tokens: &[&str]) -> Option<Flags> {
    let mut f = Flags::default();
    let sel = |s: &str| {
        [
            "BYTE_0", "BYTE_1", "BYTE_2", "BYTE_3", "WORD_0", "WORD_1", "DWORD",
        ]
        .iter()
        .position(|x| *x == s)
        .map(|p| p as u32)
    };
    for t in tokens {
        match *t {
            "idxen" => f.idxen = true,
            "offen" => f.offen = true,
            "clamp" => f.clamp = true,
            "dmask:0xf" | "dim:SQ_RSRC_IMG_1D" | "r128" | "unorm" => {}
            _ => {
                let (k, v) = t.split_once(':')?;
                match k {
                    "offset" | "inst_offset" => f.offset = v.parse().ok()?,
                    "offset0" => f.offset0 = v.parse().ok()?,
                    "offset1" => f.offset1 = v.parse().ok()?,
                    "src0_sel" => f.sel[0] = Some(sel(v)?),
                    "src1_sel" => f.sel[1] = Some(sel(v)?),
                    _ => return None,
                }
            }
        }
    }
    Some(f)
}

/// The candidate encodings of a mnemonic.
fn encodings(name: &str) -> &'static [Encoding] {
    use Encoding::*;
    if name.starts_with("s_load") || name.starts_with("s_buffer_load") {
        &[Smem]
    } else if name.starts_with("s_") {
        &[Sop2, Sop1, Sopc, Sopk, Sopp]
    } else if name.starts_with("v_") {
        &[Vop1, Vop2, Vopc, Vop3, Vop3p]
    } else if name.starts_with("buffer_") {
        &[Mubuf]
    } else if name.starts_with("global_") {
        &[Global]
    } else if name.starts_with("ds_") {
        &[Ds]
    } else if name.starts_with("image_") {
        &[Mimg]
    } else {
        &[]
    }
}

/// The opcode field's width of an encoding.
fn opcodes(e: Encoding) -> u32 {
    use Encoding::*;
    match e {
        Sopk => 1 << 5,
        Sop2 | Sopc | Sopp => 1 << 7,
        Vop2 => 1 << 6,
        Sop1 | Smem | Vopc | Vop1 | Mubuf | Ds => 1 << 8,
        Global => 1 << 7,
        Vop3 => 1 << 10,
        Vop3p => 1 << 7,
        Mimg => 1 << 8,
        _ => 0,
    }
}

/// The first word of encoding `e` with opcode `op` and zero fields.
fn skeleton(e: Encoding, op: u32) -> Vec<u32> {
    use Encoding::*;
    match e {
        Sop2 => vec![0x8000_0000 | op << 23],
        Sopk => vec![0xB000_0000 | op << 23],
        Sop1 => vec![0xBE80_0000 | op << 8],
        Sopc => vec![0xBF00_0000 | op << 16],
        Sopp => vec![0xBF80_0000 | op << 16],
        Smem => vec![0xF400_0000 | op << 18, 0xFA00_0000],
        Vop1 => vec![0x7E00_0000 | op << 9],
        Vop2 => vec![op << 25],
        Vopc => vec![0x7C00_0000 | op << 17],
        Vop3 => vec![0xD400_0000 | op << 16, 0],
        Vop3p => vec![0xCC00_0000 | op << 16, 0],
        Mubuf => vec![0xE000_0000 | op << 18, 0],
        Global => vec![0xDC00_8000 | op << 18, 0x007D_0000],
        Ds => vec![0xD800_0000 | op << 18, 0],
        // NSA with three extra dwords, r128, unorm, dmask 0xf.
        Mimg => vec![0xF000_9F06 | (op & 0x7F) << 18 | op >> 7, 0, 0, 0, 0],
        _ => vec![],
    }
}

/// `words` decoded as one instruction at byte offset `at`, among `s_nop`s
/// that cover `reach` (a branch's target) and an `s_endpgm`.
fn one(words: &[u32], at: usize, reach: usize) -> Option<Instruction> {
    const NOP: u32 = 0xBF80_0000;
    let mut code = vec![NOP; at / 4];
    code.extend(words);
    while code.len() * 4 <= reach {
        code.push(NOP);
    }
    code.push(0xBF81_0000);
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    decode(&bytes).ok()?.into_iter().find(|i| i.offset == at)
}

/// Encodes `ops` (after the mnemonic) for opcode `op` of encoding `e`.
fn encode(
    e: Encoding,
    op: u32,
    name: &str,
    ops: &[Opnd],
    f: &Flags,
    branch: u32,
) -> Option<Vec<u32>> {
    use Encoding::*;
    let lit = |w: &mut Vec<u32>, l: Option<u32>| {
        if let Some(l) = l {
            w.push(l);
        }
    };
    let mut w = skeleton(e, op);
    match e {
        Sop2 | Sopc => {
            let (d, s) = if e == Sop2 {
                (sdst(ops.first()?)?, &ops[1..])
            } else {
                (0, ops)
            };
            let [a, b] = s else { return None };
            let ((a, la), (b, lb)) = (src9(a)?, src9(b)?);
            if a > 255 || b > 255 || la.is_some() && lb.is_some() {
                return None;
            }
            w[0] |= d << 16 | b << 8 | a;
            lit(&mut w, la.or(lb));
        }
        Sop1 => {
            let [d, s] = ops else { return None };
            let (a, la) = src9(s)?;
            if a > 255 {
                return None;
            }
            w[0] |= sdst(d)? << 16 | a;
            lit(&mut w, la);
        }
        Sopk => {
            let (d, imm) = match ops {
                [d, Opnd::Imm(i)] => (sdst(d)?, *i),
                _ => return None,
            };
            w[0] |= d << 16 | imm & 0xFFFF;
        }
        Sopp => match ops {
            [] => {}
            [Opnd::Label(_)] => w[0] |= branch & 0xFFFF,
            _ => return None,
        },
        Smem => {
            let [d, b, off] = ops else { return None };
            let (Opnd::S(d, _), Opnd::S(b, _)) = (d, b) else {
                return None;
            };
            let (soff, offset) = match off {
                Opnd::Imm(i) => (125, *i),
                o => (sdst(o)?, 0),
            };
            w[0] |= d << 6 | b >> 1;
            w[1] = soff << 25 | offset;
        }
        Vop1 | Vop2 | Vopc => {
            let (dst, srcs) = match e {
                Vopc => (0, &ops[1..]),
                _ => (
                    vreg(ops.first()?).or_else(|| sdst(ops.first()?))?,
                    &ops[1..],
                ),
            };
            let srcs = match (name, srcs) {
                ("v_cndmask_b32", [a, b, Opnd::S(106, _)]) => vec![a.clone(), b.clone()],
                _ => srcs.to_vec(),
            };
            let sdwa = f.sel.iter().any(Option::is_some)
                || srcs.iter().any(|s| {
                    let (_, abs, neg, sext, _) = parts(s);
                    abs || neg || sext
                });
            let (a, la) = src9(srcs.first()?)?;
            let b = match srcs.get(1) {
                Some(s) => {
                    let (b, _) = src9(s)?;
                    if b < 256 && !sdwa {
                        return None;
                    }
                    b
                }
                None => 256,
            };
            if srcs.len() > 2 {
                return None;
            }
            let mut src0 = a;
            if sdwa {
                src0 = 0xF9;
            }
            w[0] |= match e {
                Vop1 => dst << 17 | src0,
                Vop2 => dst << 17 | (b & 0xFF) << 9 | src0,
                _ => (b & 0xFF) << 9 | src0,
            };
            if sdwa {
                let mut x = (a & 0xFF) | 6 << 8;
                if a < 256 {
                    x |= 1 << 23;
                }
                if b < 256 {
                    x |= 1 << 31;
                }
                x |= f.sel[0].unwrap_or(6) << 16 | f.sel[1].unwrap_or(6) << 24;
                for (i, s) in srcs.iter().enumerate() {
                    let (_, abs, neg, sext, _) = parts(s);
                    let shift = if i == 0 { 16 } else { 24 };
                    x |= u32::from(sext) << (shift + 3)
                        | u32::from(neg) << (shift + 4)
                        | u32::from(abs) << (shift + 5);
                }
                if e == Vopc && dst != 0 {
                    return None;
                }
                w.push(x);
            } else {
                lit(&mut w, la);
            }
        }
        Vop3 | Vop3p => {
            let mut rest = ops;
            let d = rest.first()?;
            let dst = vreg(d).or_else(|| sdst(d))?;
            rest = &rest[1..];
            let mut carry = None;
            if name.contains("_co_") {
                carry = Some(sdst(rest.first()?)?);
                rest = &rest[1..];
            }
            let mut lits = None;
            let (mut abs, mut neg, mut opsel, mut opsel_hi) = (0, 0, 0, 0);
            for (i, s) in rest.iter().enumerate() {
                let (code, l) = src9(s)?;
                if l.is_some() {
                    if lits.is_some_and(|x| Some(x) != l) {
                        return None;
                    }
                    lits = l;
                }
                let (_, a, n, sext, sel) = parts(s);
                if sext {
                    return None;
                }
                abs |= u32::from(a) << i;
                neg |= u32::from(n) << i;
                if let Some(hi) = sel {
                    opsel_hi |= 1 << i;
                    opsel |= u32::from(hi) << i;
                }
                w[1] |= code << (9 * i);
            }
            if rest.len() > 3 {
                return None;
            }
            w[0] |= dst | u32::from(f.clamp) << 15;
            if e == Vop3 {
                if opsel != 0 || opsel_hi != 0 {
                    return None;
                }
                w[0] |= match carry {
                    Some(c) => c << 8,
                    None => abs << 8,
                };
                w[1] |= neg << 29;
            } else {
                w[0] |= abs << 8 | opsel << 11 | (opsel_hi >> 2) << 14;
                w[1] |= neg << 29 | (opsel_hi & 3) << 27;
            }
            lit(&mut w, lits);
        }
        Mubuf => {
            let [d, a, r, so] = ops else { return None };
            let (Opnd::V(d, _), Opnd::V(a, _), Opnd::S(r, _)) = (d, a, r) else {
                return None;
            };
            let (so, _) = src9(so)?;
            w[0] |= u32::from(f.idxen) << 13 | u32::from(f.offen) << 12 | f.offset;
            w[1] = so << 24 | (r >> 2) << 16 | d << 8 | a;
        }
        Global => {
            let load = name.contains("_load_");
            let (d, a, data) = match ops {
                [d, a, Opnd::S(125, _)] if load => (vreg(d)?, vreg(a)?, 0),
                [a, data, Opnd::S(125, _)] if !load => (0, vreg(a)?, vreg(data)?),
                _ => return None,
            };
            w[0] |= f.offset & 0xFFF;
            w[1] |= d << 24 | data << 8 | a;
        }
        Ds => {
            let regs: Vec<u32> = ops.iter().map(vreg).collect::<Option<_>>()?;
            let read = name.contains("read") || name.contains("_rtn");
            let (dst, rest) = if read {
                (*regs.first()?, &regs[1..])
            } else {
                (0, &regs[..])
            };
            let (addr, data) = rest.split_first()?;
            w[0] |= if f.offset0 != 0 || f.offset1 != 0 {
                f.offset1 << 8 | f.offset0
            } else {
                f.offset
            };
            w[1] = dst << 24
                | data.get(1).copied().unwrap_or(0) << 16
                | data.first().copied().unwrap_or(0) << 8
                | addr;
        }
        Mimg => {
            let [Opnd::V(d, _), Opnd::List(a), Opnd::S(r, _)] = ops else {
                return None;
            };
            if a.len() != 11 {
                return None;
            }
            w[1] = (r >> 2) << 16 | d << 8 | a[0];
            for (k, &v) in a[1..].iter().enumerate() {
                w[2 + k / 4] |= v << (8 * (k % 4));
            }
        }
        _ => return None,
    }
    Some(w)
}

/// Assembles `src` (see the module notes): its words, and the byte offset
/// of each label.
pub(crate) fn assemble(src: &str) -> (Vec<u32>, HashMap<String, usize>) {
    let lines: Vec<&str> = src
        .lines()
        .map(|l| l.split("//").next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .collect();
    let mut opcode: HashMap<(String, u8), Vec<u32>> = HashMap::new();
    let mut find = |name: &str, e: Encoding| -> Vec<u32> {
        opcode
            .entry((name.to_owned(), e as u8))
            .or_insert_with(|| {
                (0..opcodes(e))
                    .filter(|&op| {
                        let w = skeleton(e, op);
                        one(&w, 0, 0).and_then(|i| i.name()) == Some(name)
                    })
                    .collect()
            })
            .clone()
    };
    // Two passes: sizes with every branch at 0, then the branches.
    let mut labels: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::new();
    for pass in 0..2 {
        out.clear();
        for line in &lines {
            if let Some(label) = line.strip_suffix(':') {
                labels.insert(label.to_owned(), 4 * out.len());
                continue;
            }
            let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
            let mut ops: Vec<&str> = rest.split(", ").filter(|s| !s.is_empty()).collect();
            let mut extra: Vec<&str> = Vec::new();
            if let Some(last) = ops.pop() {
                let mut t = last.split(' ');
                if let Some(first) = t.next() {
                    ops.push(first);
                }
                extra.extend(t);
            }
            let parsed: Vec<Opnd> = ops
                .iter()
                .map(|t| operand(t).unwrap_or_else(|| panic!("operand {t} in `{line}`")))
                .collect();
            let f = flags(&extra).unwrap_or_else(|| panic!("flags in `{line}`"));
            let at = 4 * out.len();
            let target = parsed.iter().find_map(|o| match o {
                Opnd::Label(l) => Some(l.clone()),
                _ => None,
            });
            let (branch, expected, reach) = match &target {
                Some(l) => {
                    let to = if pass == 0 { at + 4 } else { labels[l] };
                    let imm = ((to as i64 - at as i64 - 4) / 4) as u32;
                    let text = line.replace(&format!("@{l}"), &format!("label_{to:04X}"));
                    (imm, text, to)
                }
                None => (0, (*line).to_owned(), 0),
            };
            let words = encodings(name)
                .iter()
                .flat_map(|&e| find(name, e).into_iter().map(move |op| (e, op)))
                .filter_map(|(e, op)| encode(e, op, name, &parsed, &f, branch))
                .find(|w| one(w, at, reach).is_some_and(|i| i.text(false) == expected))
                .unwrap_or_else(|| panic!("no encoding of `{line}`"));
            out.extend(words);
        }
    }
    (out, labels)
}
