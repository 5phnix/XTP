//! Typed buffer accesses sixteen bits a channel (`buffer_load_format_d16_*`,
//! `buffer_store_format_d16_*`) through views in 16-bit unsigned formats,
//! run on WARP and the hardware adapter.
//!
//! The title's face network keeps its tensors (half floats, and plain 16-bit
//! integers such as indices beside them) in buffers it views as
//! `R16G16B16A16_UINT` and `R16_UINT`, and moves them with these
//! instructions: on the console each channel's 16 bits go as they are. A
//! translation that reads them as floats (`bufferLoad.f32`, then
//! `legacyF32ToF16`) and writes them as floats (`legacyF16ToF32`, then
//! `bufferStore.f32`) makes a float access through an integer view, which
//! D3D12 leaves undefined: every value read 0 on an RTX 5080, and no face
//! moved (fxceless/victory-fork#927). These programs check the bits.
//!
//! Ported from fxceless/victory-fork#927 (5dc9e63f04) by tippythetaidum
//! (its `halves_go_through_a_table_as_they_are_and_name_their_views`).
//!
//! The head's blend-shape extraction reads the network's output with a
//! plain `buffer_load_format_x` through an `R16_UINT` view and takes the low
//! half of each result as a half (`v_fma_mix_f32`). Read as a float there,
//! the output put rainbow squares on faces (tippythetaidum on #927). That
//! program is named in `lower::buffer::INTEGER_LOAD_PROGRAMS` by the
//! fingerprint of its code, and its plain typed loads move 16-bit integers;
//! the same loads of any other program keep their float path.

use super::host::{Dispatch, Host, Root, Stride, View};
use super::lower::{INTEGER_LOAD_PROGRAM_TEST, code_fingerprint};
use super::valu_tests::ONE_AT_A_TIME;
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16G16B16A16_UINT, DXGI_FORMAT_R32_FLOAT,
};

const S_ENDPGM: u32 = 0xBF81_0000;
/// Threads in the group (one wave of 32), `v0` each one's index.
const THREADS: u32 = 32;
const LOAD_FORMAT_X: u32 = 0x00;
const LOAD_D16_X: u32 = 0x80;
const LOAD_D16_XYZW: u32 = 0x83;
const STORE_D16_XYZW: u32 = 0x87;
const STORE_DWORD: u32 = 0x1C;

/// Half-float and 16-bit integer patterns the float path loses: both
/// zeros, subnormals, the largest number, infinities, not-a-numbers of both
/// signs and payloads, and integers past the largest number's pattern (as
/// indices are).
const SPECIAL: [u16; 16] = [
    0x0000, 0x0001, 0x03ff, 0x0400, 0x3c00, 0x7bff, 0x7c00, 0x7c01, 0x7e00, 0x7fff, 0x8000, 0x8001,
    0xbc00, 0xfc00, 0xfc01, 0xffff,
];

/// SOP1 `s_mov_b32 sdst, src0`.
const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

/// SMEM `s_load_dwordx4 s[dst:dst+3], s[base:base+1], offset` (soffset
/// null): the table's descriptor `offset` bytes on.
const fn s_load_dwordx4(dst: u8, base: u8, offset: u32) -> [u32; 2] {
    [
        0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        125 << 25 | offset,
    ]
}

