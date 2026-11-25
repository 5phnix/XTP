//! The geometry stage behind a translated vertex shader for the console's
//! rectangle lists whose vertex shader has no vertex inputs. The title
//! sends a rectangle as three vertices, and the console's GPU makes the
//! fourth corner from the vertex shader's outputs. The host has no
//! rectangles: such a pipeline draws each rectangle as the triangle of its
//! three vertices, and this geometry shader adds the other half.
//!
//! What it does with each triangle `v0, v1, v2`, as the vertex shader wrote
//! them:
//! - the corner is the vertex whose position shares x with one other vertex
//!   and y with the third, and differs from each in the other coordinate
//!   (so the rectangle has an area, and a corner is the only one);
//! - with a corner, the three w equal, and every x, y and w finite (no NaN,
//!   no infinity), it emits the rectangle as two triangles. Both start at
//!   `v0`, so both halves' flat values are `v0`'s, as the triangle's are
//!   without the stage; and both turn as `v0, v1, v2` does (front faces
//!   stay front faces). Which: with `(corner, a, b)` the three rotated to
//!   start at the corner and `d` the fourth corner (opposite the corner),
//!   - corner `v0`: `v0, v1, d` and `v0, d, v2` (split from the corner to
//!     `d`: no other split puts `v0` in both);
//!   - corner `v1` or `v2`: `v0, v1, v2` as it is, and the other half,
//!     `v0, v2, d` (corner `v1`) or `v0, d, v1` (corner `v2`);
//! - else it emits the triangle as it is.
//!
//! The fourth corner `d`: its position's x is the x of the vertex that
//! shares y with the corner, its y the y of the one that shares x, its w
//! the corner's (the three are equal): the values themselves, so none is
//! lost to rounding or overflow. Every other float output (the position's
//! z too) is `a + b − corner` (added, then subtracted, as written): an
//! affine value goes on across the diagonal, however the pixel shader
//! reads it (interpolated by the hardware, or by the program itself from
//! the three vertices' values, `attributeAtVertex`). Its integer outputs
//! are `v0`'s (`d` never leads a triangle, so no flat value is `d`'s).
//!
//! The comparisons are ordered (`oeq`, `one`), so they treat +0 and −0 as
//! one value, as the rasterizer does: a corner at x −0 with a neighbour at
//! x +0 is one.
//!
//! Its input and its output signatures are the vertex shader's output
//! signature, element for element: names, registers, masks, the
//! components the vertex shader writes, and the interpolation modes its
//! `PSV0` declares. So the vertex shader links to it, and it links to the
//! pixel shader as the vertex shader did. With a geometry stage, the pixel
//! shader's `SV_PrimitiveID` comes from the stage's outputs (D3D12 refuses
//! a pipeline whose stage does not write one the pixel shader declares):
//! for a pixel shader that declares it, the stage also writes it, where
//! the pixel shader has it, on every vertex: the input triangle's id (the
//! host draws a rectangle as one triangle, so both halves have the
//! rectangle's, as without the stage).
//!
//! shortcut: a pixel shader that reads `SV_PrimitiveID` (a component in
//! its read mask) gets no stage ([`rectangle_gs`] refuses it), so its
//! rectangles stay one triangle. One that declares it without reading it
//! gets the stage.
//!
//! Reimplemented from the design in fxceless's study of their VIctory fork
//! (§3.1 point 3 there); no code of that fork or of xcl is here.

use super::bitcode::{Attr, Bin, Md, Module, Term, Ty, V, pred};
use super::container::{self, Psv, SigElement};
use crate::ShaderError;
use crate::dxbc::{Container, FourCC, u32_at};

/// The most vertices one invocation emits: two triangles of three.
const MAX_VERTICES: u32 = 6;

/// DXIL's input primitive `triangle`.
const TRIANGLE: u32 = 3;

/// DXIL's output topology `triangle strip`.
const TRIANGLE_STRIP: u32 = 5;

/// The program kind of a geometry shader (DXIL part, `PSV0`).
const GEOMETRY: u32 = 2;

/// Semantic kinds (see [`SigElement::kind`]).
const POSITION: u8 = 3;
const PRIMITIVE_ID: u8 = 10;

/// DXIL's `IsFinite` (`dx.op.isSpecialFloat`).
const IS_FINITE: u32 = 10;

/// DXIL's `PrimitiveID` (`dx.op.primitiveID`).
const PRIMITIVE_ID_OP: u32 = 108;

fn unsupported(what: &'static str) -> ShaderError {
    ShaderError::Unsupported { offset: 0, what }
}

