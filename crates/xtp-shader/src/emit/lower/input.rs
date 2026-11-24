//! Program inputs: user data, system values, vertex and pixel inputs,
//! interpolation (`v_interp_*`). Backlog P1 (packed 16-bit pixel inputs),
//! P2 (exact interpolation), S3 (hardware SGPR inputs).

use std::collections::HashMap;

use super::{Lower, Stop, Val, Why, packed_half};
use crate::emit::UserData;
use crate::emit::bitcode::{Attr, Bin, Cast, V};
use crate::emit::container::SigElement;
use crate::emit::shader::flags;
use crate::gfx10::{Fields, Operand};
use crate::io::{Barycentric, Input, Interp};
use crate::ir::{Inst, Reg, Value};
use crate::native::{RootType, StageIo};
use crate::resource::Site;

/// Pixel inputs packing two 16-bit rows per dword whose second row the
/// metadata leaves out (`row2` = `row`): as for outputs
/// ([`super::export::half_outputs`]), the next 16-bit row of the signature
/// that no input entry names, taken in row order (rows the metadata does
/// give are taken first).
fn half_input_partners(io: &StageIo, inputs: &[SigElement]) -> HashMap<u8, u8> {
    let StageIo::Ps {
        inputs: entries, ..
    } = io
    else {
        return HashMap::new();
    };
    let packed = || entries.iter().filter(|i| i.fp16 && i.pack2);
    let mut free: Vec<u8> = inputs
        .iter()
        .filter(|e| e.half && e.kind == 0)
        .filter_map(|e| u8::try_from(e.row?).ok())
        .filter(|&r| !entries.iter().any(|i| i.row == r))
        .filter(|&r| !packed().any(|i| i.row2 != i.row && i.row2 == r))
        .collect();
    free.sort_unstable();
    free.dedup();
    let mut missing: Vec<_> = packed().filter(|i| i.row2 == i.row).collect();
    missing.sort_by_key(|i| i.row);
    let mut partners = HashMap::new();
    for i in missing {
        if let Some(k) = free.iter().position(|&r| r > i.row) {
            partners.insert(i.row, free.remove(k));
        }
    }
    partners
}

/// Where half `h` of a pixel input packing rows `a` and `b` comes from
/// ([`packed_half`]), or `None` for the padding of a second row the metadata
/// names (`explicit_second_row`). With a partner found by
/// [`half_input_partners`] a half no element holds stays an error: the
/// guess must not turn real data into zeros.
fn packed_input_half(
    elements: &[SigElement],
    a: u8,
    b: Option<u8>,
    h: u8,
    explicit_second_row: bool,
) -> Result<Option<(u8, u8)>, Why> {
    let half = packed_half(elements, a, b, h);
    if half.is_none() && !explicit_second_row {
        Err("a packed input half no element holds")
    } else {
        Ok(half)
    }
}

/// How an interpolation evaluates an input channel.
#[derive(Debug, Clone, Copy)]
enum Eval {
    /// Loaded with this DXIL interpolation mode (1: constant, P0).
    Mode(u8),
    /// From the vertex values of a constant element: P0 when `p0`, plus
    /// i (P1 - P0), plus j (P2 - P0), weights as floats.
    Weights {
        p0: bool,
        i: Option<V>,
        j: Option<V>,
    },
}

impl<'a> Lower<'a> {
    /// Values of the program's input registers.
    pub(super) fn inputs(&mut self) -> Result<(), Stop> {
        let io_inputs: HashMap<Value, Input> = self.x.io.inputs.iter().copied().collect();
        for &(reg, v) in &self.x.p.inputs {
            let val = match reg {
                Reg::S(_) if self.params.contains_key(&v) => continue, // loaded on first use
                // Most programs only interpolate with them: loaded on first
                // use as a value (`lazy_barycentric`).
                Reg::V(_) if matches!(io_inputs.get(&v), Some(Input::Barycentric { .. })) => {
                    continue;
                }
                Reg::S(n) => match io_inputs.get(&v) {
                    Some(&input) => self.input(input),
                    // Above the SGPRs the hardware initializes, a register
                    // holds what the last wave left, so no program depends
                    // on it: a read the paths only seem to allow (a branch
                    // past the write, taken only with one past the read)
                    // reads zero, as VGPRs do.
                    None if n <= 105 && self.x.io.initialized_sgprs.is_some_and(|k| n >= k) => {
                        Val::I32(self.s.m.i32(0))
                    }
                    None => Val::Missing("a hardware SGPR input"),
                },
                Reg::Scc => Val::Missing("SCC on entry"),
                Reg::V(_) => match io_inputs.get(&v) {
                    Some(&input) => self.input(input),
                    // Registers no one set read as zero (the validator
                    // refuses reads of undef).
                    None => Val::I32(self.s.m.i32(0)),
                },
            };
            self.vals.insert(v, val);
        }
        Ok(())
    }

