//! Synthetic worlds for the proof by play (`docs/analysis/rt-dxr.md`
//! section 5.3): instances of triangle BLASes, laid out in the console's
//! format for the guest walk ([`Layout`]), with the RDNA2 intersection the
//! walk's instruction computes ([`Layout::intersect`]) and DXR's answer to
//! the host query ([`World::query`]).
//!
//! The console's format, as family A's walk reads it (`32ed072b9f845048`):
//!
//! * a structure's nodes are 64-byte units from its base; a node pointer is
//!   the unit's index times 8, plus its type in the low 3 bits (0 and 1 the
//!   triangles of a pair node, 5 a box of four children, 6 an instance);
//! * the TLAS header holds the root pointer at +0x30 (0: empty);
//! * a box holds its four children's pointers at +0x00 (all ones: none);
//! * an instance holds at +0x30 its id and mask (`id | mask << 24`), its
//!   hit-group contribution and flags (`contribution | flags << 24`), its
//!   BLAS root pointer (bit 31: the transform is a translation) and its
//!   BLAS base over 256, then at +0x40 its world-to-object transform: the
//!   translation, then the 3×3 rows;
//! * a BLAS header holds at +0x34 and +0x38 where its parent table is: the
//!   parent of the node at unit `i` is the dword at
//!   `base + 4 · (i − ((w34 >> 6) − (w38 >> 2)))` (all ones above the root);
//! * a triangle node's word at +0x30 has bit 7 set when it holds a pair.
//!
//! The boxes' bounds and the triangles' vertices are not planted in memory:
//! the walk does not read them, the model of its intersection instruction
//! does, from the layout's own tables. That model is RADV's reading of the
//! RDNA2 hardware: boxes return their children sorted by entry distance,
//! misses last as all ones; a triangle returns `t_num`, `t_denom`, `i`, `j`
//! with `t = t_num / t_denom` and `t_denom > 0` on a front face. Here
//! `t_denom` is ±1, so that the guest's t is the model's exactly. Front
//! faces are DXR's (clockwise seen from the ray's origin in object space),
//! as calibrated on WARP (`rayquery_tests`).

use std::collections::HashMap;

use super::Why;
use super::play::{BvhRay, QueryRay};
use super::ray_flags;