/// The geometry shader (a DXIL container) that makes the fourth corner of
/// each rectangle drawn with the translated vertex shader `vs`, in a
/// pipeline whose pixel shader is `ps` (none: no pixel shader). Refused:
/// a pixel shader that reads `SV_PrimitiveID` (or has it without a
/// register, or in a row a vertex output has), and a vertex shader whose
/// position lacks x, y or w, or with a boolean output or an output without
/// a register.
pub fn rectangle_gs(vs: &[u8], ps: Option<&[u8]>) -> Result<Vec<u8>, ShaderError> {
    let primitive_id = match ps {
        Some(ps) => primitive_id(ps)?,
        None => None,
    };
    if primitive_id.as_ref().is_some_and(|&(_, read)| read) {
        return Err(unsupported("a pixel shader that reads SV_PrimitiveID"));
    }
    let elements = vertex_outputs(vs)?;
    let position = elements
        .iter()
        .position(|e| e.kind == POSITION)
        .ok_or(unsupported("a vertex shader without SV_Position"))?;
    let p = &elements[position];
    if p.col != 0 || p.cols != 4 || p.usage & 0b1011 != 0b1011 {
        return Err(unsupported("a vertex position without x, y and w"));
    }
    if elements.iter().any(|e| e.row.is_none()) {
        return Err(unsupported("a vertex output without a register"));
    }
    if elements.iter().any(|e| e.comp == 1) {
        return Err(unsupported("a boolean vertex output"));
    }
    let mut outputs = elements.clone();
    if let Some((id, _)) = primitive_id {
        let row = id
            .row
            .ok_or(unsupported("a pixel SV_PrimitiveID without a register"))?;
        if elements.iter().any(|e| e.row == Some(row)) {
            return Err(unsupported(
                "a pixel SV_PrimitiveID in a vertex output's row",
            ));
        }
        outputs.push(id);
    }
    Ok(build(&elements, &outputs, position))
}

/// The pixel shader `ps`'s (a DXIL container) `SV_PrimitiveID`, if it has
/// one: the element as the stage writes it (where the pixel shader has
/// it), and whether the program reads it (a component in its read mask).
fn primitive_id(ps: &[u8]) -> Result<Option<(SigElement, bool)>, ShaderError> {
    let c = Container::parse(ps)?;
    let Some(part) = c.part(FourCC::ISG1) else {
        return Ok(None);
    };
    let base = u32_at(part.data, 4)? as usize;
    for (i, mut e) in super::signature(part.data)?.into_iter().enumerate() {
        if e.kind != PRIMITIVE_ID {
            continue;
        }
        // An input row's second mask is the components the program reads.
        let read = byte(part.data, base + 32 * i + 25)? != 0;
        // Written whole; no interpolation mode, as the vertex shader's
        // system values have none.
        (e.usage, e.interp) = ((1 << e.cols) - 1, 0);
        return Ok(Some((e, read)));
    }
    Ok(None)
}

fn byte(data: &[u8], at: usize) -> Result<u8, ShaderError> {
    data.get(at)
        .copied()
        .ok_or(ShaderError::Malformed("truncated"))
}

/// The output signature of the vertex shader `vs`, in its `OSG1` order:
/// with the components it writes (`usage`) and the interpolation modes of
/// its `PSV0`.
fn vertex_outputs(vs: &[u8]) -> Result<Vec<SigElement>, ShaderError> {
    let c = Container::parse(vs)?;
    let osg1 = c
        .part(FourCC::OSG1)
        .ok_or(ShaderError::Malformed("no output signature"))?
        .data;
    let mut elements = super::signature(osg1)?;
    let base = u32_at(osg1, 4)? as usize;
    for (i, e) in elements.iter_mut().enumerate() {
        // An output row's second mask is the components it never writes.
        let at = base + 32 * i;
        let (mask, never) = (byte(osg1, at + 24)?, byte(osg1, at + 25)?);
        e.usage = (mask & !never) >> e.col;
    }
    let psv = c
        .part(FourCC::PSV0)
        .ok_or(ShaderError::Malformed("no PSV0"))?
        .data;
    interpolation(psv, &mut elements)?;
    Ok(elements)
}

/// Gives `outputs` the interpolation modes that `PSV0` lists for them,
/// found by start row and column.
fn interpolation(psv: &[u8], outputs: &mut [SigElement]) -> Result<(), ShaderError> {
    let info = u32_at(psv, 0)? as usize;
    if info < 36 {
        return Err(ShaderError::Malformed("PSV0 without signature counts"));
    }
    let (inputs, count) = (
        usize::from(byte(psv, 4 + 28)?),
        usize::from(byte(psv, 4 + 29)?),
    );
    let mut at = 4 + info;
    let resources = u32_at(psv, at)? as usize;
    at += 4;
    if resources > 0 {
        at += 4 + u32_at(psv, at)? as usize * resources;
    }
    at += 4 + u32_at(psv, at)? as usize;
    at += 4 + 4 * u32_at(psv, at)? as usize;
    let mut found = vec![false; outputs.len()];
    if count > 0 {
        let size = u32_at(psv, at)? as usize;
        at += 4;
        for k in 0..count {
            let e = at + size * (inputs + k);
            let (row, packed, interp) = (byte(psv, e + 9)?, byte(psv, e + 10)?, byte(psv, e + 13)?);
            if packed & 0x40 == 0 {
                continue;
            }
            let col = (packed >> 4) & 3;
            for (o, f) in outputs.iter_mut().zip(&mut found) {
                if o.row == Some(u32::from(row)) && o.col == col {
                    o.interp = interp;
                    *f = true;
                }
            }
        }
    }
    if found.contains(&false) {
        return Err(ShaderError::Malformed("a vertex output PSV0 does not list"));
    }
    Ok(())
}

/// A module under construction and the types it uses.
struct Gs {
    m: Module,
    void: Ty,
    i1: Ty,
    i32: Ty,
    f32: Ty,
}

/// One vertex's values: for each element, the components the vertex
/// shader writes.
type Vertex = Vec<[Option<V>; 4]>;

impl Gs {
    fn new() -> Gs {
        let mut m = Module::default();
        let (void, i1, i32, f32) = (m.void(), m.int(1), m.int(32), m.float());
        Gs {
            m,
            void,
            i1,
            i32,
            f32,
        }
    }

