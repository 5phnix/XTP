//! Scalars the console compiler parks in VGPR lanes, as scalar moves.
//!
//! Ported from fxceless's VIctory fork (commit 578d459): the parking VGPRs
//! found by their uses, and their saves and restores rewritten as scalar
//! moves to codes past the machine's ([`PARKED`]). Pairs restored a few
//! instructions apart are our own take on the idea of a later commit of
//! that fork (90228a7); the checks of [`unsound`] are ours.
//!
//! When it runs out of SGPRs, the console compiler saves one in a lane of
//! a spare VGPR (`v_writelane_b32 vN, sX, K`) and takes it back later
//! (`v_readlane_b32 sX, vN, K`). A VGPR whose every use is one of those two
//! forms with a constant lane is such a store: lane K of it is a scalar
//! register by another name. Both instructions ignore EXEC, as scalar moves
//! do, so a save under a partial EXEC is the move all the same. Lowered as
//! lane operations, a parked descriptor cannot be traced to its load, a
//! parked lane mask is a mask read as data, and a console wave64 run as two
//! host waves of 32 must exchange the lane (lane K is in one half only).
//! As moves, nothing cross-lane is left: both halves run them and hold the
//! scalar, a pair parked in lanes 31 and 32 included.
//!
//! A register pair parked in two neighbouring lanes (a wave64 lane mask, a
//! 64-bit address) moves as a pair (`s_mov_b64`) when every save and every
//! restore of the two lanes comes with the other half in the same block,
//! so that a parked mask comes back as the mask it is. The compiler may put
//! other instructions between the halves: a pair is saved where its second
//! half is and restored where its first half is, if nothing between writes
//! either register, touches the lanes (a restore between two restores
//! aside) or, for a restore, reads the register restored early. Otherwise
//! the halves move as two scalars. Wave32 programs have no pairs: their
//! masks are one dword.
//!
//! What a program without lane operations is lowered as, the per-lane
//! form, takes each lane through the blocks it takes, and assumes that
//! lanes do not read the scalars set in a block they skipped
//! (`emit::lower::uniform`'s notes): true of what the compiler keeps in
//! SGPRs, and of its spills, which copy them. The lane operations made a
//! program that branches per lane run as the console runs it, every lane
//! through every block, where that assumption is not needed. So that no
//! translation changes what a lane reads, a lane stays parked ([`unsound`])
//! where the value a restore gets could depend on the path a lane took:
//!
//! * read before it is written on some path (saved in a branch only some
//!   lanes take and restored after it, or a lane the VGPR came in with):
//!   the console wave ran the branch for the others, and every lane reads
//!   the saved scalar;
//! * at a join of different saved values, except at the head of a loop,
//!   where the previous iteration's value comes from a save that every
//!   lane going round runs;
//! * after a loop that defines the value restored: lanes leave a loop at
//!   different iterations, the console wave reads the last one's.
//!
//! Values are compared as what they copy (moves, joins of one value).

use std::collections::{HashMap, HashSet};

use super::{PARKED, Program, Reg, Value, effects, idoms, reverse_postorder};
use crate::gfx10::{Encoding, Fields, Instruction, NULL, Operand};

/// A lane of a VGPR: (VGPR, lane).
pub(super) type Slot = (u8, u8);

/// Parking moves SGPRs (s0-s105) only.
const LAST_SGPR: u8 = 105;
/// The last code for a parked scalar; 255 stays free, so that the code
/// after a parked one always exists.
const LAST_PARKED: u8 = 254;

/// `v_writelane_b32 vN, sX, K` (a save) or `v_readlane_b32 sX, vN, K`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Park {
    save: bool,
    vgpr: u8,
    sgpr: u8,
    lane: u8,
}

impl Park {
    fn slot(self) -> Slot {
        (self.vgpr, self.lane)
    }
}

/// A park rewritten as a scalar move: its index in the decoded list and its
/// register codes, which may be past what the machine word holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Move {
    pub at: usize,
    dst: u8,
    src: u8,
    wide: bool,
}

impl Move {
    /// Puts the move's codes into its decoded fields.
    pub(super) fn put(&self, fields: &mut Fields) {
        if let Fields::Sop {
            dst: Some(Operand::Scalar { code: to, .. }),
            src,
            ..
        } = fields
            && let Some(Operand::Scalar { code: from, .. }) = src.first_mut()
        {
            (*to, *from) = (self.dst, self.src);
        }
    }

