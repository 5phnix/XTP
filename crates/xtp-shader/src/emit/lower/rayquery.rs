//! Inline ray queries in place of a trace program's traversal walk (RT
//! phase 2b, `docs/analysis/rt-dxr.md` sections 5 and 7).
//!
//! BTP 6's trace programs walk the console's BVH themselves: a loop around
//! `image_bvh_intersect_ray` with a short stack. The host has no such
//! instruction; it has DXR 1.1's inline ray queries, which walk the host's
//! own acceleration structures on its RT cores. [`splice`] replaces a walk
//! whose site is described ([`Site`]) by one instruction of its own, a VALU
//! instruction no machine code encodes ([`RAY_QUERY`]). It reads the ray at
//! the walk's entry and writes, by kind ([`Kind`]), what the code after the
//! walk reads of it; that code runs unchanged. The lowering
//! ([`Lower::ray_query`]) emits `TraceRayInline` with an empty `Proceed`
//! loop, then the committed hit's status and t (section 5.4).
//!
//! The acceleration structure is a root SRV at a reserved register,
//! [`TLAS_REGISTER`] in [`ROOT_CBV_SPACE`], listed among the shader's root
//! SRVs as the title's root views are (`HostShader::root_srvs`), declared
//! as resource kind 16 (`RTAccelerationStructure`). The host binds its own
//! TLAS there at the dispatch (section 7), the one it built for the
//! structure the title's descriptor names ([`tlas_descriptor`]).
//!
//! Gate (section 5.2): a program is spliced only when [`SITES`] names it,
//! by the fingerprint of its code ([`fingerprint`]), and only when its walk
//! is proven by play on synthetic worlds at its translation (section 5.3,
//! [`proof`]): the program played with its walk ([`play`]) and played with
//! the host query in its place give the same answers ([`world`]). A walk the
//! play does not prove stays refused, as before. The tests prove a walk of
//! their own (`emit/rayquery_tests.rs`).
//!
//! The method (result kinds, a host query standing for the walk, a proof
//! by play) is the person's AnyPS5 design (`RayQueryResults`,
//! `RayQuerySource`, simnJS/AnyPS5), reimplemented on XTP's IR and DXIL; no
//! code of it is copied (rt-dxr.md section 2.2).

use std::collections::{HashMap, HashSet};

#[cfg(test)]
pub(crate) mod asm;
pub(crate) mod play;
pub(crate) mod proof;
mod sites;
pub(crate) mod world;

use sites::SITES;

use super::super::bitcode::{Attr, Cast, Md, Module, Term, Ty, V, pred};
use super::super::container::Binding;
use super::super::shader::{ROOT_CBV_SPACE, flags};
use super::{Lower, Stop, Val, Why};
use crate::ShaderError;
use crate::gfx10::{Encoding, Fields, Instruction, Valu};
use crate::ir::{Block, EXEC, Exit, Inst, Phi, Program, Reg, Value};
use crate::native::{NativeProgram, RootType};

/// The register of the acceleration structure's root SRV: `t240` in
/// [`ROOT_CBV_SPACE`], past every title root parameter (a root signature
/// has at most 64).
pub const TLAS_REGISTER: u8 = 240;

/// DXR ray flags (`RAY_FLAG_*`).
pub(crate) mod ray_flags {
    pub const FORCE_OPAQUE: u32 = 0x01;
    pub const CULL_BACK_FACING_TRIANGLES: u32 = 0x10;
}

/// The VOP3 opcode of the spliced instruction: past the last one GFX10
/// defines (0x37f), so no decoded instruction has it.
pub(crate) const RAY_QUERY: u16 = 0x3ff;

/// The first word of the spliced instruction ("RAYQ").
const MAGIC: u32 = 0x5241_5951;

/// What the code after a walk reads of one of its registers (section 5.3;
/// AnyPS5's `RayQueryResultKind`, the kinds the prototype needs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Not zero when a triangle was hit: 1 on the host, 0 otherwise. The
    /// walk leaves a node pointer there, which the code after it only
    /// compares with zero.
    Hit,
    /// The committed hit's t, or the ray's t-max on a miss (as the walk
    /// leaves it: t-max only shrinks on a hit).
    ///
    /// Accepted difference at the ends of the ray: the console's walk keeps
    /// a triangle at 0 <= t < t-max, DXR at t-min < t < t-max (the DXR
    /// specification, "Ray extents"). A hit exactly at t-min (a ray leaving
    /// a surface) counts on the console and not on the host. At t-max both
    /// leave it out by their rules; WARP keeps it (measured in
    /// `rayquery_tests`), which the rule leaves to the implementation.
    T,
    /// An iteration count that the code after the walk only compares with
    /// the walk's cap ([`Site::cap`]): 0 on the host, which has no cap
    /// (section 9). A walk that passes the cap (0xfffe in family A) counts
    /// as a hit at its current t-max on the console; the host answers the
    /// whole query. The proof leaves such lanes out, and checks that the
    /// code after the walk stores the same values for the others.
    Count,
}