    /// Calls `dx.op.<name>` with `opcode` and `args`.
    fn op(&mut self, block: usize, name: &str, attr: Attr, ret: Ty, opcode: u32, args: &[V]) -> V {
        let mut params = vec![self.i32];
        params.extend(args.iter().map(|&a| self.m.type_of(a)));
        let fty = self.m.func_ty(ret, params);
        self.m.ptr(fty);
        let f = self
            .m
            .function(&format!("dx.op.{name}"), fty, Some(attr), false);
        let mut all = vec![self.m.i32(opcode)];
        all.extend(args);
        self.m.call(block, f, all)
    }

    /// Writes `vertex` to the outputs, then `primitive` (if any) to the
    /// one after them (the primitive id), and emits it on stream 0.
    fn emit(
        &mut self,
        block: usize,
        elements: &[SigElement],
        vertex: &Vertex,
        primitive: Option<V>,
    ) {
        if let Some(id) = primitive {
            let args = [
                self.m.i32(elements.len() as u32),
                self.m.i32(0),
                self.m.i8(0),
                id,
            ];
            let void = self.void;
            self.op(block, "storeOutput.i32", Attr::NoUnwind, void, 5, &args);
        }
        for (id, (e, comps)) in elements.iter().zip(vertex).enumerate() {
            for (c, value) in comps.iter().enumerate() {
                let Some(value) = *value else { continue };
                let suffix = if e.comp == 9 { "f32" } else { "i32" };
                let args = [
                    self.m.i32(id as u32),
                    self.m.i32(0),
                    self.m.i8(c as u8),
                    value,
                ];
                let void = self.void;
                self.op(
                    block,
                    &format!("storeOutput.{suffix}"),
                    Attr::NoUnwind,
                    void,
                    5,
                    &args,
                );
            }
        }
        let (stream, void) = (self.m.i8(0), self.void);
        self.op(block, "emitStream", Attr::NoUnwind, void, 97, &[stream]);
    }

    /// Ends the strip on stream 0.
    fn cut(&mut self, block: usize) {
        let (stream, void) = (self.m.i8(0), self.void);
        self.op(block, "cutStream", Attr::NoUnwind, void, 98, &[stream]);
    }

    fn and(&mut self, block: usize, a: V, b: V) -> V {
        self.m.bin(block, Bin::And, a, b)
    }

    fn or(&mut self, block: usize, a: V, b: V) -> V {
        self.m.bin(block, Bin::Or, a, b)
    }

    /// Whether every comparison `(predicate, a, b)` of `all` holds (it has
    /// one at least).
    fn all(&mut self, block: usize, all: &[(u8, V, V)]) -> V {
        let mut held = None;
        for &(p, a, b) in all {
            let c = self.m.cmp(block, p, a, b);
            held = Some(match held {
                Some(h) => self.and(block, h, c),
                None => c,
            });
        }
        held.expect("a comparison")
    }

    /// Whether the float `x` is finite (neither NaN nor an infinity).
    fn finite(&mut self, block: usize, x: V) -> V {
        let i1 = self.i1;
        self.op(
            block,
            "isSpecialFloat.f32",
            Attr::ReadNone,
            i1,
            IS_FINITE,
            &[x],
        )
    }

    /// `t` where `c` holds, else `f`: component by component.
    fn choose(&mut self, block: usize, c: V, t: &Vertex, f: &Vertex) -> Vertex {
        t.iter()
            .zip(f)
            .map(|(t, f)| {
                let mut out = [None; 4];
                for (o, (t, f)) in out.iter_mut().zip(t.iter().zip(f)) {
                    if let (Some(t), Some(f)) = (*t, *f) {
                        *o = Some(self.m.select(block, c, t, f));
                    }
                }
                out
            })
            .collect()
    }
}

