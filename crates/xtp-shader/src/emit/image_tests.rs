//! Image instructions: hand-assembled GFX10 compute programs (our own
//! code) that run image instructions and store what they return. Every
//! translation is checked by the validator when `dxil.dll` is found; on the
//! host GPU (WARP in tests on the person's PC) the results are checked
//! against values worked out on the CPU, skipped without an SM 6.6 device
//! or without the feature a test needs.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, emit};
use crate::dxbc::{self, Container, FourCC};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const S_WAITCNT_0: u32 = 0xBF8C_0000;
/// Inline constants and the literal (`src0` codes).
const ZERO: u32 = 0x80;
const ONE: u32 = 0x81;
const SEVEN: u32 = 0x87;
const MINUS_ONE: u32 = 0xC1;
const LITERAL: u32 = 0xFF;
/// `src0` code of VGPR 0 (the lane's thread id).
const V0: u32 = 0x100;

fn s_mov(d: u8, src: u32) -> u32 {
    0xBE80_0300 | u32::from(d) << 16 | src
}

/// `s_load_dwordx<n>` (op 2 x4, 3 x8) of s[d..] from s[base:base+1] + offset.
fn s_load(op: u32, d: u8, base: u8, offset: u32) -> [u32; 2] {
    [
        0xF400_0000 | op << 18 | u32::from(d) << 6 | u32::from(base / 2),
        0xFA00_0000 | offset,
    ]
}

fn v_mov(d: u8, src: u32) -> u32 {
    0x7E00_0000 | u32::from(d) << 17 | 1 << 9 | src
}

/// `v_lshlrev_b32 vd, by, vs`.
fn v_lshl(d: u8, by: u32, v: u8) -> u32 {
    0x1A << 25 | u32::from(d) << 17 | u32::from(v) << 9 | (0x80 + by)
}

/// `v_and_b32 vd, src0, vs`.
fn v_and(d: u8, src0: u32, v: u8) -> u32 {
    0x1B << 25 | u32::from(d) << 17 | u32::from(v) << 9 | src0
}

/// MUBUF `op` with offen: v[data..] at s[rsrc..]'s base + v[addr] + offset.
fn mubuf(op: u32, data: u8, addr: u8, rsrc: u8, offset: u32) -> [u32; 2] {
    [
        0xE000_0000 | op << 18 | 1 << 12 | offset,
        u32::from(addr) | u32::from(data) << 8 | u32::from(rsrc / 4) << 16 | ZERO << 24,
    ]
}

/// The program `words` as a 32-lane compute program over tables of a
/// texture UAV (user data s0, slot 1) and a raw buffer UAV (s1, slot 2).
fn native(words: &[u32]) -> NativeProgram {
    let table = |user_data, slot| RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot,
    };
    let used = |user_data, slot, dwords| UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords,
        user_data,
        slot,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![table(0, 1), table(1, 2)],
        used: vec![used(0, 1, 8), used(1, 2, 4)],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [32, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `words` translated, and validated when the validator is found.
fn translate(words: &[u32]) -> Result<HostShader, ShaderError> {
    let n = native(words);
    let p = ir::build(&n.code, n.wave64, n.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &n.layout, n.stage, Some(&st))?;
    let io = io::raise(&p, &n)?;
    let bytes = dxbc::build(&[]);
    let c = Container::parse(&bytes)?;
    let s = emit(&c, &n, &p, &st, &io, &res)?;
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut s.dxil.clone())
            .unwrap_or_else(|e| panic!("validation: {e}"));
    }
    Ok(s)
}

/// The `SFI0` feature bits of a translation.
fn features(s: &HostShader) -> u64 {
    let c = Container::parse(&s.dxil).unwrap();
    u64::from_le_bytes(c.part(FourCC::SFI0).unwrap().data.try_into().unwrap())
}

/// Runs `s` with the texture UAV `image` at heap index 0 and a raw buffer
/// UAV of `out` dwords at 1: what each holds afterwards.
fn run(host: &Host, s: &HostShader, image: View, out: u32) -> Result<Vec<Vec<u32>>, String> {
    let (root, pso) = host.compute_pipeline(s)?;
    let mut data = vec![0; s.user_data_dwords().max(1) as usize];
    for u in &s.user_data {
        data[u.index as usize] = match u.slot {
            1 => 0,
            2 => 32,
            n => panic!("user data of slot {n}"),
        };
    }
    host.dispatch(
        &root,
        &pso,
        &Dispatch {
            roots: vec![(0, Root::Constants(data))],
            views: &[image, View::RawUav(out)],
            samplers: &[],
            groups: [1, 1, 1],
        },
    )
}

/// 64-bit image atomics (an R32G32 view: two data dwords per value), the
/// image in s[8:15] and the buffer in s[16:19].
mod atomic64 {
    use super::*;

    /// A 2D MIMG atomic `op`: data v[data..], address v[2:3], glc.
    fn atomic(op: u32, dmask: u8, data: u8, glc: bool) -> [u32; 2] {
        [
            0xF000_0000 | (op & 0x7F) << 18 | u32::from(glc) << 13 | u32::from(dmask) << 8 | 1 << 3,
            2 | u32::from(data) << 8 | (8 / 4) << 16,
        ]
    }
    const ADD: u32 = 0x11;
    const CMPSWAP: u32 = 0x10;
    const UMAX: u32 = 0x17;