/// MUBUF `op vdata, vaddr, s[srsrc:srsrc+3], 0`, `idxen` (by element) or
/// `offen` (by byte).
const fn mubuf(op: u32, vdata: u8, vaddr: u8, srsrc: u8, idxen: bool) -> [u32; 2] {
    let mode = if idxen { 1 << 13 } else { 1 << 12 };
    [
        0x38 << 26 | op << 18 | mode,
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// VOP2 `v_lshlrev_b32 vdst, 2, vsrc1`: the byte of dword `vsrc1`.
const fn dword_byte(vdst: u8, vsrc1: u8) -> u32 {
    0x1A << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | 130
}

/// VOP3P `v_fma_mix_f32 vdst, src0, 1.0, 0`, `src0` the low half of VGPR
/// `vsrc0` taken as a half (`op_sel_hi` bit 0 set, `op_sel` clear): the
/// half widened to a float, as the head's blend-shape extraction reads the
/// network's output.
const fn half_widened(vdst: u8, vsrc0: u8) -> [u32; 2] {
    const ONE: u32 = 242;
    const ZERO: u32 = 128;
    [
        0x33 << 26 | 0x20 << 16 | vdst as u32,
        1 << 27 | ZERO << 18 | ONE << 9 | (256 + vsrc0 as u32),
    ]
}

/// The float a half's bits stand for.
fn widened(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from(h >> 10 & 0x1F);
    let mantissa = f32::from(h & 0x3FF);
    sign * match exponent {
        0 => mantissa * 2f32.powi(-24),
        31 if mantissa == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        e => (1024.0 + mantissa) * 2f32.powi(e - 25),
    }
}

/// A used-resource entry of the table in root parameter 1: views of `kind`
/// from dword `table_offset_dwords` on.
fn entry(kind: ResourceType, table_offset_dwords: u32) -> UsedResource {
    UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: table_offset_dwords / 8,
        stride: 0,
        dwords: 4,
        user_data: 4,
        slot: 1,
    }
}

/// A one-wave compute program `body` (after the table address in `s[4:5]`,
/// the table's first descriptor in `s[8:11]` and its second, 0x20 bytes on,
/// in `s[12:15]`), whose table holds an SRV and then a UAV, lowered and
/// validated; with the number of its typed loads lowered as 16-bit
/// integers (`Lowered::integer_loads`).
fn translated(body: &[u32]) -> (HostShader, u32) {
    let used = vec![entry(ResourceType::Srv, 0), entry(ResourceType::Uav, 8)];
    translated_at(body, [0, 0x20], used, false)
}

/// [`translated`], the program named among the programs whose plain typed
/// loads move 16-bit integers, as the head's blend-shape extraction is.
fn translated_listed(body: &[u32]) -> (HostShader, u32) {
    let used = vec![entry(ResourceType::Srv, 0), entry(ResourceType::Uav, 8)];
    translated_at(body, [0, 0x20], used, true)
}

/// [`translated`] with the two descriptors loaded `at` these bytes of the
/// table, which `used` describes, and the program `listed` or not.
fn translated_at(
    body: &[u32],
    at: [u32; 2],
    used: Vec<UsedResource>,
    listed: bool,
) -> (HostShader, u32) {
    let mut words = vec![s_mov_b32(5, 0x80)];
    words.extend_from_slice(&s_load_dwordx4(8, 4, at[0]));
    words.extend_from_slice(&s_load_dwordx4(12, 4, at[1]));
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        float_mode: 0,
        vgprs: 8,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used,
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
            threads: [THREADS, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    INTEGER_LOAD_PROGRAM_TEST.set(listed.then(|| code_fingerprint(&native.code)));
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        target: super::Target::ANY,
        console_cf: false,
        per_lane_only: false,
    })
    .expect("lower");
    INTEGER_LOAD_PROGRAM_TEST.set(None);
    let integer_loads = lowered.integer_loads;
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        group_base: lowered.group_base,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        stride_table: lowered.stride_table,
        run_time_buffers: lowered.run_time_buffers,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    (shader, integer_loads)
}

/// Runs `shader` with `views` on each of [`Host::test_hosts`]; `check` gets
/// each run's first UAV as dwords.
fn run(shader: &HostShader, views: &[View], check: impl Fn(&str, &[u32])) {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (label, host) in Host::test_hosts() {
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = match (u.root, u.slot) {
                // The table starts at heap index 0.
                (RootType::Table, 1) => 0,
                other => panic!("unexpected user data {other:?}"),
            };
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        check(label, &out[0]);
    }
}

/// `v[1:2] = from[thread]` (four halves), `to[thread] = v[1:2]`, `from` the
/// descriptor in `s[8:11]`, `to` the one in `s[12:15]`.
fn moved_halves() -> Vec<u32> {
    let mut body = Vec::new();
    body.extend_from_slice(&mubuf(LOAD_D16_XYZW, 1, 0, 8, true));
    body.extend_from_slice(&mubuf(STORE_D16_XYZW, 1, 0, 12, true));
    body
}

/// Four halves a thread: [`SPECIAL`], then others across the patterns.
fn halves() -> Vec<u16> {
    (0..4 * THREADS as usize)
        .map(|i| match i {
            0..16 => SPECIAL[i],
            _ => (i as u16).wrapping_mul(0x0203).wrapping_add(0x7b00),
        })
        .collect()
}

/// 16-bit values two to a dword, the first in the low half.
fn packed(values: &[u16]) -> Vec<u32> {
    values
        .chunks(2)
        .map(|pair| u32::from(pair[0]) | u32::from(pair[1]) << 16)
        .collect()
}

/// A typed `R16G16B16A16_UINT` buffer SRV over `data`.
fn four_halves(data: &[u32]) -> View<'_> {
    View::Buffer {
        data,
        stride: Stride::Format(DXGI_FORMAT_R16G16B16A16_UINT, 8),
    }
}