/// SplitMix64: a small deterministic generator.
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn bits(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// In `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.bits() % n.max(1) as u64) as usize
    }

    pub fn unit(&mut self) -> f64 {
        (self.bits() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

pub(crate) type Tri = [[f32; 3]; 3];
type V3 = [f64; 3];

/// D3D12_RAYTRACING_INSTANCE_FLAG_TRIANGLE_CULL_DISABLE.
pub(crate) const CULL_DISABLE: u8 = 1;
/// D3D12_RAYTRACING_INSTANCE_FLAG_TRIANGLE_FRONT_COUNTERCLOCKWISE.
pub(crate) const FRONT_CCW: u8 = 2;

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn scale(a: V3, s: f64) -> V3 {
    a.map(|x| x * s)
}

fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn wide(v: [f32; 3]) -> V3 {
    v.map(f64::from)
}

fn len(a: V3) -> f64 {
    dot(a, a).sqrt()
}

/// A triangle met by a ray: its t, its barycentrics and the determinant
/// whose sign is its facing (positive: front, DXR's).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Meet {
    pub t: f64,
    pub u: f64,
    pub v: f64,
    pub det: f64,
}

/// Möller–Trumbore on `tri` for the ray `o + t·d`, in f64; what it gives
/// whether or not the ray passes inside (`None` only when it is parallel).
pub(crate) fn meet(tri: &Tri, o: V3, d: V3) -> Option<Meet> {
    let [v0, v1, v2] = tri.map(wide);
    let (e1, e2) = (sub(v1, v0), sub(v2, v0));
    let p = cross(d, e2);
    let det = dot(e1, p);
    if det == 0.0 || !det.is_finite() {
        return None;
    }
    let s = sub(o, v0);
    let u = dot(s, p) / det;
    let q = cross(s, e1);
    let v = dot(d, q) / det;
    let t = dot(e2, q) / det;
    Some(Meet { t, u, v, det })
}

impl Meet {
    pub fn inside(&self) -> bool {
        self.u >= 0.0 && self.v >= 0.0 && self.u + self.v <= 1.0
    }

    /// Whether float rounding could put the ray on either side of an edge.
    fn grazes(&self) -> bool {
        const NEAR: f64 = 1e-4;
        [self.u, self.v, 1.0 - self.u - self.v]
            .iter()
            .any(|b| b.abs() < NEAR)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Instance {
    /// World to object: rows of `[R | t]`, object = R · world + t.
    pub to_object: [[f32; 4]; 3],
    /// The console's flag for a transform that only translates.
    pub translation_only: bool,
    pub mask: u8,
    /// D3D12 instance flags ([`CULL_DISABLE`], [`FRONT_CCW`], others).
    pub flags: u8,
    pub id: u32,
    pub contribution: u32,
    pub blas: usize,
}

impl Instance {
    /// Object space of a world-space ray, in f64.
    pub fn object_ray(&self, o: V3, d: V3) -> (V3, V3) {
        let m = self.to_object.map(|r| r.map(f64::from));
        let row = |r: usize, v: V3, w: f64| {
            m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2] + w * m[r][3]
        };
        (
            [row(0, o, 1.0), row(1, o, 1.0), row(2, o, 1.0)],
            [row(0, d, 0.0), row(1, d, 0.0), row(2, d, 0.0)],
        )
    }

    /// Object to world: rows of the 3×4 inverse, as D3D12's instance
    /// descriptions take it.
    pub fn to_world(&self) -> [[f32; 4]; 3] {
        let m = self.to_object.map(|r| r.map(f64::from));
        let r = [
            [m[0][0], m[0][1], m[0][2]],
            [m[1][0], m[1][1], m[1][2]],
            [m[2][0], m[2][1], m[2][2]],
        ];
        let det = dot(r[0], cross(r[1], r[2]));
        let c = |i: usize, j: usize| {
            let (a, b) = ((i + 1) % 3, (i + 2) % 3);
            let (x, y) = ((j + 1) % 3, (j + 2) % 3);
            r[a][x] * r[b][y] - r[a][y] * r[b][x]
        };
        // The inverse is the adjugate over the determinant.
        let inv: [[f64; 3]; 3] = std::array::from_fn(|i| std::array::from_fn(|j| c(j, i) / det));
        let t = [m[0][3], m[1][3], m[2][3]];
        std::array::from_fn(|i| {
            let back = -(inv[i][0] * t[0] + inv[i][1] * t[1] + inv[i][2] * t[2]);
            [
                inv[i][0] as f32,
                inv[i][1] as f32,
                inv[i][2] as f32,
                back as f32,
            ]
        })
    }

    fn to_world_point(&self, p: V3) -> V3 {
        let w = self.to_world().map(|r| r.map(f64::from));
        std::array::from_fn(|i| w[i][0] * p[0] + w[i][1] * p[1] + w[i][2] * p[2] + w[i][3])
    }
}

/// A BLAS: leaves of one or two triangles (a pair node), in object space.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Blas {
    pub leaves: Vec<Vec<Tri>>,
    /// A box whose only child is itself, beside the BLAS's own root (its
    /// bounds, in object space): a walk that enters it never leaves until
    /// its iteration cap.
    pub cycle: Option<[[f32; 3]; 2]>,
}

impl Blas {
    pub fn triangles(&self) -> impl Iterator<Item = &Tri> {
        self.leaves.iter().flatten()
    }
}

/// What the TLAS's root is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Root {
    /// None: the header's root pointer is 0.
    Empty,
    /// A box over the instances.
    Box,
    /// The only instance itself.
    Instance,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct World {
    pub root: Root,
    pub instances: Vec<Instance>,
    pub blases: Vec<Blas>,
}

/// The host query's answer for one ray.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Answer {
    pub hit: bool,
    /// The committed t, or the ray's t-max.
    pub t: f32,
    /// A candidate triangle lies at t-min or t-max, where DXR and the
    /// console's walk differ (`Kind::T`).
    pub edge: bool,
    /// Float rounding could change the answer: a candidate grazes a
    /// triangle's edge or runs along its plane.
    pub fragile: bool,
}

/// How close to t-min or t-max a candidate counts as at it.
fn near(t: f64, at: f64) -> bool {
    (t - at).abs() <= 1e-5 * at.abs().max(1.0)
}