impl Kind {
    fn code(self) -> u32 {
        match self {
            Kind::Hit => 0,
            Kind::T => 1,
            Kind::Count => 2,
        }
    }

    fn of(code: u32) -> Option<Kind> {
        [Kind::Hit, Kind::T, Kind::Count]
            .into_iter()
            .find(|k| k.code() == code)
    }
}

/// A walk's site (section 5.2): where it is, its ray, its query and what
/// the code after it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Site {
    /// Byte offset of the walk's loop header.
    pub header: usize,
    /// Byte offset of the block the walk leaves to. The walk is every block
    /// from the header's offset up to this one.
    pub exit: usize,
    /// Byte offset of the block a check before the walk jumps to when the
    /// guest structure's root (its header's dword at +0x30) is empty: a
    /// block of the walk's range that leads to the exit without walking.
    /// The guest structure is the title's, which the host never fills (its
    /// builder does not run, section 3.5): its root reads 0 there, while
    /// the host's structure is the one queried. So the check always goes on
    /// to the walk, and the root's read goes with the walk's other reads.
    /// (On an empty host structure the query misses, as the skipped walk
    /// does.)
    pub skip: Option<usize>,
    /// The VGPRs holding the ray's origin, direction and t-max where the
    /// walk is entered.
    pub origin: [u8; 3],
    pub direction: [u8; 3],
    pub t_max: u8,
    /// t-min, a constant (float bits).
    pub t_min: u32,
    /// The instance inclusion mask.
    pub mask: u8,
    /// The ray flags ([`ray_flags`]).
    pub flags: u32,
    /// The VGPRs the code after the walk reads of it, with what each holds.
    pub results: &'static [(u8, Kind)],
    /// Registers the walk changes and gives back as it found them, which
    /// the code after it reads (EXEC, which a walk narrows to lanes and
    /// restores): proven by play.
    pub kept: &'static [Reg],
    /// The iteration count past which the walk gives up ([`Kind::Count`]).
    pub cap: Option<u32>,
    /// Where the guest structure's buffer descriptor is: the dispatch looks
    /// the host TLAS up from its address (section 8), one per descriptor.
    pub tlas: Source,
}

/// A buffer descriptor in a user-data table: the user SGPR that holds the
/// table's address, and the descriptor's byte offset in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Source {
    pub user_sgpr: u8,
    pub offset: u16,
}

/// The site [`SITES`] (`rayquery/sites/`) names for a program's code, if
/// any.
pub(crate) fn site_of(code: &[u8]) -> Option<&'static Site> {
    let key = fingerprint(code);
    SITES.iter().find(|(k, _)| *k == key).map(|(_, s)| s)
}

/// Where the dispatch of a spliced program finds the title's view of the
/// structure its walk read (section 8): the descriptor at byte `offset` of
/// the descriptor table the title binds at root parameter `slot`. The host
/// looks its own TLAS up from that descriptor (`xtp_gpu::rt`) and binds it
/// at [`TLAS_REGISTER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlasDescriptor {
    /// The root parameter, in the title's root signature, of the table.
    pub slot: u8,
    /// The descriptor's byte offset in the table.
    pub offset: u32,
}

/// The [`TlasDescriptor`] of a program [`SITES`] names: the descriptor
/// table whose address its site's user SGPR holds ([`Site::tlas`]). `None`
/// for any other program, and for a site whose SGPR does not start a table
/// of the program's root signature layout (no translation splices it then:
/// [`prepared`] refuses it).
pub fn tlas_descriptor(native: &NativeProgram) -> Option<TlasDescriptor> {
    let site = site_of(&native.code)?;
    table_of(native, &site.tlas)
}

/// [`tlas_descriptor`] of the program of a container (the bytes the title
/// gives, as `translate` takes them).
pub fn container_tlas_descriptor(container: &[u8]) -> Option<TlasDescriptor> {
    let c = crate::dxbc::Container::parse(container).ok()?;
    let part = c.part(crate::dxbc::FourCC::DXIL)?;
    crate::native_programs(part.data)
        .ok()?
        .iter()
        .find_map(tlas_descriptor)
}