    pub(super) fn input(&mut self, input: Input) -> Val {
        let (kind, col) = match input {
            Input::VertexId => (1, 0),
            Input::InstanceId => (2, 0),
            Input::Position(c) => (3, c),
            Input::Attribute { row, channel } => {
                return match self.find_input(row, channel) {
                    Some((id, col)) => self.load_input(id, col),
                    None => Val::Missing("a vertex input the signature lacks"),
                };
            }
            Input::Barycentric { kind, index } => return self.barycentric(kind, index),
            Input::PrimitiveId => return Val::Missing("the primitive id"),
            // The console code tests it against 0 (`v_cmp_ne_u32`), as the
            // host's value can be.
            Input::FrontFace => (13, 0),
            Input::ThreadIdInGroup(c) => return self.thread_id(c),
            Input::GroupId(c) => return self.compute_id("groupId.i32", 94, c),
            Input::GroupInfo => return Val::Missing("the thread-group info SGPR"),
            Input::MeshGroup => {
                let Val::I32(g) = self.compute_id("groupId.i32", 94, 0) else {
                    unreachable!()
                };
                let n = self.s.m.i32(self.x.mesh_stride);
                return Val::I32(self.s.m.bin(self.s.entry, Bin::Mul, g, n));
            }
            Input::MergedWaveInfo => {
                // shortcut: only the wave index (bits 24-27) is set; the
                // console's other bits (lane counts) are read by no program.
                let f = self.mesh_thread();
                let shift = self.s.m.i32(self.wave().trailing_zeros());
                let w = self.s.m.bin(self.s.entry, Bin::LShr, f, shift);
                let k24 = self.s.m.i32(24);
                return Val::I32(self.s.m.bin(self.s.entry, Bin::Shl, w, k24));
            }
            _ => return Val::Missing("a pixel system input"),
        };
        let free = self
            .s
            .inputs
            .iter()
            .filter_map(|e| e.row)
            .map(|r| r + 1)
            .max()
            .unwrap_or(0);
        let id = match self.s.inputs.iter().position(|e| e.kind == kind) {
            Some(id) => {
                if kind != 3 {
                    self.s.inputs[id].row.get_or_insert(free);
                }
                id
            }
            None if kind >= 3 => return Val::Missing("a pixel system input the signature lacks"),
            None => {
                // The console signature leaves out vertex and instance ids; PC
                // gives them a row after the others.
                let row = self
                    .s
                    .inputs
                    .iter()
                    .filter_map(|e| e.row)
                    .map(|r| r + 1)
                    .max()
                    .unwrap_or(0);
                self.s.inputs.push(SigElement {
                    name: ["", "SV_VertexID", "SV_InstanceID"][kind as usize].into(),
                    index: 0,
                    kind,
                    comp: 5,
                    interp: 0,
                    row: Some(row),
                    col: 0,
                    cols: 1,
                    usage: 0,
                    half: false,
                });
                self.s.inputs.len() - 1
            }
        };
        self.load_input(id, col)
    }

    /// Loads `v` if it is a barycentric input VGPR not loaded yet (see
    /// `inputs`): `None` if it is not one.
    pub(super) fn lazy_barycentric(&mut self, v: Value) -> Option<Result<(), Why>> {
        let (kind, index) = self.x.io.inputs.iter().find_map(|&(w, i)| match i {
            Input::Barycentric { kind, index } if w == v => Some((kind, index)),
            _ => None,
        })?;
        let val = self.barycentric(kind, index);
        self.vals.insert(v, val);
        Some(match val {
            Val::Missing(w) => Err(w),
            _ => Ok(()),
        })
    }

    /// Barycentric `index` (0 i, 1 j) of `kind` as a value: a channel of
    /// `SV_Barycentrics` declared with the same interpolation mode.
    ///
    /// GFX10 interpolates P0 + i (P1 - P0) + j (P2 - P0), so i and j are
    /// the weights of vertices 1 and 2. The host's weights (x, y, z) are
    /// those of vertices 0, 1 and 2, vertex 0 being the provoking vertex,
    /// which is the console's P0 (`v_interp_mov` P0 is lowered to a flat
    /// input): i is y and j is z.
    // shortcut: triangles only; on lines and points the console's i and j
    // are not checked against the host's weights.
    pub(super) fn barycentric(&mut self, kind: Barycentric, index: u8) -> Val {
        let Some(mode) = interp_mode(kind) else {
            return Val::Missing("pull-model barycentrics");
        };
        if index > 1 {
            return Val::Missing("pull-model barycentrics");
        }
        // One element per interpolation mode (the console's own, if its
        // signature declares one). DXIL allows two, one perspective and one
        // linear: a second perspective (or linear) mode is refused, since
        // its weights differ from the first's where the modes' sample points
        // do (centroid weights stay inside the triangle, center ones need
        // not), so reusing the declared element would be a silent wrong
        // value.
        let bary = |e: &SigElement| e.kind == 28;
        let clash = self.s.inputs.iter().any(|e| {
            bary(e) && e.usage != 0 && e.interp != mode && linear(e.interp) == linear(mode)
        });
        if clash {
            return Val::Missing(if linear(mode) {
                "barycentrics of two linear modes (SV_Barycentrics allows one)"
            } else {
                "barycentrics of two perspective modes (SV_Barycentrics allows one)"
            });
        }
        let id = match self
            .s
            .inputs
            .iter()
            .position(|e| bary(e) && (e.interp == mode || e.usage == 0))
        {
            Some(id) => id,
            None => {
                let index = self.s.inputs.iter().filter(|e| bary(e)).count() as u32;
                if index >= 2 {
                    return Val::Missing("barycentrics of a third interpolation mode");
                }
                // A system value the validator wants unpacked (packing
                // location -1), like other non-packed system values.
                self.s.inputs.push(SigElement {
                    name: "SV_Barycentrics".into(),
                    index,
                    kind: 28,
                    comp: 9,
                    interp: mode,
                    row: None,
                    col: 0,
                    cols: 3,
                    usage: 0,
                    half: false,
                });
                self.s.inputs.len() - 1
            }
        };
        let e = &mut self.s.inputs[id];
        if e.col != 0 || e.cols < 3 {
            return Val::Missing("a console barycentrics element without i and j");
        }
        e.row = None;
        e.interp = mode;
        self.s.flags |= flags::BARYCENTRICS;
        self.load_any_input(id, index + 1)
    }

