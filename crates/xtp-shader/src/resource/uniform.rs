//! Wave uniformity: whether every lane that reads a value holds the same
//! one. The console compiler builds some scalar addresses in VGPRs (a table
//! in the code at `s_getpc_b64` plus a uniform index, a loop counter) and
//! reads them back with `v_readfirstlane_b32`; when the value is uniform the
//! first lane's is every lane's, and the tracer follows the VGPR sum.
//!
//! Ported from Marvisak/VIctory#1225 (head b00573d) and #1231 (head
//! 8996aeb) by LooperSalty, with stricter rules (see "Not taken" below).
//!
//! A wrong claim does not fail: the emitter computes the address in each
//! lane from the VGPR term, where the console reads the first lane's, and
//! the lanes that differ read other dwords. So the claim must hold however
//! the emitter lowers the program: in the per-lane form
//! ([`crate::structure::structure`]: each lane follows its own path and
//! runs the vector instructions its guard lets it) and in the console flow
//! ([`crate::structure::structure_console`]: every lane runs every block
//! and EXEC guards the vector instructions). What these rules cannot prove
//! is refused.
//!
//! A value is uniform when it is:
//! - an SGPR input, a `v_readfirstlane_b32` or `v_readlane_b32` result, or
//!   the result of a scalar instruction whose reads are all uniform (its
//!   SCC too), but never a value of EXEC (below);
//! - the result of an operation of [`UNIFORM_VALU`] that every lane runs
//!   (no guard in the per-lane form, the EXEC of every lane in the console
//!   flow), that writes whole dwords from its own lane (no DPP, no SDWA
//!   destination narrower than a dword: the mesh analysis's `keeps_old`),
//!   and whose operands are all uniform. The tracer reads such a result from
//!   the instruction's
//!   operands ([`Tracer::vorigin`], [`Tracer::vadd`]), which is the value
//!   only in the lanes that ran it, so a write some lane may skip is never
//!   uniform, not even when every lane runs it or none does (a uniform
//!   condition, a uniform old value): when none does, the address is the
//!   old value's. The operands are those of the encoding (the mesh
//!   analysis's `source_regs`): an EXEC named there counts (and is not
//!   uniform, below), the EXEC that
//!   guards the instruction and the old value of the VGPRs it writes (which
//!   no lane keeps) do not. A carry-in is read lane by lane: its lane bits
//!   must be uniform ([`Tracer::mask_uniform`]);
//! - a phi whose arguments are all uniform, when no branch whose lanes may
//!   part ways sends lanes to it along different edges: not a join of the
//!   two arms of such a branch, nor a block of a loop it can leave or stay
//!   in lane by lane. The set of uniform phis is the greatest fixed point (a
//!   loop counter's own increment feeds its exit test).
//!
//! EXEC as a number holds the lanes that run where it is read: the per-lane
//! form builds it with a ballot of the lanes there, not from operands each
//! lane holds alike, so no value of EXEC is uniform here.
//!
//! A branch is uniform when its per-lane condition ([`LaneExit::Branch`])
//! is made of uniform atoms: SCC values that are uniform, and lane bits of
//! compares and carries of uniform values ([`Tracer::lane_bit_uniform`]).
//!
//! Without uniformity, a first-lane read is still followed when the VGPR
//! it reads is computed from constants and scalars only, in the read's
//! block after its last EXEC write ([`Tracer::first_lane_folded`]): the
//! first active lane ran each of those instructions, so the address is that
//! sum, and it is traced as such, with no lane's VGPR left in it. The
//! exposure passes compute their table index so (a clamp of constants)
//! under a narrowed EXEC, where the console flow forbids the uniform claim.
//!
//! Not taken from upstream, each a claim that fails in at least one
//! lowering (the tests that pin each are named):
//! - `s_cbranch_execz` and `s_cbranch_execnz` as uniform branches. They are
//!   in the console flow, where the wave takes them as one; the per-lane
//!   form branches on the lane's own EXEC bit, so a value only the lanes an
//!   `s_and_saveexec` let in write, an SGPR too, is per-lane at the join
//!   (`a_scalar_set_on_one_side_of_an_execz_skip_is_per_lane`).
//! - A guarded write as uniform when its old value is: lanes that run it
//!   hold the new value, lanes that skip it the old one
//!   (`tests::a_write_some_lane_may_skip_is_not_uniform`). Nor when its
//!   condition is uniform too, as this port first had it (b8714dd): see
//!   above (`a_write_no_lane_runs_is_not_read_from_its_operands`).
//! - A carry-in, and the mask a branch on VCC tests, as uniform when the
//!   SGPR value holding it is: an SGPR is the same in every lane, its bits
//!   are not (`a_carry_in_from_an_sgpr_mask_is_per_lane`,
//!   `a_loop_left_on_the_lane_bits_of_an_sgpr_is_per_lane`).
//! - DPP operands: a lane reads another lane's value, or keeps its old one
//!   where it has no source lane.
//!
//! The mesh lowering's analysis (`emit::lower::uniform`, its `Uniformity`)
//! asks another question (which values the lanes of a console wave, or of
//! its thread group, share, for `v_readfirstlane_b32` in mesh programs) by
//! other rules; this one shares its `keeps_old` and `source_regs`. Left for
//! later: each has its own post-dominators (immediate ones here, sets
//! there).

