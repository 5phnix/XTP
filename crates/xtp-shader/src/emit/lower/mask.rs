//! Lane masks: structurisation's per-lane booleans as DXIL values, and
//! masks read as numbers (ballots, see [`MaskData`]). Backlog M1 (masks
//! the boolean tracker loses, kept as numbers) and M2 (masks holding
//! lanes elsewhere) start here.

use std::collections::HashMap;

use super::{Lower, Val, Why};
use crate::emit::bitcode::{Attr, Bin, V, pred};
use crate::emit::shader::flags;
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{EXEC, Inst, Program, Reg, Value};
use crate::structure::{Atom, Bool, Structured};

/// Lane masks read as numbers (`s_bcnt1`, `s_ff1`, `v_mbcnt`, scalar
/// compares, bit tests) are ballots of the lanes' booleans, taken where the
/// mask is written: structurisation's `active` masks hold only lanes that
/// reach that block, so the ballot misses none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MaskData {
    /// Compute: host lane n is console lane n (the program declares the
    /// console's wave size), so ballots are the console's masks bit for bit
    /// and any use is exact.
    Console,
    /// Pixel and vertex programs: ballots over the host's wave, whatever its
    /// size. Only uses that mean the same on any wave are allowed: counts,
    /// the first lane, the count below a lane, equality (the HLSL wave
    /// operations the console compiler expands). Lane numbers stay host
    /// lanes throughout (`v_mbcnt`, `v_readlane` at a computed lane).
    Host,
    /// Mesh programs: their lanes and waves (LDS per console wave) are the
    /// console's but no host wave size can be declared.
    None,
}

/// What a value holding a lane mask read as a number stops with.
pub(super) const LANE_MASK: Why = "a lane mask read as data";

/// What a pixel program stops with where a wave operation reads a mask
/// whose helper lanes' bits [`HelperBits`] does not follow.
pub(super) const HELPER_BITS: Why = "a lane mask whose helper lanes' bits are not followed";

/// What a pixel program's helper lanes hold of a lane mask on the console.
///
/// A helper lane runs a pixel program for a quad's pixel the primitive does
/// not cover. On the console, EXEC on entry holds only covered pixels;
/// `s_wqm` adds the helper lanes of their quads, and the program takes them
/// out again itself (`s_and_b64 exec, exec, s[live]` before its exports and
/// stores). A mask a compare writes holds the lanes of EXEC there; a mask
/// operation's helper bits are those of its operands'.
///
/// The per-lane form ([`crate::structure`]) runs every host lane, helper
/// lanes too, as if EXEC held them from the start: each mask's per-lane bit
/// there is a covered lane's on the console, and a helper lane's where the
/// mask holds the helper lanes ([`Helpers::In`]), not where they are out of
/// it ([`Helpers::Out`]). A pixel program with wave operations declares
/// `WaveOpsIncludeHelperLanes` (SM 6.7, `shader.rs`): its helper lanes are
/// lanes of its ballots, lane reads and reductions as on the console, and a
/// wave operation on a mask whose helper lanes are out of it leaves them out
/// ([`Lower::console_bit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Helpers {
    /// A helper lane holds its per-lane bit, as a covered lane does.
    In,
    /// No helper lane is set.
    Out,
    /// Something else: helper bits that are neither (a mask with helper
    /// lanes in or with them out, say), or not followed.
    Other,
}

impl Helpers {
    fn join(a: Option<Helpers>, b: Helpers) -> Helpers {
        match a {
            None => b,
            Some(a) if a == b => a,
            Some(_) => Helpers::Other,
        }
    }

    /// The helper bits of the mask operation `f` (as `structure.rs`'s
    /// `combine` names them) of masks with helper bits `a` and `c`: the
    /// operation's per-lane result where each operand's helper bits are its
    /// per-lane bits or clear, if it is one of those.
    fn combine(f: &str, a: Helpers, c: Helpers) -> Option<Helpers> {
        use Helpers::{In, Other, Out};
        let both = |x| a == x && c == x;
        Some(match f {
            "and" if a == Out || c == Out => Out,
            "andn2" if a == Out => Out,
            "andn1" if c == Out => Out,
            "or" | "xor" if both(Out) => Out,
            "and" | "andn2" | "andn1" | "or" | "xor" | "orn2" | "orn1" | "nand" | "nor"
            | "xnor"
                if both(In) =>
            {
                In
            }
            "and" | "andn2" | "andn1" | "or" | "xor" | "orn2" | "orn1" | "nand" | "nor"
            | "xnor" => Other,
            _ => return None,
        })
    }
}