/// The table [`Source`] reads in `native`'s root signature layout.
pub(crate) fn table_of(native: &NativeProgram, source: &Source) -> Option<TlasDescriptor> {
    let mut tables = native.layout.iter().filter(|rp| {
        rp.root == RootType::Table && native.stage.user_sgpr(rp.user_data) == source.user_sgpr
    });
    let first = tables.next()?;
    // Every range of one table shares its SGPR: one parameter.
    if tables.any(|rp| rp.slot != first.slot) {
        return None;
    }
    Some(TlasDescriptor {
        slot: first.slot,
        offset: u32::from(source.offset),
    })
}

/// FNV-1a (64 bits) of a program's code: how [`SITES`] names it.
pub(crate) fn fingerprint(code: &[u8]) -> u64 {
    code.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The program with the walk [`SITES`] names for it spliced, once proven
/// by play, or as it is. Runs on the IR before anything else reads it
/// (after `geometry::prepared`): the resource tracker then never sees the
/// walk's node reads. A walk the play does not prove is refused where the
/// play stopped.
pub(crate) fn prepared(p: Program, native: &NativeProgram) -> Result<Program, ShaderError> {
    match site_of(&native.code) {
        Some(site) => spliced(p, native, site),
        None => Ok(p),
    }
}

/// [`prepared`] for the walk at `site`: its structure's descriptor must be
/// in a table the dispatch finds ([`tlas_descriptor`]), its walk proven by
/// play, then spliced.
pub(crate) fn spliced(
    p: Program,
    native: &NativeProgram,
    site: &Site,
) -> Result<Program, ShaderError> {
    if table_of(native, &site.tlas).is_none() {
        return Err(ShaderError::Unsupported {
            offset: site.header,
            what: "a ray-tracing walk whose structure is not in one descriptor table",
        });
    }
    proof::prove(&p, native, site).map_err(|f| ShaderError::Unsupported {
        offset: f.offset,
        what: f.why,
    })?;
    splice(p, site).map_err(|what| ShaderError::Unsupported {
        offset: site.header,
        what,
    })
}

/// The value register `reg` holds at the end of block `b`.
fn value_at_end(p: &Program, mut b: usize, reg: Reg) -> Option<Value> {
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(b) {
            return None;
        }
        let block = &p.blocks[b];
        if let Some(v) = block.insts.iter().rev().find_map(|i| i.write(reg)) {
            return Some(v);
        }
        if let Some(phi) = block.phis.iter().find(|phi| phi.reg == reg) {
            return Some(phi.value);
        }
        // Without a phi every predecessor brings the same value.
        match block.preds.first() {
            Some(&q) => b = q,
            None => return p.inputs.iter().find(|i| i.0 == reg).map(|i| i.1),
        }
    }
}

/// One past the highest value the program names.
fn next_value(p: &Program) -> u32 {
    let phis = p
        .blocks
        .iter()
        .flat_map(|b| &b.phis)
        .flat_map(|phi| std::iter::once(phi.value).chain(phi.args.iter().copied()));
    let insts = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .flat_map(|i| i.reads.iter().chain(&i.writes).map(|r| r.1));
    p.inputs
        .iter()
        .map(|i| i.1)
        .chain(phis)
        .chain(insts)
        .map(|v| v.0 + 1)
        .max()
        .unwrap_or(0)
}