impl World {
    pub fn empty() -> World {
        World {
            root: Root::Empty,
            instances: Vec::new(),
            blases: Vec::new(),
        }
    }

    /// DXR's answer to `ray`: closest hit, opaque, culling as its flags and
    /// the instances' say, a hit at t-min < t < t-max (the specification's
    /// rule; WARP keeps one at t-max, which `edge` leaves out).
    pub fn query(&self, ray: &QueryRay) -> Answer {
        let (t_min, t_max) = (f64::from(ray.t_min), f64::from(ray.t_max));
        let mut best: Option<f64> = None;
        let (mut edge, mut fragile) = (false, false);
        let (o, d) = (wide(ray.origin), wide(ray.direction));
        let cull_back = ray.flags & ray_flags::CULL_BACK_FACING_TRIANGLES != 0;
        if self.root == Root::Empty {
            return Answer {
                hit: false,
                t: ray.t_max,
                edge,
                fragile,
            };
        }
        for inst in &self.instances {
            if u32::from(inst.mask) & ray.mask == 0 {
                continue;
            }
            let (oo, od) = inst.object_ray(o, d);
            for tri in self.blases[inst.blas].triangles() {
                let Some(m) = meet(tri, oo, od) else {
                    continue;
                };
                let scale = len(sub(wide(tri[1]), wide(tri[0])))
                    * len(sub(wide(tri[2]), wide(tri[0])))
                    * len(od);
                let in_range = m.t > t_min - 1e-4 && m.t < t_max * (1.0 + 1e-4) + 1e-4;
                if in_range && (m.grazes() || m.det.abs() < 1e-7 * scale) {
                    fragile = true;
                }
                if !m.inside() {
                    continue;
                }
                let front = (m.det > 0.0) != (inst.flags & FRONT_CCW != 0);
                if cull_back && inst.flags & CULL_DISABLE == 0 && !front {
                    continue;
                }
                if near(m.t, t_min) || near(m.t, t_max) {
                    edge = true;
                }
                if m.t > t_min && m.t < t_max && best.is_none_or(|b| m.t < b) {
                    best = Some(m.t);
                }
            }
        }
        Answer {
            hit: best.is_some(),
            t: best.map_or(ray.t_max, |t| t as f32),
            edge,
            fragile,
        }
    }
}

/// An axis-aligned box, in f64.
#[derive(Debug, Clone, Copy)]
struct Aabb {
    min: V3,
    max: V3,
}

impl Aabb {
    fn of(points: impl IntoIterator<Item = V3>) -> Aabb {
        let mut b = Aabb {
            min: [f64::INFINITY; 3],
            max: [f64::NEG_INFINITY; 3],
        };
        for p in points {
            for ((lo, hi), x) in b.min.iter_mut().zip(&mut b.max).zip(p) {
                *lo = lo.min(x);
                *hi = hi.max(x);
            }
        }
        b
    }

    fn join(self, o: Aabb) -> Aabb {
        Aabb::of([self.min, self.max, o.min, o.max])
    }

    /// Grown by a margin that float rounding never crosses.
    fn padded(self) -> Aabb {
        let extent = (0..3)
            .map(|k| self.max[k] - self.min[k])
            .fold(0.0, f64::max);
        let far = self
            .min
            .iter()
            .chain(&self.max)
            .fold(1.0f64, |a, x| a.max(x.abs()));
        let pad = 1e-3 * extent + 1e-6 * far + 1e-4;
        Aabb {
            min: self.min.map(|x| x - pad),
            max: self.max.map(|x| x + pad),
        }
    }

    fn center(&self) -> V3 {
        scale(add(self.min, self.max), 0.5)
    }

    fn corners(&self) -> [V3; 8] {
        std::array::from_fn(|i| {
            [
                if i & 1 == 0 { self.min[0] } else { self.max[0] },
                if i & 2 == 0 { self.min[1] } else { self.max[1] },
                if i & 4 == 0 { self.min[2] } else { self.max[2] },
            ]
        })
    }