/// The [`Helpers`] of each lane mask of a pixel program, by the value of any
/// of its dwords, and of EXEC where each is written.
#[derive(Debug, Default)]
pub(super) struct HelperBits {
    masks: HashMap<Value, Helpers>,
    /// EXEC's helper bits where each mask is written (entering the block,
    /// for a join).
    exec_at: HashMap<Value, Helpers>,
}

impl HelperBits {
    /// Follows the masks of `p`, a pixel program, from EXEC on entry
    /// ([`Helpers::Out`]) through compares, mask operations and joins.
    pub(super) fn of(p: &Program) -> HelperBits {
        use Helpers::{Other, Out};
        let mut h = HelperBits::default();
        let entry = p.inputs.iter().find(|i| i.0 == Reg::S(EXEC)).map(|i| i.1);
        for &(reg, v) in &p.inputs {
            if reg == Reg::S(EXEC) || reg == Reg::S(EXEC + 1) {
                h.masks.insert(v, Out);
                h.exec_at.insert(v, Out);
            }
        }
        let order = crate::ir::reverse_postorder(&p.blocks);
        // EXEC's low dword leaving each block.
        let mut exec_out: Vec<Option<Value>> = vec![None; p.blocks.len()];
        // Kinds only rise (unknown, then In or Out, then Other): joins over
        // loops settle within a few rounds.
        for _ in 0..64 {
            let mut changed = false;
            for (n, &b) in order.iter().enumerate() {
                let block = &p.blocks[b];
                let mut exec = match block.phis.iter().find(|q| q.reg == Reg::S(EXEC)) {
                    Some(phi) => Some(phi.value),
                    None if n == 0 => entry,
                    None => block.preds.iter().find_map(|&q| exec_out[q]),
                };
                for phi in &block.phis {
                    let mut k = None;
                    for a in &phi.args {
                        if let Some(&x) = h.masks.get(a) {
                            k = Some(Helpers::join(k, x));
                        }
                    }
                    if let Some(k) = k {
                        changed |= h.rise(phi.value, k);
                    }
                }
                let entering = h.kind(exec);
                for phi in &block.phis {
                    changed |= h.rise_exec(phi.value, entering);
                }
                for inst in &block.insts {
                    let e = h.kind(inst.read(Reg::S(EXEC)).or(exec));
                    let (dst, new_exec) = h.step(inst, e, p.wave64);
                    let mut set = |code: u8, k: Helpers| {
                        for r in [code, code + 1] {
                            if let Some(v) = inst.write(Reg::S(r)) {
                                changed |= h.rise(v, k);
                                changed |= h.rise_exec(v, e);
                            }
                        }
                    };
                    if let Some((code, k)) = dst {
                        set(code, k);
                    }
                    if inst.write(Reg::S(EXEC)).is_some() {
                        set(EXEC, new_exec.unwrap_or(Other));
                    }
                    if let Some(w) = inst.write(Reg::S(EXEC)) {
                        exec = Some(w);
                    }
                }
                exec_out[b] = exec;
            }
            if !changed {
                return h;
            }
        }
        // Not settled: nothing is followed.
        for k in h.masks.values_mut().chain(h.exec_at.values_mut()) {
            *k = Other;
        }
        h
    }