    /// `s_mov_b32` or `s_mov_b64` (SOP1 3 and 4); s0 stands in for a code
    /// from [`PARKED`] up, which the word cannot hold ([`Move::put`]).
    fn instruction(&self, offset: usize) -> Instruction {
        let field = |code: u8| if code < PARKED { u32::from(code) } else { 0 };
        let opcode = 3 + u16::from(self.wide);
        Instruction {
            offset,
            encoding: Encoding::Sop1,
            opcode,
            words: vec![
                0xBE80_0000 | field(self.dst) << 16 | u32::from(opcode) << 8 | field(self.src),
            ],
        }
    }
}

/// What [`rewrite`] did.
#[derive(Debug, Default)]
pub(super) struct Parked {
    /// In list order.
    pub moves: Vec<Move>,
    /// Each slot rewritten, with its code.
    codes: Vec<(Slot, u8)>,
}

/// The park `inst` is, if it is one (a constant lane, no modifiers). The
/// lane select is taken modulo the wave size, as the machine takes it.
fn park(inst: &Instruction, fields: &Fields, wave64: bool) -> Option<Park> {
    let Fields::Valu(v) = fields else {
        return None;
    };
    if v.abs | v.neg | v.opsel | v.omod != 0
        || v.clamp
        || v.carry.is_some()
        || v.sdwa.is_some()
        || v.dpp.is_some()
    {
        return None;
    }
    let lane = |op: &Operand| match *op {
        Operand::Const(c @ 128..=192) => Some((c - 128) & if wave64 { 63 } else { 31 }),
        _ => None,
    };
    let sgpr = |op: Operand| match op {
        Operand::Scalar { code, dwords: 1 } if code <= LAST_SGPR => Some(code),
        _ => None,
    };
    match (inst.name()?, v.dst?, &v.src[..]) {
        ("v_writelane_b32", Operand::Vector { reg, dwords: 1 }, [s, k]) => Some(Park {
            save: true,
            vgpr: reg,
            sgpr: sgpr(*s)?,
            lane: lane(k)?,
        }),
        ("v_readlane_b32", d, [Operand::Vector { reg, dwords: 1 }, k]) => Some(Park {
            save: false,
            vgpr: *reg,
            sgpr: sgpr(d)?,
            lane: lane(k)?,
        }),
        _ => None,
    }
}