    /// A component of a compute thread or group id, read in the entry block.
    pub(super) fn compute_id(&mut self, name: &str, opcode: u32, c: u8) -> Val {
        let (entry, c) = (self.s.entry, self.s.m.i32(u32::from(c)));
        Val::I32(
            self.s
                .op(entry, name, Attr::ReadNone, self.s.t.i32, opcode, &[c]),
        )
    }

    pub(super) fn find_input(&self, row: u8, channel: u8) -> Option<(usize, u8)> {
        self.s.inputs.iter().enumerate().find_map(|(id, e)| {
            let r = e.row?;
            // `then`, not `then_some`: the channel is only past `col` when
            // the condition holds.
            (r == u32::from(row) && channel >= e.col && channel < e.col + e.cols)
                .then(|| (id, channel - e.col))
        })
    }

    pub(super) fn load_input(&mut self, id: usize, col: u8) -> Val {
        if self.s.inputs[id].half {
            // shortcut: 16-bit vertex inputs (the fetch routine's) are not
            // unpacked; pixel inputs are (`interp`).
            return Val::Missing("a 16-bit input");
        }
        self.load_any_input(id, col)
    }

    /// An input element's channel as declared (16-bit ones as 32-bit floats).
    pub(super) fn load_any_input(&mut self, id: usize, col: u8) -> Val {
        let e = &mut self.s.inputs[id];
        e.usage |= 1 << col;
        let (int, bit) = (e.comp != 9, e.comp == 1);
        let ty = if bit {
            self.s.t.i1
        } else if int {
            self.s.t.i32
        } else {
            self.s.t.f32
        };
        let suffix = if bit {
            ".i1"
        } else if int {
            ".i32"
        } else {
            ".f32"
        };
        let entry = self.s.entry;
        let args = [
            self.s.m.i32(id as u32),
            self.s.m.i32(0),
            self.s.m.i8(col),
            self.s.m.undef(self.s.t.i32),
        ];
        let v = self.s.op(
            entry,
            &format!("loadInput{suffix}"),
            Attr::ReadNone,
            ty,
            4,
            &args,
        );
        if bit {
            let z = self.s.m.cast(entry, Cast::ZExt, v, self.s.t.i32);
            Val::I32(z)
        } else if int {
            Val::I32(v)
        } else {
            Val::I32(self.s.m.cast(entry, Cast::Bitcast, v, self.s.t.i32))
        }
    }

    /// A user-data dword: a root parameter's value.
    pub(super) fn user(&mut self, param: usize, dword: u32) -> Val {
        let rp = self.x.native.layout[param];
        match rp.root {
            RootType::ResourceHeap | RootType::SamplerHeap => return Val::I32(self.s.m.i32(0)),
            RootType::Constants | RootType::Table => {}
            _ => return Val::Missing("a root descriptor"),
        }
        let index = u32::from(rp.user_data) + dword;
        self.user_reads.insert(
            index,
            UserData {
                index,
                slot: rp.slot,
                root: rp.root,
                dword,
            },
        );
        self.s.user_data = self.s.user_data.max(index + 1);
        let h = match self.user_data {
            Some(h) => h,
            None => {
                let h = self.s.user_data_handle();
                self.user_data = Some(h);
                h
            }
        };
        let entry = self.s.entry;
        let row = self.s.m.i32(index / 4);
        let r = self.s.op(
            entry,
            "cbufferLoadLegacy.i32",
            Attr::ReadOnly,
            self.s.t.cbuf_ret,
            59,
            &[h, row],
        );
        Val::I32(self.s.m.extract(entry, r, index % 4))
    }