use std::cell::{OnceCell, RefCell};

use super::{MAX_ROUNDS, Site, Sum, Term, Tracer};
use crate::emit::lower::uniform::{keeps_old, source_regs};
use crate::gfx10::{Fields, Operand};
use crate::ir::{EXEC, Exit, Inst, Reg, Value, reverse_postorder};
use crate::structure::{Atom, Bool, Bools, LaneExit, Structured};

/// VALU operations whose result is uniform when their operands are: the
/// address arithmetic a compiler moves into VGPRs and then reads back with
/// `v_readfirstlane_b32`.
const UNIFORM_VALU: &[&str] = &[
    "v_mov_b32",
    "v_add_co_u32",
    "v_add_nc_u32",
    "v_add_co_ci_u32",
    "v_sub_co_u32",
    "v_sub_nc_u32",
    "v_lshlrev_b32",
    "v_lshlrev_b64",
    "v_lshl_add_u32",
    "v_add3_u32",
    "v_ashrrev_i32",
    "v_med3_i32",
    "v_and_b32",
    "v_or_b32",
    "v_mul_lo_u32",
];

/// Per block: the blocks a lane-divergent branch there taints.
type Regions = Vec<Option<Vec<usize>>>;

/// What the tracer learns about the uniformity of address values, on first
/// use.
pub(super) struct AddressUniformity {
    /// Per value: [`Tracer::uniform`], once known.
    memo: RefCell<Vec<Option<bool>>>,
    /// Per value: whether a phi is uniform, once settled.
    phi_ok: RefCell<Option<Vec<bool>>>,
    regions: OnceCell<Regions>,
    /// The program in the console flow, `None` where it has no such form
    /// (the emitter then never lowers it that way).
    console: OnceCell<Option<Structured>>,
}

impl AddressUniformity {
    /// For a program of `values` values.
    pub(super) fn new(values: usize) -> Self {
        AddressUniformity {
            memo: RefCell::new(vec![None; values]),
            phi_ok: RefCell::new(None),
            regions: OnceCell::new(),
            console: OnceCell::new(),
        }
    }
}

fn is_exec(reg: Reg) -> bool {
    matches!(reg, Reg::S(c) if c == EXEC || c == EXEC + 1)
}

/// The lane mask `inst` reads lane by lane as a carry-in (the third operand
/// of `v_add_co_ci_u32`): its first SGPR and dwords.
fn carry_in(inst: &Inst) -> Option<(u8, u8)> {
    let Fields::Valu(f) = &inst.fields else {
        return None;
    };
    if !inst.name().ends_with("_co_ci_u32") {
        return None;
    }
    match f.src.get(2) {
        Some(&Operand::Scalar { code, dwords }) => Some((code, dwords)),
        _ => None,
    }
}