    /// The helper bits a mask instruction `inst` writes, run with EXEC's
    /// helper bits `e`: its destination's (its SGPR, the bits), and the new
    /// EXEC's.
    fn step(
        &self,
        inst: &Inst,
        e: Helpers,
        wave64: bool,
    ) -> (Option<(u8, Helpers)>, Option<Helpers>) {
        use Helpers::{In, Other};
        match &inst.fields {
            Fields::Sop { dst, src, .. } => {
                let width = if wave64 { "_b64" } else { "_b32" };
                let Some(op) = inst
                    .name()
                    .strip_prefix("s_")
                    .and_then(|n| n.strip_suffix(width))
                else {
                    return (None, None);
                };
                let operand = |n: usize| match src.get(n) {
                    Some(&Operand::Scalar { code, .. }) if code != NULL => {
                        self.kind(inst.read(Reg::S(code)))
                    }
                    // All lanes or none: the same in helper lanes.
                    Some(Operand::Const(128 | 193)) | Some(Operand::Literal(0 | u32::MAX)) => In,
                    _ => Other,
                };
                let dst = match dst {
                    Some(Operand::Scalar { code, .. }) if *code != NULL => Some(*code),
                    _ => None,
                };
                let saved = op
                    .strip_suffix("_saveexec")
                    .map(|f| (true, f))
                    .or_else(|| op.strip_suffix("_wrexec").map(|f| (false, f)));
                if let Some((saveexec, f)) = saved {
                    let new = Helpers::combine(f, operand(0), e).unwrap_or(Other);
                    let saved = if saveexec { e } else { new };
                    return (dst.map(|d| (d, saved)), Some(new));
                }
                let r = match op {
                    "mov" => operand(0),
                    "not" if operand(0) == In => In,
                    "not" => Other,
                    // Some lane of the quad: helper lanes join their quad.
                    "wqm" => In,
                    "cselect" => Helpers::join(Some(operand(0)), operand(1)),
                    _ => match Helpers::combine(op, operand(0), operand(1)) {
                        Some(r) => r,
                        None => return (None, None),
                    },
                };
                (dst.map(|d| (d, r)), Some(r))
            }
            Fields::Valu(v) => {
                // A compare's (or carry's) bits: the lanes of EXEC here.
                let name = inst.name();
                if name.starts_with("v_cmpx") {
                    return (None, Some(e));
                }
                let compare = v.dst.filter(|_| name.starts_with("v_cmp"));
                let dst = compare.iter().chain(&v.carry).find_map(|op| match *op {
                    Operand::Scalar { code, .. } if code != NULL => Some((code, e)),
                    _ => None,
                });
                (dst, None)
            }
            _ => (None, None),
        }
    }

    fn kind(&self, v: Option<Value>) -> Helpers {
        v.and_then(|v| self.masks.get(&v).copied())
            .unwrap_or(Helpers::Other)
    }

    /// Raises `v`'s helper bits to `k` (joined); whether they changed.
    fn rise(&mut self, v: Value, k: Helpers) -> bool {
        let old = self.masks.get(&v).copied();
        let new = Helpers::join(old, k);
        self.masks.insert(v, new);
        old != Some(new)
    }

    fn rise_exec(&mut self, v: Value, k: Helpers) -> bool {
        let old = self.exec_at.get(&v).copied();
        let new = Helpers::join(old, k);
        self.exec_at.insert(v, new);
        old != Some(new)
    }

    /// The helper bits of mask `v` (any of its dwords).
    pub(super) fn mask(&self, v: Value) -> Helpers {
        self.kind(Some(v))
    }

    /// EXEC's helper bits where mask `v` is written.
    pub(super) fn exec_at(&self, v: Value) -> Helpers {
        self.exec_at.get(&v).copied().unwrap_or(Helpers::Other)
    }
}

/// Instructions that read a lane mask (not EXEC implicitly, not a lane's
/// bit) as a number.
pub(super) fn reads_mask_as_data(st: &Structured, i: &Inst) -> bool {
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    let ops: Vec<&Operand> = match &i.fields {
        // A mask operation (its result a mask) is no data use.
        Fields::Sop { .. }
            if i.writes
                .iter()
                .any(|&(r, w)| matches!(r, Reg::S(_)) && is_mask(w)) =>
        {
            return false;
        }
        Fields::Sop { src, .. } => src.iter().collect(),
        Fields::Valu(v)
            if !matches!(i.name(), "v_cndmask_b32") && !i.name().contains("_co_ci_") =>
        {
            v.src.iter().collect()
        }
        _ => return false,
    };
    ops.into_iter().any(|op| match *op {
        Operand::Scalar { code, .. } if code != NULL => i.read(Reg::S(code)).is_some_and(is_mask),
        _ => false,
    })
}

impl<'a> Lower<'a> {
    /// Le masque initial d'EXEC dépend de la taille de la queue. Un mot de
    /// masque construit comme donnée peut aussi contenir des lanes hors EXEC.
    pub(super) fn active_mask(&mut self, v: Value, mask: Bool, at: usize) -> Result<V, Why> {
        let bit = self.boolean(mask, at)?;
        Ok(if self.x.st.active.contains(&v) {
            self.with_partial(at, bit)
        } else {
            bit
        })
    }