    pub(super) fn interp(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        let Some(&interp) = self.interps.get(&site) else {
            return Err("the first half of an interpolation");
        };
        let name = inst.name();
        let (dst, src) = match &inst.fields {
            Fields::Vintrp { dst, src, .. } => (*dst, Some(*src)),
            Fields::Valu(v) => {
                if v.abs != 0 || v.neg != 0 {
                    return Err("interpolation operand modifiers");
                }
                (v.dst.ok_or("an interpolation")?, v.src.get(1).copied())
            }
            _ => unreachable!(),
        };
        let Operand::Vector { reg, .. } = dst else {
            return Err("an interpolation");
        };
        // A float operand of the instruction.
        let float = |l: &mut Self, op: Option<Operand>| -> Result<V, Why> {
            let x = l.operand(inst, &op.ok_or("an interpolation operand")?)?;
            Ok(l.as_float(x))
        };
        // What the result adds to the evaluated channel (a second half's
        // first half, when the halves evaluate on their own).
        let mut add = None;
        let (attribute, eval) = match *interp {
            Interp::Interpolated {
                attribute,
                barycentric,
                ..
            } => {
                let mode = interp_mode(barycentric).ok_or("pull-model interpolation")?;
                let eval = if self.per_vertex(attribute.row) {
                    // The element is read per vertex elsewhere, so it is
                    // constant: weigh its vertices with SV_Barycentrics.
                    let mut ij = [None; 2];
                    for (index, w) in ij.iter_mut().enumerate() {
                        let x = match self.barycentric(barycentric, index as u8) {
                            Val::I32(x) => x,
                            Val::Missing(w) => return Err(w),
                            Val::Bit(_) => unreachable!(),
                        };
                        *w = Some(self.as_float(x));
                    }
                    Eval::Weights {
                        p0: true,
                        i: ij[0],
                        j: ij[1],
                    }
                } else {
                    Eval::Mode(mode)
                };
                (attribute, eval)
            }
            // P0, the provoking vertex: a flat (nointerpolation) input.
            Interp::Vertex {
                attribute,
                vertex: 2,
            } => (attribute, Eval::Mode(1)),
            // P10 and P20: what the first and second halves weigh, P1 - P0
            // and P2 - P0, which is what the hardware holds for an
            // attribute the program also interpolates.
            Interp::Vertex { attribute, vertex } if self.interpolated(attribute.row) => {
                let one = Some(self.s.m.f32(1f32.to_bits()));
                let (i, j) = if vertex == 0 {
                    (one, None)
                } else {
                    (None, one)
                };
                (attribute, Eval::Weights { p0: false, i, j })
            }
            Interp::Vertex { .. } => return Err("a non-provoking vertex value"),
            // A half of an interpolation with computed i or j: P0 + i P10,
            // or the old dst (f16: the third operand) + j P20.
            Interp::Weighted { attribute } if name.starts_with("v_interp_p1") => {
                let i = Some(float(self, src)?);
                (
                    attribute,
                    Eval::Weights {
                        p0: true,
                        i,
                        j: None,
                    },
                )
            }
            Interp::Weighted { attribute } => {
                let old = match &inst.fields {
                    Fields::Valu(v) => v.src.get(2).copied(),
                    _ => Some(dst),
                };
                add = Some(float(self, old)?);
                let j = Some(float(self, src)?);
                (
                    attribute,
                    Eval::Weights {
                        p0: false,
                        i: None,
                        j,
                    },
                )
            }
        };
        // (pack2, the second row, whether the metadata names it).
        let packing = match &self.x.native.io {
            StageIo::Ps { inputs, .. } => inputs
                .iter()
                .find(|i| i.row == attribute.row && i.fp16)
                .map(|i| {
                    let b = if i.row2 != i.row {
                        Some(i.row2)
                    } else if i.pack2 {
                        half_input_partners(&self.x.native.io, &self.s.inputs)
                            .get(&i.row)
                            .copied()
                    } else {
                        None
                    };
                    (i.pack2, b, i.pack2 && i.row2 != i.row)
                }),
            _ => None,
        };
        let f16 = name.ends_with("_f16");
        // The evaluated channel plus `add`, as a float.
        let total = |l: &mut Self, row: u8, ch: u8| -> Result<V, Why> {
            let x = l.eval_channel(row, ch, eval)?;
            let x = l.as_float(x);
            Ok(match add {
                Some(a) => l.s.m.bin(l.cur, Bin::Add, a, x),
                None => x,
            })
        };
        let x = match packing {
            None if f16 || attribute.high => return Err("a 16-bit read of a 32-bit input"),
            // Loaded as is: the bits the host interpolated.
            None if add.is_none() && matches!(eval, Eval::Mode(_)) => {
                self.eval_channel(attribute.row, attribute.channel, eval)?
            }
            None => {
                let x = total(self, attribute.row, attribute.channel)?;
                self.as_int(x)
            }
            Some((pack2, b, explicit_second_row)) => {
                // 16-bit values: one in the low half of each dword, or two
                // per dword (`packed_half`); the host's are 32-bit floats.
                let half = |l: &mut Self, h: u8| -> Result<Option<V>, Why> {
                    let src = if pack2 && f16 {
                        packed_input_half(&l.s.inputs, attribute.row, b, h, explicit_second_row)?
                    } else if pack2 {
                        packed_half(&l.s.inputs, attribute.row, b, h)
                    } else {
                        h.is_multiple_of(2).then_some((attribute.row, h / 2))
                    };
                    match src {
                        Some((row, ch)) => total(l, row, ch).map(Some),
                        // shortcut: a 16-bit read of the padding after a
                        // second row the metadata names reads zero. The
                        // host declares no element there; the console's
                        // half holds whatever the producing stage exported
                        // in it.
                        None if f16 && explicit_second_row => Ok(Some(l.s.m.f32(0))),
                        None => Ok(None),
                    }
                };
                let to_f16 = |l: &mut Self, f: V| {
                    let i32t = l.s.t.i32;
                    l.s.op(l.cur, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
                };
                let h = 2 * attribute.channel + u8::from(attribute.high);
                let c = self.cur;
                if name == "v_interp_p1ll_f16" {
                    // An f32 result from 16-bit vertex values (a first half
                    // evaluated on its own).
                    let f = half(self, h)?.ok_or("a packed input half no element holds")?;
                    self.as_int(f)
                } else if f16 {
                    let f = half(self, h)?.ok_or("a packed input half no element holds")?;
                    let lo = to_f16(self, f);
                    self.low_half(inst, reg, lo)
                } else if matches!(eval, Eval::Mode(1)) {
                    // The whole dword of a flat input: both halves.
                    let h = 2 * attribute.channel;
                    let z = self.s.m.i32(0);
                    let lo = match half(self, h)? {
                        Some(f) => to_f16(self, f),
                        None => z,
                    };
                    let hi = match half(self, h + 1)? {
                        Some(f) => to_f16(self, f),
                        None => z,
                    };
                    let k16 = self.s.m.i32(16);
                    let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                    self.s.m.bin(c, Bin::Or, lo, hi)
                } else {
                    return Err("32-bit interpolation of a 16-bit input");
                }
            }
        };
        // A pixel interpolates in every lane; guards only hide inactive ones.
        self.write(inst, Reg::V(reg), x, None);
        Ok(())
    }

    /// Whether the program reads attribute row `row` per vertex (an
    /// interpolation with computed barycentrics, or P10 or P20): then its
    /// element is declared constant and read with `attributeAtVertex`.
    fn per_vertex(&self, row: u8) -> bool {
        self.x.io.interps.iter().any(|(_, i)| match *i {
            Interp::Weighted { attribute } => attribute.row == row,
            Interp::Vertex { attribute, vertex } => {
                attribute.row == row && vertex != 2 && self.interpolated(row)
            }
            Interp::Interpolated { .. } => false,
        })
    }

    /// Whether the program interpolates attribute row `row` (so the
    /// hardware holds P1 - P0 and P2 - P0 for it).
    fn interpolated(&self, row: u8) -> bool {
        self.x.io.interps.iter().any(|(_, i)| match *i {
            Interp::Interpolated { attribute, .. } | Interp::Weighted { attribute } => {
                attribute.row == row
            }
            Interp::Vertex { .. } => false,
        })
    }

    /// Channel `ch` of input row `row` evaluated as `eval` says, as raw bits.
    fn eval_channel(&mut self, row: u8, ch: u8, eval: Eval) -> Result<V, Why> {
        let (id, col) = self
            .find_input(row, ch)
            .ok_or("an input the signature lacks")?;
        let mode = match eval {
            Eval::Mode(mode) => mode,
            Eval::Weights { .. } => 1,
        };
        let e = &mut self.s.inputs[id];
        if e.interp != mode && e.usage != 0 {
            return Err("one input interpolated two ways");
        }
        e.interp = mode;
        let Eval::Weights { p0, i, j } = eval else {
            return match self.load_any_input(id, col) {
                Val::I32(x) => Ok(x),
                _ => Err("an input"),
            };
        };
        let v0 = self.vertex_input(id, col, 0);
        let c = self.cur;
        let mut acc = p0.then_some(v0);
        for (k, w) in [(1, i), (2, j)] {
            let Some(w) = w else { continue };
            let vk = self.vertex_input(id, col, k);
            let d = self.s.m.bin(c, Bin::Sub, vk, v0);
            acc = Some(match acc {
                Some(a) => self.fmad(w, d, a),
                None => self.s.m.bin(c, Bin::Mul, w, d),
            });
        }
        let acc = acc.ok_or("an interpolation")?;
        Ok(self.as_int(acc))
    }

    /// Vertex `k` (0 the provoking vertex, as `barycentric` orders them)
    /// of a constant input element's channel, as a float.
    fn vertex_input(&mut self, id: usize, col: u8, k: u8) -> V {
        let e = &mut self.s.inputs[id];
        e.usage |= 1 << col;
        let int = e.comp != 9;
        let (ty, suffix) = if int {
            (self.s.t.i32, ".i32")
        } else {
            (self.s.t.f32, ".f32")
        };
        self.s.flags |= flags::BARYCENTRICS;
        let entry = self.s.entry;
        let args = [
            self.s.m.i32(id as u32),
            self.s.m.i32(0),
            self.s.m.i8(col),
            self.s.m.i8(k),
        ];
        let v = self.s.op(
            entry,
            &format!("attributeAtVertex{suffix}"),
            Attr::ReadNone,
            ty,
            137,
            &args,
        );
        if int {
            self.s.m.cast(entry, Cast::Bitcast, v, self.s.t.f32)
        } else {
            v
        }
    }

    /// `x` (an f16 in the low bits) as the low half of VGPR `reg`: 16-bit
    /// results keep the register's high half (GFX10).
    pub(super) fn low_half(&mut self, inst: &Inst, reg: u8, x: V) -> V {
        let c = self.cur;
        let old = match inst.read(Reg::V(reg)).map(|o| self.i32_of(o)) {
            Some(Ok(o)) => o,
            _ => self.s.m.i32(0),
        };
        let (high, low) = (self.s.m.i32(0xFFFF_0000), self.s.m.i32(0xFFFF));
        let old = self.s.m.bin(c, Bin::And, old, high);
        let x = self.s.m.bin(c, Bin::And, x, low);
        self.s.m.bin(c, Bin::Or, old, x)
    }
}

/// The DXIL interpolation mode of a kind of barycentrics (none for the
/// pull model).
fn interp_mode(kind: Barycentric) -> Option<u8> {
    Some(match kind {
        Barycentric::PerspectiveCenter => 2,
        Barycentric::PerspectiveCentroid => 3,
        Barycentric::PerspectiveSample => 6,
        Barycentric::LinearCenter => 4,
        Barycentric::LinearCentroid => 5,
        Barycentric::LinearSample => 7,
        Barycentric::PerspectivePullModel => return None,
    })
}

/// Whether DXIL interpolation mode `mode` is a linear (noperspective) one.
fn linear(mode: u8) -> bool {
    matches!(mode, 4 | 5 | 7)
}

#[cfg(test)]
mod tests {
    use super::{half_input_partners, packed_input_half};
    use crate::emit::container::SigElement;
    use crate::native::{PsInput, StageIo, Usage};

