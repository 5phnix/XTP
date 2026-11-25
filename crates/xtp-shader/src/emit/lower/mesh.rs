//! Mesh programs: the output counts and the primitives no lane exports
//! (the prologue), the thread index in the group, and what a console wave64
//! program needs on a host without 64-lane waves. Backlog G2 (merged ES +
//! GS as one mesh shader) and L4.
//!
//! # Wave64 programs on 32-lane hosts
//!
//! D3D12 gives a mesh shader no wave size to ask for, so a console wave64
//! program may run in host waves of 32 (NVIDIA) or fewer (WARP). Its lanes
//! are numbered from the flat thread id whatever the host's waves are
//! (console lane = id % 64, console wave = id / 64: `lane_index`, the
//! merged wave info), and mesh code reads no other lanes but within quads.
//! What still depends on whole console waves:
//!
//! * `v_readfirstlane_b32`: a value proven the same in every lane of the
//!   console wave ([`super::uniform`]) is each lane's own (`valu.rs`); the
//!   others need the console wave's first lane, which the host wave may not
//!   hold: they go through group-shared memory between group barriers
//!   (`mesh_first_lane`), where every lane of the group runs the block;
//! * group barriers: on the console a wave reaches `s_barrier` whatever
//!   its EXEC, here each lane only on its own path, and a host wave that
//!   never gets there hangs the GPU; each barrier the lowering keeps must
//!   be in a block every lane of the group runs as often as the others;
//! * LDS a group of one console wave shares: the console compiler leaves
//!   out the barriers between its lanes, which two host waves need;
//! * LDS order within a console wave of a larger group: one wave's `ds_*`
//!   instructions run in order on the console, so a lane may read another
//!   lane's write (or write after its read) with no `s_barrier` between,
//!   where two host waves race. A program with an LDS read after a write,
//!   or a write after a read, on a path of the console's flow with no
//!   group barrier the lowering keeps between them, is refused
//!   (`mesh_lds_unordered`). None of BTP 6's mesh programs has one: each
//!   of the five with LDS writes it all, crosses one barrier, then reads
//!   (`docs/analysis/mesh-wave64.md`). Two writes are not ordered against
//!   each other: two lanes writing one dword with no barrier between race
//!   in the HLSL program already.
//!
//! The output counts need nothing: they are the declared maxima, set once
//! in the entry block for the whole group (`mesh_prologue`).

use super::uniform::{Level, Uniformity};
use super::{Lower, Why};
use crate::emit::bitcode::{Attr, Bin, Rmw, V, pred};
use crate::native::StageIo;

/// A wave64 mesh program's first-lane read of a value that may differ
/// between the lanes of its console wave, in a block that lanes of the
/// group reach differently, on a host without 64-lane waves.
pub(super) const MESH_FIRST_LANE: Why = "a first-lane read of a value lanes may hold differently, where lanes of a wave64 mesh group branch differently, on a host without 64-lane waves";

/// A wave64 mesh program whose LDS leaves no room for the slots of its
/// first-lane reads (two dwords per console wave), or would read them.
pub(super) const MESH_NO_ROOM: Why = "LDS that leaves no room for the first-lane reads of a wave64 mesh program, on a host without 64-lane waves";

/// D3D12's limit on a mesh shader's group-shared memory (28 KiB).
pub(super) const MESH_LDS_DWORDS: u32 = 28 * 1024 / 4;

/// A wave64 mesh program's group barrier in a block that lanes of the group
/// reach differently (or a different number of times), on a host without
/// 64-lane waves.
pub(super) const MESH_BARRIER: Why = "a group barrier lanes of a wave64 mesh group reach differently, on a host without 64-lane waves";

/// A wave64 mesh program whose group is one console wave of more lanes than
/// the host's smallest wave, with LDS, on a host without 64-lane waves.
pub(super) const MESH_LDS_IN_ONE_WAVE: Why =
    "LDS shared within one wave64 mesh wave, on a host without 64-lane waves";