    /// A dword of a lane mask as a number: the ballot of the lanes' bits,
    /// taken in the block that writes the mask (see [`MaskData`]).
    pub(super) fn mask_dword(&mut self, v: Value) -> Result<V, Why> {
        let (lo, high) = match self.x.st.highs.get(&v) {
            Some(&lo) => (lo, true),
            None => (v, false),
        };
        let &m = self.x.st.masks.get(&lo).ok_or(LANE_MASK)?;
        // Per lane, a mask from a join resolved to a constant is that
        // constant only in the lanes here: a loop's exit mask is "no lane"
        // in every lane still in the loop, and holds the lanes that left.
        let resolved = !self.console_cf && self.x.st.resolved.contains(&lo);
        if m == Bool::FALSE && !resolved {
            // No lane anywhere: 0 whoever holds the mask.
            return Ok(self.s.m.i32(0));
        }
        if self.console_cf {
            // Every lane runs every block: the mask's bits are the lanes'
            // own, both halves' words exchanged.
            if let Some(&xy) = self.ballots.get(&lo) {
                return Ok(if high { xy.1 } else { xy.0 });
            }
            let at = match self.def_block.get(&lo) {
                None => self.s.entry,
                Some(&b) if b == self.cur_ir => self.cur,
                Some(&b) => self.tail[b].ok_or("a lane mask written after its use")?,
            };
            let bit = self.active_mask(lo, m, at)?;
            let xy = self.console_ballot(at, bit);
            self.ballots.insert(lo, xy);
            return Ok(if high { xy.1 } else { xy.0 });
        }
        if self.split {
            return Err("a lane mask read whole in a wave64 program split in halves");
        }
        if resolved || !self.x.st.active.contains(&lo) {
            // Lanes of the mask are in other blocks (lanes that left a
            // loop, say), so no ballot here holds them. In compute the
            // console flow has the mask: every lane runs every block there.
            return Err(if self.console_flow_runs() && !self.x.per_lane_only {
                super::CONSOLE_FLOW
            } else {
                "a lane mask holding lanes elsewhere read as data"
            });
        }
        let (x, y) = match self.ballots.get(&lo) {
            Some(&xy) => xy,
            None => {
                let at = match self.def_block.get(&lo) {
                    // Program inputs (EXEC on entry).
                    None => self.s.entry,
                    Some(&b) if b == self.cur_ir => self.cur,
                    Some(&b) => self.tail[b].ok_or("a lane mask written after its use")?,
                };
                if !self.balloting.insert(lo) {
                    return Err("a lane mask whose lanes depend on its own value");
                }
                let bit = self.boolean(m, at);
                self.balloting.remove(&lo);
                let bit = self.with_partial(at, bit?);
                let console = self.console_bit(lo, at, bit)?;
                let i32t = self.s.t.i32;
                let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
                let r = self.s.op(
                    at,
                    "waveActiveBallot",
                    Attr::NoUnwind,
                    four,
                    116,
                    &[console],
                );
                self.s.flags |= flags::WAVE_OPS;
                let xy = (self.s.m.extract(at, r, 0), self.s.m.extract(at, r, 1));
                let xy = self.alone(self.helper_bits.exec_at(lo), at, bit, xy)?;
                self.ballots.insert(lo, xy);
                xy
            }
        };
        Ok(if high { y } else { x })
    }

    /// Whether this lane is a helper lane (a pixel program's, at `at`).
    pub(super) fn is_helper(&mut self, at: usize) -> V {
        self.s
            .op(at, "isHelperLane.i1", Attr::ReadOnly, self.s.t.i1, 221, &[])
    }

    /// `bit`, this lane's per-lane bit of mask `v` at `at`, as the console's
    /// wave holds it: in a pixel program, clear in a helper lane where the
    /// mask's helper lanes are out of it ([`Helpers`]).
    pub(super) fn console_bit(&mut self, v: Value, at: usize, bit: V) -> Result<V, Why> {
        if !self.s.pixel {
            return Ok(bit);
        }
        match self.helper_bits.mask(v) {
            Helpers::In => Ok(bit),
            Helpers::Out => {
                let helper = self.is_helper(at);
                let t = self.s.m.bool(true);
                let covered = self.s.m.bin(at, Bin::Xor, helper, t);
                Ok(self.s.m.bin(at, Bin::And, bit, covered))
            }
            Helpers::Other => Err(HELPER_BITS),
        }
    }