/// `p` with the walk at `site` replaced by one ray-query instruction:
///
/// * the walk is every block whose offset is in `site.header..site.exit`;
///   it must be entered only at its header, from one block outside it,
///   and left only to the block at `site.exit`, which its header reaches;
///   no path of it may end the program (`Exit::End`, `Exit::Indirect`);
/// * it may read memory, intersect, branch and keep a stack in LDS that
///   nothing outside it touches; any other effect (a store, an atomic, a
///   message...) refuses the splice, since the query does not have it;
/// * the empty-structure check (`site.skip`), one conditional branch
///   outside the walk to the skip path, becomes a jump to its other side,
///   which must lead to the walk; the skip path goes with the walk;
/// * the instruction reads the ray's registers as they are at the end of
///   the block that enters the walk, and EXEC there (it runs in the lanes
///   that entered the walk);
/// * it writes each register of `site.results`; the code after the walk
///   reads those instead of what the walk left, and reads the registers of
///   `site.kept` as they were where the walk started. Any other value of
///   the walk read after it refuses the splice;
/// * then what only the walk read is removed ([`dead_code`]).
pub(crate) fn splice(mut p: Program, site: &Site) -> Result<Program, Why> {
    if p.wave64 {
        return Err("a ray-tracing walk in a wave64 program");
    }
    let one = |offset: usize| -> Result<usize, Why> {
        let mut at = p
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.offset == offset)
            .map(|(n, _)| n);
        match (at.next(), at.next()) {
            (Some(b), None) => Ok(b),
            _ => Err("a ray-tracing walk whose header or exit is not one block"),
        }
    };
    if site.exit <= site.header {
        return Err("a ray-tracing walk that ends before its header");
    }
    let header = one(site.header)?;
    let exit = one(site.exit)?;
    let walk: Vec<bool> = p
        .blocks
        .iter()
        .map(|b| (site.header..site.exit).contains(&b.offset))
        .collect();
    // A walk that ends the program, or jumps through registers, does not
    // go on to its exit on every path: the query would.
    if p.blocks
        .iter()
        .zip(&walk)
        .any(|(b, &w)| w && matches!(b.exit, Exit::End | Exit::Indirect))
    {
        return Err("a ray-tracing walk with a path that ends the program");
    }
    // What the walk does besides its registers must be what the query
    // leaves out: reads, and LDS the code outside it never touches.
    let lds = |i: &Inst| match &i.fields {
        Fields::Ds { .. } => true,
        Fields::Buffer { lds, .. } | Fields::Flat { lds, .. } => *lds,
        _ => false,
    };
    let lds_outside = p
        .blocks
        .iter()
        .zip(&walk)
        .any(|(b, &w)| !w && b.insts.iter().any(lds));
    for i in p
        .blocks
        .iter()
        .zip(&walk)
        .filter(|w| *w.1)
        .flat_map(|w| &w.0.insts)
    {
        let name = i.name();
        let control = name.starts_with("s_cbranch") || name == "s_branch";
        let local = matches!(i.fields, Fields::Ds { gds: false, .. }) && !lds_outside;
        if !(pure(i) || control || local || name.starts_with("image_bvh")) {
            return Err("a ray-tracing walk with a memory effect the query does not have");
        }
    }
    if let Some(skip) = site.skip {
        let path = one(skip)?;
        let checks: Vec<usize> = (0..p.blocks.len())
            .filter(|&b| !walk[b] && p.blocks[b].exit.successors().contains(&path))
            .collect();
        let (&[check], true) = (&checks[..], walk[path] && path != header) else {
            return Err("a ray-tracing walk's empty-structure check that is not one branch");
        };
        let Exit::Branch { taken, not_taken } = p.blocks[check].exit else {
            return Err("a ray-tracing walk's empty-structure check that is not one branch");
        };
        let on = if taken == path { not_taken } else { taken };
        if walk[on] {
            return Err("a ray-tracing walk's empty-structure check that is not one branch");
        }
        let block = &mut p.blocks[check];
        block.exit = Exit::Jump(on);
        // The branch goes, and its condition with it (`dead_code`).
        if block
            .insts
            .last()
            .is_some_and(|i| i.name().starts_with("s_cbranch"))
        {
            block.insts.pop();
        }
    }
    let mut entry = None;
    for (b, block) in p.blocks.iter().enumerate() {
        for s in block.exit.successors() {
            if walk[s] && !walk[b] {
                if s != header || entry.is_some_and(|e| e != b) {
                    return Err(
                        "a ray-tracing walk entered other than at its header from one block",
                    );
                }
                entry = Some(b);
            }
            if walk[b] && !walk[s] && s != exit {
                return Err("a ray-tracing walk left other than to its exit block");
            }
        }
    }
    let pre = entry.ok_or("a ray-tracing walk that is never entered")?;
    // The walk's blocks the header reaches (not the skip path).
    let mut walked = vec![false; p.blocks.len()];
    let mut todo = vec![header];
    while let Some(b) = todo.pop() {
        if walk[b] && !walked[b] {
            walked[b] = true;
            todo.extend(p.blocks[b].exit.successors());
        }
    }
    let leaves =
        (0..p.blocks.len()).any(|b| walked[b] && p.blocks[b].exit.successors().contains(&exit));
    if !leaves {
        return Err("a ray-tracing walk whose header never reaches its exit");
    }
    let at_entry = |reg: u8| {
        value_at_end(&p, pre, Reg::V(reg))
            .ok_or("a ray register without a value where the walk starts")
    };
    let exec = value_at_end(&p, pre, Reg::S(EXEC))
        .ok_or("no lane mask where a ray-tracing walk starts")?;
    // What the walk defines, by value: its register.
    let mut defined: HashMap<Value, Reg> = HashMap::new();
    for block in p.blocks.iter().zip(&walk).filter(|w| *w.1).map(|w| w.0) {
        defined.extend(block.phis.iter().map(|phi| (phi.value, phi.reg)));
        defined.extend(
            block
                .insts
                .iter()
                .flat_map(|i| i.writes.iter().map(|w| (w.1, w.0))),
        );
    }
    let mut next = next_value(&p);
    let mut fresh = || {
        next += 1;
        Value(next - 1)
    };
    let results: Vec<(u8, Kind, Value)> =
        site.results.iter().map(|&(r, k)| (r, k, fresh())).collect();
    // A kept register reads its value from before the walk.
    let mut kept: Vec<(Reg, Value)> = Vec::new();
    for &reg in site.kept {
        let v = value_at_end(&p, pre, reg)
            .ok_or("a kept register without a value where the walk starts")?;
        kept.push((reg, v));
    }
    let result_of = |reg: Reg| {
        results
            .iter()
            .find(|r| Reg::V(r.0) == reg)
            .map(|r| r.2)
            .or_else(|| kept.iter().find(|k| k.0 == reg).map(|k| k.1))
    };
    // The exit block's phis: what each register holds when the walk is
    // left. The new instruction's result, or the value from before the walk
    // when every way out of it brings that one (the skip path is no way
    // out any more).
    let mut subst: HashMap<Value, Value> = HashMap::new();
    let old_preds = p.blocks[exit].preds.clone();
    let outside: Vec<usize> = (0..old_preds.len())
        .filter(|&k| !walk[old_preds[k]])
        .collect();
    let mut phis = Vec::new();
    for phi in &p.blocks[exit].phis {
        let inside: Vec<Value> = (0..old_preds.len())
            .filter(|&k| walked[old_preds[k]])
            .map(|k| phi.args[k])
            .collect();
        let from_walk = match result_of(phi.reg) {
            Some(v) => v,
            None => {
                let before = value_at_end(&p, pre, phi.reg);
                match before {
                    Some(v) if inside.iter().all(|&a| a == v) => v,
                    _ => {
                        return Err(
                            "the code after a ray-tracing walk reads a register the walk changes",
                        );
                    }
                }
            }
        };
        if outside.is_empty() {
            subst.insert(phi.value, from_walk);
        } else {
            let mut args: Vec<Value> = outside.iter().map(|&k| phi.args[k]).collect();
            args.push(from_walk);
            phis.push(Phi {
                reg: phi.reg,
                value: phi.value,
                args,
            });
        }
    }
    // Values of the walk read after it (the exit block has one way in).
    for (b, block) in p.blocks.iter().enumerate().filter(|(b, _)| !walk[*b]) {
        let phi_args = block
            .phis
            .iter()
            .filter(|_| b != exit)
            .flat_map(|phi| phi.args.iter().copied());
        let reads = block.insts.iter().flat_map(|i| i.reads.iter().map(|r| r.1));
        for v in phi_args.chain(reads) {
            let Some(&reg) = defined.get(&v) else {
                continue;
            };
            let to = result_of(reg)
                .ok_or("the code after a ray-tracing walk reads a register the walk changes")?;
            subst.insert(v, to);
        }
    }
    // The instruction: it reads the ray, the results' old values (lanes
    // outside EXEC keep them) and EXEC.
    let mut reads: Vec<(Reg, Value)> = Vec::new();
    let mut read = |reg: u8, v: Value| {
        if !reads.iter().any(|r| r.0 == Reg::V(reg)) {
            reads.push((Reg::V(reg), v));
        }
    };
    for &r in site
        .origin
        .iter()
        .chain(&site.direction)
        .chain([&site.t_max])
    {
        read(r, at_entry(r)?);
    }
    for &(r, _, _) in &results {
        if let Some(old) = value_at_end(&p, pre, Reg::V(r)) {
            read(r, old);
        }
    }
    reads.push((Reg::S(EXEC), exec));
    let pack = |r: [u8; 3]| u32::from(r[0]) | u32::from(r[1]) << 8 | u32::from(r[2]) << 16;
    let mut words = vec![
        MAGIC,
        site.flags,
        u32::from(site.mask),
        site.t_min,
        pack(site.origin),
        pack(site.direction),
        u32::from(site.t_max),
    ];
    words.extend(results.iter().map(|r| u32::from(r.0) | r.1.code() << 8));
    let query = Inst {
        machine: Instruction {
            offset: site.header,
            encoding: Encoding::Vop3,
            opcode: RAY_QUERY,
            words,
        },
        fields: Fields::Valu(Valu::default()),
        reads,
        writes: results.iter().map(|r| (Reg::V(r.0), r.2)).collect(),
    };
    // The blocks without the walk, the new one where the header was.
    let kept: Vec<usize> = (0..p.blocks.len()).filter(|&b| !walk[b]).collect();
    let at = kept
        .iter()
        .position(|&b| b > header)
        .ok_or("a ray-tracing walk with nothing after it")?;
    let mut index = vec![usize::MAX; p.blocks.len()];
    for (n, &b) in kept.iter().enumerate() {
        index[b] = if n < at { n } else { n + 1 };
    }
    let new = at;
    let map = |b: usize| if b == header { new } else { index[b] };
    let mut blocks: Vec<Block> = Vec::with_capacity(kept.len() + 1);
    let mut old = p.blocks;
    for &b in &kept {
        let mut block = std::mem::replace(
            &mut old[b],
            Block {
                offset: 0,
                preds: Vec::new(),
                phis: Vec::new(),
                insts: Vec::new(),
                exit: Exit::End,
            },
        );
        block.exit = match block.exit {
            Exit::Jump(t) => Exit::Jump(map(t)),
            Exit::Branch { taken, not_taken } => Exit::Branch {
                taken: map(taken),
                not_taken: map(not_taken),
            },
            e => e,
        };
        if b == exit {
            block.preds = outside.iter().map(|&k| index[old_preds[k]]).collect();
            block.preds.push(new);
            block.phis = std::mem::take(&mut phis);
        } else {
            block.preds = block.preds.iter().map(|&q| index[q]).collect();
        }
        blocks.push(block);
    }
    blocks.insert(
        new,
        Block {
            offset: site.header,
            preds: vec![index[pre]],
            phis: Vec::new(),
            insts: vec![query],
            exit: Exit::Jump(index[exit]),
        },
    );
    for block in &mut blocks {
        for phi in &mut block.phis {
            for a in &mut phi.args {
                *a = *subst.get(a).unwrap_or(a);
            }
        }
        for inst in &mut block.insts {
            for r in &mut inst.reads {
                r.1 = *subst.get(&r.1).unwrap_or(&r.1);
            }
        }
    }
    let mut p = Program {
        wave64: p.wave64,
        vgprs: p.vgprs,
        blocks,
        inputs: p.inputs,
    };
    dead_code(&mut p);
    Ok(p)
}