/// Rewrites, in `list`, the saves and restores of the parking VGPRs as
/// scalar moves, the lanes in `kept` aside; `leaders` are the offsets that
/// start a block. Nothing changes (no moves) if the program cannot be read
/// here, or if its control may leave it with its VGPRs (`s_setpc_b64`: the
/// next half of a merged stage).
pub(super) fn rewrite(
    list: &mut [Instruction],
    leaders: &[usize],
    wave64: bool,
    vgprs: u16,
    kept: &[Slot],
) -> Parked {
    let lanes = 1 + u8::from(wave64);
    let mut parks: Vec<Option<Park>> = Vec::with_capacity(list.len());
    // Per instruction, the scalar codes it reads and writes.
    let mut reads: Vec<Vec<u8>> = Vec::with_capacity(list.len());
    let mut writes: Vec<Vec<u8>> = Vec::with_capacity(list.len());
    // VGPRs with a use other than a park.
    let mut other = [false; 256];
    for inst in list.iter() {
        let Some(name) = inst.name() else {
            return Parked::default();
        };
        if matches!(name, "s_setpc_b64" | "s_swappc_b64" | "s_call_b64") {
            return Parked::default();
        }
        let fields = inst.fields(wave64);
        let park = park(inst, &fields, wave64);
        let Ok(effects) = effects(inst, &fields, name, lanes, vgprs) else {
            return Parked::default();
        };
        let (r, w) = effects.unwrap_or_default();
        for &reg in r.iter().chain(&w) {
            if let Reg::V(v) = reg
                && park.is_none_or(|p| p.vgpr != v)
            {
                other[usize::from(v)] = true;
            }
        }
        let scalars = |regs: &[Reg]| -> Vec<u8> {
            regs.iter()
                .filter_map(|r| match *r {
                    Reg::S(c) => Some(c),
                    _ => None,
                })
                .collect()
        };
        reads.push(scalars(&r));
        writes.push(scalars(&w));
        parks.push(park);
    }
    for p in &mut parks {
        if p.is_some_and(|p| other[usize::from(p.vgpr)] || kept.contains(&p.slot())) {
            *p = None;
        }
    }
    let mut slots: Vec<Slot> = parks.iter().flatten().map(|p| p.slot()).collect();
    slots.sort_unstable();
    slots.dedup();
    if slots.is_empty() {
        return Parked::default();
    }

    // Pairs, by their lower slot, and each paired park's other half.
    let leader = |i: usize| leaders.binary_search(&list[i].offset).is_ok();
    let mut partner: Vec<Option<usize>> = vec![None; list.len()];
    let mut pairs: Vec<Slot> = Vec::new();
    for &(vgpr, lane) in slots.iter().filter(|_| wave64) {
        let upper_of_a_pair = lane > 0 && pairs.contains(&(vgpr, lane - 1));
        if upper_of_a_pair || !slots.contains(&(vgpr, lane + 1)) {
            continue;
        }
        if let Some(found) = pair_up(&parks, &reads, &writes, leader, (vgpr, lane)) {
            for (i, j) in found {
                (partner[i], partner[j]) = (Some(j), Some(i));
            }
            pairs.push((vgpr, lane));
        }
    }

    // Codes: pairs first, then single lanes; lanes past the last code stay
    // parked.
    let mut codes: Vec<(Slot, u8)> = Vec::new();
    let mut next = PARKED;
    for &(vgpr, lane) in &pairs {
        if next < LAST_PARKED {
            codes.push(((vgpr, lane), next));
            codes.push(((vgpr, lane + 1), next + 1));
            next += 2;
        }
    }
    for &slot in &slots {
        if next <= LAST_PARKED && !codes.iter().any(|c| c.0 == slot) {
            codes.push((slot, next));
            next = next.saturating_add(1);
        }
    }
    let code_of = |slot: Slot| codes.iter().find(|c| c.0 == slot).map(|c| c.1);

    let mut moves = Vec::new();
    let mut gone = Vec::new();
    for (i, p) in parks.iter().enumerate() {
        let Some(p) = *p else { continue };
        let Some(code) = code_of(p.slot()) else {
            continue;
        };
        match partner[i] {
            // Each pair at its first park.
            Some(j) if j < i => {}
            Some(j) => {
                let q = parks[j].expect("a park");
                let sgpr = p.sgpr.min(q.sgpr);
                let Some(slot) = code_of((p.vgpr, p.lane.min(q.lane))) else {
                    continue;
                };
                // A save where its second half is saved, a restore where
                // its first half is restored.
                let (at, dst, src, nop) = if p.save {
                    (j, slot, sgpr, i)
                } else {
                    (i, sgpr, slot, j)
                };
                moves.push(Move {
                    at,
                    dst,
                    src,
                    wide: true,
                });
                gone.push(nop);
            }
            None => {
                let (dst, src) = if p.save {
                    (code, p.sgpr)
                } else {
                    (p.sgpr, code)
                };
                moves.push(Move {
                    at: i,
                    dst,
                    src,
                    wide: false,
                });
            }
        }
    }
    for m in &moves {
        list[m.at] = m.instruction(list[m.at].offset);
    }
    for &i in &gone {
        // `s_nop 0`: dropped.
        list[i] = Instruction {
            offset: list[i].offset,
            encoding: Encoding::Sopp,
            opcode: 0,
            words: vec![0xBF80_0000],
        };
    }
    moves.sort_unstable_by_key(|m| m.at);
    Parked { moves, codes }
}