    /// The slab test with an inverse direction: entry t, if the ray meets
    /// the box before `t_max`. Conservative: a NaN counts as a meeting.
    fn enter(&self, o: V3, inv: V3, t_max: f64) -> Option<f64> {
        let (mut t0, mut t1) = (f64::NEG_INFINITY, f64::INFINITY);
        for k in 0..3 {
            let (a, b) = ((self.min[k] - o[k]) * inv[k], (self.max[k] - o[k]) * inv[k]);
            if a.is_nan() || b.is_nan() {
                continue;
            }
            t0 = t0.max(a.min(b));
            t1 = t1.min(a.max(b));
        }
        (t0 <= t1 && t1 >= 0.0 && t0 <= t_max).then_some(t0)
    }
}

/// A node of a laid-out structure.
#[derive(Debug, Clone)]
enum Node {
    Box {
        kids: [u32; 4],
        bounds: Box<[Option<Aabb>; 4]>,
    },
    Leaf(Vec<Tri>),
    Instance,
}

/// The console's image of a world: its memory, and the nodes the
/// intersection model reads.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub memory: HashMap<u64, u32>,
    nodes: HashMap<u64, Node>,
    /// The TLAS's root pointer (0 for an empty world).
    pub root: u32,
}

/// Unit index of a TLAS's root node: after the header.
const ROOT_UNIT: u32 = 2;
/// Box and instance nodes take two units, triangle nodes one.
const BOX: u32 = 5;
const INSTANCE: u32 = 6;

/// The root pointer a world of `root` has, laid out at any base.
pub(crate) fn root_pointer(root: Root) -> u32 {
    match root {
        Root::Empty => 0,
        Root::Box => ROOT_UNIT << 3 | BOX,
        Root::Instance => ROOT_UNIT << 3 | INSTANCE,
    }
}

/// A structure being laid out: its base and next free unit.
struct Builder<'a> {
    layout: &'a mut Layout,
    base: u64,
    next: u32,
    /// Parent pointer of each unit (BLAS only).
    parents: Vec<(u32, u32)>,
    rng: &'a mut Rng,
}

impl Builder<'_> {
    fn put(&mut self, unit: u32, offset: u64, value: u32) {
        self.layout
            .memory
            .insert(self.base + 64 * u64::from(unit) + offset, value);
    }

    fn take(&mut self, units: u32) -> u32 {
        let unit = self.next;
        self.next += units;
        unit
    }

    /// Lays out a box over `items` (leaf indices with their bounds) at
    /// `unit`, its subtrees after it; `leaf` lays out one item and returns
    /// its pointer.
    fn tree(
        &mut self,
        unit: u32,
        items: &mut [(usize, Aabb)],
        leaf: &mut dyn FnMut(&mut Self, usize) -> u32,
    ) -> Aabb {
        let pointer = unit << 3 | BOX;
        let groups: Vec<std::ops::Range<usize>> = if items.len() <= 4 {
            (0..items.len()).map(|i| i..i + 1).collect()
        } else {
            let all = items.iter().fold(items[0].1, |b, i| b.join(i.1));
            let axis = (0..3)
                .max_by(|&a, &b| (all.max[a] - all.min[a]).total_cmp(&(all.max[b] - all.min[b])))
                .unwrap_or(0);
            items.sort_by(|a, b| a.1.center()[axis].total_cmp(&b.1.center()[axis]));
            let n = items.len();
            (0..4).map(|g| g * n / 4..(g + 1) * n / 4).collect()
        };
        let mut kids = [u32::MAX; 4];
        let mut bounds = [None; 4];
        // Children in a shuffled slot order, as a builder may leave them.
        let mut slots = [0usize, 1, 2, 3];
        for k in (1..4).rev() {
            slots.swap(k, self.rng.below(k + 1));
        }
        for (g, range) in groups.into_iter().enumerate() {
            let slot = slots[g];
            let (kid, b) = if range.len() == 1 {
                let (i, b) = items[range.start];
                (leaf(self, i), b)
            } else {
                let at = self.take(2);
                let b = self.tree(at, &mut items[range], leaf);
                (at << 3 | BOX, b)
            };
            self.parents.push((kid >> 3, pointer));
            kids[slot] = kid;
            bounds[slot] = Some(b.padded());
        }
        for (k, &kid) in kids.iter().enumerate() {
            self.put(unit, 4 * k as u64, kid);
        }
        let node = Node::Box {
            kids,
            bounds: Box::new(bounds),
        };
        self.layout
            .nodes
            .insert(self.base + 64 * u64::from(unit), node);
        bounds
            .iter()
            .flatten()
            .fold(None::<Aabb>, |a, b| Some(a.map_or(*b, |a| a.join(*b))))
            .unwrap_or(Aabb {
                min: [0.0; 3],
                max: [0.0; 3],
            })
    }
}

