//! Two console lanes per physical lane for complete wave64 groups on a
//! wave32 host. Registers are pairs of SSA values. A scalar instruction
//! operates on each component, a host wave operation on each row of 32.
//! Console branches are wave-uniform, so a loop or phi has the same edges
//! for both components. Only the lowering's explicitly guarded effects
//! branch per lane: run their two arms in sequence and join each result
//! under its own predicate. A console group barrier executes once.

use super::opt::{Effect, effect, map_operands};
use super::{Bin, Module, Op, Term, Ty, V, for_each_operand};
use std::collections::HashMap;

#[derive(Clone, Default)]
pub(super) struct Halves {
    /// Missing entries have the same value in both lanes.
    upper: HashMap<V, V>,
    /// A guarded region has a distinct copy for the upper lanes.
    blocks: HashMap<usize, usize>,
    /// Join block -> (lower join, upper guard head).
    guards: HashMap<usize, (usize, usize)>,
}

impl Halves {
    fn value(&self, v: V) -> V {
        self.upper.get(&v).copied().unwrap_or(v)
    }

    fn block(&self, b: usize) -> usize {
        self.blocks.get(&b).copied().unwrap_or(b)
    }
}

impl Module {
    pub fn pair_lanes(&mut self) {
        self.paired = Some(Box::default());
    }

    pub fn paired_lanes(&self) -> bool {
        self.paired.is_some()
    }

    pub(super) fn push(&mut self, block: usize, ty: Ty, op: Op) -> V {
        let Some(pair) = &self.paired else {
            return self.push_one(block, ty, op);
        };
        let upper_block = pair.block(block);
        // Pure operations with identical operands in the same physical
        // lanes can share an SSA value. Each lane performs its own atomics
        // and stores even when their arguments happen to agree.
        let mut separate = block != upper_block;
        for_each_operand(&op, |v| separate |= pair.value(v) != v);
        separate |= match &op {
            Op::Call(f, _) => {
                let func = &self.funcs[*f as usize];
                matches!(effect(func), Effect::Writes) && func.name != "dx.op.barrier"
            }
            Op::Store(..) | Op::AtomicRmw(..) | Op::CmpXchg(..) => true,
            _ => false,
        };
        if !separate {
            return self.push_one(block, ty, op);
        }
        let mut other = op.clone();
        map_operands(&mut other, |v| pair.value(v));
        let lo = self.push_one(block, ty, op);
        let hi = self.push_one(upper_block, ty, other);
        self.paired.as_mut().unwrap().upper.insert(lo, hi);
        lo
    }

    /// Wave w, physical lane i owns console flat ids 64*w+i and 64*w+i+32.
    pub fn paired_flat(&mut self, at: usize, base: V, lane: V) -> V {
        let ty = self.type_of(lane);
        let wave = self.push_one(at, ty, Op::Bin(Bin::Add, base, base));
        let lo = self.push_one(at, ty, Op::Bin(Bin::Add, wave, lane));
        let k32 = self.i32(32);
        let hi = self.push_one(at, ty, Op::Bin(Bin::Add, lo, k32));
        self.paired.as_mut().unwrap().upper.insert(lo, hi);
        lo
    }

    /// Both logical lanes get both words: keep the original pair intact
    /// for later uses and give the low word its own (shared) SSA name.
    pub fn paired_words(&mut self, at: usize, x: V) -> (V, V) {
        let hi = self.paired.as_ref().unwrap().value(x);
        if hi == x {
            return (x, x);
        }
        let zero = self.i32(0);
        let lo = self.push_one(at, self.type_of(x), Op::Bin(Bin::Or, x, zero));
        (lo, hi)
    }

    pub fn phi(&mut self, b: usize, ty: Ty) -> V {
        let lo = self.phi_one(b, ty);
        if let Some(pair) = &self.paired {
            let hi = self.phi_one(pair.block(b), ty);
            self.paired.as_mut().unwrap().upper.insert(lo, hi);
        }
        lo
    }

    pub fn set_phi(&mut self, phi: V, incoming: Vec<(V, usize)>) {
        if let Some(pair) = &self.paired {
            let other = incoming
                .iter()
                .map(|&(v, b)| (pair.value(v), pair.block(b)))
                .collect();
            self.set_phi_one(pair.value(phi), other);
        }
        self.set_phi_one(phi, incoming);
    }

    pub fn guard_branch(&mut self, head: usize, g: V, then: usize, join: usize) {
        let Some(pair) = &self.paired else {
            self.terminate(head, Term::CondBr(g, then, join));
            return;
        };
        let high_g = pair.value(g);
        let high_head = pair.block(head);
        let high_then = self.new_block();
        // At the outermost guard the low join starts the upper guard. A
        // nested guard already has separate heads and needs separate joins.
        let (low_join, high_head, high_join) = if high_head == head {
            let bridge = self.new_block();
            (bridge, bridge, join)
        } else {
            (join, high_head, self.new_block())
        };
        let pair = self.paired.as_mut().unwrap();
        pair.blocks.insert(then, high_then);
        if high_join != join {
            pair.blocks.insert(join, high_join);
        }
        pair.guards.insert(join, (low_join, high_head));
        self.terminate(head, Term::CondBr(g, then, low_join));
        self.terminate(high_head, Term::CondBr(high_g, high_then, high_join));
    }

    pub fn guard_end(&mut self, then: usize, join: usize) {
        let low_join = if let Some(pair) = &self.paired {
            let (low_join, _) = pair.guards[&join];
            self.terminate(pair.block(then), Term::Br(pair.block(join)));
            low_join
        } else {
            join
        };
        self.terminate(then, Term::Br(low_join));
    }

    pub fn guard_phi(&mut self, join: usize, ty: Ty, inside: (V, usize), outside: (V, usize)) -> V {
        let Some(pair) = &self.paired else {
            let phi = self.phi_one(join, ty);
            self.set_phi_one(phi, vec![inside, outside]);
            return phi;
        };
        let (low_join, high_head) = pair.guards[&join];
        let high_join = pair.block(join);
        let high_edges = vec![
            (pair.value(inside.0), pair.block(inside.1)),
            (pair.value(outside.0), high_head),
        ];
        let lo = self.phi_one(low_join, ty);
        let hi = self.phi_one(high_join, ty);
        self.set_phi_one(lo, vec![inside, outside]);
        self.set_phi_one(hi, high_edges);
        self.paired.as_mut().unwrap().upper.insert(lo, hi);
        lo
    }
}