    fn head() -> Vec<u32> {
        let mut w = vec![s_mov(4, 0), s_mov(5, ZERO), s_mov(6, 1), s_mov(7, ZERO)];
        w.extend(s_load(3, 8, 4, 0));
        w.extend(s_load(2, 16, 6, 0));
        w.push(S_WAITCNT_0);
        w
    }

    /// Every lane adds 2^32 + 2^31 to texel `lane & 3` of a 4 × 1 image:
    /// eight lanes per texel, so the low dwords carry into the high ones.
    fn shared_add() -> Vec<u32> {
        let mut w = head();
        w.extend([v_and(2, 0x83, 0), v_mov(3, ZERO)]);
        w.extend([v_mov(20, LITERAL), 0x8000_0000, v_mov(21, ONE)]);
        w.extend(atomic(ADD, 0x3, 20, false));
        w.extend([S_WAITCNT_0, S_ENDPGM]);
        w
    }

    /// Each lane at its own texel of a 32 × 1 image: adds (lane << 32) +
    /// 0xFFFF_FFFF and gets the old value back (0), then swaps in
    /// (7 << 32) + lane where the texel equals what it added, getting that
    /// back. The returned pairs go to dwords 4 × lane of the buffer.
    fn own_texel() -> Vec<u32> {
        let mut w = head();
        w.extend([v_mov(2, V0), v_mov(3, ZERO)]);
        w.extend([v_mov(20, MINUS_ONE), v_mov(21, V0)]);
        w.extend(atomic(ADD, 0x3, 20, true));
        // cmpswap data: the new value, then the compare.
        w.extend([
            v_mov(24, V0),
            v_mov(25, SEVEN),
            v_mov(26, MINUS_ONE),
            v_mov(27, V0),
        ]);
        w.push(S_WAITCNT_0);
        w.extend(atomic(CMPSWAP, 0xF, 24, true));
        w.push(S_WAITCNT_0);
        w.extend([v_mov(22, 0x100 + 24), v_mov(23, 0x100 + 25)]);
        w.push(v_lshl(28, 4, 0));
        w.extend(mubuf(0x1E, 20, 28, 16, 0));
        w.extend([S_WAITCNT_0, S_ENDPGM]);
        w
    }

    /// The translations ask for 64-bit integer operations and 64-bit
    /// atomics on typed and on descriptor-heap resources (the flags and
    /// SFI0 bits DXC sets for `InterlockedAdd` on a heap
    /// `RWTexture2D<uint64_t>`), and the validator accepts them.
    #[test]
    fn sixty_four_bit_image_atomics_declare_their_features() {
        for words in [shared_add(), own_texel()] {
            let s = translate(&words).unwrap();
            let f = features(&s);
            for bit in [0x8000, 0x40_0000, 0x1000_0000] {
                assert_ne!(f & bit, 0, "SFI0 {f:#x} lacks {bit:#x}");
            }
        }
    }

    /// Other data masks are no 32- or 64-bit atomic.
    #[test]
    fn an_image_atomics_data_mask_is_one_or_two_values() {
        for (op, dmask) in [(ADD, 0x7), (UMAX, 0xF), (CMPSWAP, 0x1), (CMPSWAP, 0x7)] {
            let mut w = head();
            w.extend([v_mov(2, ZERO), v_mov(3, ZERO)]);
            w.extend(atomic(op, dmask, 20, false));
            w.push(S_ENDPGM);
            let e = translate(&w).err().map(|e| e.to_string());
            assert!(
                e.as_deref()
                    .is_some_and(|e| e.contains("an image atomic's data mask")),
                "{op:#x} dmask {dmask:#x}: {e:?}"
            );
        }
    }

    fn host() -> Option<Host> {
        let host = match Host::new(false) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("no SM 6.6 device ({e}); skipped");
                return None;
            }
        };
        if !host.int64_image_atomics() {
            eprintln!("the device has no 64-bit atomics on heap typed resources; skipped");
            return None;
        }
        Some(host)
    }

    #[test]
    fn sixty_four_bit_image_adds_carry_between_dwords() {
        let Some(host) = host() else { return };
        let s = translate(&shared_add()).unwrap();
        let out = run(&host, &s, View::Uint2TextureUav(4, 1), 4).unwrap();
        // 8 × (2^32 + 2^31) = 12 × 2^32: low dwords 0, high 12 (8 without
        // the carry).
        assert_eq!(out[0], [0, 12, 0, 12, 0, 12, 0, 12]);
    }

    #[test]
    fn sixty_four_bit_image_compare_swaps_return_the_old_value() {
        let Some(host) = host() else { return };
        let s = translate(&own_texel()).unwrap();
        let out = run(&host, &s, View::Uint2TextureUav(32, 1), 4 * 32).unwrap();
        for lane in 0..32u32 {
            let l = lane as usize;
            assert_eq!(out[0][2 * l..2 * l + 2], [lane, 7], "texel {lane}");
            assert_eq!(
                out[1][4 * l..4 * l + 4],
                [0, 0, u32::MAX, lane],
                "lane {lane}"
            );
        }
    }
}
