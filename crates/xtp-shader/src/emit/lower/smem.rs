//! Scalar memory (SMEM): constant-buffer reads, scalar loads read as
//! descriptor words or as data (`descriptor_data`), the program's own
//! constant table. Backlog R1 (data reads through root descriptors: the
//! loads `smem` hands to `descriptor_data` today) and R2 (descriptor
//! memory as data).

use std::collections::HashMap;

use super::{Lower, Val, Why};
use crate::emit::bitcode::{Attr, Bin, V, pred};
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{Inst, Reg, Value};
use crate::native::ResourceType;
use crate::resource::{Address, CODE, Role, Site, Source};

impl<'a> Lower<'a> {
    pub(super) fn smem(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        let Fields::Smem {
            data,
            offset,
            soffset,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let name = inst.name();
        if let Some(a) = self.scalar.get(&site).copied().filter(|a| a.param == CODE) {
            return self.code_smem(inst, a);
        }
        let Some(rest) = name.strip_prefix("s_buffer_load_dword") else {
            return self.descriptor_data(inst, site);
        };
        let count = match rest {
            "" => 1,
            "x2" => 2,
            "x4" => 4,
            "x8" => 8,
            "x16" => 16,
            _ => return Err("a scalar buffer load"),
        };
        let Some(Operand::Scalar { code, .. }) = *data else {
            return Err("a scalar buffer load");
        };
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked descriptor")?;
        let kind = self.used_kind(src);
        let c = self.cur;
        let (base, extra) = self.built_offset(src)?;
        let offset = &((*offset as u32).wrapping_add(base) as i32);
        let mut addr = self.s.m.i32(*offset as u32);
        if let Some(x) = extra {
            addr = self.s.m.bin(c, Bin::Add, addr, x);
        }
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            let so = self.operand(inst, soffset)?;
            addr = self.s.m.bin(c, Bin::Add, addr, so);
        }
        if matches!(src, Source::Built { address, .. } if address.param == CODE) {
            // A descriptor built around a table in the code.
            for k in 0..count {
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, addr, off);
                let x = self.code_dword(a)?;
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        if matches!(kind, Some(u) if u.kind == ResourceType::Srv) {
            let h = self.handle(site, src, false, (11, 0))?;
            for k in 0..count {
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, addr, off);
                let mask = self.s.m.i8(1);
                let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
                let r = self.s.op(
                    c,
                    "rawBufferLoad.f32",
                    Attr::ReadOnly,
                    self.s.t.res_ret,
                    139,
                    &[h, a, u, mask, al],
                );
                let x = self.s.m.extract(c, r, 0);
                let x = self.as_int(x);
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        let h = self.handle(site, src, false, (13, 0x1_0000))?;
        let constant = match addr {
            V::Const(_)
                if matches!(soffset, Operand::Scalar { code: NULL, .. }) && extra.is_none() =>
            {
                Some(*offset as u32)
            }
            _ => None,
        };
        let mut rows: HashMap<u32, V> = HashMap::new();
        for k in 0..count {
            let x = match constant {
                Some(base) => {
                    let byte = base + 4 * k;
                    let row = match rows.get(&(byte >> 4)) {
                        Some(&r) => r,
                        None => {
                            let ri = self.s.m.i32(byte >> 4);
                            let r = self.s.op(
                                c,
                                "cbufferLoadLegacy.i32",
                                Attr::ReadOnly,
                                self.s.t.cbuf_ret,
                                59,
                                &[h, ri],
                            );
                            rows.insert(byte >> 4, r);
                            r
                        }
                    };
                    self.s.m.extract(c, row, (byte >> 2) & 3)
                }
                None => {
                    let off = self.s.m.i32(4 * k);
                    let a = self.s.m.bin(c, Bin::Add, addr, off);
                    self.cbuffer_dword(h, a)
                }
            };
            self.write(inst, Reg::S(code + k as u8), x, None);
        }
        Ok(())
    }

    /// A descriptor load (`s_load_dword*` from a table or heap) whose dwords
    /// the code also reads as numbers: the console compiler computes sizes
    /// from descriptor bits (an image's width, height, depth, levels; a
    /// buffer's record count) instead of querying them. Those fields are
    /// rebuilt from the host view's dimensions in the GFX10 layout (image
    /// dword 1 bits 30-31 and dword 2 bits 0-13 width - 1, dword 2 bits
    /// 14-29 height - 1, dword 3 identity swizzle, last level at 16, type at
    /// 28, dword 4 depth or layers - 1; buffer dword 2 the record count).
    // shortcut: the base level reads 0 (the host view starts at its own
    // level 0, so sizes the code shifts by it come out right) and the other
    // dwords (addresses, formats, tiling) have no host value: reading them
    // stops the program.
    pub(super) fn descriptor_data(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        const OTHER: Why = "a descriptor's address or format read as data";
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            ..
        } = inst.fields
        else {
            return Err("a scalar memory read as data");
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1u32,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar memory read as data"),
        };
        // How the program uses the descriptor tells the view's kind: its
        // own uses, else those of a twin load of the same dwords (a
        // non-uniform index reloads the descriptor in each waterfall loop,
        // one load for the size bits, another for the access).
        let uses = |twin: &dyn Fn(Site) -> bool| {
            self.x
                .res
                .descriptors
                .iter()
                .filter(|d| d.role == Role::Resource)
                .find_map(|d| {
                    d.source.leaves().into_iter().find_map(|l| match *l {
                        Source::Loaded { load, first } if twin(load) => Some((d.site, first)),
                        _ => None,
                    })
                })
        };
        let use_at =
            uses(&|load| load == site).or_else(|| uses(&|load| self.twin_loads(site, load)));
        let (first, src, props, image, uav) = match use_at {
            Some((at, first)) => {
                let src = Source::Loaded { load: site, first };
                let used = self.used_kind(&src);
                let ui: &Inst = &self.x.p.blocks[at.0].insts[at.1];
                let un = ui.name();
                let uav = un.contains("store")
                    || un.contains("atomic")
                    || used.is_some_and(|u| u.kind == ResourceType::Uav);
                let u = if uav { 0x1000 } else { 0 };
                let (props, image) = match &ui.fields {
                    Fields::Image { dim, .. } => {
                        let kind = match dim {
                            0 => 1,
                            1 => 2,
                            2 => 4,
                            4 => 6,
                            5 => 7,
                            _ => return Err("an image dimension (cube or MSAA)"),
                        };
                        ((kind | u, 0x409), Some(kind))
                    }
                    Fields::Buffer { idxen, format, .. } => {
                        let stride = used.map_or(0, |u| u.stride);
                        let props = if un.starts_with("buffer_load_format")
                            || un.starts_with("buffer_store_format")
                        {
                            (10 | u, 0x409)
                        } else if *idxen
                            && format.is_none()
                            && stride >= 4
                            && stride.is_multiple_of(4)
                        {
                            (12 | u, stride)
                        } else {
                            (11 | u, 0)
                        };
                        if used.is_some_and(|u| u.kind == ResourceType::Cbv) {
                            return Err("a constant buffer's descriptor read as data");
                        }
                        (props, None)
                    }
                    _ => return Err("a scalar memory read as data"),
                };
                (first, src, props, image, uav)
            }
            None => {
                // An image the program only measures (it reads the size
                // bits; other programs sample it): eight dwords are an image
                // descriptor, taken here as a 2D texture. Not where the
                // table's resource entry says another kind or a four-dword
                // descriptor (two buffers loaded at once).
                // shortcut: a 3D or array image reads depth or layers 1, and
                // the heap holds a view of another dimension than the 2D one
                // the handle declares.
                let src = Source::Loaded {
                    load: site,
                    first: 0,
                };
                let other = self
                    .used_kind(&src)
                    .is_some_and(|u| u.kind != ResourceType::Srv || u.dwords != 8);
                if count != 8 || other {
                    return Err("a descriptor read only as data");
                }
                (0, src, (2, 0x409), Some(2), false)
            }
        };
        let h = if !self.x.res.waterfall.loads.contains(&site) && self.reads_lane_own(site) {
            // Its offset is a waterfall loop's first-lane read, which is each
            // lane's own value: a heap index per lane.
            let index = self.heap_index(site, first, false)?;
            self.s.heap_handle(self.cur, index, false, true, props)
        } else {
            self.handle(site, &src, false, props)?
        };
        let c = self.cur;
        let i32t = self.s.t.i32;
        let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
        let mip = if image.is_some() && !uav {
            self.s.m.i32(0)
        } else {
            self.s.m.undef(i32t)
        };
        let d = self
            .s
            .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, mip]);
        let field = |l: &mut Self, k: u32| l.s.m.extract(c, d, k);
        let words: Vec<(u32, V)> = match image {
            Some(kind) => {
                let one = self.s.m.i32(1);
                let w = field(self, 0);
                let (height, depth) = match kind {
                    1 => (one, one),
                    2 => (field(self, 1), one),
                    6 => (one, field(self, 1)),
                    _ => (field(self, 1), field(self, 2)),
                };
                let levels = if uav { one } else { field(self, 3) };
                let less = |l: &mut Self, x: V| l.s.m.bin(c, Bin::Sub, x, one);
                let (w, height, depth, levels) = (
                    less(self, w),
                    less(self, height),
                    less(self, depth),
                    less(self, levels),
                );
                let k = |l: &mut Self, v: u32| l.s.m.i32(v);
                let (k30, k2, k14, k16, mask) = (
                    k(self, 30),
                    k(self, 2),
                    k(self, 14),
                    k(self, 16),
                    k(self, 0x3FFF),
                );
                let d1 = self.s.m.bin(c, Bin::Shl, w, k30);
                let hi = self.s.m.bin(c, Bin::LShr, w, k2);
                let hi = self.s.m.bin(c, Bin::And, hi, mask);
                let ht = self.s.m.bin(c, Bin::Shl, height, k14);
                let d2 = self.s.m.bin(c, Bin::Or, hi, ht);
                let ty = [0, 8, 9, 0, 10, 0, 12, 13][kind as usize];
                let base = k(self, 0xFAC | ty << 28);
                let last = self.s.m.bin(c, Bin::Shl, levels, k16);
                let d3 = self.s.m.bin(c, Bin::Or, base, last);
                vec![(1, d1), (2, d2), (3, d3), (4, depth)]
            }
            None => vec![(2, field(self, 0))],
        };
        for k in 0..count {
            let rel = k.wrapping_sub(u32::from(first));
            match words.iter().find(|w| w.0 == rel) {
                Some(&(_, x)) => self.write(inst, Reg::S(code + k as u8), x, None),
                None => {
                    if let Some(v) = inst.write(Reg::S(code + k as u8)) {
                        self.vals.insert(v, Val::Missing(OTHER));
                    }
                }
            }
        }
        Ok(())
    }

    /// Two scalar loads of the same dwords: same opcode and immediate
    /// offset, base read from the same values, and the same or an
    /// equivalent register offset.
    fn twin_loads(&self, a: Site, b: Site) -> bool {
        let (ia, ib): (&Inst, &Inst) = (
            &self.x.p.blocks[a.0].insts[a.1],
            &self.x.p.blocks[b.0].insts[b.1],
        );
        let (
            Fields::Smem {
                base: Operand::Scalar { code: ba, .. },
                offset: oa,
                soffset: sa,
                ..
            },
            Fields::Smem {
                base: Operand::Scalar { code: bb, .. },
                offset: ob,
                soffset: sb,
                ..
            },
        ) = (&ia.fields, &ib.fields)
        else {
            return false;
        };
        let same_base = (0..2).all(|k| {
            let va = ia.read(Reg::S(ba + k));
            va.is_some() && va == ib.read(Reg::S(bb + k))
        });
        let same_soffset = match (sa, sb) {
            (Operand::Scalar { code: NULL, .. }, Operand::Scalar { code: NULL, .. }) => true,
            (Operand::Scalar { code: ca, .. }, Operand::Scalar { code: cb, .. })
                if *ca != NULL && *cb != NULL =>
            {
                match (ia.read(Reg::S(*ca)), ib.read(Reg::S(*cb))) {
                    (Some(x), Some(y)) => self.same_scalar(x, y),
                    _ => false,
                }
            }
            _ => false,
        };
        ia.name() == ib.name() && oa == ob && same_base && same_soffset
    }

    /// The instruction that writes `v`.
    fn def_of(&self, v: Value) -> Option<&'a Inst> {
        let p = self.x.p;
        let &b = self.def_block.get(&v)?;
        p.blocks[b]
            .insts
            .iter()
            .find(|i| i.writes.iter().any(|w| w.1 == v))
    }

    /// The same value, or two `v_readfirstlane_b32` of the same VGPR value
    /// (each waterfall loop reads the index anew).
    fn same_scalar(&self, x: Value, y: Value) -> bool {
        if x == y {
            return true;
        }
        let first_lane_of = |v: Value| {
            self.def_of(v)
                .filter(|i| i.name() == "v_readfirstlane_b32")
                .and_then(|i| {
                    i.reads
                        .iter()
                        .find(|r| matches!(r.0, Reg::V(_)))
                        .map(|r| r.1)
                })
        };
        matches!((first_lane_of(x), first_lane_of(y)), (Some(a), Some(b)) if a == b)
    }

    /// Whether the scalar load at `site` reads its base or offset from a
    /// waterfall loop's first-lane read, which is each lane's own value.
    fn reads_lane_own(&self, site: Site) -> bool {
        let inst = &self.x.p.blocks[site.0].insts[site.1];
        inst.reads
            .iter()
            .any(|r| self.x.res.waterfall.reads.contains(&r.1))
    }

    /// Whether `v` is a dword a scalar load writes whose dwords the program
    /// uses as a resource or sampler descriptor.
    pub(super) fn descriptor_dword(&self, v: Value) -> bool {
        let Some(&b) = self.def_block.get(&v) else {
            return false;
        };
        let Some(i) = self.x.p.blocks[b]
            .insts
            .iter()
            .position(|i| i.writes.iter().any(|w| w.1 == v))
        else {
            return false;
        };
        self.x.p.blocks[b].insts[i]
            .name()
            .starts_with("s_load_dword")
            && self.x.res.descriptors.iter().any(|d| {
                d.source
                    .leaves()
                    .iter()
                    .any(|l| matches!(l, Source::Loaded { load, .. } if *load == (b, i)))
            })
    }

    /// `s_load_dword*` from the program's constant table.
    pub(super) fn code_smem(&mut self, inst: &Inst, a: Address) -> Result<(), Why> {
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            ..
        } = inst.fields
        else {
            return Err("a scalar load");
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1u32,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar load from the code"),
        };
        let c = self.cur;
        let mut addr = self.s.m.i32(a.offset);
        if let Some(d) = a.dynamic {
            let d = self.i32_of(d)?;
            addr = self.s.m.bin(c, Bin::Add, addr, d);
        }
        for k in 0..count {
            let off = self.s.m.i32(4 * k);
            let at = self.s.m.bin(c, Bin::Add, addr, off);
            let x = self.code_dword(at)?;
            self.write(inst, Reg::S(code + k as u8), x, None);
        }
        Ok(())
    }

    /// The dword of the program's code at byte `byte` (a constant table
    /// the compiler appends to the code), as an internal constant array.
    // shortcut: an index outside the table reads its last dword (the
    // console would read whatever memory follows the code).
    pub(super) fn code_dword(&mut self, byte: V) -> Result<V, Why> {
        let (g, base, len) = match self.code_table {
            Some(t) => t,
            None => {
                let first = self
                    .x
                    .res
                    .scalar
                    .iter()
                    .map(|s| s.1)
                    .chain(self.x.res.descriptors.iter().flat_map(|d| {
                        d.source.leaves().into_iter().filter_map(|l| match l {
                            Source::Built { address, .. } => Some(*address),
                            _ => None,
                        })
                    }))
                    .filter(|a| a.param == CODE)
                    .map(|a| a.offset & !3)
                    .min()
                    .ok_or("a code table")?;
                let code = &self.x.native.code;
                let words: Vec<u32> = code
                    .get(first as usize..)
                    .ok_or("a code table past the code")?
                    .chunks(4)
                    .map(|c| {
                        let mut w = [0u8; 4];
                        w[..c.len()].copy_from_slice(c);
                        u32::from_le_bytes(w)
                    })
                    .collect();
                if words.is_empty() {
                    return Err("a code table past the code");
                }
                let elems: Vec<V> = words.iter().map(|&w| self.s.m.i32(w)).collect();
                let arr = self.s.m.array(words.len() as u32, self.s.t.i32);
                let init = self.s.m.aggregate(arr, &elems);
                let g = self.s.m.constant_global("code", init);
                let t = (g, first, words.len() as u32);
                self.code_table = Some(t);
                t
            }
        };
        let c = self.cur;
        let k = self.s.m.i32(base);
        let rel = self.s.m.bin(c, Bin::Sub, byte, k);
        let two = self.s.m.i32(2);
        let index = self.s.m.bin(c, Bin::LShr, rel, two);
        let last = self.s.m.i32(len - 1);
        let over = self.s.m.cmp(c, pred::IUGT, index, last);
        let index = self.s.m.select(c, over, last, index);
        let at = self.s.m.element(c, g, index);
        Ok(self.s.m.load(c, at))
    }

    /// The dword at a computed byte address of a constant buffer.
    pub(super) fn cbuffer_dword(&mut self, h: V, a: V) -> V {
        let c = self.cur;
        let four = self.s.m.i32(4);
        let ri = self.s.m.bin(c, Bin::LShr, a, four);
        let r = self.s.op(
            c,
            "cbufferLoadLegacy.i32",
            Attr::ReadOnly,
            self.s.t.cbuf_ret,
            59,
            &[h, ri],
        );
        let two = self.s.m.i32(2);
        let three = self.s.m.i32(3);
        let comp = self.s.m.bin(c, Bin::LShr, a, two);
        let comp = self.s.m.bin(c, Bin::And, comp, three);
        let mut x = self.s.m.extract(c, r, 0);
        for j in 1..4 {
            let e = self.s.m.extract(c, r, j);
            let jc = self.s.m.i32(j);
            let is = self.s.m.cmp(c, pred::EQ, comp, jc);
            x = self.s.m.select(c, is, e, x);
        }
        x
    }
}