/// A wave64 mesh program of several console waves whose LDS accesses only
/// its console waves order (see the module notes), on a host without
/// 64-lane waves.
pub(super) const MESH_LDS_ORDER: Why = "LDS read after a write, or written after a read, with no group barrier between, in a wave64 mesh program on a host without 64-lane waves";

/// An LDS access's kinds ([`lds_kinds`]).
const READS: u8 = 1;
const WRITES: u8 = 2;

/// What `inst` does to the program's LDS: reads it, writes it, or both
/// (atomics, exchanges, GWS and counters); 0 for no access (swizzles and
/// permutes move data between lanes, not through LDS).
fn lds_kinds(inst: &crate::ir::Inst) -> u8 {
    let n = inst.name();
    match inst.fields {
        crate::gfx10::Fields::Buffer { lds: true, .. } => WRITES,
        crate::gfx10::Fields::Ds { .. } => {
            if n.starts_with("ds_swizzle") || n.contains("permute") || n == "ds_nop" {
                0
            } else if n.starts_with("ds_read") {
                READS
            } else if n.starts_with("ds_write") {
                WRITES
            } else {
                READS | WRITES
            }
        }
        _ => 0,
    }
}

impl<'a> Lower<'a> {
    /// Mesh: the counts and the primitives no lane exports. The console's
    /// counts come from one lane's allocation request, which DXIL cannot
    /// express (one call, dominating every output write).
    // shortcut: the counts are the declared maxima and each lane first culls
    // its primitive (all indices 0, zero area), so primitives past the
    // program's count are culled and vertices past it unused; this costs
    // the host the unused vertex and primitive slots. Pass the program's own
    // counts if that shows in frame time.
    // shortcut: D3D12 limits a mesh shader's outputs to 32 KiB (vertex
    // scalars × maximum vertices plus primitive scalars × maximum
    // primitives, 4 bytes each), which `dxil.dll` checks and the host's
    // driver does not. 53 of BTP 6's mesh programs (character skin and
    // transform programs, groups of 128 lanes, 128 vertices, 65 to 71
    // scalars a vertex) are declared past it, 33,280 to 36,352 bytes. The
    // maxima are the group's lanes, one vertex each, so they cannot shrink;
    // 25 to 34 of those scalars are 16-bit values, which declared two per
    // dword would fit, the pixel programs that read them unpacking them.
    pub(super) fn mesh_prologue(&mut self, mesh: crate::emit::shader::Mesh) -> Result<(), Why> {
        let threads = self.s.threads.expect("mesh")[0];
        if mesh.topology != 2 {
            return Err("a mesh shader not drawing triangles");
        }
        if threads > 128 || mesh.max_vertices > 256 || mesh.max_primitives > 256 {
            return Err("a mesh shader beyond the host's limits");
        }
        self.uniform = Some(Uniformity::of(self.x));
        if self.mesh_halves() {
            self.mesh_waves_kept()?;
        }
        let e = self.s.entry;
        let void = self.s.t.void;
        let (v, p) = (
            self.s.m.i32(mesh.max_vertices),
            self.s.m.i32(mesh.max_primitives),
        );
        self.s
            .op(e, "setMeshOutputCounts", Attr::NoUnwind, void, 168, &[v, p]);
        let flat = self.mesh_thread();
        let guard = (threads > mesh.max_primitives).then(|| self.s.m.cmp(e, pred::IULT, flat, p));
        let at = self.begin_guarded(guard);
        let z = self.s.m.i32(0);
        self.s.op(
            self.cur,
            "emitIndices",
            Attr::NoUnwind,
            void,
            169,
            &[flat, z, z, z],
        );
        self.end_guarded(at, &[]);
        Ok(())
    }

    /// Mesh: whether this is a console wave64 program on a host whose
    /// waves may hold part of a console wave only (no 64-lane waves).
    pub(super) fn mesh_halves(&self) -> bool {
        self.s.mesh.is_some() && self.x.native.wave64 && !self.x.target.runs(64)
    }