    /// A 16-bit TEXCOORD element of `cols` channels at `row`.
    fn element(row: u32, cols: u8) -> SigElement {
        SigElement {
            name: "TEXCOORD".into(),
            index: row,
            kind: 0,
            comp: 9,
            interp: 2,
            row: Some(row),
            col: 0,
            cols,
            usage: 0,
            half: true,
        }
    }

    #[test]
    fn packed_pixel_inputs_find_their_second_row() {
        let entry = |row, row2, pack2| PsInput {
            usage: Usage::Generic,
            row,
            mask: 0xF,
            attr: row,
            flat: false,
            fp16: true,
            pack2,
            row2,
        };
        // Rows 0 and 3 pack and leave their second row out; 5 names its
        // own (6). Unnamed 16-bit rows: 1, 4, 6, 7.
        let io = StageIo::Ps {
            inputs: vec![entry(0, 0, true), entry(3, 3, true), entry(5, 6, true)],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        };
        let sig: Vec<_> = [0, 1, 3, 4, 5, 6, 7].map(|r| element(r, 4)).into();
        let p = half_input_partners(&io, &sig);
        assert_eq!(p.get(&0), Some(&1));
        assert_eq!(p.get(&3), Some(&4));
        assert_eq!(p.get(&5), None);
    }

