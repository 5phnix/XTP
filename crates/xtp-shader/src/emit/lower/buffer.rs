//! Vector memory: buffer loads, stores and atomics (MUBUF, MTBUF) with
//! the GFX10 buffer formats, and global loads (`global_load_*`). Backlog
//! R3 (structured buffers of unknown stride: through the host's runtime
//! stride table, [`Lower::runtime_access`]).

use super::descriptor::UNKNOWN_STRIDE;
use super::{Lower, Why};
use crate::emit::bitcode::{Attr, Bin, Cast, V};
use crate::emit::shader::flags;
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{Inst, Reg};
use crate::native::{ResourceType, UsedResource};
use crate::resource::{CODE, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Numeric {
    Unorm,
    Snorm,
    Uscaled,
    Sscaled,
    Uint,
    Sint,
    Float,
}

/// A GFX10 buffer format (the unified format of typed buffer loads): the
/// channel widths from the lowest bits up, and how the channels convert.
/// Formats are numbered in families (`8`, `16`, `8_8`, ...: AMD names
/// list channels from the highest bits), each with its numeric formats in
/// the order unorm, snorm, uscaled, sscaled, uint, sint, float.
pub(super) fn buffer_format(format: u8) -> Option<(&'static [u32], Numeric)> {
    use Numeric::*;
    const NORM: &[Numeric] = &[Unorm, Snorm, Uscaled, Sscaled, Uint, Sint];
    const ALL: &[Numeric] = &[Unorm, Snorm, Uscaled, Sscaled, Uint, Sint, Float];
    const WIDE: &[Numeric] = &[Uint, Sint, Float];
    // Numbers and order from LLVM's `UfmtGFX10` (`SIDefines.h`); the GFX10
    // buffer formats have no 5_9_9_9. Channel order from Mesa
    // (`ac_formats.c`): R11G11B10_FLOAT is 10_11_11, and 10_10_10_2 has
    // its 2-bit channel in the lowest bits.
    const FAMILIES: [(&[u32], &[Numeric]); 14] = [
        (&[8], NORM),
        (&[16], ALL),
        (&[8, 8], NORM),
        (&[32], WIDE),
        (&[16, 16], ALL),
        (&[11, 11, 10], ALL),     // 10_11_11
        (&[10, 11, 11], ALL),     // 11_11_10
        (&[2, 10, 10, 10], NORM), // 10_10_10_2
        (&[10, 10, 10, 2], NORM), // 2_10_10_10
        (&[8, 8, 8, 8], NORM),
        (&[32, 32], WIDE),
        (&[16, 16, 16, 16], ALL),
        (&[32, 32, 32], WIDE),
        (&[32, 32, 32, 32], WIDE),
    ];
    let mut n = usize::from(format.checked_sub(1)?);
    for (bits, numerics) in FAMILIES {
        if n < numerics.len() {
            return Some((bits, numerics[n]));
        }
        n -= numerics.len();
    }
    None
}

/// How far a float channel of `width` bits (16, or the unsigned 11- and
/// 10-bit floats of the packed formats) shifts up to be a half:
/// the small floats are a half's exponent (5 bits, biased 15) and the top
/// of its mantissa, without the sign, so `legacyF16ToF32` widens them
/// exactly, denormals, infinities and NaNs included.
pub(super) fn half_shift(width: u32) -> u32 {
    debug_assert!(matches!(width, 10 | 11 | 16), "{width}-bit float");
    if width == 16 { 0 } else { 15 - width }
}

impl<'a> Lower<'a> {
    /// `channels` channels of the element at byte `off` of raw buffer `h`,
    /// converted from the GFX10 buffer format `format` as the hardware's
    /// typed loads do (channels past the format's read 0, 0, 0, 1).
    // shortcut: the descriptor's channel swizzle is taken as the identity
    // (it is for raw views and the descriptors code builds), elements of 4
    // bytes or more as dword-aligned. Norm formats divide, which the AMD
    // driver does 1 ULP off the hardware's correctly rounded conversion for
    // some values; correct it with an integer check if a title compares
    // converted values exactly.
    pub(super) fn format_load(
        &mut self,
        h: V,
        off: V,
        format: u8,
        channels: u32,
    ) -> Result<Vec<V>, Why> {
        let (bits, numeric) = buffer_format(format).ok_or("a buffer format")?;
        let c = self.cur;
        let total: u32 = bits.iter().sum();
        let (u, align) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let load = |l: &mut Self, at: V, n: u32| -> Vec<V> {
            let mask = l.s.m.i8((1u8 << n) - 1);
            let r = l.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                l.s.t.res_ret,
                139,
                &[h, at, u, mask, align],
            );
            (0..n)
                .map(|k| {
                    let x = l.s.m.extract(c, r, k);
                    l.as_int(x)
                })
                .collect()
        };
        let words = if total < 32 {
            // A byte or half inside the dword that holds it.
            let (not3, three) = (self.s.m.i32(!3), self.s.m.i32(3));
            let at = self.s.m.bin(c, Bin::And, off, not3);
            let w = load(self, at, 1)[0];
            let byte = self.s.m.bin(c, Bin::And, off, three);
            let sh = self.s.m.bin(c, Bin::Shl, byte, three);
            vec![self.s.m.bin(c, Bin::LShr, w, sh)]
        } else {
            load(self, off, total / 32)
        };
        let float = |l: &mut Self, f: V| l.as_int(f);
        let mut out = Vec::new();
        let mut at = 0;
        for &w in bits {
            let word = words[(at / 32) as usize];
            let field = if w == 32 {
                word
            } else {
                let sh = self.s.m.i32(at % 32);
                let t = self.s.m.bin(c, Bin::LShr, word, sh);
                let m = self.s.m.i32((1 << w) - 1);
                self.s.m.bin(c, Bin::And, t, m)
            };
            at += w;
            // Sign-extended from the field's top bit.
            let signed = |l: &mut Self| {
                if w == 32 {
                    return field;
                }
                let k = l.s.m.i32(32 - w);
                let t = l.s.m.bin(c, Bin::Shl, field, k);
                l.s.m.bin(c, Bin::AShr, t, k)
            };
            let f32t = self.s.t.f32;
            let x = match numeric {
                Numeric::Unorm => {
                    let f = self.s.m.cast(c, Cast::UiToFp, field, f32t);
                    let max = self.s.m.f32((((1u64 << w) - 1) as f32).to_bits());
                    let q = self.s.m.bin(c, Bin::SDiv, f, max);
                    float(self, q)
                }
                Numeric::Snorm => {
                    let s = signed(self);
                    let f = self.s.m.cast(c, Cast::SiToFp, s, f32t);
                    let max = self.s.m.f32((((1u64 << (w - 1)) - 1) as f32).to_bits());
                    let q = self.s.m.bin(c, Bin::SDiv, f, max);
                    let lo = self.s.m.f32((-1.0f32).to_bits());
                    let q = self.binary(35, q, lo);
                    float(self, q)
                }
                Numeric::Uscaled => {
                    let f = self.s.m.cast(c, Cast::UiToFp, field, f32t);
                    float(self, f)
                }
                Numeric::Sscaled => {
                    let s = signed(self);
                    let f = self.s.m.cast(c, Cast::SiToFp, s, f32t);
                    float(self, f)
                }
                Numeric::Uint => field,
                Numeric::Sint => signed(self),
                Numeric::Float if w == 32 => field,
                Numeric::Float => {
                    let h = match half_shift(w) {
                        0 => field,
                        k => {
                            let sh = self.s.m.i32(k);
                            self.s.m.bin(c, Bin::Shl, field, sh)
                        }
                    };
                    let f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
                    float(self, f)
                }
            };
            out.push(x);
        }
        let one = match numeric {
            Numeric::Uint | Numeric::Sint => 1,
            _ => 1.0f32.to_bits(),
        };
        for k in out.len() as u32..channels {
            out.push(self.s.m.i32(if k == 3 { one } else { 0 }));
        }
        out.truncate(channels as usize);
        Ok(out)
    }

    pub(super) fn buffer(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Buffer {
            data,
            addr,
            soffset,
            offset,
            offen,
            idxen,
            format,
            lds,
            tfe,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let name = inst.name();
        if *lds || *tfe {
            return Err("a buffer access to LDS or with tfe");
        }
        if format.is_some() && !name.starts_with("tbuffer_load_format_") {
            return Err(name);
        }
        let site = (b, i);
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let store = name.starts_with("buffer_store_");
        // DXIL atomic operations: add, and, or, xor, signed and unsigned
        // min and max, exchange; compare-exchange apart.
        let atomic = match name.strip_prefix("buffer_atomic_") {
            None => None,
            Some("add") => Some(0),
            Some("and") => Some(1),
            Some("or") => Some(2),
            Some("xor") => Some(3),
            Some("smin") => Some(4),
            Some("smax") => Some(5),
            Some("umin") => Some(6),
            Some("umax") => Some(7),
            Some("swap") => Some(8),
            Some("cmpswap") => Some(u32::MAX),
            Some(_) => return Err(name),
        };
        // d16: two 16-bit channels per VGPR, low half first.
        let d16 = name.starts_with("buffer_") && name.contains("_format_d16_");
        let plain = name.replace("_format_d16_", "_format_");
        let (typed, count) = match plain
            .strip_prefix("buffer_load_")
            .or(plain.strip_prefix("buffer_store_"))
            .or(plain.strip_prefix("tbuffer_load_"))
        {
            _ if atomic.is_some() => (false, 1),
            Some("dword") => (false, 1),
            Some("dwordx2") => (false, 2),
            Some("dwordx3") => (false, 3),
            Some("dwordx4") => (false, 4),
            Some("format_x") => (true, 1),
            Some("format_xy") => (true, 2),
            Some("format_xyz") => (true, 3),
            Some("format_xyzw") => (true, 4),
            _ => return Err(name),
        };
        let Operand::Vector { reg: areg, .. } = *addr else {
            return Err("a buffer address");
        };
        let vaddr = |l: &mut Self, k: u8| -> Result<V, Why> {
            let v = inst.read(Reg::V(areg + k)).ok_or("a buffer address")?;
            l.i32_of(v)
        };
        let index = if *idxen { Some(vaddr(self, 0)?) } else { None };
        let mut off = self.s.m.i32(u32::from(*offset));
        if *offen {
            let o = vaddr(self, u8::from(*idxen))?;
            off = self.s.m.bin(c, Bin::Add, off, o);
        }
        if !matches!(
            soffset,
            Operand::Scalar { code: NULL, .. } | Operand::Const(128)
        ) {
            let so = self.operand(inst, soffset)?;
            off = self.s.m.bin(c, Bin::Add, off, so);
        }
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked buffer")?;
        let used = self.used_kind(src);
        let (base, extra) = self.built_offset(src)?;
        if base != 0 {
            let b = self.s.m.i32(base);
            off = self.s.m.bin(c, Bin::Add, off, b);
        }
        if let Some(x) = extra {
            off = self.s.m.bin(c, Bin::Add, off, x);
        }
        // Lanes outside EXEC run a load too where nothing branches them away
        // (every lane in the console flow; under a guard in the per-lane
        // form). With a heap index per lane, theirs may be no index at all
        // (past the heap: a fault, the device removed), so they skip the
        // load and keep their registers.
        let lane_guard = guard.filter(|_| self.lane_descriptor(src));
        if used.is_some_and(|u| u.kind == ResourceType::Cbv) && !typed {
            // A constant buffer read per lane (a dynamically indexed array).
            // shortcut: the console's CBV descriptor is taken to have a 16-byte
            // stride, as the vector loads of whole rows suggest.
            let Some(Operand::Vector { reg, .. }) = *data else {
                return Err("a buffer result");
            };
            let xs = self.guarded_load(lane_guard, |l| {
                let h = l.handle(site, src, false, (13, 0x1_0000))?;
                let c = l.cur;
                let byte = match index {
                    Some(i) => {
                        let sixteen = l.s.m.i32(16);
                        let rows = l.s.m.bin(c, Bin::Mul, i, sixteen);
                        l.s.m.bin(c, Bin::Add, rows, off)
                    }
                    None => off,
                };
                let mut xs = Vec::new();
                for k in 0..count {
                    let four = l.s.m.i32(4 * k);
                    let a = l.s.m.bin(c, Bin::Add, byte, four);
                    xs.push(l.cbuffer_dword(h, a));
                }
                Ok(xs)
            })?;
            for (k, x) in xs.into_iter().enumerate() {
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        let uav = store || atomic.is_some() || used.is_some_and(|u| u.kind == ResourceType::Uav);
        let u = if uav { 0x1000 } else { 0 };
        let iu = self.s.m.undef(self.s.t.i32);
        if let Some(op) = atomic {
            let d = data.as_ref().ok_or("buffer atomic data")?;
            let vals = self.vgprs(inst, d, if op == u32::MAX { 2 } else { 1 })?;
            // (handle, coordinates); none: through the runtime stride table.
            let static_view = match index {
                _ if self.through_table(index, used, src)? => None,
                Some(index) => {
                    let stride = used.map_or(0, |u| u.stride);
                    let h = self.handle(site, src, false, (12 | u, stride))?;
                    Some((h, [index, off, iu]))
                }
                None => Some((self.handle(site, src, false, (11 | u, 0))?, [off, iu, iu])),
            };
            let Operand::Vector { reg, .. } = *d else {
                return Err("buffer atomic data");
            };
            let returns = matches!(inst.fields, Fields::Buffer { cache, .. } if cache.glc);
            let old = match inst.read(Reg::V(reg)) {
                Some(v) if returns => self.i32_of(v)?,
                _ => self.s.m.i32(0),
            };
            let i32t = self.s.t.i32;
            let atomic_op = |l: &mut Self, h: V, coords: [V; 3]| -> V {
                if op == u32::MAX {
                    // cmpswap: data is (new, compare).
                    let mut args = vec![h];
                    args.extend(coords);
                    args.extend([vals[1], vals[0]]);
                    l.s.op(
                        l.cur,
                        "atomicCompareExchange.i32",
                        Attr::NoUnwind,
                        i32t,
                        79,
                        &args,
                    )
                } else {
                    let mut args = vec![h, l.s.m.i32(op)];
                    args.extend(coords);
                    args.push(vals[0]);
                    l.s.op(l.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args)
                }
            };
            let at = self.begin_guarded(guard);
            let r = match static_view {
                Some((h, coords)) => atomic_op(self, h, coords),
                None => {
                    // Out of range the console returns 0.
                    let done = self.runtime_access(src, index, off, 4, 11 | u, |l, h, byte| {
                        Ok(vec![atomic_op(l, h, [byte, iu, iu])])
                    });
                    match done {
                        Ok(r) => r[0],
                        Err(why) => {
                            self.end_guarded(at, &[]);
                            return Err(why);
                        }
                    }
                }
            };
            let r = self.end_guarded(at, &[(r, old)])[0];
            if returns {
                self.write(inst, Reg::V(reg), r, None);
            }
            return Ok(());
        }
        if store {
            let regs = if d16 { count.div_ceil(2) } else { count };
            let mut vals =
                self.vgprs(inst, data.as_ref().ok_or("buffer store data")?, regs as u8)?;
            if d16 {
                // shortcut: d16 data is taken as f16 (integer formats would
                // keep 16 bits), widened for the 32-bit float store.
                let mut wide = Vec::new();
                for k in 0..count {
                    let mut x = vals[(k / 2) as usize];
                    if k % 2 == 1 {
                        let k16 = self.s.m.i32(16);
                        x = self.s.m.bin(c, Bin::LShr, x, k16);
                    }
                    let f32t = self.s.t.f32;
                    let f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[x]);
                    wide.push(self.as_int(f));
                }
                vals = wide;
            }
            let mut args = Vec::new();
            let (op, name) = if typed {
                // shortcut: typed stores are taken to write all four channels
                // of a float format.
                if count != 4 || *offen || *offset != 0 {
                    return Err("a typed buffer store of some channels or at an offset");
                }
                args.push(self.handle(site, src, false, (10 | u, 0x409))?);
                args.extend([index.unwrap_or(self.s.m.i32(0)), iu]);
                for x in vals {
                    args.push(self.as_float(x));
                }
                (69, "bufferStore.f32")
            } else {
                match index {
                    _ if self.through_table(index, used, src)? => {
                        // The store in a block of its own under the guard:
                        // made only where the table's range checks pass.
                        let mask = (1u8 << count) - 1;
                        let at = self.begin_guarded(guard);
                        let done = self.runtime_access(
                            src,
                            index,
                            off,
                            4 * count,
                            11 | u,
                            |l, h, byte| {
                                let mut args = vec![h, byte, iu];
                                args.extend(&vals);
                                args.resize(3 + 4, iu);
                                args.push(l.s.m.i8(mask));
                                args.push(l.s.m.i32(4));
                                let void = l.s.t.void;
                                l.s.op(
                                    l.cur,
                                    "rawBufferStore.i32",
                                    Attr::NoUnwind,
                                    void,
                                    140,
                                    &args,
                                );
                                Ok(Vec::new())
                            },
                        );
                        self.end_guarded(at, &[]);
                        return done.map(|_| ());
                    }
                    Some(index) => {
                        let stride = used.map_or(0, |u| u.stride);
                        args.push(self.handle(site, src, false, (12 | u, stride))?);
                        args.extend([index, off]);
                    }
                    None => {
                        args.push(self.handle(site, src, false, (11 | u, 0))?);
                        args.extend([off, iu]);
                    }
                }
                args.extend(&vals);
                args.resize(3 + 4, iu);
                (140, "rawBufferStore.i32")
            };
            args.push(self.s.m.i8(if typed { 0xF } else { (1u8 << count) - 1 }));
            if !typed {
                args.push(self.s.m.i32(4));
            }
            let at = self.begin_guarded(guard);
            let void = self.s.t.void;
            self.s.op(self.cur, name, Attr::NoUnwind, void, op, &args);
            self.end_guarded(at, &[]);
            return Ok(());
        }
        let Some(Operand::Vector { reg, .. }) = *data else {
            return Err("a buffer result");
        };
        // (What can stop the load is checked before its guarded block.)
        let stride = used.map_or(0, |u| u.stride);
        let mut runtime = false;
        match *format {
            Some(_) if index.is_some() => return Err("a typed-format load by index"),
            Some(_) => {}
            None if typed => {
                if *offen || matches!(off, V::Inst(_)) || *offset != 0 {
                    return Err("a typed buffer load at an offset");
                }
                if uav {
                    self.s.flags |= flags::TYPED_UAV_LOADS;
                }
            }
            None => runtime = self.through_table(index, used, src)?,
        }
        let xs = self.guarded_load(lane_guard, |l| {
            if let Some(f) = *format {
                if l.raw_through_table(src) {
                    // The format's bytes (a byte or a half is read in its
                    // dword, which the view's bytes hold whole).
                    let width = buffer_format(f)
                        .map_or(4, |(bits, _)| (bits.iter().sum::<u32>() / 8).max(1));
                    return l.runtime_access(src, None, off, width, 11 | u, |l, h, byte| {
                        l.format_load(h, byte, f, count)
                    });
                }
                let h = l.handle(site, src, false, (11 | u, 0))?;
                return l.format_load(h, off, f, count);
            }
            let c = l.cur;
            let r = if typed {
                let h = l.handle(site, src, false, (10 | u, 0x409))?;
                let index = index.unwrap_or(l.s.m.i32(0));
                l.s.op(
                    c,
                    "bufferLoad.f32",
                    Attr::ReadOnly,
                    l.s.t.res_ret,
                    68,
                    &[h, index, iu],
                )
            } else {
                let mask = l.s.m.i8((1u8 << count) - 1);
                let align = l.s.m.i32(4);
                let (h, coords) = match index {
                    _ if runtime => {
                        // Loads where the table's range checks fail read 0.
                        return l.runtime_access(
                            src,
                            index,
                            off,
                            4 * count,
                            11 | u,
                            |l, h, byte| {
                                let c = l.cur;
                                let r = l.s.op(
                                    c,
                                    "rawBufferLoad.f32",
                                    Attr::ReadOnly,
                                    l.s.t.res_ret,
                                    139,
                                    &[h, byte, iu, mask, align],
                                );
                                Ok((0..count)
                                    .map(|k| {
                                        let x = l.s.m.extract(c, r, k);
                                        l.as_int(x)
                                    })
                                    .collect())
                            },
                        );
                    }
                    Some(index) => (l.handle(site, src, false, (12 | u, stride))?, [index, off]),
                    None => (l.handle(site, src, false, (11 | u, 0))?, [off, iu]),
                };
                l.s.op(
                    c,
                    "rawBufferLoad.f32",
                    Attr::ReadOnly,
                    l.s.t.res_ret,
                    139,
                    &[h, coords[0], coords[1], mask, align],
                )
            };
            let mut xs = Vec::new();
            if d16 {
                // Two f16 channels per VGPR (an odd last high half 0).
                let k16 = l.s.m.i32(16);
                let i32t = l.s.t.i32;
                for k in 0..count.div_ceil(2) {
                    let mut x = None;
                    for ch in (2 * k..(2 * k + 2).min(count)).rev() {
                        let f = l.s.m.extract(c, r, ch);
                        let h = l.s.op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f]);
                        x = Some(match x {
                            Some(hi) => {
                                let hi = l.s.m.bin(c, Bin::Shl, hi, k16);
                                l.s.m.bin(c, Bin::Or, h, hi)
                            }
                            None => h,
                        });
                    }
                    xs.push(x.expect("a channel"));
                }
                return Ok(xs);
            }
            for k in 0..count {
                let x = l.s.m.extract(c, r, k);
                xs.push(l.as_int(x));
            }
            Ok(xs)
        })?;
        for (k, x) in xs.into_iter().enumerate() {
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }

    /// Whether an access by element of a buffer whose used-resource entry
    /// is `used` takes its stride from the host's runtime stride table: one
    /// whose stride is not known here (0), on a target that has the table
    /// ([`crate::emit::Target::stride_table`]). A known stride below 4 or
    /// off a multiple of 4, or an unknown one without the table, is refused.
    fn runtime_stride(&self, used: Option<&UsedResource>) -> Result<bool, Why> {
        match used.map_or(0, |u| u.stride) {
            s if s >= 4 && s.is_multiple_of(4) => Ok(false),
            0 if self.x.target.stride_table => Ok(true),
            _ => Err(UNKNOWN_STRIDE),
        }
    }

    /// Whether a raw access of `src`'s buffer (an untyped load, store or
    /// atomic, a format load) goes through the runtime stride table: by
    /// element (`index`) when its stride is not known here
    /// ([`Self::runtime_stride`]), by offset alone for a view-heap
    /// descriptor on a target with the table ([`Self::raw_through_table`]).
    fn through_table(
        &self,
        index: Option<V>,
        used: Option<&UsedResource>,
        src: &Source,
    ) -> Result<bool, Why> {
        match index {
            Some(_) => self.runtime_stride(used),
            None => Ok(self.raw_through_table(src)),
        }
    }

    /// Runs `access` (with the raw handle and the byte in its view) on the
    /// view the runtime stride table names for `src`'s slot, in the lanes
    /// where an access of `width` bytes at byte `off` (of element `index`,
    /// or by offset alone) passes the table's range checks
    /// ([`Self::runtime_view`]); what it gives is 0 in the others. `props`
    /// are the raw handle's (11, with the UAV bit for unordered access). The
    /// caller's guard, if any, is around it: nothing is read for a lane
    /// that must not access.
    pub(super) fn runtime_access(
        &mut self,
        src: &Source,
        index: Option<V>,
        off: V,
        width: u32,
        props: u32,
        access: impl FnOnce(&mut Self, V, V) -> Result<Vec<V>, Why>,
    ) -> Result<Vec<V>, Why> {
        let (e, byte, valid) = self.runtime_view(src, index, off, width)?;
        let at = self.begin_guarded(Some(valid));
        let h = self
            .s
            .heap_handle(self.cur, e.raw, false, e.nonuniform, (props, 0));
        let xs = match access(self, h, byte) {
            Ok(xs) => xs,
            Err(why) => {
                self.end_guarded(at, &[]);
                return Err(why);
            }
        };
        let zero = self.s.m.i32(0);
        let pairs: Vec<(V, V)> = xs.iter().map(|&x| (x, zero)).collect();
        Ok(self.end_guarded(at, &pairs))
    }

    /// The dwords `load` gives, computed only in lanes where `guard` holds
    /// (none: all); 0 in the others, whose registers the write's own guard
    /// keeps. `load` runs inside the guarded block: a failure there still
    /// closes it.
    pub(super) fn guarded_load(
        &mut self,
        guard: Option<V>,
        load: impl FnOnce(&mut Self) -> Result<Vec<V>, Why>,
    ) -> Result<Vec<V>, Why> {
        #[cfg(test)]
        if guard.is_some() {
            GUARDED_LOADS.with(|n| n.set(n.get() + 1));
        }
        let at = self.begin_guarded(guard);
        match load(self) {
            Ok(xs) => {
                let zero = self.s.m.i32(0);
                let pairs: Vec<(V, V)> = xs.iter().map(|&x| (x, zero)).collect();
                Ok(self.end_guarded(at, &pairs))
            }
            Err(why) => {
                self.end_guarded(at, &[]);
                Err(why)
            }
        }
    }

    /// A global memory load (`global_load_*`): from the program's constant
    /// table, or from a root SRV at the address the title sets.
    pub(super) fn flat(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Flat { dst, .. } = &inst.fields else {
            unreachable!()
        };
        let name = inst.name();
        let rest = name
            .strip_prefix("global_load_")
            .ok_or("a global memory write")?;
        let (n, sub) = match rest {
            "dword" => (1, None),
            "dwordx2" => (2, None),
            "dwordx3" => (3, None),
            "dwordx4" => (4, None),
            "ubyte" => (1, Some((8, false))),
            "sbyte" => (1, Some((8, true))),
            "ushort" => (1, Some((16, false))),
            "sshort" => (1, Some((16, true))),
            _ => return Err(name),
        };
        let a = *self
            .scalar
            .get(&(b, i))
            .ok_or("an untracked global address")?;
        let guard = self.guard(b, i)?;
        let Some(Operand::Vector { reg, .. }) = *dst else {
            return Err("a global load result");
        };
        let c = self.cur;
        let mut off = self.s.m.i32(a.offset);
        if let Some(d) = a.dynamic {
            let d = self.i32_of(d)?;
            off = self.s.m.bin(c, Bin::Add, off, d);
        }
        let h = if a.param == CODE {
            None
        } else {
            let src = Source::Built {
                address: a,
                bits: 0,
                words: [0, 0],
            };
            Some(self.handle((b, i), &src, false, (11, 0))?)
        };
        let dword = |l: &mut Self, byte: V| -> Result<V, Why> {
            match h {
                None => l.code_dword(byte),
                Some(h) => {
                    let c = l.cur;
                    let (u, mask, al) = (l.s.m.undef(l.s.t.i32), l.s.m.i8(1), l.s.m.i32(4));
                    let r = l.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        l.s.t.res_ret,
                        139,
                        &[h, byte, u, mask, al],
                    );
                    let x = l.s.m.extract(c, r, 0);
                    Ok(l.as_int(x))
                }
            }
        };
        for k in 0..n {
            let x = match sub {
                None => {
                    let four = self.s.m.i32(4 * k);
                    let at = self.s.m.bin(c, Bin::Add, off, four);
                    dword(self, at)?
                }
                Some((width, signed)) => {
                    // shortcut: the field is taken not to cross a dword.
                    let (not3, three) = (self.s.m.i32(!3), self.s.m.i32(3));
                    let at = self.s.m.bin(c, Bin::And, off, not3);
                    let w = dword(self, at)?;
                    let byte = self.s.m.bin(c, Bin::And, off, three);
                    let sh = self.s.m.bin(c, Bin::Shl, byte, three);
                    let x = self.s.m.bin(c, Bin::LShr, w, sh);
                    let left = self.s.m.i32(32 - width);
                    let x = self.s.m.bin(c, Bin::Shl, x, left);
                    self.s
                        .m
                        .bin(c, if signed { Bin::AShr } else { Bin::LShr }, x, left)
                }
            };
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    /// Loads [`Lower::guarded_load`] put under a guard on this thread.
    static GUARDED_LOADS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::super::{Parts, lower};
    use super::GUARDED_LOADS;
    use crate::native::{
        ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
    };
    use crate::{io, ir, resource, structure};

    /// Lowers a wave32 compute program of 32 threads with a table of
    /// structured dword buffers at s0 (a raw UAV at dword 16) and 512
    /// bytes of LDS: whether it uses a non-uniform handle, and how many
    /// loads the lowering put under a guard (summed over its attempts).
    fn guarded_loads(words: &[u32]) -> (u32, u32) {
        let range = |kind, table_offset_dwords, stride| UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords,
            lower_bound: 0,
            stride,
            dwords: 4,
            user_data: 0,
            slot: 0,
        };
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: false,
            vgprs: 32,
            layout: vec![RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            }],
            used: vec![
                range(ResourceType::Srv, 0, 4),
                range(ResourceType::Uav, 16, 0),
            ],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 1 << 15 | 1 << 1,
                threads: [32, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = ir::build(&native.code, false, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        GUARDED_LOADS.with(|n| n.set(0));
        let l = lower(&Parts {
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
        .unwrap();
        (
            l.nonuniform_handles,
            GUARDED_LOADS.with(std::cell::Cell::get),
        )
    }

    /// A waterfall loop's buffer load reads each lane's own descriptor; a
    /// lane outside EXEC (from 24 on, or one whose descriptor an earlier
    /// round took) may hold no descriptor to load through, so the load is
    /// guarded. A load through a descriptor of the whole wave under the
    /// same kind of EXEC keeps no guard of its own (its write still has
    /// one). (No device needed: `waterfall_tests` runs the first program.)
    #[test]
    fn loads_through_a_lanes_own_descriptor_skip_lanes_outside_exec() {
        use crate::resource::tests::WATERFALL_JOINED_BUFFER_WAVE32;
        let (nonuniform, guarded) = guarded_loads(WATERFALL_JOINED_BUFFER_WAVE32);
        assert_eq!(nonuniform, 1);
        assert!(guarded > 0);
        let uniform = [
            0xBE85_0380, // s_mov_b32 s5, 0
            0xBE84_0300, // s_mov_b32 s4, s0
            0xF408_0202,
            0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
            0x7DA4_0080, // v_cmpx_eq_u32 exec, 0, v0
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            0xE030_2000,
            0x8002_0100, // buffer_load_dword v1, v0, s[8:11], 0 idxen
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0xD834_0000,
            0x0000_0100, // ds_write_b32 v0, v1
            0xBF81_0000, // s_endpgm
        ];
        assert_eq!(guarded_loads(&uniform), (0, 0));
    }
}