/// Whether removing `i` changes nothing but its results: ALU instructions
/// and memory reads. (Branches, stores, atomics, barriers, messages and
/// whatever this does not name stay.)
fn pure(i: &Inst) -> bool {
    let name = i.name();
    let load = name.contains("load") && !name.contains("lds");
    match &i.fields {
        Fields::Sop { .. } => {
            !(name.is_empty()
                || name.starts_with("s_setreg")
                || name.starts_with("s_setpc")
                || name.starts_with("s_swappc")
                || name.starts_with("s_set_gpr_idx")
                || name.starts_with("s_cbranch")
                || name.starts_with("s_rfe")
                || name.starts_with("s_round_mode")
                || name.starts_with("s_denorm_mode"))
        }
        Fields::Valu(_) => !name.is_empty(),
        Fields::Smem { .. } => name.starts_with("s_load") || name.starts_with("s_buffer_load"),
        Fields::Buffer { lds: false, .. } => load,
        Fields::Flat { lds: false, .. } => load,
        Fields::Ds { gds: false, .. } => {
            name.starts_with("ds_read")
                || name.starts_with("ds_swizzle")
                || name.contains("permute")
        }
        Fields::Image { .. } => {
            name.starts_with("image_load")
                || name.starts_with("image_sample")
                || name.starts_with("image_gather")
                || name.starts_with("image_get")
        }
        _ => false,
    }
}