/// The geometry shader for the vertex outputs `elements`, `position` the
/// index of `SV_Position` among them, that writes `outputs`: `elements`,
/// then the primitive id if there is one more.
fn build(elements: &[SigElement], outputs: &[SigElement], position: usize) -> Vec<u8> {
    let mut g = Gs::new();
    let main_ty = g.m.func_ty(g.void, vec![]);
    g.m.ptr(main_ty);
    let main = g.m.function("main", main_ty, None, true);
    let (entry, fan, plain, end) = (
        g.m.new_block(),
        g.m.new_block(),
        g.m.new_block(),
        g.m.new_block(),
    );

    // The three vertices, as the vertex shader wrote them.
    let mut v: Vec<Vertex> = Vec::with_capacity(3);
    for k in 0..3u32 {
        let mut vertex = Vec::with_capacity(elements.len());
        for (id, e) in elements.iter().enumerate() {
            let mut comps = [None; 4];
            for c in 0..e.cols {
                if e.usage >> c & 1 == 0 {
                    continue;
                }
                let (ty, suffix) = if e.comp == 9 {
                    (g.f32, "f32")
                } else {
                    (g.i32, "i32")
                };
                let args = [g.m.i32(id as u32), g.m.i32(0), g.m.i8(c), g.m.i32(k)];
                comps[usize::from(c)] = Some(g.op(
                    entry,
                    &format!("loadInput.{suffix}"),
                    Attr::ReadNone,
                    ty,
                    4,
                    &args,
                ));
            }
            vertex.push(comps);
        }
        v.push(vertex);
    }

    // Which vertex is the corner: it shares x with one of the others and y
    // with the other one, and differs from each in the other coordinate.
    // `along[k]`: vertex k shares x with the vertex after it (y with the
    // one before).
    let pos = |k: usize, c: usize| v[k][position][c].expect("x, y and w are written");
    let [x, y, w] = [0, 1, 3].map(|c| [0, 1, 2].map(|k| pos(k, c)));
    let (mut corner, mut along) = ([None; 3], [None; 3]);
    for k in 0..3 {
        let (i, j) = ((k + 1) % 3, (k + 2) % 3);
        let (eq, ne) = (pred::OEQ, pred::ONE);
        let one = g.all(
            entry,
            &[
                (eq, x[k], x[i]),
                (eq, y[k], y[j]),
                (ne, x[k], x[j]),
                (ne, y[k], y[i]),
            ],
        );
        let other = g.all(
            entry,
            &[
                (eq, x[k], x[j]),
                (eq, y[k], y[i]),
                (ne, x[k], x[i]),
                (ne, y[k], y[j]),
            ],
        );
        corner[k] = Some(g.or(entry, one, other));
        along[k] = Some(one);
    }
    let [c0, c1, c2] = corner.map(|c| c.expect("made above"));
    let along = along.map(|a| a.expect("made above"));
    let any = g.or(entry, c0, c1);
    let any = g.or(entry, any, c2);
    let same_w = g.all(entry, &[(pred::OEQ, w[0], w[1]), (pred::OEQ, w[0], w[2])]);
    let mut rectangle = g.and(entry, any, same_w);
    for value in x.into_iter().chain(y).chain(w) {
        let finite = g.finite(entry, value);
        rectangle = g.and(entry, rectangle, finite);
    }
    // The input triangle's id, for a pixel shader that declares one.
    let primitive = (outputs.len() > elements.len()).then(|| {
        let i32 = g.i32;
        g.op(
            entry,
            "primitiveID.i32",
            Attr::ReadNone,
            i32,
            PRIMITIVE_ID_OP,
            &[],
        )
    });
    g.m.terminate(entry, Term::CondBr(rectangle, fan, plain));

    // No rectangle: the triangle as it is.
    for vertex in &v {
        g.emit(plain, elements, vertex, primitive);
    }
    g.cut(plain);
    g.m.terminate(plain, Term::Br(end));

    // A rectangle. `(corner, a, b)` is `(v0, v1, v2)` rotated to start at
    // the corner (one of the three: the predicate makes it the only one).
    let pick = |g: &mut Gs, of: [V; 3]| {
        let inner = g.m.select(fan, c1, of[1], of[2]);
        g.m.select(fan, c0, of[0], inner)
    };
    // The corner shares x with `a`, y with `b`: `d` is (b.x, a.y), else
    // (a.x, b.y).
    let x_with_a = pick(&mut g, along);
    let mut d: Vertex = Vec::with_capacity(elements.len());
    for (id, e) in elements.iter().enumerate() {
        let mut dc = [None; 4];
        for (c, slot) in dc.iter_mut().enumerate() {
            let (Some(p0), Some(p1), Some(p2)) = (v[0][id][c], v[1][id][c], v[2][id][c]) else {
                continue;
            };
            let (at, after, before) = ([p0, p1, p2], [p1, p2, p0], [p2, p0, p1]);
            *slot = Some(match (id == position, c) {
                (true, 0 | 1) => {
                    let (a, b) = (pick(&mut g, after), pick(&mut g, before));
                    let (t, f) = if c == 0 { (b, a) } else { (a, b) };
                    g.m.select(fan, x_with_a, t, f)
                }
                (true, 3) => pick(&mut g, at),
                _ if e.comp == 9 => {
                    let (a, b) = (pick(&mut g, after), pick(&mut g, before));
                    let corner = pick(&mut g, at);
                    let sum = g.m.bin(fan, Bin::Add, a, b);
                    g.m.bin(fan, Bin::Sub, sum, corner)
                }
                _ => p0,
            });
        }
        d.push(dc);
    }
    // Both halves start at v0 (see the module): `v0, v1, q` and `v0, r, s`.
    let q = g.choose(fan, c0, &d, &v[2]);
    let r = g.choose(fan, c1, &v[2], &d);
    let s = g.choose(fan, c1, &d, &v[1]);
    let s = g.choose(fan, c0, &v[2], &s);
    for triangle in [[&v[0], &v[1], &q], [&v[0], &r, &s]] {
        for vertex in triangle {
            g.emit(fan, elements, vertex, primitive);
        }
        g.cut(fan);
    }
    g.m.terminate(fan, Term::Br(end));
    g.m.terminate(end, Term::Ret);
    finish(g, main, elements, outputs)
}

/// A signature's metadata: one node per element, as the other stages
/// declare theirs.
fn signature_md(m: &mut Module, elements: &[SigElement]) -> Md {
    fn c32(m: &mut Module, v: u32) -> Option<Md> {
        let c = m.i32(v);
        Some(m.md_value(c))
    }
    fn c8(m: &mut Module, v: u8) -> Option<Md> {
        let c = m.i8(v);
        Some(m.md_value(c))
    }
    let mut list = Vec::with_capacity(elements.len());
    for (id, e) in elements.iter().enumerate() {
        let index = c32(m, e.index);
        let indices = Some(m.md_node(vec![index]));
        let usage = if e.usage != 0 {
            let (tag, mask) = (c32(m, 3), c32(m, u32::from(e.usage)));
            Some(m.md_node(vec![tag, mask]))
        } else {
            None
        };
        let ops = vec![
            c32(m, id as u32),
            Some(m.md_str(&e.name)),
            c8(m, e.comp),
            c8(m, e.kind),
            indices,
            c8(m, e.interp),
            c32(m, 1),
            c8(m, e.cols),
            c32(m, e.row.unwrap_or(u32::MAX)),
            c8(m, if e.row.is_some() { e.col } else { u8::MAX }),
            usage,
        ];
        list.push(Some(m.md_node(ops)));
    }
    m.md_node(list)
}

