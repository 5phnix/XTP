//! Buffers of a stride known at translation through the views the GPU side
//! makes for them: the title's structured views as they are, or raw views
//! of the same bytes with `XTP_RAW_STRUCTURED=1` ([`super::raw_structured`],
//! Marvisak/VIctory#1762), with or without the runtime stride table. Hand-
//! assembled compute programs run on WARP and the default adapter (outside
//! `XTP_TEST_ADAPTER=warp`): each reads a buffer's record count (descriptor
//! dword 2) as data and accesses the buffer through the same descriptor.
//!
//! The cases follow the measurements of VIctory#1762's thread (an RTX 5080
//! and WARP): AlanQiang0418's counts by access kind (b86d853) and indexed
//! reads at strides 8 to 24 (f050b1d), which an RTX read wrong through a
//! structured handle over a raw view. Written anew for this test host, with
//! the stride table, atomics, a scalar load of an unordered view and reads
//! inside an element added.

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, StrideEntry, Target};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};

/// The views a program runs with, as the GPU side makes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `XTP_RAW_STRUCTURED` off: structured views stay structured.
    Structured,
    /// `XTP_RAW_STRUCTURED=1`: structured views become raw views.
    Raw,
    /// `XTP_RAW_STRUCTURED=1` with the runtime stride table, whose entries
    /// name each slot's own (raw) view.
    Table,
}

const MODES: [Mode; 3] = [Mode::Structured, Mode::Raw, Mode::Table];

/// The kind of the title's view of the buffer read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A raw view (no stride in the used-resource entry).
    Raw,
    /// RGBA32F elements (16 bytes in the entry).
    Typed,
    /// Structured, elements of this many bytes.
    Structured(u32),
}

impl Kind {
    /// The used-resource entry's stride.
    fn stride(self) -> u32 {
        match self {
            Kind::Raw => 0,
            Kind::Typed => 16,
            Kind::Structured(s) => s,
        }
    }

    /// How the host views the buffer in `mode`.
    fn view(self, mode: Mode) -> Stride {
        match (self, mode) {
            (Kind::Raw, _) | (Kind::Structured(_), Mode::Raw | Mode::Table) => Stride::Raw,
            (Kind::Typed, _) => Stride::Typed,
            (Kind::Structured(s), Mode::Structured) => Stride::Structured(s),
        }
    }

    /// What the console's descriptor holds in dword 2 for [`BYTES`] bytes:
    /// elements of a structured or typed view, bytes of a raw one.
    fn records(self) -> u32 {
        match self {
            Kind::Raw => BYTES,
            Kind::Typed | Kind::Structured(_) => BYTES / self.stride(),
        }
    }
}

/// The buffer read: 48 dwords.
const BYTES: u32 = 192;

/// What the buffer holds: dword `i` is the float `100 + i` (floats, so that
/// a typed RGBA32F read returns the same bits).
fn data() -> Vec<u32> {
    (0..BYTES / 4)
        .map(|i| (100.0 + i as f32).to_bits())
        .collect()
}

/// The table at `s0` (user SGPR 0); the buffer's descriptor at table dword 0
/// into `s[8:11]`, the output's at dword 8 into `s[16:19]`; the record count
/// (`s10`) into `v1`. The access follows, then [`EPILOGUE`].
const PROLOGUE: [u32; 8] = [
    0xBE85_0380, // s_mov_b32 s5, 0
    0xBE84_0300, // s_mov_b32 s4, s0
    0xF408_0202,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
    0xF408_0402,
    0xFA00_0020, // s_load_dwordx4 s[16:19], s[4:5], 0x20
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x7E02_020A, // v_mov_b32 v1, s10 (the record count)
];

/// Stores the count (`v1`) and what the access left in `v2` at dwords 0
/// and 1 of the output.
const EPILOGUE: [u32; 6] = [
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0xE070_0000,
    0x8004_0100, // buffer_store_dword v1, off, s[16:19], 0
    0xE070_0004,
    0x8004_0200, // buffer_store_dword v2, off, s[16:19], 0 offset:4
    0xBF81_0000, // s_endpgm
];

/// Loads element 1 into `v2`.
const INDEXED_LOAD: &[u32] = &[
    0x7E06_0281, // v_mov_b32 v3, 1
    0xE030_2000,
    0x8002_0203, // buffer_load_dword v2, v3, s[8:11], 0 idxen
];