/// The parks of lanes `lane` and `lane + 1` of `vgpr`, two by two: each
/// with the first park after it, in its block, of the other half of the
/// same register pair in the same direction, nothing between in the way
/// (see the module notes). `None` if one is left alone.
fn pair_up(
    parks: &[Option<Park>],
    reads: &[Vec<u8>],
    writes: &[Vec<u8>],
    leader: impl Fn(usize) -> bool,
    (vgpr, lane): Slot,
) -> Option<Vec<(usize, usize)>> {
    let half =
        |i: usize| parks[i].filter(|p| p.vgpr == vgpr && (p.lane == lane || p.lane == lane + 1));
    let mut done = vec![false; parks.len()];
    let mut found = Vec::new();
    for i in 0..parks.len() {
        let Some(p) = half(i) else { continue };
        if done[i] {
            continue;
        }
        // The other half: its lane and register.
        let (other_lane, other) = if p.lane == lane {
            (lane + 1, p.sgpr + 1)
        } else {
            (lane, p.sgpr.checked_sub(1)?)
        };
        let mut j = i + 1;
        loop {
            if j == parks.len() || leader(j) {
                return None;
            }
            if let Some(q) = half(j) {
                if q.save == p.save && q.lane == other_lane && q.sgpr == other && !done[j] {
                    break;
                }
                // Only restores may come between restores.
                if q.save || p.save {
                    return None;
                }
            }
            let written = |c: u8| writes[j].contains(&c);
            if written(p.sgpr) || written(other) || !p.save && reads[j].contains(&other) {
                return None;
            }
            j += 1;
        }
        (done[i], done[j]) = (true, true);
        found.push((i, j));
    }
    Some(found)
}

/// Where a value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Def {
    Input,
    Phi(usize, usize),
    Inst(usize, usize),
}

/// What a value holds, as far as copies show: a value, or a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Root {
    Value(Value),
    Const(u32),
}

/// Roots of values: through moves and joins of one value.
struct Roots<'a> {
    p: &'a Program,
    def: HashMap<Value, Def>,
    memo: HashMap<Value, Root>,
    busy: HashSet<Value>,
}

impl Roots<'_> {
    fn root(&mut self, v: Value) -> Root {
        if let Some(&r) = self.memo.get(&v) {
            return r;
        }
        // A cycle through joins: the value itself.
        if !self.busy.insert(v) {
            return Root::Value(v);
        }
        let p = self.p;
        let r = match self.def.get(&v).copied() {
            Some(Def::Inst(b, i)) => match copied(&p.blocks[b].insts[i], v) {
                Some(Root::Value(source)) => self.root(source),
                Some(c) => c,
                None => Root::Value(v),
            },
            Some(Def::Phi(b, k)) => {
                let mut only = None;
                let mut many = false;
                for &a in &p.blocks[b].phis[k].args {
                    let r = self.root(a);
                    if r == Root::Value(v) || only == Some(r) {
                        continue;
                    }
                    many |= only.is_some();
                    only = Some(r);
                }
                match only {
                    Some(r) if !many => r,
                    _ => Root::Value(v),
                }
            }
            _ => Root::Value(v),
        };
        self.busy.remove(&v);
        self.memo.insert(v, r);
        r
    }
}

/// What `v`, written by `inst`, copies: a dword of a moved register or an
/// inline constant (`s_mov_b32`, `s_mov_b64`).
fn copied(inst: &super::Inst, v: Value) -> Option<Root> {
    let Fields::Sop {
        dst: Some(Operand::Scalar { code: to, .. }),
        src,
        ..
    } = &inst.fields
    else {
        return None;
    };
    let wide = match inst.name() {
        "s_mov_b32" => false,
        "s_mov_b64" => true,
        _ => return None,
    };
    let k = (0..=u8::from(wide)).find(|&k| inst.write(Reg::S(to.wrapping_add(k))) == Some(v))?;
    match *src.first()? {
        Operand::Scalar { code, .. } if code != NULL => {
            inst.read(Reg::S(code.wrapping_add(k))).map(Root::Value)
        }
        Operand::Const(c @ 128..=208) => {
            let low = crate::resource::inline(c)?;
            // 64-bit inline integers are sign-extended.
            let high = if (low as i32) < 0 { u32::MAX } else { 0 };
            Some(Root::Const(if k == 0 { low } else { high }))
        }
        Operand::Literal(x) if !wide => Some(Root::Const(x)),
        _ => None,
    }
}