/// The module's metadata and the container around it, for a stage that
/// reads `inputs` and writes `outputs`.
fn finish(mut g: Gs, main: V, inputs: &[SigElement], outputs: &[SigElement]) -> Vec<u8> {
    let m = &mut g.m;
    let i32c = |m: &mut Module, v: u32| {
        let c = m.i32(v);
        m.md_value(c)
    };
    // View-id state: the input scalars, then for each of the four streams
    // its output scalars and the inputs each depends on (every output on
    // every input, as the other stages declare it). Only stream 0 has
    // outputs.
    let scalars = |list: &[SigElement]| {
        list.iter()
            .filter_map(|e| e.row.map(|r| r * 4 + u32::from(e.col + e.cols)))
            .max()
            .unwrap_or(0)
    };
    let (ins, outs) = (scalars(inputs), scalars(outputs));
    let mut view = vec![ins, outs];
    for _ in 0..ins {
        for k in 0..outs.div_ceil(32) {
            let bits = (outs - 32 * k).min(32);
            view.push(if bits == 32 {
                u32::MAX
            } else {
                (1 << bits) - 1
            });
        }
    }
    view.extend([0, 0, 0]);
    let arr = m.array(view.len() as u32, g.i32);
    let words: Vec<V> = view.iter().map(|&w| m.i32(w)).collect();
    let arr = m.aggregate(arr, &words);
    let arr = m.md_value(arr);
    let node = m.md_node(vec![Some(arr)]);
    m.named_md("dx.viewIdState", vec![node]);

    let ins = signature_md(m, inputs);
    let outs = signature_md(m, outputs);
    let sigs = m.md_node(vec![Some(ins), Some(outs), None]);
    // Geometry state (tag 1): input primitive, most vertices, stream mask,
    // output topology, instances.
    let state = [TRIANGLE, MAX_VERTICES, 1, TRIANGLE_STRIP, 1].map(|n| Some(i32c(m, n)));
    let state = m.md_node(state.to_vec());
    let tag = i32c(m, 1);
    let props = m.md_node(vec![Some(tag), Some(state)]);
    let main = m.md_value(main);
    let name = m.md_str("main");
    let entry = m.md_node(vec![Some(main), Some(name), Some(sigs), None, Some(props)]);

    let ident = m.md_str("xtp");
    let ident = m.md_node(vec![Some(ident)]);
    m.named_md("llvm.ident", vec![ident]);
    let (one, six) = (i32c(m, 1), i32c(m, 6));
    let version = m.md_node(vec![Some(one), Some(six)]);
    m.named_md("dx.version", vec![version]);
    m.named_md("dx.valver", vec![version]);
    let gs = m.md_str("gs");
    let model = m.md_node(vec![Some(gs), Some(six), Some(six)]);
    m.named_md("dx.shaderModel", vec![model]);
    m.named_md("dx.entryPoints", vec![entry]);

    let bitcode = m.write();
    let mut dxil = Vec::with_capacity(24 + bitcode.len());
    dxil.extend((GEOMETRY << 16 | 0x66).to_le_bytes());
    dxil.extend((((24 + bitcode.len()) / 4) as u32).to_le_bytes());
    dxil.extend(b"DXIL");
    dxil.extend(0x106u32.to_le_bytes());
    dxil.extend(16u32.to_le_bytes());
    dxil.extend((bitcode.len() as u32).to_le_bytes());
    dxil.extend(&bitcode);

    let isg1 = container::signature(inputs, true);
    let osg1 = container::signature(outputs, false);
    let mut psv0 = container::psv(&Psv {
        stage: GEOMETRY as u8,
        flag: false,
        inputs,
        outputs,
        prims: &[],
        mesh: None,
        bindings: &[],
        threads: [0; 3],
        wave_size: None,
    });
    // The geometry runtime info (after the info's size): input primitive,
    // output topology, stream mask, position written; the most vertices.
    let info = 4;
    psv0[info..info + 4].copy_from_slice(&TRIANGLE.to_le_bytes());
    psv0[info + 4..info + 8].copy_from_slice(&TRIANGLE_STRIP.to_le_bytes());
    psv0[info + 8..info + 12].copy_from_slice(&1u32.to_le_bytes());
    psv0[info + 12] = 1;
    psv0[info + 26..info + 28].copy_from_slice(&(MAX_VERTICES as u16).to_le_bytes());
    let (sfi0, hash) = ([0u8; 8], [0u8; 20]);
    container::build(&[
        (b"SFI0", &sfi0),
        (b"ISG1", &isg1),
        (b"OSG1", &osg1),
        (b"PSV0", &psv0),
        (b"HASH", &hash),
        (b"DXIL", &dxil),
    ])
}