    /// Mesh, on a host without 64-lane waves: refuses what needs whole
    /// console waves besides first-lane reads (see the module notes).
    fn mesh_waves_kept(&self) -> Result<(), Why> {
        let StageIo::Ms(ms) = &self.x.native.io else {
            return Ok(());
        };
        // A group no larger than the host's smallest wave is one host wave,
        // whose LDS accesses run in order as the console's do.
        let split = ms.threads > self.x.target.lanes_min;
        if ms.lds_bytes > 0 && split && ms.threads <= 64 {
            return Err(MESH_LDS_IN_ONE_WAVE);
        }
        if split && self.mesh_lds_unordered() {
            return Err(MESH_LDS_ORDER);
        }
        let uniform = self.uniform.as_ref().expect("analysed");
        let p = self.x.p;
        let divergent = (0..p.blocks.len()).find(|&b| {
            !self.no_barrier.contains(&b)
                && p.blocks[b].insts.iter().any(|i| i.name() == "s_barrier")
                && uniform.control(b) != Level::Group
        });
        match divergent {
            Some(_) => Err(MESH_BARRIER),
            None => Ok(()),
        }
    }

    /// Mesh: whether an LDS access may follow one of the other kind (a
    /// read after a write, a write after a read) with no group barrier the
    /// lowering keeps between them. Over the program's own edges: the
    /// console's flow, which runs both sides of a branch its lanes take
    /// apart, one after the other, and follows back edges into the next
    /// iteration. Per block on entry: the kinds of access since the last
    /// kept barrier, on any path there.
    fn mesh_lds_unordered(&self) -> bool {
        let p = self.x.p;
        let kept = |b: usize, i: &crate::ir::Inst| {
            i.name() == "s_barrier" && !self.no_barrier.contains(&b)
        };
        let order = crate::ir::reverse_postorder(&p.blocks);
        let mut entry = vec![0u8; p.blocks.len()];
        let mut unordered = false;
        loop {
            let mut changed = false;
            for &b in &order {
                let mut since = entry[b];
                for inst in &p.blocks[b].insts {
                    if kept(b, inst) {
                        since = 0;
                        continue;
                    }
                    let k = lds_kinds(inst);
                    unordered |= k & READS != 0 && since & WRITES != 0
                        || k & WRITES != 0 && since & READS != 0;
                    since |= k;
                }
                for t in p.blocks[b].exit.successors() {
                    if entry[t] | since != entry[t] {
                        entry[t] |= since;
                        changed = true;
                    }
                }
            }
            if !changed {
                return unordered;
            }
        }
    }

    /// Mesh: whether the `v_readfirstlane_b32` writing `w` reads a value
    /// the same in every lane of its console wave ([`super::uniform`]).
    pub(super) fn mesh_first_lane_uniform(&self, w: Option<crate::ir::Value>) -> bool {
        let level = w.and_then(|w| self.uniform.as_ref()?.first_lane(w));
        level.is_some_and(|l| l <= Level::Wave)
    }