/// Loads byte 4 of element 1 into `v2`.
const INDEXED_LOAD_AT_4: &[u32] = &[
    0x7E06_0281, // v_mov_b32 v3, 1
    0x7E08_0284, // v_mov_b32 v4, 4
    0xE030_3000,
    0x8002_0203, // buffer_load_dword v2, v[3:4], s[8:11], 0 idxen offen
];

/// Loads byte 12 into `v2`.
const OFFSET_LOAD: &[u32] = &[
    0x7E06_028C, // v_mov_b32 v3, 12
    0xE030_1000,
    0x8002_0203, // buffer_load_dword v2, v3, s[8:11], 0 offen
];

/// Loads byte 12 with a scalar load, into `v2`.
const SCALAR_LOAD: &[u32] = &[
    0xF420_0504,
    0xFA00_000C, // s_buffer_load_dword s20, s[8:11], 0xc
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x7E04_0214, // v_mov_b32 v2, s20
];

/// Loads channel x of element 1 into `v2`.
const TYPED_LOAD: &[u32] = &[
    0x7E06_0281, // v_mov_b32 v3, 1
    0xE000_2000,
    0x8002_0203, // buffer_load_format_x v2, v3, s[8:11], 0 idxen
];

/// Stores 42 (`v2`) at element 1.
const INDEXED_STORE: &[u32] = &[
    0x7E04_02AA, // v_mov_b32 v2, 42
    0x7E06_0281, // v_mov_b32 v3, 1
    0xE070_2000,
    0x8002_0203, // buffer_store_dword v2, v3, s[8:11], 0 idxen
];

/// Stores 42 (`v2`) at byte 12.
const OFFSET_STORE: &[u32] = &[
    0x7E04_02AA, // v_mov_b32 v2, 42
    0x7E06_028C, // v_mov_b32 v3, 12
    0xE070_1000,
    0x8002_0203, // buffer_store_dword v2, v3, s[8:11], 0 offen
];

/// Adds 42 (`v2`, which keeps it: no glc) at element 1.
const INDEXED_ATOMIC: &[u32] = &[
    0x7E04_02AA, // v_mov_b32 v2, 42
    0x7E06_0281, // v_mov_b32 v3, 1
    0xE0C8_2000,
    0x8002_0203, // buffer_atomic_add v2, v3, s[8:11], 0 idxen
];

/// A used-resource entry of the table at `s0`.
fn entry(kind: ResourceType, table_offset_dwords: u32, stride: u32) -> UsedResource {
    UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride,
        dwords: 4,
        user_data: 0,
        slot: 0,
    }
}

