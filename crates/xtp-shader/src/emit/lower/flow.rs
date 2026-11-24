//! Control flow: each block's exit (branches, loops and their iteration
//! limit, the end of the program), the blocks that split edges, and the
//! phis, filled once every block is lowered.

use std::collections::{HashMap, HashSet};

use super::{Lower, Stop, Val, Why, unsupported};
use crate::emit::bitcode::{Attr, Bin, Term, V, pred};
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{EXEC, Exit, Inst, Program, Reg, Value};
use crate::structure::{Atom, Bool, LaneExit, Structured};

/// Headers of the loops a lane leaves once an induction variable passes a
/// value the loop does not change: a header phi that every iteration adds
/// the same constant to, compared with a constant or a loop-invariant
/// value by a test the lane passes every iteration (at the header, or at
/// the loop's only latch), and staying in the loop only while the variable
/// has not passed that value (`i < n` going up, `i > n` going down, `i != n`
/// by steps of 1, the form compilers give `i < n`). Their iteration count
/// is the program's own, not data the host may not have mirrored, so they
/// keep the higher limit ([`crate::emit::counted_loop_limit`]).
// shortcut: the bound itself may come from such data (a count in a buffer
// the host has not written), and then the loop runs to the higher limit.
pub(super) fn counted_loops(p: &Program, st: &Structured) -> HashSet<usize> {
    let order = crate::ir::reverse_postorder(&p.blocks);
    let mut rpo = vec![usize::MAX; p.blocks.len()];
    for (n, &b) in order.iter().enumerate() {
        rpo[b] = n;
    }
    let mut found = HashSet::new();
    for &h in &order {
        let latches: Vec<usize> = p.blocks[h]
            .preds
            .iter()
            .copied()
            .filter(|&q| rpo[q] != usize::MAX && rpo[h] <= rpo[q])
            .collect();
        if latches.is_empty() {
            continue;
        }
        let l = Counted::new(p, h, &latches, &rpo);
        let mut tests = vec![h];
        if let [latch] = latches[..]
            && latch != h
        {
            tests.push(latch);
        }
        if tests.into_iter().any(|b| l.exit_counts(st, b)) {
            found.insert(h);
        }
    }
    found
}

/// A compare's test of its first operand against its second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Test {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl Test {
    fn parse(s: &str) -> Option<Test> {
        Some(match s {
            "lt" => Test::Lt,
            "le" => Test::Le,
            "gt" => Test::Gt,
            "ge" => Test::Ge,
            "eq" => Test::Eq,
            "ne" | "lg" => Test::Ne,
            _ => return None,
        })
    }

    /// The same test with the operands swapped.
    fn swapped(self) -> Test {
        match self {
            Test::Lt => Test::Gt,
            Test::Le => Test::Ge,
            Test::Gt => Test::Lt,
            Test::Ge => Test::Le,
            t => t,
        }
    }

    fn negated(self) -> Test {
        match self {
            Test::Lt => Test::Ge,
            Test::Le => Test::Gt,
            Test::Gt => Test::Le,
            Test::Ge => Test::Lt,
            Test::Eq => Test::Ne,
            Test::Ne => Test::Eq,
        }
    }
}

/// An integer operand: a value or a constant.
#[derive(Clone, Copy)]
enum Term32 {
    Value(Value),
    Const(i64),
}

fn term(inst: &Inst, op: &Operand) -> Option<Term32> {
    match *op {
        Operand::Scalar { code, .. } if code != NULL => inst.read(Reg::S(code)).map(Term32::Value),
        Operand::Vector { reg, .. } => inst.read(Reg::V(reg)).map(Term32::Value),
        Operand::Const(c @ 128..=208) => {
            crate::resource::inline(c).map(|x| Term32::Const(i64::from(x as i32)))
        }
        Operand::Literal(x) => Some(Term32::Const(i64::from(x as i32))),
        _ => None,
    }
}

/// A loop for [`counted_loops`].
struct Counted<'a> {
    p: &'a Program,
    header: usize,
    body: HashSet<usize>,
    /// Block and instruction (none: a phi) of each value the loop writes.
    defs: HashMap<Value, (usize, Option<usize>)>,
}

/// How far [`Counted::resolve`] follows phis.
const PHI_DEPTH: u32 = 8;

impl<'a> Counted<'a> {
    fn new(p: &'a Program, header: usize, latches: &[usize], rpo: &[usize]) -> Self {
        let mut body = HashSet::from([header]);
        let mut work = latches.to_vec();
        while let Some(b) = work.pop() {
            if body.insert(b) {
                work.extend(p.blocks[b].preds.iter().filter(|&&q| rpo[q] != usize::MAX));
            }
        }
        let mut defs = HashMap::new();
        for &b in &body {
            for phi in &p.blocks[b].phis {
                defs.insert(phi.value, (b, None));
            }
            for (i, inst) in p.blocks[b].insts.iter().enumerate() {
                for &(_, v) in &inst.writes {
                    defs.insert(v, (b, Some(i)));
                }
            }
        }
        Counted {
            p,
            header,
            body,
            defs,
        }
    }