/// The lanes [`rewrite`] unparked into `p` that must stay parked (see the
/// module notes): read before written on some path, joined from different
/// values outside a loop head, or restored after a loop that defines them.
pub(super) fn unsound(p: &Program, parked: &Parked) -> Vec<Slot> {
    let mut bad: Vec<Slot> = Vec::new();
    let mut mark = |code: u8| {
        if let Some(&(slot, _)) = parked.codes.iter().find(|c| c.1 == code)
            && !bad.contains(&slot)
        {
            bad.push(slot);
        }
    };
    let parked_code = |reg: Reg| match reg {
        Reg::S(c) if c >= PARKED => Some(c),
        _ => None,
    };
    for &(reg, _) in &p.inputs {
        if let Some(c) = parked_code(reg) {
            mark(c);
        }
    }

    let mut def = HashMap::new();
    for &(_, v) in &p.inputs {
        def.insert(v, Def::Input);
    }
    for (b, block) in p.blocks.iter().enumerate() {
        for (k, phi) in block.phis.iter().enumerate() {
            def.insert(phi.value, Def::Phi(b, k));
        }
        for (i, inst) in block.insts.iter().enumerate() {
            for &(_, v) in &inst.writes {
                def.insert(v, Def::Inst(b, i));
            }
        }
    }
    let order = reverse_postorder(&p.blocks);
    let mut index = vec![usize::MAX; p.blocks.len()];
    for (i, &b) in order.iter().enumerate() {
        index[b] = i;
    }
    let idom = idoms(&p.blocks, &order, &index);
    let dominates = |a: usize, mut b: usize| loop {
        if b == a {
            return true;
        }
        if idom[b] == b || idom[b] == usize::MAX {
            return false;
        }
        b = idom[b];
    };
    // Each loop's blocks, by its head: what reaches a back edge's source
    // without passing the head.
    let mut loops: Vec<(usize, Vec<bool>)> = Vec::new();
    for (h, block) in p.blocks.iter().enumerate() {
        let tails: Vec<usize> = block
            .preds
            .iter()
            .copied()
            .filter(|&q| dominates(h, q))
            .collect();
        if tails.is_empty() {
            continue;
        }
        let mut inside = vec![false; p.blocks.len()];
        inside[h] = true;
        let mut stack = tails;
        while let Some(b) = stack.pop() {
            if !inside[b] {
                inside[b] = true;
                stack.extend(&p.blocks[b].preds);
            }
        }
        loops.push((h, inside));
    }
    let mut roots = Roots {
        p,
        def,
        memo: HashMap::new(),
        busy: HashSet::new(),
    };

    for (b, block) in p.blocks.iter().enumerate() {
        for phi in &block.phis {
            let Some(c) = parked_code(phi.reg) else {
                continue;
            };
            // Per edge: its source, and the root of what arrives by it.
            let edges: Vec<(usize, Value, Root)> = block
                .preds
                .iter()
                .zip(&phi.args)
                .map(|(&q, &a)| (q, a, roots.root(a)))
                .collect();
            let back = |q: usize| dominates(b, q);
            let distinct = |take: &dyn Fn(usize) -> bool| {
                let mut seen: Vec<Root> = Vec::new();
                for &(q, _, r) in &edges {
                    if take(q) && r != Root::Value(phi.value) && !seen.contains(&r) {
                        seen.push(r);
                    }
                }
                seen.len()
            };
            if distinct(&|_| true) <= 1 {
                continue;
            }
            // A loop head: one value from outside, and around each back
            // edge a save that dominates its source.
            let is_head = edges.iter().any(|e| back(e.0));
            let saved_around = edges.iter().filter(|e| back(e.0)).all(
                |&(q, a, _)| matches!(roots.def.get(&a), Some(&Def::Inst(s, _)) if dominates(s, q)),
            );
            if !is_head || !saved_around || distinct(&|q| !back(q)) > 1 {
                mark(c);
            }
        }
    }

    // Restores (nothing else reads a parked code).
    for (b, block) in p.blocks.iter().enumerate() {
        for inst in &block.insts {
            for &(reg, v) in &inst.reads {
                let Some(c) = parked_code(reg) else {
                    continue;
                };
                let at = match roots.root(v) {
                    Root::Value(r) => match roots.def.get(&r) {
                        Some(&Def::Phi(d, _) | &Def::Inst(d, _)) => d,
                        _ => continue,
                    },
                    Root::Const(_) => continue,
                };
                if loops.iter().any(|(_, inside)| inside[at] && !inside[b]) {
                    mark(c);
                }
            }
        }
    }
    bad
}

#[cfg(test)]
mod tests;