impl Layout {
    /// `world` laid out with its TLAS at `base` (256-byte aligned) and its
    /// BLASes after it.
    pub fn new(world: &World, base: u64, rng: &mut Rng) -> Layout {
        let mut layout = Layout {
            memory: HashMap::new(),
            nodes: HashMap::new(),
            root: root_pointer(world.root),
        };
        layout.memory.insert(base + 0x30, layout.root);
        // The BLASes, each at its own 1 MiB.
        let mut blas_roots = Vec::new();
        for (n, blas) in world.blases.iter().enumerate() {
            let at = base + 0x10_0000 * (n as u64 + 1);
            blas_roots.push((at, layout.blas(blas, at, rng)));
        }
        // The TLAS.
        let bounds: Vec<Aabb> = world
            .instances
            .iter()
            .map(|inst| {
                let local = world.blases[inst.blas]
                    .triangles()
                    .flat_map(|t| t.map(wide));
                let b = Aabb::of(local);
                Aabb::of(b.corners().map(|c| inst.to_world_point(c)))
            })
            .collect();
        let mut instance = |b: &mut Builder, i: usize| -> u32 {
            let unit = b.take(2);
            let inst = &world.instances[i];
            let (blas_base, blas_root) = blas_roots[inst.blas];
            b.put(unit, 0x30, inst.id & 0xFF_FFFF | u32::from(inst.mask) << 24);
            b.put(
                unit,
                0x34,
                inst.contribution & 0xFF_FFFF | u32::from(inst.flags) << 24,
            );
            b.put(
                unit,
                0x38,
                blas_root | u32::from(inst.translation_only) << 31,
            );
            b.put(unit, 0x3C, (blas_base >> 8) as u32);
            let m = inst.to_object;
            let words = [
                m[0][3], m[1][3], m[2][3], m[0][0], m[0][1], m[0][2], m[1][0], m[1][1], m[1][2],
                m[2][0], m[2][1], m[2][2],
            ];
            for (k, w) in words.iter().enumerate() {
                b.put(unit, 0x40 + 4 * k as u64, w.to_bits());
            }
            b.layout
                .nodes
                .insert(b.base + 64 * u64::from(unit), Node::Instance);
            unit << 3 | INSTANCE
        };
        let mut b = Builder {
            layout: &mut layout,
            base,
            next: ROOT_UNIT,
            parents: Vec::new(),
            rng,
        };
        match world.root {
            Root::Empty => {}
            Root::Instance => {
                instance(&mut b, 0);
            }
            Root::Box => {
                let root = b.take(2);
                let mut items: Vec<(usize, Aabb)> = bounds.iter().copied().enumerate().collect();
                b.tree(root, &mut items, &mut instance);
            }
        }
        layout
    }