/// Asserts that `out`, read back from an `R16G16B16A16_UINT` UAV, holds
/// `expected` as they are.
fn same_halves(label: &str, expected: &[u16], out: &[u32]) {
    let lost: Vec<String> = expected
        .iter()
        .enumerate()
        .filter_map(|(i, &h)| {
            let got = (out[i / 2] >> (16 * (i % 2))) as u16;
            (got != h).then(|| format!("{h:#06x} -> {got:#06x}"))
        })
        .collect();
    assert!(
        lost.is_empty(),
        "{label}: {} of {} halves changed: {}",
        lost.len(),
        expected.len(),
        lost.join(", ")
    );
}

/// `buffer_load_format_d16_xyzw` from an `R16G16B16A16_UINT` buffer and
/// `buffer_store_format_d16_xyzw` to another: the halves stored are the
/// halves loaded, not-a-number patterns and integers above `0x7bff`
/// included (the face network's tensors).
#[test]
fn halves_cross_r16g16b16a16_uint_views_as_they_are() {
    let (shader, integer_loads) = translated(&moved_halves());
    assert_eq!(integer_loads, 1, "16-bit integer loads: the d16 load");
    let halves = halves();
    let data = packed(&halves);
    let views = [
        four_halves(&data),
        View::BufferUav(
            2 * THREADS,
            Stride::Format(DXGI_FORMAT_R16G16B16A16_UINT, 8),
        ),
    ];
    run(&shader, &views, |label, out| {
        same_halves(label, &halves, out)
    });
}

/// The review of fork #927 (by avt0m4t48) found its first version naming,
/// for a descriptor loaded 0x20 bytes into a range of views one
/// used-resource entry describes from dword 0, the range's start (dword 0)
/// instead of the descriptor's own (dword 8), so its host looked at the
/// wrong slot. XTP names no slot for the host here, and the handle a
/// sixteen-bit access goes through is the loaded descriptor's own: the
/// halves come from the SRV in heap slot 1, not the decoy in slot 0 where
/// the range starts, and go to the UAV in slot 2.
#[test]
fn a_descriptor_loaded_inside_a_range_is_the_one_moved() {
    // One entry for a range of SRVs from dword 0, one for the UAV at 16.
    let used = vec![entry(ResourceType::Srv, 0), entry(ResourceType::Uav, 16)];
    let (shader, integer_loads) = translated_at(&moved_halves(), [0x20, 0x40], used, false);
    assert_eq!(integer_loads, 1, "16-bit integer loads: the d16 load");
    let halves = halves();
    let data = packed(&halves);
    let decoy = vec![0x5a5a_a5a5; data.len()];
    let views = [
        four_halves(&decoy),
        four_halves(&data),
        View::BufferUav(
            2 * THREADS,
            Stride::Format(DXGI_FORMAT_R16G16B16A16_UINT, 8),
        ),
    ];
    run(&shader, &views, |label, out| {
        same_halves(label, &halves, out)
    });
}

/// `buffer_load_format_d16_x` from an `R16_UINT` buffer (the network's
/// indices): the low half of the register is the element's 16 bits.
#[test]
fn indices_load_from_r16_uint_views_as_they_are() {
    // v1 = from[thread] (one half), to[thread] = v1 as a dword.
    let mut body = Vec::new();
    body.extend_from_slice(&mubuf(LOAD_D16_X, 1, 0, 8, true));
    body.push(dword_byte(2, 0));
    body.extend_from_slice(&mubuf(STORE_DWORD, 1, 2, 12, false));
    let (shader, integer_loads) = translated(&body);
    assert_eq!(integer_loads, 1, "16-bit integer loads: the d16 load");
    let indices: Vec<u16> = (0..THREADS as usize)
        .map(|i| match i {
            0..16 => SPECIAL[i],
            _ => (i as u16).wrapping_mul(0x0d0b).wrapping_add(0x7c00),
        })
        .collect();
    let data = packed(&indices);
    let views = [
        View::Buffer {
            data: &data,
            stride: Stride::Format(DXGI_FORMAT_R16_UINT, 2),
        },
        View::RawUav(THREADS),
    ];
    run(&shader, &views, |label, out| {
        let lost: Vec<String> = indices
            .iter()
            .zip(out)
            .filter_map(|(&i, &got)| {
                let got = got as u16;
                (got != i).then(|| format!("{i:#06x} -> {got:#06x}"))
            })
            .collect();
        assert!(
            lost.is_empty(),
            "{label}: {} of {} indices changed: {}",
            lost.len(),
            indices.len(),
            lost.join(", ")
        );
    });
}