    /// The words (low, high) of a ballot a lane reads where EXEC's helper
    /// bits are `exec` (the console's ballot `xy`, in which this lane's
    /// per-lane bit is `bit`): in a pixel program where EXEC holds no helper
    /// lane, a helper lane, which the console does not run there, sees only
    /// its own bit. It then runs as a wave of its own: a loop that serves
    /// the lanes of a key it read from the mask (`s_ff1`, `v_readlane`)
    /// serves it in its first pass, where the covered lanes' keys would
    /// never match it.
    pub(super) fn alone(
        &mut self,
        exec: Helpers,
        at: usize,
        bit: V,
        xy: (V, V),
    ) -> Result<(V, V), Why> {
        if !self.s.pixel {
            return Ok(xy);
        }
        match exec {
            Helpers::In => Ok(xy),
            Helpers::Out => {
                let lane = self.s.op(
                    at,
                    "waveGetLaneIndex",
                    Attr::ReadOnly,
                    self.s.t.i32,
                    111,
                    &[],
                );
                let (k31, k5, one, z) = (
                    self.s.m.i32(31),
                    self.s.m.i32(5),
                    self.s.m.i32(1),
                    self.s.m.i32(0),
                );
                let sh = self.s.m.bin(at, Bin::And, lane, k31);
                let own = self.s.m.bin(at, Bin::Shl, one, sh);
                let word = self.s.m.bin(at, Bin::LShr, lane, k5);
                let helper = self.is_helper(at);
                let mine = self.s.m.bin(at, Bin::And, helper, bit);
                let mut out = [xy.0, xy.1];
                for (n, w) in out.iter_mut().enumerate() {
                    let k = self.s.m.i32(n as u32);
                    let here = self.s.m.cmp(at, pred::EQ, word, k);
                    let set = self.s.m.bin(at, Bin::And, mine, here);
                    let own = self.s.m.select(at, set, own, z);
                    *w = self.s.m.select(at, helper, own, *w);
                }
                Ok((out[0], out[1]))
            }
            Helpers::Other => Err(HELPER_BITS),
        }
    }

    pub(super) fn boolean(&mut self, b: Bool, at: usize) -> Result<V, Why> {
        if b == Bool::TRUE || b == Bool::FALSE {
            return Ok(self.s.m.bool(b == Bool::TRUE));
        }
        if let Some(&v) = self.bools.get(&(at, b)) {
            return Ok(v);
        }
        let (atom, lo, hi) = self.x.st.bools.node(b).expect("not a constant");
        let a = self.atom(atom, at)?;
        let m = &mut self.s.m;
        let v = match (lo, hi) {
            (Bool::FALSE, Bool::TRUE) => a,
            (Bool::TRUE, Bool::FALSE) => {
                let t = m.bool(true);
                m.bin(at, Bin::Xor, a, t)
            }
            (Bool::FALSE, h) => {
                let h = self.boolean(h, at)?;
                self.s.m.bin(at, Bin::And, a, h)
            }
            (l, Bool::TRUE) => {
                let l = self.boolean(l, at)?;
                self.s.m.bin(at, Bin::Or, a, l)
            }
            (l, h) => {
                let l = self.boolean(l, at)?;
                let h = self.boolean(h, at)?;
                self.s.m.select(at, a, h, l)
            }
        };
        self.bools.insert((at, b), v);
        Ok(v)
    }