/// Removes the instructions and phis nothing reads ([`pure`] ones).
fn dead_code(p: &mut Program) {
    loop {
        let read: HashSet<Value> = p
            .blocks
            .iter()
            .flat_map(|b| {
                b.phis
                    .iter()
                    .flat_map(|phi| phi.args.iter().copied())
                    .chain(b.insts.iter().flat_map(|i| i.reads.iter().map(|r| r.1)))
            })
            .collect();
        let mut removed = false;
        for block in &mut p.blocks {
            let before = block.insts.len() + block.phis.len();
            block
                .insts
                .retain(|i| !pure(i) || i.writes.iter().any(|w| read.contains(&w.1)));
            block.phis.retain(|phi| read.contains(&phi.value));
            removed |= block.insts.len() + block.phis.len() != before;
        }
        if !removed {
            return;
        }
    }
}

/// A spliced instruction's site, as its words hold it.
struct Query {
    flags: u32,
    mask: u32,
    t_min: u32,
    origin: [u8; 3],
    direction: [u8; 3],
    t_max: u8,
    results: Vec<(u8, Kind)>,
}

impl Query {
    fn of(inst: &Inst) -> Option<Query> {
        let m = &inst.machine;
        if m.encoding != Encoding::Vop3 || m.opcode != RAY_QUERY || m.words.first() != Some(&MAGIC)
        {
            return None;
        }
        let w = &m.words;
        let unpack = |x: u32| [x as u8, (x >> 8) as u8, (x >> 16) as u8];
        Some(Query {
            flags: *w.get(1)?,
            mask: *w.get(2)?,
            t_min: *w.get(3)?,
            origin: unpack(*w.get(4)?),
            direction: unpack(*w.get(5)?),
            t_max: *w.get(6)? as u8,
            results: w
                .get(7..)?
                .iter()
                .map(|&x| Some((x as u8, Kind::of(x >> 8)?)))
                .collect::<Option<_>>()?,
        })
    }
}