#[cfg(test)]
mod tests {
    use crate::emit::tests::{compute, emit_for};
    use crate::native::{ComputeIo, ResourceType, RootParam, RootType, StageIo, UsedResource};

    const S_ENDPGM: u32 = 0xBF81_0000;

    /// A wave32 compute program with a table at s0 whose entries are
    /// `kind` descriptors of `dwords` dwords (`stride` for buffers), and
    /// 512 bytes of LDS.
    fn translate(
        words: &[u32],
        kind: ResourceType,
        dwords: u8,
        stride: u32,
    ) -> Result<(), crate::ShaderError> {
        let mut p = compute(words, false);
        p.layout = vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }];
        p.used = vec![UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride,
            dwords,
            user_data: 0,
            slot: 0,
        }];
        p.io = StageIo::Cs(ComputeIo {
            rsrc2: 1 << 15 | 1 << 1,
            threads: [64, 1, 1],
        });
        emit_for(&p, crate::emit::Target::ANY).map(|_| ())
    }

    fn only_data(r: Result<(), crate::ShaderError>) -> bool {
        matches!(
            r,
            Err(crate::ShaderError::Unsupported {
                what: "a descriptor read only as data",
                ..
            })
        )
    }

    /// An image the program only measures: its height bits from an eight-
    /// dword table entry, no access. Taken as a 2D texture; not where the
    /// table says the entry is a UAV or a four-dword descriptor.
    #[test]
    fn an_image_only_measured_is_taken_as_a_2d_texture() {
        let words = [
            0xBE85_0380, // s_mov_b32 s5, 0
            0xBE84_0300, // s_mov_b32 s4, s0
            0xF40C_0202,
            0xFA00_0000, // s_load_dwordx8 s[8:15], s[4:5], null
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            0x9010_8E0A, // s_lshr_b32 s16, s10, 14 (height - 1)
            0x7E02_0210, // v_mov_b32 v1, s16
            0xD834_0000,
            0x0000_0100, // ds_write_b32 v0, v1
            S_ENDPGM,
        ];
        assert_eq!(translate(&words, ResourceType::Srv, 8, 0), Ok(()));
        for (kind, dwords) in [(ResourceType::Uav, 8), (ResourceType::Srv, 4)] {
            assert!(
                only_data(translate(&words, kind, dwords, 0)),
                "{kind:?}, {dwords}"
            );
        }
    }

    /// A buffer descriptor loaded twice, the first load used by a
    /// structured load, the second read for its record count (dword 2):
    /// the second takes the first's view. Not when the loads read other
    /// entries.
    #[test]
    fn a_descriptor_read_as_data_takes_the_view_of_its_twin_load() {
        let twin = |offset: u32| {
            [
                0xBE84_0300, // s_mov_b32 s4, s0
                0xBE85_0380, // s_mov_b32 s5, 0
                0xF408_0202,
                0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
                0xF408_0302,
                0xFA00_0000 | offset, // s_load_dwordx4 s[12:15], s[4:5], offset
                0xBF8C_C07F,          // s_waitcnt lgkmcnt(0)
                0x7E02_020E,          // v_mov_b32 v1, s14 (record count)
                0xE030_2000,
                0x8002_0200, // buffer_load_dword v2, v0, s[8:11], 0 idxen
                0xBF8C_3F70, // s_waitcnt vmcnt(0)
                0x4A04_0501, // v_add_nc_u32 v2, v1, v2
                0xD834_0000,
                0x0000_0200, // ds_write_b32 v0, v2
                S_ENDPGM,
            ]
        };
        assert_eq!(translate(&twin(0), ResourceType::Srv, 4, 4), Ok(()));
        assert!(only_data(translate(&twin(0x10), ResourceType::Srv, 4, 4)));
    }
}