    pub(super) fn atom(&mut self, atom: Atom, at: usize) -> Result<V, Why> {
        match atom {
            Atom::Lane(v) => self.lane_bits.get(&v).copied().ok_or_else(|| {
                self.lane_fail
                    .get(&v)
                    .copied()
                    .unwrap_or("a lane mask from a carry")
            }),
            Atom::Uniform(v) => match self.get(v) {
                Ok(Val::Bit(x)) => Ok(x),
                Ok(Val::I32(x)) => {
                    let z = self.s.m.i32(0);
                    Ok(self.s.m.cmp(at, pred::NE, x, z))
                }
                Ok(Val::Missing(w)) => Err(w),
                // Mesh programs count no mask: the SCC of a count, "some
                // lane is set", is each lane's own bit, as a mask
                // operation's is in a branch (structurisation's rule).
                Err(w) => match self.count_scc(v) {
                    Some(m) if self.mask_data == MaskData::None => self.boolean(m, at),
                    _ => Err(w),
                },
            },
            Atom::Phi(id) => {
                if let Some(&v) = self.mask_phis.get(&id) {
                    return Ok(v);
                }
                let &(b, i) = self
                    .phi_at
                    .get(&id)
                    .ok_or("a lane-mask join without a phi")?;
                let blk = self.block[b].ok_or("an unreachable join")?;
                let v = self.s.m.phi(blk, self.s.t.i1);
                self.mask_phis.insert(id, v);
                self.pending_masks.push((v, b, i));
                Ok(v)
            }
            // shortcut: "some lane of the quad is set" is taken as true: the
            // lanes it adds only feed derivatives, and exports keep their own
            // masks. Wrong if such a section had a side effect.
            Atom::Quad(_) => Ok(self.s.m.bool(true)),
            // Split in halves, a host lane is not its console lane (the
            // upper half's lanes 0-31 are console lanes 32-63), so the
            // per-lane form has no bit of a mask held as a number to test
            // (a select or carry on one, which the split test does not
            // see): the console flow has.
            Atom::Opaque(_) | Atom::Constant(_) if self.split && !self.console_cf => {
                Err(if self.x.per_lane_only || !self.console_flow_runs() {
                    "a lane mask held as a number in a wave64 program split in halves"
                } else {
                    super::CONSOLE_FLOW
                })
            }
            // With console lanes, a mask held as a number is the console's:
            // the lane tests its own bit.
            Atom::Opaque(v) if self.console_lanes => {
                // The number may itself be known only as this mask's dword
                // (a mask copied dword by dword): no number to test then.
                if !self.opaque_open.insert(v) {
                    return Err("an opaque lane mask");
                }
                let words = (|| {
                    let hi = match self.pair_high(v) {
                        Some(h) => Some(self.get(h)?),
                        None => None,
                    };
                    Ok((self.get(v)?, hi))
                })();
                self.opaque_open.remove(&v);
                let (lo, hi) = words?;
                let word = |w: Val| match w {
                    Val::I32(x) => Ok(x),
                    _ => Err("an opaque lane mask"),
                };
                let lo = word(lo)?;
                let hi = hi.map(word).transpose()?;
                Ok(self.lane_bit(at, lo, hi))
            }
            Atom::Constant(k) if self.console_lanes => {
                let lo = self.s.m.i32(k as u32);
                let hi = self.s.m.i32((k >> 32) as u32);
                Ok(self.lane_bit(at, lo, Some(hi)))
            }
            Atom::Opaque(_) => Err("an opaque lane mask"),
            Atom::Constant(_) => Err("a constant lane mask"),
        }
    }

    /// This lane's bit of the mask `lo`, `hi` (wave64; host lane = console
    /// lane, so not in a program split in halves: [`Self::atom`] sends
    /// those to the console flow first).
    pub(super) fn lane_bit(&mut self, at: usize, lo: V, hi: Option<V>) -> V {
        debug_assert!(!self.split || self.console_cf);
        if self.console_cf {
            // The console lane's bit: its half's word, its host lane's bit.
            let (lane, half) = self.console_lane();
            let (k31, one, z) = (self.s.m.i32(31), self.s.m.i32(1), self.s.m.i32(0));
            let word = match hi {
                Some(hi) => {
                    let low = self.s.m.cmp(at, pred::EQ, half, z);
                    self.s.m.select(at, low, lo, hi)
                }
                None => lo,
            };
            let sh = self.s.m.bin(at, Bin::And, lane, k31);
            let t = self.s.m.bin(at, Bin::LShr, word, sh);
            let t = self.s.m.bin(at, Bin::And, t, one);
            return self.s.m.cmp(at, pred::NE, t, z);
        }
        let lane = self.s.op(
            at,
            "waveGetLaneIndex",
            Attr::ReadOnly,
            self.s.t.i32,
            111,
            &[],
        );
        let (k31, k32, one, z) = (
            self.s.m.i32(31),
            self.s.m.i32(32),
            self.s.m.i32(1),
            self.s.m.i32(0),
        );
        let word = match hi {
            Some(hi) => {
                let low = self.s.m.cmp(at, pred::IULT, lane, k32);
                self.s.m.select(at, low, lo, hi)
            }
            None => lo,
        };
        let sh = self.s.m.bin(at, Bin::And, lane, k31);
        let t = self.s.m.bin(at, Bin::LShr, word, sh);
        let t = self.s.m.bin(at, Bin::And, t, one);
        self.s.m.cmp(at, pred::NE, t, z)
    }