impl<'a> Lower<'a> {
    /// The ray query of a spliced walk ([`splice`]), when instruction `i`
    /// of block `b` is one: `TraceRayInline` in the lanes EXEC holds, an
    /// empty `Proceed` loop (no candidate needs a commit: the queries force
    /// opacity), then each result by its kind. Results it cannot compute
    /// are missing, as any value-only instruction's.
    pub(super) fn ray_query(&mut self, b: usize, i: usize) -> Option<Result<(), Stop>> {
        let inst: &'a Inst = &self.x.p.blocks[b].insts[i];
        let q = Query::of(inst)?;
        if let Err(why) = self.lower_query(inst, b, i, &q) {
            for &(_, v) in &inst.writes {
                self.vals.insert(v, Val::Missing(why));
            }
        }
        Some(Ok(()))
    }

    fn lower_query(&mut self, inst: &Inst, b: usize, i: usize, q: &Query) -> Result<(), Why> {
        if self.s.m.paired_lanes() {
            return Err("a ray query with paired lanes");
        }
        let float = |l: &mut Self, r: u8| -> Result<V, Why> {
            let v = inst.read(Reg::V(r)).ok_or("an unread ray register")?;
            l.float_of(v)
        };
        let [ox, oy, oz] = [q.origin[0], q.origin[1], q.origin[2]];
        let [dx, dy, dz] = [q.direction[0], q.direction[1], q.direction[2]];
        let origin = [float(self, ox)?, float(self, oy)?, float(self, oz)?];
        let direction = [float(self, dx)?, float(self, dy)?, float(self, dz)?];
        let t_max = float(self, q.t_max)?;
        let mut olds = Vec::with_capacity(q.results.len());
        for &(r, _) in &q.results {
            let old = match inst.read(Reg::V(r)).map(|v| self.i32_of(v)) {
                Some(Ok(x)) => x,
                // Lanes outside EXEC keep an undefined value: 0, as other
                // guarded writes do (the validator refuses undef reads).
                _ => self.s.m.i32(0),
            };
            olds.push(old);
        }
        let guard = self.guard(b, i)?;
        let tlas = self.tlas_handle();
        let (i1, i32t, f32t, void) = (self.s.t.i1, self.s.t.i32, self.s.t.f32, self.s.t.void);
        let flags = self.s.m.i32(q.flags);
        let entry = self.s.entry;
        let query = self.s.op(
            entry,
            "allocateRayQuery",
            Attr::NoUnwind,
            i32t,
            178,
            &[flags],
        );
        let at = self.begin_guarded(guard);
        let mask = self.s.m.i32(q.mask);
        let t_min = self.s.m.f32(q.t_min);
        let args = [
            query,
            tlas,
            flags,
            mask,
            origin[0],
            origin[1],
            origin[2],
            t_min,
            direction[0],
            direction[1],
            direction[2],
            t_max,
        ];
        self.s.op(
            self.cur,
            "rayQuery_TraceRayInline",
            Attr::NoUnwind,
            void,
            179,
            &args,
        );
        let (walk, done) = (self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(self.cur, Term::Br(walk));
        let more = self.s.op(
            walk,
            "rayQuery_Proceed.i1",
            Attr::NoUnwind,
            i1,
            180,
            &[query],
        );
        self.s.m.terminate(walk, Term::CondBr(more, walk, done));
        self.cur = done;
        let status = self.s.op(
            done,
            "rayQuery_StateScalar.i32",
            Attr::ReadOnly,
            i32t,
            184,
            &[query],
        );
        // COMMITTED_TRIANGLE_HIT.
        let one = self.s.m.i32(1);
        let hit = self.s.m.cmp(done, pred::EQ, status, one);
        let mut t = None;
        let mut values = Vec::with_capacity(q.results.len());
        for (&(_, kind), &old) in q.results.iter().zip(&olds) {
            let x = match kind {
                Kind::Hit => self.s.m.cast(done, Cast::ZExt, hit, i32t),
                Kind::T => {
                    let t = *t.get_or_insert_with(|| {
                        self.s.op(
                            done,
                            "rayQuery_StateScalar.f32",
                            Attr::ReadOnly,
                            f32t,
                            200,
                            &[query],
                        )
                    });
                    self.s.m.cast(done, Cast::Bitcast, t, i32t)
                }
                Kind::Count => self.s.m.i32(0),
            };
            values.push((x, old));
        }
        let merged = self.end_guarded(at, &values);
        for (&(r, _), x) in q.results.iter().zip(merged) {
            if let Some(new) = inst.write(Reg::V(r)) {
                self.vals.insert(new, Val::I32(x));
            }
        }
        Ok(())
    }

    /// The acceleration structure's handle (`t`[`TLAS_REGISTER`] in
    /// [`ROOT_CBV_SPACE`]), made in the entry block and listed among the
    /// root SRVs; the module then needs ray-tracing tier 1.1.
    fn tlas_handle(&mut self) -> V {
        if let Some(&(_, h)) = self.s.root_srvs.iter().find(|r| r.0 == TLAS_REGISTER) {
            return h;
        }
        let r = self.s.m.i32(u32::from(TLAS_REGISTER));
        let space = self.s.m.i32(ROOT_CBV_SPACE);
        let srv = self.s.m.i8(0);
        let bind = self.s.m.aggregate(self.s.t.bind, &[r, r, space, srv]);
        let f = self.s.m.bool(false);
        let entry = self.s.entry;
        let handle = self.s.t.handle;
        let h = self.s.op(
            entry,
            "createHandleFromBinding",
            Attr::ReadNone,
            handle,
            217,
            &[bind, r, f],
        );
        // RTAccelerationStructure.
        let h = self.s.annotate(entry, h, 16, 0);
        self.s.root_srvs.push((TLAS_REGISTER, h));
        self.s.tlas = Some(TLAS_REGISTER);
        self.s.flags |= flags::RAYTRACING_TIER_1_1;
        h
    }
}

/// The `dx.resources` entry of the acceleration structure, SRV `id`: a
/// `struct.RaytracingAccelerationStructure` of resource kind 16 with an
/// element type of i32 (tag 0, component type 4), as DXC 10.0.26100
/// declares one.
pub(in crate::emit) fn tlas_md(m: &mut Module, i32t: Ty, id: u32, empty: Md) -> Md {
    let ty = m.named_struct("struct.RaytracingAccelerationStructure", vec![i32t]);
    let p = m.ptr(ty);
    let u = m.undef(p);
    let mut c = |v: u32| {
        let k = m.i32(v);
        m.md_value(k)
    };
    let (tag, i32_kind) = (c(0), c(4));
    let (id, space, register, count, kind, zero) = (
        c(id),
        c(ROOT_CBV_SPACE),
        c(u32::from(TLAS_REGISTER)),
        c(1),
        c(16),
        c(0),
    );
    let element = m.md_node(vec![Some(tag), Some(i32_kind)]);
    let u = m.md_value(u);
    m.md_node(vec![
        Some(id),
        Some(u),
        Some(empty),
        Some(space),
        Some(register),
        Some(count),
        Some(kind),
        Some(zero),
        Some(element),
    ])
}

/// The acceleration structure's `PSV0` binding: a raw SRV (4) of resource
/// kind 16.
pub(in crate::emit) fn tlas_binding() -> Binding {
    Binding {
        kind: 4,
        space: ROOT_CBV_SPACE,
        register: u32::from(TLAS_REGISTER),
        dxil_kind: 16,
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, SITES, fingerprint};

    /// Each site of `sites/` says what a walk can be: one program each, a
    /// skip path inside the walk, a cap exactly when a count is read, and
    /// registers that do not overlap where they must not.
    #[test]
    fn rayquery_sites_are_consistent() {
        for (n, (key, site)) in SITES.iter().enumerate() {
            assert!(SITES[..n].iter().all(|(k, _)| k != key), "{key:#x}");
            assert!(site.header < site.exit, "{key:#x}");
            assert!(
                site.skip
                    .is_none_or(|s| (site.header..site.exit).contains(&s) && s != site.header),
                "{key:#x}"
            );
            let counted = site.results.iter().any(|r| r.1 == Kind::Count);
            assert_eq!(site.cap.is_some(), counted, "{key:#x}");
            let mut ray: Vec<u8> = site.origin.into_iter().chain(site.direction).collect();
            ray.push(site.t_max);
            let mut distinct = ray.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert_eq!(distinct.len(), ray.len(), "{key:#x}");
            let mut results: Vec<u8> = site.results.iter().map(|r| r.0).collect();
            results.sort_unstable();
            results.dedup();
            assert_eq!(results.len(), site.results.len(), "{key:#x}");
        }
    }

    #[test]
    fn rayquery_kinds_round_trip_through_their_codes() {
        for k in [Kind::Hit, Kind::T, Kind::Count] {
            assert_eq!(Kind::of(k.code()), Some(k));
        }
        assert_eq!(Kind::of(3), None);
    }

    /// FNV-1a 64's published vectors: the empty input and "a".
    #[test]
    fn rayquery_fingerprint_is_fnv1a_64() {
        assert_eq!(fingerprint(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fingerprint(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