    /// Lays out `blas` at `base`; its root pointer.
    fn blas(&mut self, blas: &Blas, base: u64, rng: &mut Rng) -> u32 {
        let mut b = Builder {
            layout: self,
            base,
            next: 1,
            parents: Vec::new(),
            rng,
        };
        let mut leaf = |b: &mut Builder, i: usize| -> u32 {
            let unit = b.take(1);
            let tris = blas.leaves[i].clone();
            let pair = tris.len() == 2;
            let noise = (b.rng.bits() as u32) & !0x80;
            b.put(unit, 0x30, noise | u32::from(pair) << 7);
            b.layout
                .nodes
                .insert(b.base + 64 * u64::from(unit), Node::Leaf(tris));
            unit << 3
        };
        let bounds = |tris: &[Tri]| Aabb::of(tris.iter().flat_map(|t| t.map(wide)));
        let mut items: Vec<(usize, Aabb)> =
            blas.leaves.iter().map(|l| bounds(l)).enumerate().collect();
        let root = match (items.len(), blas.cycle) {
            (1, None) => leaf(&mut b, 0),
            (_, None) => {
                let root = b.take(2);
                b.tree(root, &mut items, &mut leaf);
                root << 3 | BOX
            }
            (_, Some([lo, hi])) => {
                // A root over the BLAS's tree and the cycle.
                let root = b.take(2);
                let inner = b.take(2);
                let inner_bounds = b.tree(inner, &mut items, &mut leaf);
                let cycle = b.take(2);
                let cycle_bounds = Aabb::of([wide(lo), wide(hi)]);
                let cycle_pointer = cycle << 3 | BOX;
                for (k, kid) in [cycle_pointer, u32::MAX, u32::MAX, u32::MAX]
                    .iter()
                    .enumerate()
                {
                    b.put(cycle, 4 * k as u64, *kid);
                }
                b.layout.nodes.insert(
                    b.base + 64 * u64::from(cycle),
                    Node::Box {
                        kids: [cycle_pointer, u32::MAX, u32::MAX, u32::MAX],
                        bounds: Box::new([Some(cycle_bounds), None, None, None]),
                    },
                );
                let root_pointer = root << 3 | BOX;
                let kids = [inner << 3 | BOX, cycle_pointer, u32::MAX, u32::MAX];
                for (k, kid) in kids.iter().enumerate() {
                    b.put(root, 4 * k as u64, *kid);
                }
                b.layout.nodes.insert(
                    b.base + 64 * u64::from(root),
                    Node::Box {
                        kids,
                        bounds: Box::new([
                            Some(inner_bounds.padded()),
                            Some(cycle_bounds),
                            None,
                            None,
                        ]),
                    },
                );
                b.parents.push((inner, root_pointer));
                b.parents.push((cycle, root_pointer));
                root_pointer
            }
        };
        // The parent table after the nodes.
        let first = 1u32;
        let table = 64 * u64::from(b.next);
        b.put(0, 0x34, 64 * first);
        b.put(0, 0x38, table as u32);
        let units = b.next;
        let mut parents = vec![u32::MAX; units as usize];
        for &(unit, parent) in &b.parents {
            parents[unit as usize] = parent;
            // A two-unit node's second unit: the same parent.
            if let Some(p) = parents.get_mut(unit as usize + 1)
                && *p == u32::MAX
            {
                *p = parent;
            }
        }
        for unit in first..units {
            let at = base + table + 4 * u64::from(unit - first);
            b.layout.memory.insert(at, parents[unit as usize]);
        }
        root
    }

    /// What `image_bvh_intersect_ray` returns for `ray` in the structure at
    /// `base`.
    pub fn intersect(&self, base: u64, ray: &BvhRay) -> Result<[u32; 4], Why> {
        let at = base + 64 * u64::from(ray.node >> 3);
        let kind = ray.node & 7;
        let node = self
            .nodes
            .get(&at)
            .ok_or("an intersection with a node its world does not have")?;
        let (o, inv) = (wide(ray.origin), wide(ray.inverse));
        match (node, kind) {
            (Node::Box { kids, bounds }, 4 | 5) => {
                let t_max = f64::from(ray.t_max);
                let mut met: Vec<(f64, usize)> = (0..4)
                    .filter(|&k| kids[k] != u32::MAX)
                    .filter_map(|k| Some((bounds[k]?.enter(o, inv, t_max)?, k)))
                    .collect();
                met.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                let mut out = [u32::MAX; 4];
                for (slot, &(_, k)) in met.iter().enumerate() {
                    out[slot] = kids[k];
                }
                Ok(out)
            }
            (Node::Leaf(tris), 0..=3) => {
                let tri = tris
                    .get(kind as usize)
                    .ok_or("a triangle its node does not hold")?;
                let miss = [f32::INFINITY.to_bits(), 1f32.to_bits(), 0, 0];
                let Some(m) = meet(tri, o, wide(ray.direction)) else {
                    return Ok(miss);
                };
                if !m.inside() {
                    return Ok(miss);
                }
                let s = if m.det > 0.0 { 1.0 } else { -1.0 };
                Ok([
                    ((m.t * s) as f32).to_bits(),
                    (s as f32).to_bits(),
                    ((m.u * s) as f32).to_bits(),
                    ((m.v * s) as f32).to_bits(),
                ])
            }
            _ => Err("a node read as another kind"),
        }
    }
}