impl Tracer<'_> {
    /// The VGPR value a `v_readfirstlane_b32` writing `v` reads, when it is
    /// uniform: the first lane's is then every lane's.
    pub(super) fn uniform_first_lane(&self, v: Value) -> Option<Value> {
        self.first_lane_source(v).filter(|&s| self.uniform(s))
    }

    /// `t`, with a first-lane read of a uniform value replaced by its source.
    pub(super) fn unlaned(&self, t: Term) -> Term {
        match t {
            Term::Value(v) => self
                .uniform_first_lane(v)
                .map_or(t, |src| self.vorigin(src)),
            Term::Const(_) => t,
        }
    }

    /// `t` for the high dword of an address: as [`Self::unlaned`], and also
    /// through a first-lane read of a VGPR its block writes after its last
    /// EXEC write, which the first active lane ran. Only the high half's
    /// shape is checked; no lowering uses its value.
    pub(super) fn unlaned_high(&self, t: Term) -> Term {
        let Term::Value(v) = t else {
            return t;
        };
        match self.first_lane_source(v) {
            Some(src) if self.uniform(src) || self.same_exec(v, src) => self.vorigin(src),
            _ => t,
        }
    }

    /// The sum the first-lane read `v` takes, when the VGPR it reads is
    /// computed from constants and scalars only ([`Self::folded`]), in the
    /// read's block after its last EXEC write: the first active lane ran
    /// each of those instructions, so the address is that sum whatever the
    /// other lanes hold, and no lowering needs a lane's VGPR for it.
    pub(super) fn first_lane_folded(&self, v: Value) -> Option<Sum> {
        let src = self.first_lane_source(v)?;
        let (b, i) = self.defs[v.0 as usize]?;
        self.folded(src, b, self.exec_written(b, i))
    }

    /// The first instruction of block `b` from which EXEC is the one
    /// instruction `i` runs with (after the last EXEC write before it).
    fn exec_written(&self, b: usize, i: usize) -> usize {
        self.p.blocks[b].insts[..i]
            .iter()
            .rposition(|x| x.writes.iter().any(|w| is_exec(w.0)))
            .map_or(0, |k| k + 1)
    }

    /// Whether `src` is written in the block of `v`'s definition, before
    /// it, with the EXEC it runs with.
    fn same_exec(&self, v: Value, src: Value) -> bool {
        match (self.defs[v.0 as usize], self.defs[src.0 as usize]) {
            (Some((b, i)), Some((sb, si))) => sb == b && si < i && si >= self.exec_written(b, i),
            _ => false,
        }
    }

    /// The sum VGPR value `v` holds in the lanes that ran the instructions
    /// computing it: written in block `b` from instruction `from` on, from
    /// constants and scalars, by additions (a base or a scalar term may
    /// stay) and by the other integer operations of [`UNIFORM_VALU`] on
    /// constants.
    fn folded(&self, v: Value, b: usize, from: usize) -> Option<Sum> {
        let (db, di) = self.defs[v.0 as usize]?;
        if db != b || di < from {
            return None;
        }
        let inst = self.def(v)?;
        let Fields::Valu(f) = &inst.fields else {
            return None;
        };
        if f.dpp.is_some()
            || f.sdwa.is_some()
            || f.neg != 0
            || f.abs != 0
            || f.opsel != 0
            || f.clamp
            || f.omod != 0
        {
            return None;
        }
        let Some(Operand::Vector { reg, .. }) = f.dst else {
            return None;
        };
        // The dword of the result `v` is.
        let k = (0..2).find(|&k| inst.write(Reg::V(reg + k)) == Some(v))?;
        let sum = |op: &Operand| -> Option<Sum> {
            match *op {
                Operand::Vector { reg, dwords: 1 } => self.folded(inst.read(Reg::V(reg))?, b, from),
                Operand::Scalar { .. } | Operand::Const(_) | Operand::Literal(_) => {
                    Some(self.sum(self.term(inst, op)?))
                }
                _ => None,
            }
        };
        let constant = |op: &Operand| {
            sum(op).and_then(|s| (s.base.is_none() && s.dynamic.is_none()).then_some(s.offset))
        };
        let src = |n: usize| f.src.get(n);
        let name = inst.name();
        if matches!(name, "v_add_co_u32" | "v_add_nc_u32") && k == 0 {
            let (x, y) = (sum(src(0)?)?, sum(src(1)?)?);
            if x.base.is_some() && y.base.is_some() || x.dynamic.is_some() && y.dynamic.is_some() {
                return None;
            }
            return Some(Sum {
                base: x.base.or(y.base),
                offset: x.offset.wrapping_add(y.offset),
                dynamic: x.dynamic.or(y.dynamic),
            });
        }
        let c = |n: usize| src(n).and_then(constant);
        let value = match (name, k) {
            ("v_mov_b32", 0) => c(0)?,
            ("v_sub_co_u32" | "v_sub_nc_u32", 0) => c(0)?.wrapping_sub(c(1)?),
            ("v_lshlrev_b32", 0) => c(1)?.wrapping_shl(c(0)? & 31),
            ("v_lshl_add_u32", 0) => c(0)?.wrapping_shl(c(1)? & 31).wrapping_add(c(2)?),
            ("v_add3_u32", 0) => c(0)?.wrapping_add(c(1)?).wrapping_add(c(2)?),
            ("v_ashrrev_i32", 0) => ((c(1)? as i32) >> (c(0)? & 31)) as u32,
            ("v_med3_i32", 0) => {
                let mut x = [c(0)? as i32, c(1)? as i32, c(2)? as i32];
                x.sort_unstable();
                x[1] as u32
            }
            ("v_and_b32", 0) => c(0)? & c(1)?,
            ("v_or_b32", 0) => c(0)? | c(1)?,
            ("v_mul_lo_u32", 0) => c(0)?.wrapping_mul(c(1)?),
            ("v_lshlrev_b64", k) => {
                let Some(&Operand::Vector { reg: r, dwords: 2 }) = src(1) else {
                    return None;
                };
                let dword = |n| {
                    let s = self.folded(inst.read(Reg::V(r + n))?, b, from)?;
                    (s.base.is_none() && s.dynamic.is_none()).then_some(u64::from(s.offset))
                };
                let x = (dword(0)? | dword(1)? << 32) << (c(0)? & 63);
                (x >> (32 * u32::from(k))) as u32
            }
            _ => return None,
        };
        Some(Sum {
            base: None,
            offset: value,
            dynamic: None,
        })
    }

    /// Whether every lane that reads `v` holds the same value (module
    /// documentation).
    fn uniform(&self, v: Value) -> bool {
        let memo = self.uniformity.memo.borrow()[v.0 as usize];
        if let Some(known) = memo {
            return known;
        }
        let known = self.uniform_now(v);
        self.uniformity.memo.borrow_mut()[v.0 as usize] = Some(known);
        known
    }

    fn uniform_now(&self, v: Value) -> bool {
        let n = v.0 as usize;
        if let Some(reg) = self.inputs[n] {
            return matches!(reg, Reg::S(_)) && !is_exec(reg);
        }
        if self.phis[n].is_some() {
            return self.phi_uniform(v);
        }
        let (Some(site), Some(inst)) = (self.defs[n], self.def(v)) else {
            return false;
        };
        if inst.writes.iter().any(|&(r, w)| w == v && is_exec(r)) {
            return false;
        }
        if matches!(inst.name(), "v_readfirstlane_b32" | "v_readlane_b32") {
            return true;
        }
        match &inst.fields {
            Fields::Sop { .. } => inst.reads.iter().all(|&(_, x)| self.uniform(x)),
            Fields::Valu(_) if !keeps_old(inst) && UNIFORM_VALU.contains(&inst.name()) => {
                self.valu_uniform(site, inst)
            }
            _ => false,
        }
    }

    /// Whether the VALU instruction `inst` at `site` writes the same value
    /// in every lane, in both forms of the program: every lane runs it, from
    /// uniform operands. The console flow, which is built on first use, is
    /// checked last.
    fn valu_uniform(&self, (b, i): Site, inst: &Inst) -> bool {
        let Some(st) = self.lanes else {
            return false;
        };
        st.guards[b][i].is_none()
            && self.reads_uniform(inst)
            && self
                .console()
                .is_none_or(|cs| cs.execs[b][i] == Some(Bool::TRUE))
    }

    /// Whether the operands of the VALU instruction `inst` (`source_regs`)
    /// are uniform, a carry-in by its lane bits. Its other reads, EXEC as its
    /// guard and the old value of the VGPRs it writes, do not count: the
    /// callers take instructions every lane runs whole, or lane bits that do
    /// not depend on them.
    fn reads_uniform(&self, inst: &Inst) -> bool {
        if !matches!(inst.fields, Fields::Valu(_)) {
            return false;
        }
        let sources = source_regs(inst, if self.p.wave64 { 2 } else { 1 });
        let carry = carry_in(inst);
        if let Some((code, dwords)) = carry {
            let lo = inst.read(Reg::S(code));
            let hi = (dwords > 1).then(|| inst.read(Reg::S(code + 1))).flatten();
            if !self.mask_uniform(lo, hi) {
                return false;
            }
        }
        let carried = |r: Reg| match (r, carry) {
            (Reg::S(c), Some((code, n))) => (code..code + n).contains(&c),
            _ => false,
        };
        // Without a list of operands (`v_movrel*`), every read counts.
        let operand = |r: Reg| sources.as_ref().is_none_or(|s| s.contains(&r));
        inst.reads
            .iter()
            .all(|&(reg, x)| !operand(reg) || carried(reg) || self.uniform(x))
    }

    /// Whether the lane mask `lo` (with `hi` in wave64) has the same bit in
    /// every lane that reads it, in both forms of the program.
    fn mask_uniform(&self, lo: Option<Value>, hi: Option<Value>) -> bool {
        let (Some(lo), Some(st)) = (lo, self.lanes) else {
            return false;
        };
        if self.p.wave64 && hi.is_none() {
            return false;
        }
        let bits = |s: &Structured| match hi {
            Some(_) => s.mask_bits(lo, hi),
            None => s.mask_bits(lo, None).or_else(|| s.others.get(&lo).copied()),
        };
        let uniform_in = |s: &Structured| bits(s).is_some_and(|m| self.bool_uniform(&s.bools, m));
        uniform_in(st) && self.console().is_none_or(uniform_in)
    }

    /// Whether the per-lane boolean `b` is the same in every lane: made of
    /// uniform atoms only.
    fn bool_uniform(&self, bools: &Bools, b: Bool) -> bool {
        bools.support(b).into_iter().all(|a| match a {
            Atom::Uniform(scc) => self.uniform(scc),
            Atom::Lane(v) => self.lane_bit_uniform(v),
            _ => false,
        })
    }

    /// Whether the lane's bit of the compare or carry result `v`
    /// ([`Atom::Lane`]) is the same in every lane: a compare or carry of
    /// uniform values. The bit is the one before EXEC masking, so neither the
    /// instruction's guard nor the old value of a VGPR it writes counts.
    fn lane_bit_uniform(&self, v: Value) -> bool {
        let Some(inst) = self.def(v) else {
            return false;
        };
        let Fields::Valu(f) = &inst.fields else {
            return false;
        };
        let name = inst.name();
        let carry_out = UNIFORM_VALU.contains(&name) && f.carry.is_some();
        (name.starts_with("v_cmp") || carry_out) && f.dpp.is_none() && self.reads_uniform(inst)
    }

    /// The program in the console flow, made on first use.
    fn console(&self) -> Option<&Structured> {
        self.uniformity
            .console
            .get_or_init(|| crate::structure::structure_console(self.p).ok())
            .as_ref()
    }

    /// Whether the phi `v` is the same in every lane that reads it.
    fn phi_uniform(&self, v: Value) -> bool {
        if self.lanes.is_none() {
            return false;
        }
        if self.uniformity.phi_ok.borrow().is_none() {
            self.settle_phis();
        }
        self.uniformity
            .phi_ok
            .borrow()
            .as_ref()
            .is_some_and(|ok| ok[v.0 as usize])
    }

    /// Greatest fixed point: every phi uniform until an argument or a
    /// divergent branch says otherwise.
    fn settle_phis(&self) {
        let u = &self.uniformity;
        let mut ok: Vec<bool> = self.phis.iter().map(Option::is_some).collect();
        *u.phi_ok.borrow_mut() = Some(ok.clone());
        let regions = self.regions();
        let mut settled = false;
        for _ in 0..MAX_ROUNDS {
            u.memo.borrow_mut().fill(None);
            let mut tainted = vec![false; self.p.blocks.len()];
            for (b, region) in regions.iter().enumerate() {
                if let Some(region) = region
                    && !self.branch_uniform(b)
                {
                    for &x in region {
                        tainted[x] = true;
                    }
                }
            }
            let mut changed = false;
            for (b, block) in self.p.blocks.iter().enumerate() {
                for phi in &block.phis {
                    let i = phi.value.0 as usize;
                    if ok[i] && (tainted[b] || !phi.args.iter().all(|&a| self.uniform(a))) {
                        ok[i] = false;
                        changed = true;
                    }
                }
            }
            *u.phi_ok.borrow_mut() = Some(ok.clone());
            if !changed {
                settled = true;
                break;
            }
        }
        if !settled {
            *u.phi_ok.borrow_mut() = Some(vec![false; ok.len()]);
        }
        u.memo.borrow_mut().fill(None);
    }

    /// Whether every lane that reaches the closing branch of block `b` takes
    /// it the same way.
    fn branch_uniform(&self, b: usize) -> bool {
        let Some(st) = self.lanes else {
            return false;
        };
        match st.exits[b] {
            LaneExit::Branch { when, .. } => self.bool_uniform(&st.bools, when),
            LaneExit::Jump(_) | LaneExit::End => true,
            LaneExit::Indirect => false,
        }
    }

    /// The blocks tainted by the branch closing each block, were it divergent.
    fn regions(&self) -> &Regions {
        self.uniformity.regions.get_or_init(|| {
            let blocks = &self.p.blocks;
            let n = blocks.len();
            let mut fwd = vec![false; n];
            for b in reverse_postorder(blocks) {
                fwd[b] = true;
            }
            let succs: Vec<Vec<usize>> = blocks
                .iter()
                .map(|b| match b.exit {
                    Exit::Jump(t) => vec![t],
                    Exit::Branch { taken, not_taken } => vec![taken, not_taken],
                    Exit::End => vec![n],
                    Exit::Indirect => Vec::new(),
                })
                .collect();
            let ipdom = postdominators(n, &succs, &fwd);
            let walk = |from: usize, stop: usize, next: &dyn Fn(usize) -> Vec<usize>| {
                let mut seen = vec![false; n];
                let mut stack = vec![from];
                while let Some(x) = stack.pop() {
                    if x >= n || seen[x] {
                        continue;
                    }
                    seen[x] = true;
                    if x != stop {
                        stack.extend(next(x));
                    }
                }
                seen
            };
            let forward = |x: usize| succs[x].clone();
            let backward = |x: usize| blocks[x].preds.clone();
            (0..n)
                .map(|b| {
                    let Exit::Branch { taken, not_taken } = blocks[b].exit else {
                        return None;
                    };
                    if !fwd[b] || taken == not_taken {
                        return None;
                    }
                    // The joins of its two arms, up to where they meet.
                    let stop = ipdom[b];
                    let (r1, r2) = (walk(taken, stop, &forward), walk(not_taken, stop, &forward));
                    // Its loop: the blocks it reaches that reach it back.
                    let (d1, d2) = (
                        walk(taken, usize::MAX, &forward),
                        walk(not_taken, usize::MAX, &forward),
                    );
                    let up = walk(b, usize::MAX, &backward);
                    Some(
                        (0..n)
                            .filter(|&x| (r1[x] && r2[x]) || ((d1[x] || d2[x]) && up[x]))
                            .collect(),
                    )
                })
                .collect()
        })
    }
}