/// Shaders for the GPU tests of the stage (`xtp-gpu`'s rectangle tests):
/// shaped as translated ones are, with no vertex input.
pub mod probe {
    use super::super::bitcode::{Attr, Bin, Cast, Term, pred};
    use super::super::container::SigElement;
    use super::super::shader::Shader;

    fn element(name: &str, index: u32, kind: u8, comp: u8, interp: u8, row: u32) -> SigElement {
        let cols = if comp == 9 { 4 } else { 1 };
        SigElement {
            name: name.into(),
            index,
            kind,
            comp,
            interp,
            row: Some(row),
            col: 0,
            cols,
            usage: (1 << cols) - 1,
            half: false,
        }
    }

    /// The probe's vertex outputs and pixel inputs: `SV_Position` in row
    /// 0, `TEXCOORD0` (four floats, linear) in row 1, `TEXCOORD1` (a uint,
    /// constant) in row 2, as a translated vertex shader declares them.
    fn varyings() -> Vec<SigElement> {
        vec![
            element("SV_Position", 0, 3, 9, 4, 0),
            element("TEXCOORD", 0, 0, 9, 2, 1),
            element("TEXCOORD", 1, 0, 5, 1, 2),
        ]
    }

    /// A vertex shader without vertex inputs that writes, for vertex id
    /// `i`, `positions[i]` to `SV_Position`, `values[i]` to `TEXCOORD0`
    /// and `flat[i]` to `TEXCOORD1` (ids past them take the last vertex's).
    /// All three slices are as long, and not empty.
    pub fn vertex(positions: &[[f32; 4]], values: &[[f32; 4]], flat: &[u32]) -> Vec<u8> {
        assert!(!positions.is_empty() && positions.len() == values.len());
        assert_eq!(positions.len(), flat.len());
        let words = |n: usize, row: usize, c: usize| match row {
            0 => positions[n][c].to_bits(),
            1 => values[n][c].to_bits(),
            _ => flat[n],
        };
        with_outputs(varyings(), positions.len(), words)
    }

    /// [`vertex`], also writing `distances[i]` to `SV_ClipDistance0`
    /// (`kind` 6) or `SV_CullDistance0` (`kind` 7): one float in row 3,
    /// which the probe pixel shader does not read. `distances` is as long
    /// as the others.
    pub fn vertex_with_distance(
        positions: &[[f32; 4]],
        values: &[[f32; 4]],
        flat: &[u32],
        kind: u8,
        distances: &[f32],
    ) -> Vec<u8> {
        assert!(matches!(kind, 6 | 7), "a clip or cull distance");
        assert!(!positions.is_empty() && positions.len() == values.len());
        assert_eq!(positions.len(), flat.len());
        assert_eq!(positions.len(), distances.len());
        let mut outputs = varyings();
        let name = if kind == 6 {
            "SV_ClipDistance"
        } else {
            "SV_CullDistance"
        };
        let mut distance = element(name, 0, kind, 9, 2, 3);
        (distance.cols, distance.usage) = (1, 1);
        outputs.push(distance);
        let words = |n: usize, row: usize, c: usize| match row {
            0 => positions[n][c].to_bits(),
            1 => values[n][c].to_bits(),
            2 => flat[n],
            _ => distances[n].to_bits(),
        };
        with_outputs(outputs, positions.len(), words)
    }

    /// A vertex shader without vertex inputs that writes `outputs` (the
    /// components their `usage` says), `word(vertex, element, column)` for
    /// each of `vertices` vertex ids (ids past them take the last one's).
    pub(in super::super) fn with_outputs(
        outputs: Vec<SigElement>,
        vertices: usize,
        word: impl Fn(usize, usize, usize) -> u32,
    ) -> Vec<u8> {
        let mut s = Shader::new(false);
        let b = s.entry;
        let (i32t, f32t, void) = (s.t.i32, s.t.f32, s.t.void);
        s.inputs.push(SigElement {
            name: "SV_VertexID".into(),
            index: 0,
            kind: 1,
            comp: 5,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 1,
            usage: 1,
            half: false,
        });
        let undef = s.m.undef(i32t);
        let args = [s.m.i32(0), s.m.i32(0), s.m.i8(0), undef];
        let id = s.op(b, "loadInput.i32", Attr::ReadNone, i32t, 4, &args);
        for (n, e) in outputs.iter().enumerate() {
            for c in 0..usize::from(e.cols) {
                if e.usage >> c & 1 == 0 {
                    continue;
                }
                let mut value = s.m.i32(word(vertices - 1, n, c));
                for k in (0..vertices - 1).rev() {
                    let (at, here) = (s.m.i32(k as u32), s.m.i32(word(k, n, c)));
                    let is = s.m.cmp(b, pred::EQ, id, at);
                    value = s.m.select(b, is, here, value);
                }
                let (suffix, value) = if e.comp == 9 {
                    (".f32", s.m.cast(b, Cast::Bitcast, value, f32t))
                } else {
                    (".i32", value)
                };
                let args = [s.m.i32(n as u32), s.m.i32(0), s.m.i8(c as u8), value];
                s.op(
                    b,
                    &format!("storeOutput{suffix}"),
                    Attr::NoUnwind,
                    void,
                    5,
                    &args,
                );
            }
        }
        s.outputs = outputs;
        s.m.terminate(b, Term::Ret);
        s.finish()
    }

    /// A pixel shader that writes `TEXCOORD0.xyz` and `TEXCOORD1` (as a
    /// float) to target 0 (four floats), with the inputs of [`vertex`].
    pub fn pixel() -> Vec<u8> {
        pixel_with(2, PrimitiveId::None)
    }