/// `v1 = from[thread]` (`buffer_load_format_x`, not d16), `v3` = the half in
/// `v1`'s low 16 bits widened (`v_fma_mix_f32`), `to[thread] = v3`; with
/// `whole`, `to[thread] = v1` instead (the register read whole as well).
fn format_load_read_as_half(whole: bool) -> Vec<u32> {
    let mut body = Vec::new();
    body.extend_from_slice(&mubuf(LOAD_FORMAT_X, 1, 0, 8, true));
    body.extend_from_slice(&half_widened(3, 1));
    body.push(dword_byte(2, 0));
    let stored = if whole { 1 } else { 3 };
    body.extend_from_slice(&mubuf(STORE_DWORD, stored, 2, 12, false));
    body
}

/// A program named among those whose plain typed loads move 16-bit
/// integers, as the head's blend-shape extraction is, reads its
/// `buffer_load_format_x` from an `R16_UINT` buffer as unsigned integers:
/// the half in each register's low 16 bits, widened by `v_fma_mix_f32`,
/// comes out as that half's value, not-a-numbers as not-a-numbers.
#[test]
fn a_listed_program_reads_its_format_loads_as_16_bit_integers() {
    let (shader, integer_loads) = translated_listed(&format_load_read_as_half(false));
    assert_eq!(
        integer_loads, 1,
        "16-bit integer loads: the listed format load"
    );
    // Normal numbers, both zeros, infinities and not-a-numbers; no
    // subnormal, which a host may flush.
    const SAMPLES: [u16; 16] = [
        0x0000, 0x8000, 0x3c00, 0xbc00, 0x4000, 0x3555, 0x7bff, 0xfbff, 0x0400, 0x8400, 0x7c00,
        0xfc00, 0x7c01, 0x7e00, 0xffff, 0x5640,
    ];
    let halves: Vec<u16> = (0..THREADS as usize)
        .map(|i| match i {
            0..16 => SAMPLES[i],
            _ => {
                let h = (i as u16).wrapping_mul(0x0b3d).wrapping_add(0x1234);
                if h & 0x7c00 == 0 { h | 0x0400 } else { h }
            }
        })
        .collect();
    let data = packed(&halves);
    let views = [
        View::Buffer {
            data: &data,
            stride: Stride::Format(DXGI_FORMAT_R16_UINT, 2),
        },
        View::RawUav(THREADS),
    ];
    run(&shader, &views, |label, out| {
        let lost: Vec<String> = halves
            .iter()
            .zip(out)
            .filter_map(|(&h, &got)| {
                let (want, got) = (widened(h), f32::from_bits(got));
                let same = if want.is_nan() {
                    got.is_nan()
                } else {
                    got == want
                };
                (!same).then(|| format!("{h:#06x}: {want} -> {got}"))
            })
            .collect();
        assert!(
            lost.is_empty(),
            "{label}: {} of {} halves read wrong: {}",
            lost.len(),
            halves.len(),
            lost.join(", ")
        );
    });
}

/// The same loads in any other program keep their float path, whether
/// their registers are read as halves or whole: from an `R32_FLOAT` buffer
/// each float comes back as it was.
#[test]
fn other_programs_keep_the_float_path_of_format_loads() {
    let (_, integer_loads) = translated(&format_load_read_as_half(false));
    assert_eq!(
        integer_loads, 0,
        "16-bit integer loads: an unlisted program"
    );
    let (shader, integer_loads) = translated(&format_load_read_as_half(true));
    assert_eq!(
        integer_loads, 0,
        "16-bit integer loads: an unlisted program"
    );
    let floats: Vec<u32> = (0..THREADS)
        .map(|i| (i as f32 * 1.5 - 7.25).to_bits())
        .collect();
    let views = [
        View::Buffer {
            data: &floats,
            stride: Stride::Format(DXGI_FORMAT_R32_FLOAT, 4),
        },
        View::RawUav(THREADS),
    ];
    run(&shader, &views, |label, out| {
        assert_eq!(out, &floats[..], "{label}: the floats loaded");
    });
}

/// Programs are named by FNV-1a on 64 bits of their code: the function's
/// published values.
#[test]
fn programs_are_named_by_the_fnv_1a_hash_of_their_code() {
    assert_eq!(code_fingerprint(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(code_fingerprint(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(code_fingerprint(b"foobar"), 0x8594_4171_f739_67e8);
}