/// One thread running [`PROLOGUE`], `access`, [`EPILOGUE`] over a buffer of
/// `kind` (an unordered view where `uav`) and a raw output.
fn program(access: &[u32], kind: Kind, uav: bool) -> NativeProgram {
    let words: Vec<u32> = PROLOGUE
        .iter()
        .chain(access)
        .chain(&EPILOGUE)
        .copied()
        .collect();
    let source = if uav {
        ResourceType::Uav
    } else {
        ResourceType::Srv
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![
            entry(source, 0, kind.stride()),
            entry(ResourceType::Uav, 8, 0),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// Sets [`super::raw_structured`] for this thread while it lives.
struct RawStructured;

impl RawStructured {
    fn set(on: bool) -> Self {
        super::RAW_STRUCTURED_TEST.set(Some(on));
        RawStructured
    }
}

impl Drop for RawStructured {
    fn drop(&mut self) {
        super::RAW_STRUCTURED_TEST.set(None);
    }
}

/// `native` translated for `mode` (with [`super::raw_structured`] as the
/// mode has it), validated where `dxil.dll` is.
fn translate(native: &NativeProgram, mode: Mode) -> Result<HostShader, crate::ShaderError> {
    let _raw = RawStructured::set(mode != Mode::Structured);
    let target = Target {
        stride_table: mode == Mode::Table,
        ..Target::ANY
    };
    let mut cs = super::tests::emit_for(native, target)?;
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut cs.dxil).expect("DXIL validation");
    }
    Ok(cs)
}

/// The devices to run on, one GPU test at a time (see
/// `valu_tests::ONE_AT_A_TIME`): WARP, and the default adapter unless
/// `XTP_TEST_ADAPTER` keeps tests off it.
fn hosts() -> (
    std::sync::MutexGuard<'static, ()>,
    Vec<(&'static str, Host)>,
) {
    let one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut hosts = Vec::new();
    for (name, warp) in [("WARP", true), ("the default adapter", false)] {
        match Host::new(warp) {
            Ok(h) => hosts.push((name, h)),
            Err(e) => eprintln!("{name}: no SM 6.6 device ({e}); skipped"),
        }
    }
    // In tests WARP may be the only device: once is enough.
    hosts.truncate(if super::host::test_adapter().is_some() {
        1
    } else {
        2
    });
    (one, hosts)
}

/// Runs `cs` over the buffer of `kind` holding [`data`] (an unordered view
/// of zeros where `uav`) in heap slot 0 and a raw output of two dwords in
/// slot 1, as `mode` views them: what each unordered view read back holds.
fn run(host: &Host, cs: &HostShader, kind: Kind, uav: bool, mode: Mode) -> Vec<Vec<u32>> {
    let data = data();
    let stride = kind.view(mode);
    let views = [
        if uav {
            View::BufferUav(BYTES / 4, stride)
        } else {
            View::Buffer {
                data: &data,
                stride,
            }
        },
        View::RawUav(2),
    ];
    let mut roots = vec![(
        0,
        Root::Constants(vec![0; cs.user_data_dwords().max(1) as usize]),
    )];
    if cs.stride_table {
        // Each slot's own view, raw here (a typed one has no raw view of
        // its own: typed loads do not read the table).
        let own = |slot, stride, count| StrideEntry {
            stride,
            raw_slot: Some(slot),
            bias: 0,
            count,
        };
        let source = match kind {
            Kind::Raw => own(0, 0, BYTES / 4),
            Kind::Typed => StrideEntry::NONE,
            Kind::Structured(s) => own(0, s, BYTES / s),
        };
        let table: Vec<u32> = [source, own(1, 0, 2)]
            .iter()
            .flat_map(|e| {
                let bits = e.bits().expect("an entry the table holds");
                [bits as u32, (bits >> 32) as u32]
            })
            .collect();
        roots.push((
            1,
            Root::View {
                srv: true,
                data: table,
            },
        ));
    }
    let (root, pso) = host.compute_pipeline(cs).expect("a pipeline");
    host.dispatch(
        &root,
        &pso,
        &Dispatch {
            roots,
            views: &views,
            samplers: &[],
            groups: [1, 1, 1],
        },
    )
    .expect("a dispatch")
}

/// A program of [`program`] and what it must give.
struct Case {
    name: String,
    access: &'static [u32],
    kind: Kind,
    uav: bool,
    /// The access reads or writes raw (by byte offset, or a scalar load):
    /// over a structured view (`XTP_RAW_STRUCTURED` off) it is the very
    /// mismatch `XTP_RAW_STRUCTURED` removes (NVIDIA reads other dwords or
    /// zeros, watch.md items 148, 160), so only its count is checked there.
    raw_access: bool,
    /// Dword 1 of the output: the value loaded, or the one stored.
    value: u32,
    /// The buffer's dword 3 after the access, for unordered views.
    stored: Option<u32>,
}

/// Runs every case in every [`MODES`] on every host; the failures.
fn check(cases: &[Case]) -> Vec<String> {
    let (_one, hosts) = hosts();
    let mut failures = Vec::new();
    for case in cases {
        let native = program(case.access, case.kind, case.uav);
        for mode in MODES {
            let what = format!("{} ({mode:?} views)", case.name);
            let cs = match translate(&native, mode) {
                Ok(cs) => cs,
                Err(e) => {
                    failures.push(format!("{what}: not translated: {e}"));
                    continue;
                }
            };
            let values = !(case.raw_access
                && mode == Mode::Structured
                && matches!(case.kind, Kind::Structured(_)));
            for (host_name, host) in &hosts {
                let out = run(host, &cs, case.kind, case.uav, mode);
                let got = out.last().expect("the output");
                let count = case.kind.records();
                if got[0] != count {
                    failures.push(format!(
                        "{what} on {host_name}: record count {}, not {count}",
                        got[0]
                    ));
                }
                if values && got[1] != case.value {
                    failures.push(format!(
                        "{what} on {host_name}: value {:#x}, not {:#x}",
                        got[1], case.value
                    ));
                }
                if let Some(want) = case.stored.filter(|_| values)
                    && out[0][3] != want
                {
                    failures.push(format!(
                        "{what} on {host_name}: dword 3 {}, not {want}",
                        out[0][3]
                    ));
                }
            }
        }
    }
    failures
}

/// A [`Case`] as a row: name, access, kind, unordered view, raw access,
/// value, dword 3 after.
type Row = (
    &'static str,
    &'static [u32],
    Kind,
    bool,
    bool,
    u32,
    Option<u32>,
);

/// A buffer's record count read as data is the console's whatever the host
/// views and however the program accesses the buffer: 16 for 192 bytes of
/// 12-byte structures, by index, by byte offset, by a scalar load, through
/// a shader or an unordered view (stores and atomics); 192 bytes for a raw
/// view, 12 for an RGBA32F one. With `XTP_RAW_STRUCTURED=1` a structured
/// handle's size of the raw view the host makes is other numbers on
/// NVIDIA, a raw handle's size its bytes; by byte offset or a scalar load
/// the count was the bytes whatever the view, and a count read beside a
/// scalar load stopped the translation.
#[test]
fn record_counts_read_as_data_are_the_consoles() {
    let data = data();
    let s12 = Kind::Structured(12);
    #[rustfmt::skip]
    let rows: [Row; 11] = [
        ("indexed load", INDEXED_LOAD, s12, false, false, data[3], None),
        ("offset load", OFFSET_LOAD, s12, false, true, data[3], None),
        ("scalar load", SCALAR_LOAD, s12, false, true, data[3], None),
        ("raw offset load", OFFSET_LOAD, Kind::Raw, false, true, data[3], None),
        ("raw scalar load", SCALAR_LOAD, Kind::Raw, false, true, data[3], None),
        ("typed load", TYPED_LOAD, Kind::Typed, false, false, data[4], None),
        ("indexed store", INDEXED_STORE, s12, true, false, 42, Some(42)),
        ("offset store", OFFSET_STORE, s12, true, true, 42, Some(42)),
        ("indexed atomic", INDEXED_ATOMIC, s12, true, false, 42, Some(42)),
        ("scalar load of an unordered view", SCALAR_LOAD, s12, true, true, 0, None),
        ("raw offset store", OFFSET_STORE, Kind::Raw, true, true, 42, Some(42)),
    ];
    let cases: Vec<Case> = rows
        .into_iter()
        .map(
            |(name, access, kind, uav, raw_access, value, stored)| Case {
                name: name.to_owned(),
                access,
                kind,
                uav,
                raw_access,
                value,
                stored,
            },
        )
        .collect();
    let failures = check(&cases);
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A load by element reads `index * stride` (plus the offset) at every
/// stride, and the count is the elements: an RTX 5080 read dword 1 or 2 of
/// element 1 at strides 8 to 24 through a structured handle over a raw
/// view (VIctory#1762), so with `XTP_RAW_STRUCTURED=1` such loads go
/// through raw handles (or the table's raw views); without it, the
/// structured handle reads the structured view the title made.
#[test]
fn loads_by_element_read_index_times_stride_at_every_stride() {
    let data = data();
    let mut cases = Vec::new();
    for s in [4, 8, 12, 16, 24] {
        let at = (s / 4) as usize;
        let load = |name: String, access, value| Case {
            name,
            access,
            kind: Kind::Structured(s),
            uav: false,
            raw_access: false,
            value,
            stored: None,
        };
        cases.push(load(
            format!("element 1, stride {s}"),
            INDEXED_LOAD,
            data[at],
        ));
        // Byte 4 of the element (inside it from stride 8 on).
        if s >= 8 {
            cases.push(load(
                format!("byte 4 of element 1, stride {s}"),
                INDEXED_LOAD_AT_4,
                data[at + 1],
            ));
        }
    }
    let failures = check(&cases);
    assert!(failures.is_empty(), "{failures:#?}");
}