    #[test]
    fn explicit_packed_input_tail_is_padding() {
        // Four halves in row 6 followed by only two in its explicit partner
        // row 7. The next two reads address the packing's zero padding.
        let sig = vec![element(6, 4), element(7, 2)];
        assert_eq!(
            packed_input_half(&sig, 6, Some(7), 5, true),
            Ok(Some((7, 1)))
        );
        assert_eq!(packed_input_half(&sig, 6, Some(7), 6, true), Ok(None));
        assert_eq!(packed_input_half(&sig, 6, Some(7), 7, true), Ok(None));
        // With an inferred partner the same gap is not assumed to be zero.
        assert!(packed_input_half(&sig, 6, Some(7), 6, false).is_err());
        assert!(packed_input_half(&sig, 6, None, 4, false).is_err());
    }

    /// A pixel program interpolating the high half of channel z or w of a
    /// 16-bit input packing rows 0 (four halves) and 1 (two), whose
    /// metadata names the second row (`row2`) or leaves it out: the
    /// translation, validated where `dxil.dll` is found, or the error.
    fn packed_read(high_channel: u8, row2: u8) -> Result<(), String> {
        use crate::dxbc::{self, Container, FourCC};
        use crate::emit::{container, emit, validator::Validator};
        use crate::native::{HwStage, NativeProgram};
        use crate::{io, ir, resource, structure};
        // src0 of the VOP3 interpolations: attr0, the channel, high half.
        let attr = u32::from(high_channel) << 6 | 1 << 8;
        let words = [
            0xD742_0002u32, // v_interp_p1ll_f16 v2, v0, attr0.<c>.high
            attr | 256 << 9,
            0xD75A_0002, // v_interp_p2_f16 v2, attr0.<c>.high, v1, v2
            attr | 257 << 9 | 258 << 18,
            0xF800_180F,
            0x0202_0202, // exp mrt0, v2, v2, v2, v2 done vm
            0xBF81_0000,
        ];
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![PsInput {
                    usage: Usage::Generic,
                    row: 0,
                    mask: 0xF,
                    attr: 0,
                    flat: false,
                    fp16: true,
                    pack2: true,
                    row2,
                }],
                // PERSP_CENTER: i and j in v0 and v1.
                input_addr: 1 << 1,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        // The console's signature: 16-bit floats (component type 6).
        let mut isg1 = container::signature(&[element(0, 4), element(1, 2)], true);
        for k in 0..2 {
            isg1[8 + 32 * k + 16..][..4].copy_from_slice(&6u32.to_le_bytes());
        }
        let target = SigElement {
            name: "SV_Target".into(),
            kind: 16,
            row: Some(0),
            usage: 15,
            half: false,
            ..element(0, 4)
        };
        let osg1 = container::signature(&[target], false);
        let bytes = dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let out = emit(&c, &native, &p, &st, &io, &res).map_err(|e| e.to_string())?;
        if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
            v.validate(&mut out.dxil.clone()).unwrap();
        }
        Ok(())
    }

    #[test]
    fn a_packed_pixel_input_reads_the_second_row_the_metadata_leaves_out() {
        // Half 5 is channel 1 of row 1, which no input entry names.
        packed_read(2, 0).unwrap();
        // Half 7 is past row 1's two halves: with a guessed second row it
        // is no padding the metadata describes.
        let e = packed_read(3, 0).unwrap_err();
        assert!(e.contains("a packed input half no element holds"), "{e}");
        // With the second row named, it is padding.
        packed_read(2, 1).unwrap();
        packed_read(3, 1).unwrap();
    }

    /// Pixel programs reading I and J as values, or interpolating with
    /// computed ones (VIctory #453's tests).
    mod barycentrics {
        use crate::dxbc::{self, Container, FourCC};
        use crate::emit::container::{self, SigElement};
        use crate::emit::validator::Validator;
        use crate::emit::{HostShader, emit, signature};
        use crate::native::{HwStage, NativeProgram, PsInput, StageIo, Usage};
        use crate::{ShaderError, io, ir, resource, structure};

        /// `SPI_PS_INPUT_ADDR` bits: PERSP_CENTER, PERSP_CENTROID,
        /// LINEAR_CENTER.
        const PERSP_CENTER: u32 = 1 << 1;
        const PERSP_CENTROID: u32 = 1 << 2;
        const LINEAR_CENTER: u32 = 1 << 5;
        const S_ENDPGM: u32 = 0xBF81_0000;

        /// `exp mrt0, v<r>, v<r>, v<r>, v<r> done vm`.
        fn exp_mrt0(r: u32) -> [u32; 2] {
            [0xF800_180F, r * 0x0101_0101]
        }