    /// The value of the high dword of the wave64 register pair whose low
    /// dword holds `lo` (none in wave32).
    pub(super) fn pair_high(&self, lo: Value) -> Option<Value> {
        if !self.x.native.wave64 {
            return None;
        }
        let p = self.x.p;
        let pick = |reg: Reg| match reg {
            Reg::S(c) => Some(Reg::S(c + 1)),
            _ => None,
        };
        if let Some(&(reg, _)) = p.inputs.iter().find(|i| i.1 == lo) {
            let h = pick(reg)?;
            return p.inputs.iter().find(|i| i.0 == h).map(|i| i.1);
        }
        if let Some(&(b, i)) = self.phi_at.get(&lo) {
            let h = pick(p.blocks[b].phis[i].reg)?;
            return p.blocks[b]
                .phis
                .iter()
                .find(|q| q.reg == h)
                .map(|q| q.value);
        }
        // Written with it by one instruction; else (two instructions, one
        // for each dword) the dword the mask's reader takes with it.
        let together = || {
            let b = *self.def_block.get(&lo)?;
            let inst = p.blocks[b]
                .insts
                .iter()
                .find(|i| i.writes.iter().any(|w| w.1 == lo))?;
            let &(reg, _) = inst.writes.iter().find(|w| w.1 == lo)?;
            inst.write(pick(reg)?)
        };
        together().or_else(|| self.x.st.pairs.get(&lo).copied())
    }

    pub(super) fn is_mask(&self, v: Option<Value>) -> bool {
        v.is_some_and(|v| self.x.st.masks.contains_key(&v))
    }

    pub(super) fn lane(&mut self, inst: &Inst, value: Option<bool>) {
        let b = self.s.m.bool(value == Some(true));
        self.lane_bits_for(inst, b);
    }

    pub(super) fn lane_bits_for(&mut self, inst: &Inst, r: V) {
        let Fields::Valu(v) = &inst.fields else {
            return;
        };
        let code = if inst.name().starts_with("v_cmpx") {
            Some(EXEC)
        } else {
            match v.dst {
                Some(Operand::Scalar { code, .. }) => Some(code),
                _ => None,
            }
        };
        if let Some(lo) = code.and_then(|c| inst.write(Reg::S(c))) {
            self.lane_bits.insert(lo, r);
        }
        for &(_, w) in &inst.writes {
            self.vals
                .entry(w)
                .or_insert(Val::Missing("a lane mask read as data"));
        }
    }

    /// The mask `s_bcnt1_i32_b*` counts where `scc` is that count's SCC
    /// ("the count is not zero": some lane of the mask is set), as lane bits.
    fn count_scc(&self, scc: Value) -> Option<Bool> {
        let p = self.x.p;
        let &b = self.def_block.get(&scc)?;
        let inst = p.blocks[b]
            .insts
            .iter()
            .find(|i| i.write(Reg::Scc) == Some(scc))?;
        let width = if self.x.native.wave64 { "64" } else { "32" };
        if inst.name().strip_prefix("s_bcnt1_i32_b") != Some(width) {
            return None;
        }
        let Fields::Sop { src, .. } = &inst.fields else {
            return None;
        };
        let &Operand::Scalar { code, .. } = src.first()? else {
            return None;
        };
        let lo = inst.read(Reg::S(code))?;
        let hi = if self.x.native.wave64 {
            Some(inst.read(Reg::S(code + 1))?)
        } else {
            None
        };
        self.x.st.mask_bits(lo, hi)
    }