    /// `v`, or the one value a phi of the loop merges with itself (a
    /// register the loop does not write still gets phis).
    fn resolve(&self, v: Value, depth: u32) -> Value {
        let Some(&(b, None)) = self.defs.get(&v) else {
            return v;
        };
        let Some(phi) = self.p.blocks[b].phis.iter().find(|q| q.value == v) else {
            return v;
        };
        if depth == 0 {
            return v;
        }
        let mut other = None;
        for &a in &phi.args {
            let a = self.resolve(a, depth - 1);
            if a == v {
                continue;
            }
            match other {
                None => other = Some(a),
                Some(o) if o == a => {}
                Some(_) => return v,
            }
        }
        other.unwrap_or(v)
    }

    fn invariant(&self, v: Value) -> bool {
        !self.defs.contains_key(&self.resolve(v, PHI_DEPTH))
    }

    fn def(&self, v: Value) -> Option<&'a Inst> {
        match *self.defs.get(&v)? {
            (b, Some(i)) => Some(&self.p.blocks[b].insts[i]),
            (_, None) => None,
        }
    }

    /// `q` as `x + step`: an addition or subtraction of a nonzero constant
    /// that writes `q` (its result, not a carry).
    fn added(&self, q: Value) -> Option<(Value, i64)> {
        let inst = self.def(q)?;
        let name = inst.name();
        let sum = |a: Term32, b: Term32, sign: i64| match (a, b) {
            (Term32::Value(x), Term32::Const(c)) => Some((self.resolve(x, PHI_DEPTH), sign * c)),
            (Term32::Const(c), Term32::Value(x)) if sign > 0 => {
                Some((self.resolve(x, PHI_DEPTH), c))
            }
            _ => None,
        };
        let (x, step) = match &inst.fields {
            Fields::Valu(v) => {
                let plain = v.neg == 0
                    && v.abs == 0
                    && v.sdwa.is_none()
                    && v.dpp.is_none()
                    && !v.clamp
                    && v.omod == 0;
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return None;
                };
                if !plain || inst.write(Reg::V(reg)) != Some(q) {
                    return None;
                }
                let (a, b) = (term(inst, v.src.first()?)?, term(inst, v.src.get(1)?)?);
                match name {
                    "v_add_nc_u32" | "v_add_co_u32" | "v_add_nc_i32" | "v_add_u32" => sum(a, b, 1)?,
                    "v_sub_nc_u32" | "v_sub_co_u32" | "v_sub_nc_i32" => sum(a, b, -1)?,
                    "v_subrev_nc_u32" | "v_subrev_co_u32" => sum(b, a, -1)?,
                    _ => return None,
                }
            }
            Fields::Sop { dst, src, imm } => {
                let Some(Operand::Scalar { code, .. }) = *dst else {
                    return None;
                };
                if inst.write(Reg::S(code)) != Some(q) {
                    return None;
                }
                match (name, imm) {
                    ("s_addk_i32", Some(k)) => {
                        let old = inst.read(Reg::S(code))?;
                        (self.resolve(old, PHI_DEPTH), i64::from(*k as i16))
                    }
                    ("s_add_u32" | "s_add_i32", None) => {
                        sum(term(inst, src.first()?)?, term(inst, src.get(1)?)?, 1)?
                    }
                    ("s_sub_u32" | "s_sub_i32", None) => {
                        sum(term(inst, src.first()?)?, term(inst, src.get(1)?)?, -1)?
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        (step != 0).then_some((x, step))
    }

    /// The constant every iteration adds to `x`, if `x` is a phi of the
    /// header whose back edges all bring it plus that constant, or such a
    /// phi plus it.
    fn induction(&self, x: Value) -> Option<i64> {
        let x = self.resolve(x, PHI_DEPTH);
        match *self.defs.get(&x)? {
            (b, None) if b == self.header => {
                let block = &self.p.blocks[b];
                let phi = block.phis.iter().find(|q| q.value == x)?;
                let mut step = None;
                for (n, q) in block.preds.iter().enumerate() {
                    if !self.body.contains(q) {
                        continue;
                    }
                    let back = self.resolve(*phi.args.get(n)?, PHI_DEPTH);
                    let (from, s) = self.added(back)?;
                    if from != x || *step.get_or_insert(s) != s {
                        return None;
                    }
                }
                step
            }
            (_, None) => None,
            (_, Some(_)) => {
                let (from, s) = self.added(x)?;
                (self.induction(from)? == s).then_some(s)
            }
        }
    }

    /// Whether a lane where `atom` equals `holds` has an induction variable
    /// that has not passed a loop-invariant bound.
    fn bounds(&self, atom: Atom, holds: bool) -> bool {
        let (Atom::Lane(v) | Atom::Uniform(v)) = atom else {
            return false;
        };
        let Some(inst) = self.def(v) else {
            return false;
        };
        let name = inst.name();
        let rest = ["v_cmpx_", "v_cmp_", "s_cmpk_", "s_cmp_"]
            .into_iter()
            .find_map(|prefix| name.strip_prefix(prefix));
        let Some((test, ty)) = rest.and_then(|r| r.split_once('_')) else {
            return false;
        };
        let Some(test) = Test::parse(test).filter(|_| matches!(ty, "u32" | "i32")) else {
            return false;
        };
        let ops = match &inst.fields {
            Fields::Valu(x) if x.neg == 0 && x.abs == 0 && x.sdwa.is_none() && x.dpp.is_none() => {
                match (x.src.first(), x.src.get(1)) {
                    (Some(a), Some(b)) => [term(inst, a), term(inst, b)],
                    _ => return false,
                }
            }
            Fields::Sop {
                src, imm: Some(k), ..
            } => {
                let k = if ty == "i32" {
                    i64::from(*k as i16)
                } else {
                    i64::from(*k)
                };
                [
                    src.first().and_then(|a| term(inst, a)),
                    Some(Term32::Const(k)),
                ]
            }
            Fields::Sop { src, imm: None, .. } => match (src.first(), src.get(1)) {
                (Some(a), Some(b)) => [term(inst, a), term(inst, b)],
                _ => return false,
            },
            _ => return false,
        };
        [(0, 1, test), (1, 0, test.swapped())]
            .into_iter()
            .any(|(at, other, test)| {
                let (Some(Term32::Value(i)), Some(n)) = (ops[at], ops[other]) else {
                    return false;
                };
                let fixed = match n {
                    Term32::Const(_) => true,
                    Term32::Value(n) => self.invariant(n),
                };
                let Some(step) = self.induction(i).filter(|_| fixed) else {
                    return false;
                };
                match if holds { test } else { test.negated() } {
                    Test::Lt | Test::Le => step > 0,
                    Test::Gt | Test::Ge => step < 0,
                    Test::Ne => step.abs() == 1,
                    Test::Eq => false,
                }
            })
    }

    /// Whether the branch ending block `b` keeps a lane in the loop only
    /// while a test of [`Self::bounds`] holds.
    fn exit_counts(&self, st: &Structured, b: usize) -> bool {
        let LaneExit::Branch {
            when,
            taken,
            not_taken,
        } = st.exits[b]
        else {
            return false;
        };
        let stay = match (self.body.contains(&taken), self.body.contains(&not_taken)) {
            (true, false) => true,
            (false, true) => false,
            _ => return false,
        };
        st.bools.support(when).into_iter().any(|atom| {
            let Some(a) = st.bools.var(atom) else {
                return false;
            };
            // Every lane that stays has the atom's value `holds`.
            [true, false].into_iter().any(|holds| {
                !st.bools.meet(&[(when, stay), (a, !holds)]) && self.bounds(atom, holds)
            })
        })
    }
}

impl<'a> Lower<'a> {
    pub(super) fn exit(&mut self, b: usize) -> Result<(), Stop> {
        let offset = self.x.p.blocks[b]
            .insts
            .last()
            .map_or(self.x.p.blocks[b].offset, |i| i.machine.offset);
        let term = match self.x.st.exits[b] {
            LaneExit::Jump(t) if self.counters.contains_key(&t) && self.rpo[t] <= self.rpo[b] => {
                let ok = self.count_iteration(t, pred::IULT);
                let abort = self.abort_block();
                Term::CondBr(ok, self.block[t].expect("reachable"), abort)
            }
            LaneExit::Jump(t) => {
                self.enter_loop(b, t, self.cur);
                Term::Br(self.block[t].expect("reachable"))
            }
            LaneExit::Branch {
                taken, not_taken, ..
            } if self.console_cf => {
                let cond = self.console_branch(b).map_err(|w| unsupported(offset, w))?;
                self.branch_term(b, cond, taken, not_taken)
            }
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } => {
                // A negated condition branches the other way round.
                let (cond, taken, not_taken) = match self.x.st.bools.node(when) {
                    Some((atom, Bool::TRUE, Bool::FALSE)) => {
                        let a = self
                            .atom(atom, self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        (a, not_taken, taken)
                    }
                    _ => {
                        let c = self
                            .boolean(when, self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        (c, taken, not_taken)
                    }
                };
                self.branch_term(b, cond, taken, not_taken)
            }
            LaneExit::End => {
                if self.s.pixel && !self.exported[b] && !self.has_done_export(b) {
                    // A lane leaving without its final export was killed.
                    let t = self.s.m.bool(true);
                    self.s
                        .op(self.cur, "discard", Attr::NoUnwind, self.s.t.void, 82, &[t]);
                }
                Term::Ret
            }
            LaneExit::Indirect => return Err(unsupported(offset, "an indirect jump")),
        };
        let _ = Exit::End;
        self.s.m.terminate(self.cur, term);
        Ok(())
    }

    /// The terminator of block `b` branching to `taken` where `cond` holds.
    fn branch_term(&mut self, b: usize, cond: V, taken: usize, not_taken: usize) -> Term {
        // A back edge is taken only below the iteration limit.
        let back = |l: &Self, to: usize| l.counters.contains_key(&to) && l.rpo[to] <= l.rpo[b];
        // (No `xor` of a compare: a loop exit testing one hung the
        // AMD driver.)
        let cond = if back(self, taken) {
            let ok = self.count_iteration(taken, pred::IULT);
            self.s.m.bin(self.cur, Bin::And, cond, ok)
        } else if back(self, not_taken) {
            let stop = self.count_iteration(not_taken, pred::IUGE);
            self.s.m.bin(self.cur, Bin::Or, cond, stop)
        } else {
            cond
        };
        // Critical edges get a block of their own, so loops have
        // dedicated exits and preheaders as DXC emits them.
        let (t, f) = (self.edge(b, taken), self.edge(b, not_taken));
        for (to, via) in [(taken, t), (not_taken, f)] {
            if !back(self, to) {
                // From an edge block, or from here.
                let from = if Some(via) == self.block[to] {
                    self.cur
                } else {
                    via
                };
                self.enter_loop(b, to, from);
            }
        }
        Term::CondBr(cond, t, f)
    }

    /// Console flow: whether block `b`'s `s_cbranch_*` is taken, the same
    /// in both halves (wave-wide mask tests through [`Self::any_lane`]).
    fn console_branch(&mut self, b: usize) -> Result<V, Why> {
        let inst = self.x.p.blocks[b].insts.last().ok_or("a branch")?;
        let name = inst.name();
        let c = self.cur;
        let (any, set) = match name {
            "s_cbranch_scc0" | "s_cbranch_scc1" => {
                let v = inst.read(Reg::Scc).ok_or("a branch's SCC")?;
                let scc = self.scc_bit(v)?;
                (scc, name == "s_cbranch_scc1")
            }
            "s_cbranch_vccz" | "s_cbranch_vccnz" | "s_cbranch_execz" | "s_cbranch_execnz" => {
                let reg = if name.contains("vcc") {
                    crate::ir::VCC
                } else {
                    EXEC
                };
                let lo = inst.read(Reg::S(reg)).ok_or("a branch's mask")?;
                let &m = self
                    .x
                    .st
                    .masks
                    .get(&lo)
                    .ok_or("a branch on an untracked mask")?;
                (self.any_lane(m)?, name.ends_with("nz"))
            }
            _ => return Err("a branch the console flow does not model"),
        };
        Ok(if set {
            any
        } else {
            let t = self.s.m.bool(true);
            self.s.m.bin(c, Bin::Xor, any, t)
        })
    }

    /// Console flow: an SCC value as a bit. A mask operation's SCC (result
    /// not zero) is whether some lane of the console wave has its result's
    /// bit: for `s_and_saveexec` and the like, the new EXEC's, which is
    /// not the first mask the instruction writes (the saved EXEC is).
    /// Structurisation says which mask it is ([`Structured::sccs`]).
    ///
    /// [`Structured::sccs`]: crate::structure::Structured::sccs
    fn scc_bit(&mut self, v: Value) -> Result<V, Why> {
        if let Some(&Val::Bit(x)) = self.vals.get(&v) {
            return Ok(x);
        }
        let &(mask, negated) = self
            .x
            .st
            .sccs
            .get(&v)
            .ok_or("an SCC the emitter did not compute")?;
        // (A compare either way, no `xor` of one: see `branch_term`.)
        let c = self.cur;
        let bit = self.boolean(mask, c)?;
        let (lo, hi) = self.console_ballot(c, bit);
        let or = self.s.m.bin(c, Bin::Or, lo, hi);
        let z = self.s.m.i32(0);
        let p = if negated { pred::EQ } else { pred::NE };
        Ok(self.s.m.cmp(c, p, or, z))
    }

    /// A forward edge into loop header `to` (from DXIL block `via`) starts
    /// its count at 0.
    pub(super) fn enter_loop(&mut self, _from: usize, to: usize, via: usize) {
        if let Some((_, incoming)) = self.counters.get_mut(&to) {
            let z = self.s.m.i32(0);
            incoming.push((z, via));
        }
    }

    /// A back edge to header `to` (a back edge keeps its block, no
    /// critical-edge block): the count goes up; the result compares it with
    /// [`LOOP_LIMIT`](super::LOOP_LIMIT) by `p` (below: go on; at or above:
    /// stop), with the spin-wait limit for a loop that only waits for
    /// memory to change (`spin.rs`), or with the higher limit of a counted
    /// loop ([`counted_loops`]).
    pub(super) fn count_iteration(&mut self, to: usize, p: u8) -> V {
        let c = self.cur;
        let (phi, _) = self.counters[&to];
        let one = self.s.m.i32(1);
        let next = self.s.m.bin(c, Bin::Add, phi, one);
        let limit = if self.spin.contains(&to) {
            crate::emit::spin_limit()
        } else if self.counted.contains(&to) {
            crate::emit::counted_loop_limit()
        } else {
            crate::emit::loop_limit()
        };
        #[cfg(test)]
        self.limits.push(limit);
        let limit = self.s.m.i32(limit);
        self.counters
            .get_mut(&to)
            .expect("header")
            .1
            .push((next, c));
        self.s.m.cmp(c, p, next, limit)
    }

    /// The block a lane stopped by [`LOOP_LIMIT`](super::LOOP_LIMIT) leaves through.
    // shortcut: limit hits are not reported to the host.
    pub(super) fn abort_block(&mut self) -> usize {
        if let Some(a) = self.abort {
            return a;
        }
        let a = self.s.m.new_block();
        if self.s.pixel {
            let t = self.s.m.bool(true);
            self.s
                .op(a, "discard", Attr::NoUnwind, self.s.t.void, 82, &[t]);
        }
        self.s.m.terminate(a, Term::Ret);
        self.abort = Some(a);
        a
    }

    /// The DXIL block a branch from IR block `from` to `to` goes to.
    pub(super) fn edge(&mut self, from: usize, to: usize) -> usize {
        let target = self.block[to].expect("reachable");
        let preds = self.x.p.blocks[to]
            .preds
            .iter()
            .filter(|&&q| self.block[q].is_some())
            .count();
        // Back edges stay as they are: the exiting block is the latch.
        if preds < 2 || self.rpo[to] <= self.rpo[from] {
            return target;
        }
        let e = self.s.m.new_block();
        self.s.m.terminate(e, Term::Br(target));
        self.edges.insert((from, to), e);
        e
    }

    pub(super) fn fill_phis(&mut self) -> Result<(), Stop> {
        let p = self.x.p;
        // A phi that only another read phi consumes is live too. Without this, a value with no DXIL form that
        // reaches a read through two joins is taken for dead at the first join and replaced by zero below, and the
        // lowering accepts what it refuses when the same value is read directly.
        let mut work: Vec<Value> = self.read_phis.iter().copied().collect();
        while let Some(v) = work.pop() {
            let Some(&(b, i)) = self.phi_at.get(&v) else {
                continue;
            };
            for &arg in &p.blocks[b].phis[i].args {
                if self.phi_at.contains_key(&arg) && self.read_phis.insert(arg) {
                    work.push(arg);
                }
            }
        }
        for (v, b, i) in std::mem::take(&mut self.data_phis) {
            let phi = &p.blocks[b].phis[i];
            let bit = phi.reg == Reg::Scc;
            let mut incoming = Vec::new();
            for (n, &q) in p.blocks[b].preds.iter().enumerate() {
                let Some(qb) = self.tail[q] else { continue };
                let qb = self.edges.get(&(q, b)).copied().unwrap_or(qb);
                let arg = phi.args[n];
                if self.read_phis.contains(&phi.value) && !self.vals.contains_key(&arg) {
                    let _ = self.lazy_barycentric(arg);
                }
                let x = match (self.vals.get(&arg).copied(), bit) {
                    (Some(Val::I32(x)), false) | (Some(Val::Bit(x)), true) => x,
                    (Some(Val::Bit(_)), false) if self.read_phis.contains(&phi.value) => {
                        return Err(Stop::Retry(phi.value, "an SCC joined with SGPR values"));
                    }
                    // A descriptor's dwords reaching a join that also takes
                    // data: the registers are reused after the descriptor's
                    // last access, so the program reads the joined value
                    // only after the data edge (the console compiler's phi
                    // of a value defined on one edge only). Only dwords
                    // that have no host value (addresses, formats): any
                    // other reason the descriptor load stopped with (a cube
                    // or MSAA image, a constant buffer's descriptor read as
                    // data) stops the program as it would without the join.
                    // shortcut: undefined on the descriptor edge; wrong if
                    // the program read the descriptor's dwords as data there.
                    (Some(Val::Missing(why)), false)
                        if why == super::smem::DESCRIPTOR_WORD && self.descriptor_dword(arg) =>
                    {
                        self.s.m.undef(self.s.t.i32)
                    }
                    (Some(Val::Missing(why)), _) if self.read_phis.contains(&phi.value) => {
                        return Err(Stop::Retry(phi.value, why));
                    }
                    _ if self.read_phis.contains(&phi.value) => {
                        return Err(Stop::Retry(phi.value, "a join of mismatched values"));
                    }
                    _ if bit => self.s.m.bool(false),
                    _ => self.s.m.i32(0),
                };
                incoming.push((x, qb));
            }
            self.s.m.set_phi(v, incoming);
        }
        for (phi, incoming) in std::mem::take(&mut self.counters).into_values() {
            self.s.m.set_phi(phi, incoming);
        }
        while let Some((v, b, i)) = self.pending_masks.pop() {
            let phi = &p.blocks[b].phis[i];
            let offset = p.blocks[b].offset;
            let mut incoming = Vec::new();
            for (n, &q) in p.blocks[b].preds.iter().enumerate() {
                let Some(qb) = self.tail[q] else { continue };
                let qb = self.edges.get(&(q, b)).copied().unwrap_or(qb);
                // Console flow: every lane arrives by every edge, so a join
                // also takes masks that are constants or built from scalars.
                let arg = &phi.args[n];
                let others = self.console_cf.then(|| self.x.st.others.get(arg)).flatten();
                let m =
                    *self.x.st.masks.get(arg).or(others).ok_or_else(|| {
                        unsupported(offset, "a lane-mask join of an untracked mask")
                    })?;
                let x = self.boolean(m, qb).map_err(|w| unsupported(offset, w))?;
                incoming.push((x, qb));
            }
            self.s.m.set_phi(v, incoming);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Parts, lower};
    use crate::emit::tests::{compute, emit_for};
    use crate::emit::{counted_loop_limit, loop_limit, spin_limit};
    use crate::native::{ComputeIo, ResourceType, RootParam, RootType, StageIo, UsedResource};
    use crate::{io, ir, resource, structure};

    const S_ENDPGM: u32 = 0xBF81_0000;

    /// The limit each back edge of `words` compares its loop's count with:
    /// a wave32 compute program of 32 threads whose s0 holds a table of
    /// structured buffers of dwords and s1 a root constant, with 512 bytes
    /// of LDS.
    fn limits(words: &[u32]) -> Vec<u32> {
        let param = |root, user_data| RootParam {
            root,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data,
            slot: user_data,
        };
        let mut native = compute(words, false);
        native.layout = vec![param(RootType::Table, 0), param(RootType::Constants, 1)];
        native.used = vec![UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 4,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }];
        native.io = StageIo::Cs(ComputeIo {
            rsrc2: 1 << 15 | 2 << 1,
            threads: [32, 1, 1],
        });
        let p = ir::build(&native.code, false, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        lower(&Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
            target: crate::emit::Target::ANY,
            console_cf: false,
            per_lane_only: false,
        })
        .unwrap()
        .loop_limits
    }

    /// A uniform loop `for (i = 0; i < s1; i += step)` writing LDS each
    /// round, its test `test` (`s_cmp_*_u32 s2, s1` by default).
    fn uniform_loop(step: u32, test: u32) -> [u32; 8] {
        [
            0xBE82_0380, // s_mov_b32 s2, 0
            0x7E02_0202, // 4: v_mov_b32 v1, s2
            0xD834_0000,
            0x0000_0100,             // ds_write_b32 v0, v1
            0x8102_0002 | step << 8, // s_add_i32 s2, s2, step (inline constant)
            test,
            0xBF85_FFFA, // s_cbranch_scc1 -> 4
            S_ENDPGM,
        ]
    }

    /// Counted loops keep the higher limit: 2^20 unless
    /// `XTP_SHADER_LOOP_LIMIT` says otherwise. 2^14 cut short the loops of
    /// 16,385 to 2^20 iterations.
    #[test]
    fn counted_loops_keep_the_higher_limit() {
        if std::env::var_os("XTP_SHADER_LOOP_LIMIT").is_none() {
            assert_eq!(counted_loop_limit(), 1 << 20);
            assert_eq!(loop_limit(), 1 << 14);
        }
        // i < s1, i += 1 (and += 3).
        let lt = 0xBF0A_0102; // s_cmp_lt_u32 s2, s1
        assert_eq!(limits(&uniform_loop(0x81, lt)), [counted_loop_limit()]);
        assert_eq!(limits(&uniform_loop(0x83, lt)), [counted_loop_limit()]);
        // s1 > i, the operands the other way round.
        let gt = 0xBF08_0201; // s_cmp_gt_u32 s1, s2
        assert_eq!(limits(&uniform_loop(0x81, gt)), [counted_loop_limit()]);
        // i != s1 by steps of 1: the form compilers give `i < n`.
        let ne = 0xBF07_0102; // s_cmp_lg_u32 s2, s1
        assert_eq!(limits(&uniform_loop(0x81, ne)), [counted_loop_limit()]);
        // A divergent loop: each lane counts to its own v0 and leaves once
        // v1 >= v0 (an exit mask, EXEC narrowed each round).
        let lanes = [
            0x7E02_0280, // v_mov_b32 v1, 0
            0xBE82_0380, // s_mov_b32 s2, 0
            0xD834_0000,
            0x0000_0100, // 8: ds_write_b32 v0, v1
            0x4A02_0281, // v_add_nc_u32 v1, 1, v1
            0x7D8C_0101, // v_cmp_ge_u32 vcc_lo, v1, v0
            0x8802_026A, // s_or_b32 s2, vcc_lo, s2
            0x8A7E_027E, // s_andn2_b32 exec_lo, exec_lo, s2
            0xBF89_FFF9, // s_cbranch_execnz -> 8
            0x887E_027E, // s_or_b32 exec_lo, exec_lo, s2
            S_ENDPGM,
        ];
        assert_eq!(limits(&lanes), [counted_loop_limit()]);
    }

    /// Loops with no induction variable bounded by a loop-invariant value
    /// keep 2^14: one stepping over its bound (`!=` by 2), one counting
    /// away from it, one whose bound moves, one that walks a buffer until
    /// it reads 0.
    #[test]
    fn other_loops_keep_the_general_limit() {
        let ne = 0xBF07_0102; // s_cmp_lg_u32 s2, s1
        assert_eq!(limits(&uniform_loop(0x82, ne)), [loop_limit()]);
        let lt = 0xBF0A_0102; // s_cmp_lt_u32 s2, s1
        assert_eq!(limits(&uniform_loop(0xC1, lt)), [loop_limit()]);
        let moving = [
            0xBE82_0380, // s_mov_b32 s2, 0
            0x7E02_0202, // 4: v_mov_b32 v1, s2
            0xD834_0000,
            0x0000_0100, // ds_write_b32 v0, v1
            0x8102_8102, // s_add_i32 s2, s2, 1
            0x8104_8304, // s_add_i32 s4, s4, 3
            0xBF0A_0402, // s_cmp_lt_u32 s2, s4
            0xBF85_FFF9, // s_cbranch_scc1 -> 4
            S_ENDPGM,
        ];
        assert_eq!(limits(&moving), [loop_limit()]);
        let walks = [
            0xBE85_0380, // s_mov_b32 s5, 0
            0xBE84_0300, // s_mov_b32 s4, s0
            0xF408_0202,
            0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
            0xBE82_0380, // s_mov_b32 s2, 0
            0xE030_6000,
            0x8002_0100, // 0x14: buffer_load_dword v1, v0, s[8:11], 0 idxen glc
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0x4A00_0081, // v_add_nc_u32 v0, 1, v0
            0x7D84_0280, // v_cmp_eq_u32 vcc_lo, 0, v1
            0x8802_026A, // s_or_b32 s2, vcc_lo, s2
            0x8A7E_027E, // s_andn2_b32 exec_lo, exec_lo, s2
            0xBF89_FFF8, // s_cbranch_execnz -> 0x14
            0x887E_027E, // s_or_b32 exec_lo, exec_lo, s2
            0xD834_0000,
            0x0000_0100, // ds_write_b32 v0, v1
            S_ENDPGM,
        ];
        assert_eq!(limits(&walks), [loop_limit()]);
    }

    /// A spin wait (rereading one flag until it is no longer 0) compares
    /// with the spin limit, 4096 under the default loop limit.
    #[test]
    fn spin_waits_emit_the_spin_limit() {
        if std::env::var_os("XTP_SHADER_LOOP_LIMIT").is_none() {
            assert_eq!(spin_limit(), 4096);
        }
        let spin = [
            0xBE85_0380, // s_mov_b32 s5, 0
            0xBE84_0300, // s_mov_b32 s4, s0
            0xF408_0202,
            0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
            0xBE82_0380, // s_mov_b32 s2, 0
            0xE030_6000,
            0x8002_0100, // 0x14: buffer_load_dword v1, v0, s[8:11], 0 idxen glc
            0x7D84_0280, // v_cmp_eq_u32 vcc_lo, 0, v1
            0x8802_026A, // s_or_b32 s2, vcc_lo, s2
            0x8A7E_027E, // s_andn2_b32 exec_lo, exec_lo, s2
            0xBF89_FFFA, // s_cbranch_execnz -> 0x14
            0x887E_027E, // s_or_b32 exec_lo, exec_lo, s2
            0xD834_0000,
            0x0000_0100, // ds_write_b32 v0, v1
            S_ENDPGM,
        ];
        assert_eq!(limits(&spin), [spin_limit()]);
    }

    /// s8 holds a buffer descriptor's first dword on one edge (a root
    /// constant, s1, chose the load) and 5 on the other, and the program
    /// reads the joined s8 only where s1 chose the data edge. Lane t stores
    /// at dword t of the UAV at table entry 1: with s1 = 0, the 5 of the
    /// data edge; otherwise the dword t of the buffer at entry 0.
    const DESCRIPTOR_JOIN: [u32; 23] = [
        0x3408_0082, // v_lshlrev_b32 v4, 2, v0
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0302,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0xBF85_0009, // s_cbranch_scc1 -> 0x40
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE030_2000,
        0x8002_0200, // buffer_load_dword v2, v0, s[8:11], 0 idxen
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xE070_1000,
        0x8003_0204, // buffer_store_dword v2, v4, s[12:15], 0 offen
        0xBF82_0001, // s_branch -> 0x44
        0xBE88_0385, // 0x40: s_mov_b32 s8, 5
        0xBF06_8001, // 0x44: s_cmp_eq_u32 s1, 0
        0xBF84_0003, // s_cbranch_scc0 -> 0x58
        0x7E06_0208, // v_mov_b32 v3, s8
        0xE070_1000,
        0x8003_0304, // buffer_store_dword v3, v4, s[12:15], 0 offen
        S_ENDPGM,    // 0x58
    ];

    /// [`DESCRIPTOR_JOIN`] in groups of 32, entry 0 of the table at s0 a
    /// buffer of `kind` (stride 4), entry 1 a raw UAV.
    fn descriptor_join(kind: ResourceType) -> crate::NativeProgram {
        let mut p = compute(&DESCRIPTOR_JOIN, false);
        let param = |root, user_data| RootParam {
            root,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data,
            slot: user_data,
        };
        let entry = |kind, table_offset_dwords, stride| UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords,
            lower_bound: 0,
            stride,
            dwords: 4,
            user_data: 0,
            slot: 0,
        };
        p.layout = vec![param(RootType::Table, 0), param(RootType::Constants, 1)];
        p.used = vec![entry(kind, 0, 4), entry(ResourceType::Uav, 8, 0)];
        p.io = StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [32, 1, 1],
        });
        p
    }

    /// The descriptor edge of [`DESCRIPTOR_JOIN`] brings an undefined dword
    /// instead of stopping the program ("a descriptor's address or format
    /// read as data"), and both edges give what the console does (on WARP).
    #[test]
    fn descriptor_dwords_join_data_as_undefined_on_their_edge() {
        let cs = emit_for(
            &descriptor_join(ResourceType::Srv),
            crate::emit::Target::ANY,
        )
        .unwrap();
        #[cfg(windows)]
        {
            use crate::emit::host::{Dispatch, Host, Root, Stride, View};
            let Ok(host) = Host::new(true) else {
                eprintln!("no WARP device; skipped");
                return;
            };
            let (root, pso) = host.compute_pipeline(&cs).unwrap();
            let data: Vec<u32> = (0..32).map(|t| 1000 + t).collect();
            let views = [
                View::Buffer {
                    data: &data,
                    stride: Stride::Structured(4),
                },
                View::RawUav(32),
            ];
            for (choice, want) in [(0, vec![5; 32]), (1, data.clone())] {
                let mut user = vec![0; (cs.user_data_dwords() as usize).max(2)];
                user[1] = choice;
                let out = host
                    .dispatch(
                        &root,
                        &pso,
                        &Dispatch {
                            roots: vec![(0, Root::Constants(user))],
                            views: &views,
                            samplers: &[],
                            groups: [1, 1, 1],
                        },
                    )
                    .unwrap();
                assert_eq!(out[0], want, "s1 = {choice}");
            }
        }
    }

    /// The same join with entry 0 a constant buffer: its descriptor's
    /// dwords stop with their own reason, which the join keeps.
    #[test]
    fn a_join_keeps_the_refusal_of_a_constant_buffers_descriptor() {
        let r = emit_for(
            &descriptor_join(ResourceType::Cbv),
            crate::emit::Target::ANY,
        );
        assert!(
            matches!(
                r,
                Err(crate::ShaderError::Unsupported {
                    what: "a constant buffer's descriptor read as data",
                    ..
                })
            ),
            "{r:?}"
        );
    }
}