        /// VOP2 `v_add_f32 v<dst>, v<a>, v<b>`.
        fn v_add_f32(dst: u32, a: u32, b: u32) -> u32 {
            3 << 25 | dst << 17 | b << 9 | (256 + a)
        }

        fn element(name: &str, kind: u8, row: u32) -> SigElement {
            SigElement {
                name: name.into(),
                index: 0,
                kind,
                comp: 9,
                interp: 0,
                row: Some(row),
                col: 0,
                cols: 4,
                usage: 0,
                half: false,
            }
        }

        /// Translates a pixel program with the input VGPRs `input_addr`
        /// enables and one float4 TEXCOORD0 attribute (attr 0, row 0).
        fn pixel(words: &[u32], input_addr: u32) -> Result<HostShader, ShaderError> {
            let native = NativeProgram {
                stage: HwStage::Ps,
                wave64: false,
                vgprs: 8,
                layout: vec![],
                used: vec![],
                io: StageIo::Ps {
                    inputs: vec![PsInput {
                        usage: Usage::Generic,
                        row: 0,
                        mask: 0b1111,
                        attr: 0,
                        flat: false,
                        fp16: false,
                        pack2: false,
                        row2: 0,
                    }],
                    input_addr,
                    db_shader_control: 0,
                    patches: vec![],
                },
                code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let mut target = element("SV_Target", 16, 0);
            target.usage = 15;
            let isg1 = container::signature(&[element("TEXCOORD", 0, 0)], true);
            let osg1 = container::signature(&[target], false);
            let bytes = dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
            let c = Container::parse(&bytes).unwrap();
            let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
            let st = structure::structure(&p)?;
            let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
            let io = io::raise(&p, &native)?;
            emit(&c, &native, &p, &st, &io, &res)
        }

        /// The translation's `SFI0` features and `ISG1` rows.
        fn parts(shader: &HostShader) -> (u64, Vec<SigElement>) {
            let c = Container::parse(&shader.dxil).unwrap();
            let sfi0 = c.part(FourCC::SFI0).unwrap().data.try_into().unwrap();
            (
                u64::from_le_bytes(sfi0),
                signature(c.part(FourCC::ISG1).unwrap().data).unwrap(),
            )
        }

        fn validate(shader: &HostShader) {
            if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
                v.validate(&mut shader.dxil.clone()).unwrap();
            }
        }

        /// The `PSV0` interpolation mode of TEXCOORD0 (row 0, x-w, float).
        fn texcoord_mode(shader: &HostShader) -> u8 {
            let c = Container::parse(&shader.dxil).unwrap();
            let psv = c.part(FourCC::PSV0).unwrap().data;
            let row = [1, 0, 0x44, 0, 3];
            let at = psv.windows(5).position(|w| w == row).unwrap();
            psv[at + 5]
        }

        /// Custom interpolation reads I and J (v0, v1 with PERSP_CENTER) as
        /// plain floats: they are channels y and z of `SV_Barycentrics`, the
        /// weights of vertices 1 and 2.
        #[test]
        fn barycentrics_read_as_values_are_sv_barycentrics() {
            let [e0, e1] = exp_mrt0(2);
            let words = [v_add_f32(2, 0, 1), e0, e1, S_ENDPGM];
            let shader = pixel(&words, PERSP_CENTER).unwrap();
            let (features, inputs) = parts(&shader);
            assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
            let bary: Vec<_> = inputs.iter().filter(|e| e.kind == 28).collect();
            assert_eq!(bary.len(), 1, "{inputs:?}");
            assert_eq!((bary[0].index, bary[0].col, bary[0].cols), (0, 0, 3));
            // Not packed: the validator wants packing location -1.
            assert_eq!(bary[0].row, None);
            // Only y and z (I and J) are read.
            let isg1 = Container::parse(&shader.dxil).unwrap();
            let isg1 = isg1.part(FourCC::ISG1).unwrap().data;
            let rw: Vec<u8> = (0..2).map(|k| isg1[8 + 32 * k + 25]).collect();
            assert!(rw.contains(&0b110), "{rw:?}");
            validate(&shader);
        }

        /// Two kinds of barycentrics read directly: one element per mode.
        #[test]
        fn two_barycentric_modes_get_two_elements() {
            // v0-v1 perspective i, j; v2-v3 linear i, j.
            let [e0, e1] = exp_mrt0(4);
            let words = [v_add_f32(4, 0, 3), e0, e1, S_ENDPGM];
            let shader = pixel(&words, PERSP_CENTER | LINEAR_CENTER).unwrap();
            let (_, inputs) = parts(&shader);
            let mut indices: Vec<u32> = inputs
                .iter()
                .filter(|e| e.kind == 28)
                .map(|e| e.index)
                .collect();
            indices.sort();
            assert_eq!(indices, [0, 1], "{inputs:?}");
            validate(&shader);
        }