    /// Whether `v_readfirstlane_b32` `inst`, in block `b`, is the first-lane
    /// read of the console compiler's test for a value the whole wave holds:
    ///
    /// ```text
    /// v_readfirstlane_b32 s, v
    /// v_cmp_eq_u32 m, s, v           (the lanes that hold the first lane's)
    /// s_xor_b64 d, m, exec           (those that do not)
    /// [s_bcnt1_i32_b64 n, d]
    /// s_cbranch_scc1 general         (some lane differs: a waterfall loop)
    /// ...                            (else the scalar path, which uses s)
    /// ```
    ///
    /// Each lane may take its own value as the first lane's: its compare
    /// then holds, its bit of `d` is clear, and it takes the scalar path with
    /// its own value, as the console's wave does when every lane holds it;
    /// the general path gives each lane what the scalar path gives it with
    /// its own value (it exists for the waves whose lanes differ). The
    /// branch is the lane's own bit (the SCC of a count in a mesh program,
    /// [`Self::atom`]). No read of `s` but the compare may lie where the
    /// scalar path does not decide it: every other reader is in a block the
    /// scalar path's first block dominates.
    pub(super) fn uniform_test_first_lane(&self, inst: &Inst, b: usize) -> bool {
        let p = self.x.p;
        let block = &p.blocks[b];
        let Fields::Valu(rf) = &inst.fields else {
            return false;
        };
        let (Some(&(Reg::S(_), s)), Some(&Operand::Vector { reg, .. })) =
            (inst.writes.first(), rf.src.first())
        else {
            return false;
        };
        let Some(v) = inst.read(Reg::V(reg)) else {
            return false;
        };
        let Some(at) = block
            .insts
            .iter()
            .position(|i| i.machine.offset == inst.machine.offset)
        else {
            return false;
        };
        let rest = &block.insts[at + 1..];
        let reads = |i: &Inst, x: Value| i.reads.iter().any(|r| r.1 == x);
        // The compare of the value with its first lane's.
        let Some(cmp) = rest.iter().find(|i| {
            matches!(i.name(), "v_cmp_eq_u32" | "v_cmp_eq_i32")
                && matches!(&i.fields, Fields::Valu(f) if f.sdwa.is_none() && f.dpp.is_none())
                && reads(i, s)
                && reads(i, v)
        }) else {
            return false;
        };
        let same: Vec<Value> = cmp
            .writes
            .iter()
            .filter(|w| matches!(w.0, Reg::S(_)))
            .map(|w| w.1)
            .collect();
        // The lanes that differ: the compare's mask against EXEC.
        let xor = if self.x.native.wave64 {
            "s_xor_b64"
        } else {
            "s_xor_b32"
        };
        let Some(differ) = rest.iter().find(|i| {
            i.name() == xor && i.read(Reg::S(EXEC)).is_some() && same.iter().any(|&m| reads(i, m))
        }) else {
            return false;
        };
        // The block's branch on "some lane differs".
        let Some(last) = block.insts.last() else {
            return false;
        };
        let taken_differ = match last.name() {
            "s_cbranch_scc1" => true,
            "s_cbranch_scc0" => false,
            _ => return false,
        };
        let crate::ir::Exit::Branch { taken, not_taken } = block.exit else {
            return false;
        };
        let Some(scc) = last.read(Reg::Scc) else {
            return false;
        };
        let counted = rest.iter().any(|i| {
            i.name().starts_with("s_bcnt1_i32_")
                && i.write(Reg::Scc) == Some(scc)
                && differ.writes.iter().any(|w| reads(i, w.1))
        });
        if differ.write(Reg::Scc) != Some(scc) && !counted {
            return false;
        }
        let scalar = if taken_differ { not_taken } else { taken };
        // Every other read of the value where the scalar path decides it.
        let order = crate::ir::reverse_postorder(&p.blocks);
        let mut index = vec![usize::MAX; p.blocks.len()];
        for (n, &q) in order.iter().enumerate() {
            index[q] = n;
        }
        let idom = crate::ir::idoms(&p.blocks, &order, &index);
        let dominated = |mut q: usize| loop {
            if q == scalar {
                break true;
            }
            if idom[q] == q || idom[q] == usize::MAX {
                break false;
            }
            q = idom[q];
        };
        p.blocks.iter().enumerate().all(|(q, blk)| {
            let insts = blk
                .insts
                .iter()
                .all(|i| std::ptr::eq(i, cmp) || !reads(i, s) || dominated(q));
            let phis = blk.phis.iter().all(|phi| {
                phi.args
                    .iter()
                    .zip(&blk.preds)
                    .all(|(&a, &pred)| a != s || dominated(pred))
            });
            insts && phis
        })
    }
}