    /// Mesh, on a host without 64-lane waves: `x` of the first lane in
    /// EXEC of this lane's console wave (lane 0 when none), through
    /// group-shared memory. `active`: the lane's EXEC bit here, `None`
    /// when every lane that reaches the instruction has it.
    ///
    /// Every lane of the group must get here as often as the others: the
    /// exchange is between group barriers ([`Uniformity::control`]). With
    /// every lane active, the first lane is the console wave's lane 0,
    /// which stores `x` for its wave: one barrier, then all read, then one
    /// more before the slot can change. Otherwise lane 0 empties its wave's
    /// slot, the active lanes take its minimum with their lane, and the
    /// lane found (lane 0 when none) stores `x`: four barriers. The slots
    /// are two dwords per console wave ([`Self::mesh_slots`]).
    pub(super) fn mesh_first_lane(&mut self, x: V, active: Option<V>) -> Result<V, Why> {
        let uniform = self.uniform.as_ref().expect("analysed");
        if uniform.control(self.cur_ir) != Level::Group {
            return Err(MESH_FIRST_LANE);
        }
        let slots = self.mesh_slots()?;
        let c = self.cur;
        let flat = self.mesh_thread();
        let (k6, k63, one, zero) = (
            self.s.m.i32(6),
            self.s.m.i32(63),
            self.s.m.i32(1),
            self.s.m.i32(0),
        );
        let wave = self.s.m.bin(c, Bin::LShr, flat, k6);
        let lane = self.s.m.bin(c, Bin::And, flat, k63);
        let first = self.s.m.bin(c, Bin::Shl, wave, one);
        let value = self.s.m.bin(c, Bin::Add, first, one);
        let first = self.s.m.element(c, slots, first);
        let value = self.s.m.element(c, slots, value);
        let lead = self.s.m.cmp(c, pred::EQ, lane, zero);
        let owner = match active {
            None => lead,
            Some(on) => {
                let none = self.s.m.i32(u32::MAX);
                let at = self.begin_guarded(Some(lead));
                self.s.m.store(self.cur, first, none);
                self.end_guarded(at, &[]);
                self.group_sync();
                let at = self.begin_guarded(Some(on));
                self.s.m.atomic(self.cur, Rmw::UMin, first, lane);
                self.end_guarded(at, &[]);
                self.group_sync();
                let c = self.cur;
                let f = self.s.m.load(c, first);
                let empty = self.s.m.cmp(c, pred::EQ, f, none);
                let f = self.s.m.select(c, empty, zero, f);
                self.s.m.cmp(c, pred::EQ, lane, f)
            }
        };
        let at = self.begin_guarded(Some(owner));
        self.s.m.store(self.cur, value, x);
        self.end_guarded(at, &[]);
        self.group_sync();
        let r = self.s.m.load(self.cur, value);
        // No lane stores the slot again before every lane has read it.
        self.group_sync();
        Ok(r)
    }

    /// Mesh: the group-shared slots of first-lane reads, two dwords per
    /// console wave, made on first use ([`Lower::xchg`], which only the
    /// compute halves' exchange uses otherwise). The container's
    /// group-shared size counts them: `lds_dwords` grows by their size once
    /// the program's own LDS array, if it uses one, has its size (LDS
    /// accesses check their addresses against `lds_dwords` no more then:
    /// none that the size could change is let through, see below).
    fn mesh_slots(&mut self) -> Result<V, Why> {
        if let Some(g) = self.xchg {
            return Ok(g);
        }
        let StageIo::Ms(ms) = &self.x.native.io else {
            unreachable!("mesh")
        };
        let (own, waves) = (ms.lds_bytes.div_ceil(4), ms.threads.div_ceil(64));
        let ds: Vec<&str> = (self.x.p.blocks.iter().flat_map(|b| &b.insts))
            .filter(|i| matches!(i.fields, crate::gfx10::Fields::Ds { .. }))
            .map(|i| i.name())
            .filter(|n| !n.starts_with("ds_swizzle") && *n != "ds_bpermute_b32")
            .collect();
        // Refused for LDS of their own: accesses without LDS (`lds.rs` would
        // take them once `lds_dwords` grows), and halves that may straddle
        // the end of the program's LDS (clamped to `lds_dwords`).
        if own + 2 * waves > MESH_LDS_DWORDS
            || own == 0 && !ds.is_empty()
            || own > 0 && ds.iter().any(|n| n.contains("16"))
        {
            return Err(MESH_NO_ROOM);
        }
        if !ds.is_empty() {
            // At the program's own size, before `lds_dwords` grows.
            self.s.lds();
        }
        let arr = self.s.m.array(2 * waves, self.s.t.i32);
        let g = self.s.m.global("first_lane", arr, 3);
        // (LDS the program allocates but never accesses has no array.)
        self.s.lds_dwords = if ds.is_empty() { 0 } else { own } + 2 * waves;
        self.xchg = Some(g);
        Ok(g)
    }

    /// A group barrier with a group-shared memory fence.
    fn group_sync(&mut self) {
        let void = self.s.t.void;
        let mode = self.s.m.i32(1 | 8);
        self.s
            .op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }

    /// Mesh: the thread's index in the group, which is its vertex and
    /// primitive index.
    pub(super) fn mesh_thread(&mut self) -> V {
        if let Some(f) = self.flat {
            return f;
        }
        let e = self.s.entry;
        let f = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            self.s.t.i32,
            96,
            &[],
        );
        self.flat = Some(f);
        f
    }
}
