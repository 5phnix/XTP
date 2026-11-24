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
//!   hold;
//! * group barriers: on the console a wave reaches `s_barrier` whatever
//!   its EXEC, here each lane only on its own path, and a host wave that
//!   never gets there hangs the GPU; each barrier the lowering keeps must
//!   be in a block every lane of the group runs as often as the others;
//! * LDS a group of one console wave shares: the console compiler leaves
//!   out the barriers between its lanes, which two host waves need.
//!
//! The output counts need nothing: they are the declared maxima, set once
//! in the entry block for the whole group (`mesh_prologue`).

use super::uniform::{Level, Uniformity};
use super::{Lower, Why};
use crate::emit::bitcode::{Attr, V, pred};
use crate::native::StageIo;

/// A wave64 mesh program's first-lane read of a value that may differ
/// between the lanes of its console wave, on a host without 64-lane waves.
pub(super) const MESH_FIRST_LANE: Why =
    "a first-lane read of a value lanes may hold differently, in a wave64 mesh program on a host without 64-lane waves";

/// A wave64 mesh program's group barrier in a block that lanes of the group
/// reach differently (or a different number of times), on a host without
/// 64-lane waves.
pub(super) const MESH_BARRIER: Why =
    "a group barrier lanes of a wave64 mesh group reach differently, on a host without 64-lane waves";

/// A wave64 mesh program whose group is one console wave of more than 32
/// lanes, with LDS, on a host without 64-lane waves.
pub(super) const MESH_LDS_IN_ONE_WAVE: Why =
    "LDS shared within one wave64 mesh wave, on a host without 64-lane waves";

impl<'a> Lower<'a> {
    /// Mesh: the counts and the primitives no lane exports. The console's
    /// counts come from one lane's allocation request, which DXIL cannot
    /// express (one call, dominating every output write).
    // shortcut: the counts are the declared maxima and each lane first culls
    // its primitive (all indices 0, zero area), so primitives past the
    // program's count are culled and vertices past it unused; this costs
    // the host the unused vertex and primitive slots. Pass the program's own
    // counts if that shows in frame time.
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
        if ms.lds_bytes > 0 && ms.threads > 32 && ms.threads <= 64 {
            return Err(MESH_LDS_IN_ONE_WAVE);
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

    /// Mesh: whether the `v_readfirstlane_b32` writing `w` reads a value
    /// the same in every lane of its console wave ([`super::uniform`]).
    pub(super) fn mesh_first_lane_uniform(&self, w: Option<crate::ir::Value>) -> bool {
        let level = w.and_then(|w| self.uniform.as_ref()?.first_lane(w));
        level.is_some_and(|l| l <= Level::Wave)
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