/// A rotation from a random unit quaternion.
fn rotation(rng: &mut Rng) -> [[f64; 3]; 3] {
    let mut q = [0.0; 4];
    loop {
        for x in &mut q {
            *x = rng.range(-1.0, 1.0);
        }
        let n = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        if n > 0.1 && n < 1.0 {
            q = q.map(|x| x / n);
            break;
        }
    }
    let [w, x, y, z] = q;
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
        ],
        [
            2.0 * (x * y + z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - x * w),
        ],
        [
            2.0 * (x * z - y * w),
            2.0 * (y * z + x * w),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ]
}

/// A random world-to-object transform: a translation, or a rotation
/// with scales (and sometimes a mirror) and a translation.
fn transform(rng: &mut Rng) -> ([[f32; 4]; 3], bool) {
    let t: V3 = std::array::from_fn(|_| rng.range(-40.0, 40.0));
    if rng.chance(0.3) {
        let mut m = [[0f32; 4]; 3];
        for i in 0..3 {
            m[i][i] = 1.0;
            m[i][3] = t[i] as f32;
        }
        return (m, true);
    }
    let r = rotation(rng);
    let s: V3 = std::array::from_fn(|_| rng.range(0.5, 2.0));
    let mirror = if rng.chance(0.2) { -1.0 } else { 1.0 };
    let m = std::array::from_fn(|i| {
        let k = if i == 0 { mirror } else { 1.0 };
        [
            (k * s[i] * r[i][0]) as f32,
            (k * s[i] * r[i][1]) as f32,
            (k * s[i] * r[i][2]) as f32,
            t[i] as f32,
        ]
    });
    (m, false)
}

/// One lane's ray, in world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ray {
    pub origin: [f32; 3],
    pub direction: [f32; 3],
    pub t_max: f32,
}

/// A world-space triangle around `p` across the ray direction `d`, of size
/// `size`, that the ray meets inside (`hit`) or misses beside.
fn triangle_at(rng: &mut Rng, p: V3, d: V3, size: f64, hit: bool) -> [V3; 3] {
    // Two axes across a normal within 60 degrees of the ray.
    let dn = scale(d, 1.0 / len(d));
    let tilt: V3 = std::array::from_fn(|_| rng.range(-0.5, 0.5));
    let n = add(dn, tilt);
    let n = scale(n, 1.0 / len(n));
    let any = if n[0].abs() < 0.5 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let a = cross(n, any);
    let a = scale(a, 1.0 / len(a));
    let b = cross(n, a);
    let b = scale(b, 1.0 / len(b));
    // Barycentrics of p: inside with a margin, or outside with one.
    let (u, v) = if hit {
        let u = rng.range(0.15, 0.6);
        (u, rng.range(0.15, 0.85 - u))
    } else {
        match rng.below(3) {
            0 => (rng.range(-0.8, -0.2), rng.range(0.1, 0.6)),
            1 => (rng.range(0.1, 0.6), rng.range(-0.8, -0.2)),
            _ => (rng.range(0.7, 1.0), rng.range(0.6, 1.0)),
        }
    };
    // Vertices v0 + e1, v0 + e2 with p = v0 + u e1 + v e2.
    let e1 = add(
        scale(a, size * rng.range(0.6, 1.4)),
        scale(b, size * rng.range(-0.3, 0.3)),
    );
    let e2 = add(
        scale(b, size * rng.range(0.6, 1.4)),
        scale(a, size * rng.range(-0.3, 0.3)),
    );
    let v0 = sub(sub(p, scale(e1, u)), scale(e2, v));
    [v0, add(v0, e1), add(v0, e2)]
}

/// The world-space triangle `w` in `inst`'s object space, turned so that
/// it faces as `front` says (DXR's facing, in object space) for `d`.
fn placed(inst: &Instance, w: [V3; 3], d: V3, front: bool) -> Tri {
    let m = inst.to_object.map(|r| r.map(f64::from));
    let obj = |p: V3| -> [f32; 3] {
        std::array::from_fn(|i| (m[i][0] * p[0] + m[i][1] * p[1] + m[i][2] * p[2] + m[i][3]) as f32)
    };
    let mut tri = w.map(obj);
    let od = std::array::from_fn(|i| m[i][0] * d[0] + m[i][1] * d[1] + m[i][2] * d[2]);
    let det = -dot(
        od,
        cross(
            sub(wide(tri[1]), wide(tri[0])),
            sub(wide(tri[2]), wide(tri[0])),
        ),
    );
    if (det > 0.0) != front {
        tri.swap(1, 2);
    }
    tri
}