/// The immediate post-dominator of each block, `n` being the virtual exit
/// every `End` block leads to (Cooper, Harvey and Kennedy's iteration on the
/// reverse graph). `usize::MAX` for blocks that never reach the exit, and
/// those `fwd` leaves out (unreachable).
fn postdominators(n: usize, succs: &[Vec<usize>], fwd: &[bool]) -> Vec<usize> {
    let mut rev_succs = vec![Vec::new(); n + 1];
    for b in 0..n {
        if !fwd[b] {
            continue;
        }
        for &s in &succs[b] {
            rev_succs[s].push(b);
        }
    }
    // Postorder of the reverse graph from the exit, then reversed.
    let mut post = Vec::with_capacity(n + 1);
    let mut seen = vec![false; n + 1];
    let mut stack = vec![(n, 0usize)];
    seen[n] = true;
    while let Some((node, idx)) = stack.last_mut() {
        let u = *node;
        if *idx < rev_succs[u].len() {
            let v = rev_succs[u][*idx];
            *idx += 1;
            if !seen[v] {
                seen[v] = true;
                stack.push((v, 0));
            }
        } else {
            post.push(u);
            stack.pop();
        }
    }
    post.reverse();
    let mut rev_idx = vec![usize::MAX; n + 1];
    for (idx, &b) in post.iter().enumerate() {
        rev_idx[b] = idx;
    }
    let mut ipdom = vec![usize::MAX; n + 1];
    ipdom[n] = n;
    let intersect = |mut a: usize, mut b: usize, ipdom: &[usize]| -> usize {
        while a != b {
            while rev_idx[a] > rev_idx[b] {
                a = ipdom[a];
            }
            while rev_idx[b] > rev_idx[a] {
                b = ipdom[b];
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &post[1..] {
            let mut new = usize::MAX;
            for &s in &succs[b] {
                if ipdom[s] != usize::MAX {
                    new = if new == usize::MAX {
                        s
                    } else {
                        intersect(s, new, &ipdom)
                    };
                }
            }
            if new != usize::MAX && ipdom[b] != new {
                ipdom[b] = new;
                changed = true;
            }
        }
    }
    ipdom
}

#[cfg(test)]
mod tests {
    use super::super::Tracer;
    use crate::ir::Reg;
    use crate::native::HwStage;

    /// The guarded-write rule alone, in the per-lane form (the program as
    /// if it had no console flow, so that rule cannot refuse it instead):
    /// v7 is written under EXEC narrowed by `compare`, with no branch, so
    /// lanes outside it keep the old v7. With a per-lane compare, lanes 0
    /// to 4 hold 12 and the others 0 (upstream took the write as uniform
    /// because its old value is); with a uniform one, every lane holds 12
    /// or every lane 0, which the tracer cannot tell apart from the
    /// operands (b8714dd took it as uniform).
    #[test]
    fn a_write_some_lane_may_skip_is_not_uniform() {
        for (compare, what) in [
            (0x7D88_0085, "v_cmp_gt_u32 vcc_lo, 5, v0: lanes 0 to 4"),
            (
                0x7D84_1280,
                "v_cmp_eq_u32 vcc_lo, 0, v9: every lane or none",
            ),
        ] {
            let words: [u32; 7] = [
                0x7E0E_0280, // v_mov_b32 v7, 0
                0x7E12_0205, // v_mov_b32 v9, s5
                compare,
                0xBE88_3C6A, // s_and_saveexec_b32 s8, vcc_lo
                0x7E0E_028C, // v_mov_b32 v7, 12
                0x887E_087E, // s_or_b32 exec_lo, exec_lo, s8
                0xBF81_0000, // s_endpgm
            ];
            let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            let p = crate::ir::build(&code, false, 256).unwrap();
            let st = crate::structure::structure(&p).unwrap();
            let t = Tracer::new(&p, &[], HwStage::Cs, Some(&st)).unwrap();
            t.uniformity.console.set(None).unwrap();
            let insts = &p.blocks[0].insts;
            let v7 = |i: usize| insts[i].write(Reg::V(7)).unwrap();
            assert!(st.guards[0][4].is_some(), "{what}: the move is guarded");
            assert!(t.uniform(v7(0)), "{what}: the old value is uniform");
            assert!(!t.uniform(v7(4)), "{what}");
        }
    }
}