        /// Center and centroid perspective I/J both read as values: DXIL
        /// allows one perspective `SV_Barycentrics` element (the validator:
        /// "There can only be up to two input attributes of SV_Barycentrics
        /// with different perspective interpolation mode"), so the program
        /// is refused rather than given two, or center weights for centroid
        /// ones.
        #[test]
        fn center_and_centroid_barycentrics_are_refused() {
            // v0-v1 perspective center i, j; v2-v3 perspective centroid i, j.
            let [e0, e1] = exp_mrt0(4);
            let words = [v_add_f32(4, 0, 3), e0, e1, S_ENDPGM];
            match pixel(&words, PERSP_CENTER | PERSP_CENTROID) {
                Err(ShaderError::Unsupported { what, .. }) => {
                    assert!(what.contains("two perspective modes"), "{what}")
                }
                Err(e) => panic!("{e:?}"),
                Ok(shader) => panic!("translated: {:?}", parts(&shader).1),
            }
            // Centroid alone (v2, v3) still translates, with one element.
            let words = [v_add_f32(4, 2, 3), e0, e1, S_ENDPGM];
            let shader = pixel(&words, PERSP_CENTER | PERSP_CENTROID).unwrap();
            let (_, inputs) = parts(&shader);
            assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
            validate(&shader);
        }

        /// I and J only fed to `v_interp_*` stay host interpolation: no
        /// barycentrics are declared.
        #[test]
        fn interpolation_alone_declares_no_barycentrics() {
            let [e0, e1] = exp_mrt0(2);
            let words = [
                0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
                0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
                e0,
                e1,
                S_ENDPGM,
            ];
            let shader = pixel(&words, PERSP_CENTER).unwrap();
            let (features, inputs) = parts(&shader);
            assert_eq!(features & 0x2_0000, 0);
            assert!(inputs.iter().all(|e| e.kind != 28), "{inputs:?}");
            validate(&shader);
        }

        /// Both: the attribute is interpolated and J is used as a value.
        #[test]
        fn interpolation_and_a_barycentric_value_together() {
            let [e0, e1] = exp_mrt0(3);
            let words = [
                0xC808_0000,        // v_interp_p1_f32 v2, v0, attr0.x
                0xC809_0001,        // v_interp_p2_f32 v2, v1, attr0.x
                v_add_f32(3, 2, 1), // v3 = attribute + j
                e0,
                e1,
                S_ENDPGM,
            ];
            let shader = pixel(&words, PERSP_CENTER).unwrap();
            let (features, inputs) = parts(&shader);
            assert_ne!(features & 0x2_0000, 0);
            assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
            validate(&shader);
        }

        /// An interpolation whose i is computed (`v_mul_f32 v3, 0.5, v0`)
        /// reads the attribute's vertices: TEXCOORD0 becomes constant, read
        /// with `attributeAtVertex`; i and j are `SV_Barycentrics` values.
        #[test]
        fn computed_barycentrics_read_the_vertices() {
            let [e0, e1] = exp_mrt0(2);
            let words = [
                0x1006_00F0, // v_mul_f32 v3, 0.5, v0
                0xC808_0003, // v_interp_p1_f32 v2, v3, attr0.x
                0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
                e0,
                e1,
                S_ENDPGM,
            ];
            let shader = pixel(&words, PERSP_CENTER).unwrap();
            let (features, inputs) = parts(&shader);
            assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
            assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
            assert_eq!(texcoord_mode(&shader), 1);
            validate(&shader);
        }

        /// Weights from constants alone: the vertices, no `SV_Barycentrics`.
        #[test]
        fn constant_weights_need_no_barycentrics() {
            let [e0, e1] = exp_mrt0(2);
            let words = [
                0x7E06_02F0, // v_mov_b32 v3, 0.5
                0xC808_0003, // v_interp_p1_f32 v2, v3, attr0.x
                0xC809_0003, // v_interp_p2_f32 v2, v3, attr0.x
                e0,
                e1,
                S_ENDPGM,
            ];
            let shader = pixel(&words, PERSP_CENTER).unwrap();
            let (features, inputs) = parts(&shader);
            assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
            assert!(inputs.iter().all(|e| e.kind != 28), "{inputs:?}");
            assert_eq!(texcoord_mode(&shader), 1);
            validate(&shader);
        }

        /// The same attribute interpolated with the loaded i and j and with
        /// a computed j: both from its vertices, the first weighed with
        /// `SV_Barycentrics`.
        #[test]
        fn computed_and_loaded_barycentrics_on_one_attribute() {
            let [e0, e1] = exp_mrt0(2);
            let words = [
                0xC810_0100, // v_interp_p1_f32 v4, v0, attr0.y
                0xC811_0101, // v_interp_p2_f32 v4, v1, attr0.y
                0x1006_02F0, // v_mul_f32 v3, 0.5, v1
                0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
                0xC809_0003, // v_interp_p2_f32 v2, v3, attr0.x
                v_add_f32(2, 2, 4),
                e0,
                e1,
                S_ENDPGM,
            ];
            let shader = pixel(&words, PERSP_CENTER).unwrap();
            let (_, inputs) = parts(&shader);
            assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
            assert_eq!(texcoord_mode(&shader), 1);
            validate(&shader);
        }

        /// P10 of an attribute the program interpolates is P1 - P0.
        #[test]
        fn p10_of_an_interpolated_attribute() {
            let [e0, e1] = exp_mrt0(2);
            let words = [
                0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
                0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
                0xC812_0000, // v_interp_mov_f32 v4, p10, attr0.x
                v_add_f32(2, 2, 4),
                e0,
                e1,
                S_ENDPGM,
            ];
            let shader = pixel(&words, PERSP_CENTER).unwrap();
            assert_eq!(texcoord_mode(&shader), 1);
            validate(&shader);
            // Alone, what the hardware holds is not known.
            let [e0, e1] = exp_mrt0(4);
            let words = [0xC812_0000, e0, e1, S_ENDPGM];
            assert!(pixel(&words, PERSP_CENTER).is_err());
        }
    }
}