/// A world built around `rays` (lane, ray): instances whose triangles the
/// rays meet at chosen t, in front of their origin or behind it, inside
/// t-max or past it, front or back facing, beside decoys and filler; masks
/// in and out of `mask`, all flags, translations, rotations, scales and
/// mirrors, shared BLASes, pairs, and BLASes deep enough to overflow a short
/// stack.
pub(crate) fn around(rays: &[(usize, Ray)], root: Root, mask: u32, rng: &mut Rng) -> World {
    let mut world = World {
        root,
        instances: Vec::new(),
        blases: Vec::new(),
    };
    if root == Root::Empty || rays.is_empty() {
        world.root = Root::Empty;
        return world;
    }
    let count = match root {
        Root::Instance => 1,
        _ if rng.chance(0.15) => 10 + rng.below(8),
        _ => 1 + rng.below(7),
    };
    for n in 0..count {
        let (to_object, translation_only) = transform(rng);
        // A mask outside the query's, or one with at least one of its bits.
        let bits: Vec<u32> = (0..8).filter(|b| mask >> b & 1 != 0).collect();
        let inst_mask = if bits.is_empty() || rng.chance(0.25) {
            (rng.bits() as u32 & !mask & 0xFF) as u8
        } else {
            (rng.bits() as u32 & 0xFF | 1 << bits[rng.below(bits.len())]) as u8
        };
        let mut flags = rng.below(4) as u8;
        if rng.chance(0.25) {
            flags |= 4 << rng.below(2);
        }
        let shared = n > 0 && rng.chance(0.2);
        let inst = Instance {
            to_object,
            translation_only,
            mask: inst_mask,
            flags,
            id: rng.bits() as u32 & 0xFF_FFFF,
            contribution: rng.bits() as u32 & 0xFF_FFFF,
            blas: if shared {
                rng.below(world.blases.len())
            } else {
                world.blases.len()
            },
        };
        if !shared {
            let deep = rng.chance(0.15);
            let mut tris: Vec<Tri> = Vec::new();
            let targets = if deep {
                12 + rng.below(20)
            } else {
                1 + rng.below(4)
            };
            for _ in 0..targets {
                let (_, ray) = rays[rng.below(rays.len())];
                let (o, d) = (wide(ray.origin), wide(ray.direction));
                let t_max = f64::from(ray.t_max);
                let t = match rng.below(10) {
                    0 => -t_max * rng.range(0.1, 0.9),
                    1 => t_max * rng.range(1.1, 2.0),
                    _ => t_max * rng.range(0.05, 0.95),
                };
                let p = add(o, scale(d, t));
                let size = rng.range(0.05, 0.5) * t_max.max(0.1);
                let hit = !rng.chance(0.25);
                let w = triangle_at(rng, p, d, size, hit);
                tris.push(placed(&inst, w, d, rng.chance(0.6)));
            }
            // Filler near the rays' paths.
            let filler = if deep {
                40 + rng.below(60)
            } else {
                rng.below(6)
            };
            for _ in 0..filler {
                let (_, ray) = rays[rng.below(rays.len())];
                let (o, d) = (wide(ray.origin), wide(ray.direction));
                let t = f64::from(ray.t_max) * rng.range(-0.5, 1.5);
                let off: V3 = std::array::from_fn(|_| rng.range(-1.0, 1.0));
                let p = add(add(o, scale(d, t)), off);
                let size = rng.range(0.05, 0.4);
                let w = triangle_at(rng, p, d, size, false);
                tris.push(placed(&inst, w, d, rng.chance(0.5)));
            }
            // Shuffled, then paired at random.
            for k in (1..tris.len()).rev() {
                tris.swap(k, rng.below(k + 1));
            }
            let mut leaves = Vec::new();
            let mut it = tris.into_iter().peekable();
            while let Some(t) = it.next() {
                match it.peek() {
                    Some(_) if rng.chance(0.5) => leaves.push(vec![t, it.next().unwrap_or(t)]),
                    _ => leaves.push(vec![t]),
                }
            }
            world.blases.push(Blas {
                leaves,
                cycle: None,
            });
        }
        world.instances.push(inst);
    }
    world
}