    /// What the probe pixel shader has of `SV_PrimitiveID`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PrimitiveId {
        /// Not in its input signature.
        None,
        /// In its input signature, never read (a read mask of nothing).
        Declared,
        /// Read, and added to `TEXCOORD1` before it is written.
        Read,
    }

    /// [`pixel`], with `TEXCOORD0` interpolated as DXIL mode `mode` says
    /// (1 constant, 2 linear, 3 centroid, 4 noperspective, 6 per sample;
    /// the vertex shader declares it linear), and `SV_PrimitiveID` as
    /// `primitive_id` says.
    pub fn pixel_with(mode: u8, primitive_id: PrimitiveId) -> Vec<u8> {
        let mut s = Shader::new(true);
        let b = s.entry;
        let (i32t, f32t, void) = (s.t.i32, s.t.f32, s.t.void);
        let mut inputs = varyings();
        inputs[0].usage = 0;
        inputs[1].usage = 0b0111;
        inputs[1].interp = mode;
        if primitive_id != PrimitiveId::None {
            let mut id = element("SV_PrimitiveID", 0, 10, 5, 1, 3);
            id.usage = u8::from(primitive_id == PrimitiveId::Read);
            inputs.push(id);
        }
        let undef = s.m.undef(i32t);
        let load = |s: &mut Shader, id: u32, c: u8, ty, suffix: &str| {
            let args = [s.m.i32(id), s.m.i32(0), s.m.i8(c), undef];
            s.op(
                b,
                &format!("loadInput{suffix}"),
                Attr::ReadNone,
                ty,
                4,
                &args,
            )
        };
        let mut out = Vec::with_capacity(4);
        for c in 0..3 {
            out.push(load(&mut s, 1, c, f32t, ".f32"));
        }
        let mut flat = load(&mut s, 2, 0, i32t, ".i32");
        if primitive_id == PrimitiveId::Read {
            let id = load(&mut s, 3, 0, i32t, ".i32");
            flat = s.m.bin(b, Bin::Add, flat, id);
        }
        out.push(s.m.cast(b, Cast::UiToFp, flat, f32t));
        for (c, value) in out.into_iter().enumerate() {
            let args = [s.m.i32(0), s.m.i32(0), s.m.i8(c as u8), value];
            s.op(b, "storeOutput.f32", Attr::NoUnwind, void, 5, &args);
        }
        s.inputs = inputs;
        s.outputs = vec![SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 0b1111,
            half: false,
        }];
        s.m.terminate(b, Term::Ret);
        s.finish()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::probe;
    use super::*;

    /// Validates `container` with the system's `dxil.dll` when there is
    /// one (as the other emission tests do; noted on standard error
    /// without one).
    fn validate(what: &str, container: &[u8]) {
        use super::super::validator::Validator;
        match Validator::load(Validator::default_path().as_deref()) {
            Ok(mut v) => {
                if let Err(e) = v.validate(&mut container.to_vec()) {
                    panic!("{what}: validation: {e}");
                }
            }
            Err(e) => eprintln!("{what}: not validated ({e})"),
        }
    }

    fn signature_of(container: &[u8], fourcc: FourCC) -> Vec<u8> {
        Container::parse(container)
            .expect("a container")
            .part(fourcc)
            .expect("the part")
            .data
            .to_vec()
    }

    fn quad_probe() -> Vec<u8> {
        let positions = [[0.0, 0.0, 0.0, 1.0]; 3];
        probe::vertex(&positions, &[[0.5; 4]; 3], &[1, 2, 3])
    }

    /// The probes and the stage made for them validate, and the stage's
    /// input and output signatures are the vertex shader's output
    /// signature, byte for byte.
    #[test]
    fn the_stage_validates_and_mirrors_the_vertex_outputs() {
        let (vs, ps) = (quad_probe(), probe::pixel());
        validate("vertex probe", &vs);
        validate("pixel probe", &ps);
        let gs = rectangle_gs(&vs, Some(&ps)).expect("a stage");
        validate("stage", &gs);
        let outputs = signature_of(&vs, FourCC::OSG1);
        assert_eq!(signature_of(&gs, FourCC::OSG1), outputs);
        let read: Vec<(String, u32, u8, u8, u8)> =
            super::super::signature(&signature_of(&gs, FourCC::ISG1))
                .unwrap()
                .into_iter()
                .map(|e| (e.name, e.index, e.kind, e.col, e.cols))
                .collect();
        let written: Vec<(String, u32, u8, u8, u8)> = super::super::signature(&outputs)
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.index, e.kind, e.col, e.cols))
            .collect();
        assert_eq!(read, written);
        let elements = vertex_outputs(&gs).expect("its own outputs");
        assert_eq!(
            elements
                .iter()
                .map(|e| (e.interp, e.usage))
                .collect::<Vec<_>>(),
            [(4, 0b1111), (2, 0b1111), (1, 0b1)],
            "interpolation modes and written components"
        );
    }

    /// Rows that hold two elements, components the vertex shader does not
    /// write, and clip distances: the stage reads and writes what the
    /// vertex shader writes, in the same rows and columns.
    #[test]
    fn packed_rows_and_unwritten_components_stay_as_the_vertex_shader_has_them() {
        let el = |name: &str, index, kind, comp, interp, (row, col, cols), usage| SigElement {
            name: String::from(name),
            index,
            kind,
            comp,
            interp,
            row: Some(row),
            col,
            cols,
            usage,
            half: false,
        };
        let outputs = vec![
            el("SV_Position", 0, 3, 9, 4, (0, 0, 4), 0b1111),
            el("TEXCOORD", 0, 0, 9, 2, (1, 0, 4), 0b0101),
            el("TEXCOORD", 1, 0, 5, 1, (2, 0, 2), 0b11),
            el("TEXCOORD", 2, 0, 4, 1, (2, 2, 1), 0b1),
            el("TEXCOORD", 3, 0, 9, 2, (3, 0, 3), 0b111),
            el("SV_ClipDistance", 0, 6, 9, 2, (4, 0, 2), 0b11),
        ];
        let vs = probe::with_outputs(outputs, 3, |v, e, c| (v * 31 + e * 7 + c) as u32);
        validate("vertex shader", &vs);
        let gs = rectangle_gs(&vs, None).expect("a stage");
        validate("stage", &gs);
        assert_eq!(
            signature_of(&gs, FourCC::OSG1),
            signature_of(&vs, FourCC::OSG1)
        );
        let usage: Vec<(Option<u32>, u8, u8, u8)> = vertex_outputs(&gs)
            .unwrap()
            .iter()
            .map(|e| (e.row, e.col, e.usage, e.interp))
            .collect();
        assert_eq!(
            usage,
            [
                (Some(0), 0, 0b1111, 4),
                (Some(1), 0, 0b0101, 2),
                (Some(2), 0, 0b11, 1),
                (Some(2), 2, 0b1, 1),
                (Some(3), 0, 0b111, 2),
                (Some(4), 0, 0b11, 2),
            ]
        );
    }

    /// The probe pixel shaders of the GPU tests validate: each
    /// interpolation mode of `TEXCOORD0`, and the primitive id declared or
    /// read. The stage serves them all but the one that reads the id.
    #[test]
    fn the_probe_pixel_shaders_validate() {
        use probe::PrimitiveId;
        let vs = quad_probe();
        for mode in [1, 2, 3, 4, 6] {
            let ps = probe::pixel_with(mode, PrimitiveId::None);
            validate(&format!("pixel shader, mode {mode}"), &ps);
            assert!(rectangle_gs(&vs, Some(&ps)).is_ok(), "mode {mode}");
        }
        let (positions, values) = ([[0.0, 0.0, 0.0, 1.0]; 3], [[0.5; 4]; 3]);
        for kind in [6, 7] {
            let vs = probe::vertex_with_distance(&positions, &values, &[1, 2, 3], kind, &[1.0; 3]);
            validate(&format!("vertex shader with distance kind {kind}"), &vs);
            let gs = rectangle_gs(&vs, Some(&probe::pixel())).expect("a stage");
            validate(&format!("stage with distance kind {kind}"), &gs);
        }
    }

    /// A pixel shader that declares `SV_PrimitiveID` and never reads it
    /// (its read mask empty) gets the stage; one that reads it does not.
    #[test]
    fn only_a_read_primitive_id_refuses_the_stage() {
        use probe::PrimitiveId;
        let vs = quad_probe();
        let declared = probe::pixel_with(2, PrimitiveId::Declared);
        validate("pixel shader declaring a primitive id", &declared);
        let gs = rectangle_gs(&vs, Some(&declared)).expect("a stage");
        validate("stage for it", &gs);
        // It writes the primitive id where the pixel shader has it, after
        // the vertex shader's outputs (D3D12 refuses the pipeline else).
        let written: Vec<(u8, Option<u32>, u8, u8)> = vertex_outputs(&gs)
            .unwrap()
            .iter()
            .map(|e| (e.kind, e.row, e.col, e.usage))
            .collect();
        assert_eq!(
            written,
            [
                (POSITION, Some(0), 0, 0b1111),
                (0, Some(1), 0, 0b1111),
                (0, Some(2), 0, 0b1),
                (PRIMITIVE_ID, Some(3), 0, 0b1),
            ]
        );
        let inputs = signature_of(&gs, FourCC::ISG1);
        assert_eq!(
            inputs,
            container::signature(&vertex_outputs(&vs).unwrap(), true)
        );
        let read = probe::pixel_with(2, PrimitiveId::Read);
        validate("pixel shader reading a primitive id", &read);
        assert!(matches!(
            rectangle_gs(&vs, Some(&read)),
            Err(ShaderError::Unsupported { what, .. }) if what.contains("SV_PrimitiveID")
        ));
    }

    /// No stage for a position the vertex shader does not write whole.
    #[test]
    fn what_the_stage_cannot_serve_is_refused() {
        let mut outputs = vec![SigElement {
            name: "SV_Position".into(),
            index: 0,
            kind: 3,
            comp: 9,
            interp: 4,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 0b0111,
            half: false,
        }];
        let no_w = probe::with_outputs(outputs.clone(), 1, |_, _, _| 0);
        assert!(matches!(
            rectangle_gs(&no_w, None),
            Err(ShaderError::Unsupported { what, .. }) if what.contains("x, y and w")
        ));
        outputs[0].kind = 0;
        outputs[0].name = "TEXCOORD".into();
        outputs[0].usage = 0b1111;
        let no_position = probe::with_outputs(outputs, 1, |_, _, _| 0);
        assert!(rectangle_gs(&no_position, None).is_err());
    }
}
