//! Scalar buffer loads (`s_buffer_load_dword*`) through descriptors no
//! used-resource entry describes: a buffer the program picks from the
//! resource heap by an index it computed, as the title's meshlet culling
//! (3b471c122be79b3e) reads its records. The console reads the bytes the
//! descriptor names, whatever view the title made there; the translation
//! reads the slot through a raw handle (`lower/smem.rs`), a constant-buffer
//! handle only where the slot is known to hold a constant buffer.
//!
//! Two kinds of check:
//! - the kind of every resource-heap handle of a translation, from dxc's
//!   disassembly (the Windows SDK's `dxc.exe`, else not checked): one slot,
//!   one kind of view, whatever path the program reaches it by;
//! - on WARP, what the loads read: a raw and a structured view in a heap
//!   slot, at constant, computed and unaligned offsets, past the view's
//!   end (0, as the console reads past the record count).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::HostShader;
use super::host::{Dispatch, Host, Root, Stride, View};
use super::tests::{compute, emit_for};
use crate::native::{ComputeIo, ResourceType, RootParam, RootType, StageIo, UsedResource};

const S_ENDPGM: u32 = 0xBF81_0000;
/// `s_waitcnt lgkmcnt(0)`.
const WAIT_SCALAR: u32 = 0xBF8C_C07F;
/// Operand code of `null` (no register).
const NULL: u8 = 125;
/// Operand code of the inline constant `n` (0 to 64).
const fn inline(n: u8) -> u8 {
    0x80 + n
}

/// `s_mov_b32 s<d>, <src>` (an operand code).
fn s_mov(d: u8, src: u8) -> u32 {
    0xBE80_0300 | u32::from(d) << 16 | u32::from(src)
}

/// `s_lshl_b32 s<d>, s<s>, n`.
fn s_lshl(d: u8, s: u8, n: u8) -> u32 {
    0x8F00_0000 | u32::from(d) << 16 | u32::from(inline(n)) << 8 | u32::from(s)
}

/// `s_cmp_eq_u32 <a>, <b>` (operand codes).
fn s_cmp_eq_u32(a: u8, b: u8) -> u32 {
    0xBF06_0000 | u32::from(b) << 8 | u32::from(a)
}

/// `s_cselect_b64 s[d:d+1], s[a:a+1], s[b:b+1]`.
fn s_cselect_b64(d: u8, a: u8, b: u8) -> u32 {
    0x8580_0000 | u32::from(d) << 16 | u32::from(b) << 8 | u32::from(a)
}

/// SMEM `op` (0 to 4 `s_load_dword` to `_dwordx16`, 8 to 12
/// `s_buffer_load_dword` to `_dwordx16`) into s<data> through s[base:...]
/// at the byte offset `offset` plus `soffset` (an SGPR, or [`NULL`]).
fn smem(op: u32, data: u8, base: u8, offset: u32, soffset: u8) -> [u32; 2] {
    [
        0xF400_0000 | op << 18 | u32::from(data) << 6 | u32::from(base / 2),
        u32::from(soffset) << 25 | offset,
    ]
}

/// `v_mov_b32 v<v>, <src>` (an operand code).
fn v_mov(v: u8, src: u8) -> u32 {
    0x7E00_0200 | u32::from(v) << 17 | u32::from(src)
}

/// `buffer_store_dword v<data>, off, s[rsrc:rsrc+3], 0 offset:<offset>`.
fn store(data: u8, rsrc: u8, offset: u32) -> [u32; 2] {
    [
        0xE070_0000 | offset,
        0x8000_0000 | u32::from(rsrc / 4) << 16 | u32::from(data) << 8,
    ]
}

/// `ds_write_b32 v<addr>, v<data> offset:<offset>`.
fn ds_write(addr: u8, data: u8, offset: u16) -> [u32; 2] {
    [
        0xD834_0000 | u32::from(offset),
        u32::from(data) << 8 | u32::from(addr),
    ]
}

/// The program's instructions in SP3 syntax.
fn text(words: &[u32]) -> Vec<String> {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    crate::gfx10::decode(&code)
        .unwrap()
        .iter()
        .map(|i| i.text(false))
        .collect()
}

fn param(root: RootType, user_data: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: if root == RootType::Descriptor { 2 } else { 1 },
        base_register: 0,
        user_data,
        slot: user_data,
    }
}

fn entry(kind: ResourceType, table_offset_dwords: u32) -> UsedResource {
    UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 1,
        dwords: 4,
        user_data: 0,
        slot: 0,
    }
}

/// A wave32 compute program of 32 threads with these root parameters in
/// its user SGPRs, used-resource entries, and 512 bytes of LDS.
fn program(words: &[u32], layout: Vec<RootParam>, used: Vec<UsedResource>) -> crate::NativeProgram {
    let mut p = compute(words, false);
    let sgprs: u32 = layout.iter().map(|r| r.sgprs).sum();
    p.layout = layout;
    p.used = used;
    p.io = StageIo::Cs(ComputeIo {
        rsrc2: 1 << 15 | sgprs << 1,
        threads: [32, 1, 1],
    });
    p
}

/// The heap slot the root constant in s0 names (index times 32 bytes),
/// loaded into s[16:19] from the resource heap at s1.
fn heap_slot() -> Vec<u32> {
    let mut w = vec![s_lshl(12, 0, 5), s_mov(2, 1), s_mov(3, inline(0))];
    w.extend(smem(2, 16, 2, 0, 12)); // s_load_dwordx4 s[16:19], s[2:3], s12
    w.push(WAIT_SCALAR);
    w
}

/// Scalar loads of dwords 2 and 3 through s[16:19] into s[20:21], each
/// lane writing them to LDS bytes 0 and 4.
fn scalar_reads() -> Vec<u32> {
    let mut w = smem(9, 20, 16, 0x8, NULL).to_vec(); // s_buffer_load_dwordx2 s[20:21], s[16:19], 0x8
    w.extend([WAIT_SCALAR, v_mov(2, inline(0)), v_mov(1, 20)]);
    w.extend(ds_write(2, 1, 0));
    w.push(v_mov(1, 21));
    w.extend(ds_write(2, 1, 4));
    w
}

/// [`program`] with a root constant (s0) and the resource heap (s1), no
/// entry.
fn heap_program(words: &[u32]) -> crate::NativeProgram {
    program(
        words,
        vec![
            param(RootType::Constants, 0),
            param(RootType::ResourceHeap, 1),
        ],
        vec![],
    )
}

/// DXIL disassembly by the Windows SDK, when it is installed.
pub(super) fn disassembly(shader: &HostShader) -> Option<String> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dxc = match std::env::var_os("XTP_PC_DXC") {
        Some(p) => PathBuf::from(p),
        None => super::validator::Validator::default_path()
            .map(|p| p.with_file_name("dxc.exe"))
            .unwrap_or_default(),
    };
    if !dxc.exists() {
        eprintln!("no dxc.exe at {}; not disassembled", dxc.display());
        return None;
    }
    let file = std::env::temp_dir().join(format!(
        "xtp-smem-{}-{}.dxil",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&file, &shader.dxil).unwrap();
    let out = std::process::Command::new(&dxc)
        .arg("-dumpbin")
        .arg(&file)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8(out.stdout).unwrap())
}

/// The kind of view (`annotateHandle`'s first property: 11 a raw buffer,
/// 13 a constant buffer, with 0x1000 unordered access) of every handle
/// made from the resource heap in `shader`, in the disassembly's order;
/// None where the Windows SDK's `dxc.exe` is not there.
fn heap_kinds(shader: &HostShader) -> Option<Vec<u32>> {
    let listing = disassembly(shader)?;
    let mut heap = std::collections::HashSet::new();
    let mut kinds = Vec::new();
    for line in listing.lines() {
        let Some((name, call)) = line.trim().split_once(" = ") else {
            continue;
        };
        // createHandleFromHeap(i32 218, index, sampler heap, non-uniform)
        if let Some(args) = call.split_once("@dx.op.createHandleFromHeap(") {
            let args: Vec<&str> = args.1.split(", ").collect();
            if args.get(2) == Some(&"i1 false") {
                heap.insert(name.to_owned());
            }
            continue;
        }
        // annotateHandle(i32 216, %dx.types.Handle %h, { i32 kind, ... })
        let Some((_, args)) = call.split_once("@dx.op.annotateHandle(i32 216, %dx.types.Handle ")
        else {
            continue;
        };
        let (handle, props) = args.split_once(", ").unwrap();
        if heap.contains(handle) {
            let kind = props.split("{ i32 ").nth(1).unwrap();
            let kind = kind.split(',').next().unwrap().trim();
            kinds.push(kind.parse().unwrap());
        }
    }
    Some(kinds)
}

/// Each kind in `kinds` once, in order.
fn distinct(mut kinds: Vec<u32>) -> Vec<u32> {
    kinds.sort_unstable();
    kinds.dedup();
    kinds
}

/// The encodings these tests write, as the decoder reads them.
#[test]
fn the_test_encodings_read_back() {
    let mut words = heap_slot();
    words.extend(scalar_reads());
    words.extend(store(1, 16, 4));
    words.extend([s_cmp_eq_u32(0, inline(0)), s_cselect_b64(24, 16, 20)]);
    words.extend(smem(3, 16, 2, 0x20, 12));
    words.push(S_ENDPGM);
    assert_eq!(
        text(&words),
        [
            "s_lshl_b32 s12, s0, 5",
            "s_mov_b32 s2, s1",
            "s_mov_b32 s3, 0",
            "s_load_dwordx4 s[16:19], s[2:3], s12",
            "s_waitcnt lgkmcnt(0)",
            "s_buffer_load_dwordx2 s[20:21], s[16:19], 0x000008",
            "s_waitcnt lgkmcnt(0)",
            "v_mov_b32 v2, 0",
            "v_mov_b32 v1, s20",
            "ds_write_b32 v2, v1",
            "v_mov_b32 v1, s21",
            "ds_write_b32 v2, v1 offset:4",
            // (v0: the address VGPR, unused without offen or idxen.)
            "buffer_store_dword v1, v0, s[16:19], 0 offset:4",
            "s_cmp_eq_u32 s0, 0",
            "s_cselect_b64 s[24:25], s[16:17], s[20:21]",
            "s_load_dwordx8 s[16:23], s[2:3], s12 offset:0x000020",
            "s_endpgm",
        ]
    );
}

/// A heap slot the program reads only with scalar loads: a raw handle,
/// not the constant-buffer one it took before (watch item 147: the
/// meshlet culling's records read as nothing).
#[test]
fn a_heap_slot_read_only_by_scalar_loads_is_read_raw() {
    let mut words = heap_slot();
    words.extend(scalar_reads());
    words.push(S_ENDPGM);
    let cs = emit_for(&heap_program(&words), super::Target::ANY).unwrap();
    if let Some(kinds) = heap_kinds(&cs) {
        assert_eq!(distinct(kinds), [11]);
    }
}

/// The program also stores into the slot: the scalar loads take the
/// unordered-access handle the store takes.
#[test]
fn a_heap_slot_also_stored_to_is_read_through_its_unordered_view() {
    let mut words = heap_slot();
    words.extend(scalar_reads());
    words.extend(store(1, 16, 0x40));
    words.push(S_ENDPGM);
    let cs = emit_for(&heap_program(&words), super::Target::ANY).unwrap();
    if let Some(kinds) = heap_kinds(&cs) {
        assert_eq!(distinct(kinds), [11 | 0x1000]);
    }
}

/// The slot the scalar loads read is also reached another way, and stored
/// to there: through a choice between it and the next slot (SCC), through
/// a second load whose offset the program shifts again, through a load of
/// eight dwords from the same address. Each way the scalar loads take the
/// unordered-access view of the slot, not a second kind (they compared
/// only the same source, or a twin load of the same width and offset
/// value, before).
#[test]
fn a_slot_reached_another_way_has_one_kind_of_view() {
    let reads = scalar_reads();
    let tail = |w: &mut Vec<u32>, rsrc: u8| {
        w.extend(&reads);
        w.extend(store(1, rsrc, 0x40));
        w.push(S_ENDPGM);
    };
    // A choice: the slot or the next one, by whether s0 is 0.
    let mut chosen = heap_slot();
    chosen.extend(smem(2, 20, 2, 0x20, 12)); // the next slot into s[20:23]
    chosen.extend([
        WAIT_SCALAR,
        s_cmp_eq_u32(0, inline(0)),
        s_cselect_b64(24, 16, 20),
        s_cselect_b64(26, 18, 22),
    ]);
    tail(&mut chosen, 24);
    // The offset shifted again for a second load of the slot.
    let mut again = heap_slot();
    again.push(s_lshl(13, 0, 5));
    again.extend(smem(2, 24, 2, 0, 13)); // s_load_dwordx4 s[24:27], s[2:3], s13
    again.push(WAIT_SCALAR);
    tail(&mut again, 24);
    // Eight dwords from the slot's address (the scalar loads read the
    // first four), four for the store.
    let mut wide = vec![s_lshl(12, 0, 5), s_mov(2, 1), s_mov(3, inline(0))];
    wide.extend(smem(3, 16, 2, 0, 12)); // s_load_dwordx8 s[16:23], s[2:3], s12
    wide.extend(smem(2, 24, 2, 0, 12)); // s_load_dwordx4 s[24:27], s[2:3], s12
    wide.push(WAIT_SCALAR);
    tail(&mut wide, 24);
    for (what, words) in [("chosen", chosen), ("again", again), ("wide", wide)] {
        let cs = emit_for(&heap_program(&words), super::Target::ANY).unwrap();
        if let Some(kinds) = heap_kinds(&cs) {
            assert_eq!(distinct(kinds), [11 | 0x1000], "{what}");
        }
    }
}

/// Where the slot is known to hold a constant buffer, the scalar loads
/// read it as one, as before: a CBV entry of the table, a root descriptor.
/// An SRV entry is read raw.
#[test]
fn a_slot_known_to_hold_a_constant_buffer_is_read_as_one() {
    let mut table = vec![s_mov(5, inline(0)), s_mov(4, 0)];
    table.extend(smem(2, 16, 4, 0, NULL)); // s_load_dwordx4 s[16:19], s[4:5], null
    table.push(WAIT_SCALAR);
    table.extend(scalar_reads());
    table.push(S_ENDPGM);
    for (kind, want) in [(ResourceType::Cbv, 13), (ResourceType::Srv, 11)] {
        let p = program(
            &table,
            vec![param(RootType::Table, 0)],
            vec![entry(kind, 0)],
        );
        let cs = emit_for(&p, super::Target::ANY).unwrap();
        if let Some(kinds) = heap_kinds(&cs) {
            assert_eq!(distinct(kinds), [want], "{kind:?}");
        }
    }
    // A buffer descriptor built around the root descriptor at s[0:1].
    let mut built = vec![
        s_mov(16, 0),
        s_mov(17, 1),
        s_mov(18, 0xC1), // -1
        s_mov(19, 0xFF),
        0x1101_4FAC,
    ];
    built.extend(scalar_reads());
    built.push(S_ENDPGM);
    let p = program(&built, vec![param(RootType::Descriptor, 0)], vec![]);
    let cs = emit_for(&p, super::Target::ANY).unwrap();
    assert_eq!(cs.root_cbvs, [0]);
    assert!(cs.root_srvs.is_empty());
    if let Some(kinds) = heap_kinds(&cs) {
        assert!(kinds.is_empty(), "{kinds:?}");
    }
}

/// The meshlet culling's shape on WARP, in groups of 32: the table at s0
/// holds a raw buffer (entry 0: the heap index, then three byte offsets)
/// and the UAV the program writes (entry 8). The program loads the
/// descriptor at that index of the resource heap (s1) and reads it only
/// with scalar loads, every lane storing what they read at dword 0 on of
/// the UAV:
/// - dwords 0 to 3, the last (7), past the end (8);
/// - two dwords from the last, four from dword 6 (the end in between);
/// - the unaligned byte 0x16 (the console ignores the two low bits: dword
///   5);
/// - at the offsets read from the table: 8 (dword 2), 0x40 (past the end),
///   0x0e plus 4 (dword 4).
fn scalar_reads_of_a_heap_slot() -> crate::NativeProgram {
    let mut w = vec![s_mov(5, inline(0)), s_mov(4, 0)];
    w.extend(smem(2, 8, 4, 0, NULL)); // s_load_dwordx4 s[8:11], s[4:5], null
    w.extend(smem(2, 24, 4, 0x20, NULL)); // s_load_dwordx4 s[24:27], s[4:5], 0x20
    w.push(WAIT_SCALAR);
    w.extend(smem(10, 12, 8, 0, NULL)); // s_buffer_load_dwordx4 s[12:15], s[8:11], null
    w.extend([
        WAIT_SCALAR,
        s_lshl(12, 12, 5),
        s_mov(2, 1),
        s_mov(3, inline(0)),
    ]);
    w.extend(smem(2, 16, 2, 0, 12)); // s_load_dwordx4 s[16:19], s[2:3], s12
    w.push(WAIT_SCALAR);
    for (op, data, offset, soffset) in [
        (10, 32, 0, NULL), // s_buffer_load_dwordx4 s[32:35], s[16:19], null
        (8, 36, 0x1c, NULL),
        (8, 37, 0x20, NULL),
        (9, 38, 0x1c, NULL), // s[38:39]
        (8, 40, 0x16, NULL),
        (8, 41, 0, 13),
        (8, 42, 0, 14),
        (8, 43, 4, 15),
        (10, 44, 0x18, NULL), // s[44:47]
    ] {
        w.extend(smem(op, data, 16, offset, soffset));
    }
    w.push(WAIT_SCALAR);
    for k in 0..16u8 {
        w.push(v_mov(1, 32 + k));
        w.extend(store(1, 24, 4 * u32::from(k)));
    }
    w.push(S_ENDPGM);
    let mut p = program(
        &w,
        vec![param(RootType::Table, 0), param(RootType::ResourceHeap, 1)],
        vec![entry(ResourceType::Srv, 0), entry(ResourceType::Uav, 8)],
    );
    p.io = StageIo::Cs(ComputeIo {
        rsrc2: 2 << 1,
        threads: [32, 1, 1],
    });
    p
}

/// Runs [`scalar_reads_of_a_heap_slot`] on WARP with `slot` at heap index
/// 2: the 16 dwords it stores. None without a WARP device.
fn run_scalar_reads(slot: View) -> Option<Vec<u32>> {
    let cs = emit_for(&scalar_reads_of_a_heap_slot(), super::Target::ANY).unwrap();
    if let Some(kinds) = heap_kinds(&cs) {
        // The table's raw buffer and the slot raw, the table's UAV.
        assert_eq!(distinct(kinds), [11, 11 | 0x1000]);
    }
    // Kept on WARP: the structured-slot test below reads a structured
    // descriptor through a raw handle without the stride table (`stride_tests`
    // covers hardware).
    let host = Host::test_warp()?;
    let (root, pso) = host.compute_pipeline(&cs).unwrap();
    let table = [2u32, 8, 0x40, 0x0e];
    let views = [
        View::Buffer {
            data: &table,
            stride: Stride::Raw,
        },
        View::RawUav(16),
        slot,
    ];
    let user = vec![0; (cs.user_data_dwords() as usize).max(2)];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(user))],
                views: &views,
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap();
    Some(out[0].clone())
}

/// What [`scalar_reads_of_a_heap_slot`] stores for a slot of the 8 dwords
/// `d`: 0 past them.
fn console_reads(d: [u32; 8]) -> Vec<u32> {
    vec![
        d[0], d[1], d[2], d[3], d[7], 0, d[7], 0, d[5], d[2], 0, d[4], d[6], d[7], 0, 0,
    ]
}

/// A raw buffer view in the heap slot: every load reads the view's bytes,
/// 0 past its end. Through the constant-buffer handle of before, WARP
/// removed its device on this dispatch.
#[test]
fn scalar_loads_read_a_raw_view_chosen_from_the_heap() {
    let data: [u32; 8] = std::array::from_fn(|k| 100 + k as u32);
    let slot = View::Buffer {
        data: &data,
        stride: Stride::Raw,
    };
    if let Some(out) = run_scalar_reads(slot) {
        assert_eq!(out, console_reads(data));
    }
}

/// A structured view (two 16-byte elements) in the heap slot: the raw
/// handle reads its bytes as well on WARP, 0 past its end.
#[test]
fn scalar_loads_read_a_structured_view_chosen_from_the_heap() {
    let data: [u32; 8] = std::array::from_fn(|k| 200 + k as u32);
    let slot = View::Buffer {
        data: &data,
        stride: Stride::Structured(16),
    };
    if let Some(out) = run_scalar_reads(slot) {
        assert_eq!(out, console_reads(data));
    }
}

/// Self-sized scalar buffer loads (`Options::self_sized_loads_via_memory`)
/// redirect the load through a host-supplied raw view (`SELF_SIZED_RAW_SRV`)
/// instead of reusing the descriptor handle whose dimensions computed the
/// offset, returning 0 when the offset wraps past the end (e.g. `size - 8`
/// on a 4-byte buffer) while still applying the non-16-byte `bias`.
#[test]
fn self_sized_scalar_load_uses_separate_raw_view_and_zeros_past_end() {
    let s_sub_u32 = |d: u8, s: u8, imm: u8| {
        0x8080_0000 | u32::from(d) << 16 | u32::from(inline(imm)) << 8 | u32::from(s)
    };
    let words = [
        s_mov(4, 0),
        s_mov(5, inline(0)),
        smem(2, 8, 4, 0, NULL)[0],
        smem(2, 8, 4, 0, NULL)[1], // s_load_dwordx4 s[8:11], s[4:5], null (SRV)
        smem(2, 16, 4, 0x20, NULL)[0],
        smem(2, 16, 4, 0x20, NULL)[1], // s_load_dwordx4 s[16:19], s[4:5], 0x20 (UAV)
        WAIT_SCALAR,
        s_sub_u32(12, 10, 8), // s12 = size - 8 (wraps near 4 GiB when size == 4)
        smem(9, 20, 8, 0, 12)[0],
        smem(9, 20, 8, 0, 12)[1], // s_buffer_load_dwordx2 s[20:21], s[8:11], s12
        WAIT_SCALAR,
        v_mov(0, 20),
        store(0, 16, 0)[0],
        store(0, 16, 0)[1],
        v_mov(1, 21),
        store(1, 16, 4)[0],
        store(1, 16, 4)[1],
        S_ENDPGM,
    ];
    assert!(text(&words).iter().any(|l| l.starts_with("s_sub_u32 s12")));
    let mut p = program(
        &words,
        vec![param(RootType::Table, 0)],
        vec![entry(ResourceType::Srv, 0), entry(ResourceType::Uav, 8)],
    );
    p.io = StageIo::Cs(ComputeIo {
        rsrc2: 1 << 1,
        threads: [32, 1, 1],
    });

    let cs_default = emit_for(&p, super::Target::ANY).unwrap();
    assert!(
        !cs_default
            .user_data
            .iter()
            .any(|u| u.root == super::SELF_SIZED_RAW_SRV),
        "default translation must stay unchanged"
    );

    let cs_alt = super::with_options(
        super::Options {
            self_sized_loads_via_memory: true,
        },
        || emit_for(&p, super::Target::ANY).unwrap(),
    );
    let ud_entry = cs_alt
        .user_data
        .iter()
        .find(|u| u.root == super::SELF_SIZED_RAW_SRV)
        .expect("self-sized load must request SELF_SIZED_RAW_SRV");
    assert_eq!((ud_entry.slot, ud_entry.dword), (0, 0));
    assert!(
        cs_alt
            .user_data
            .iter()
            .any(|u| u.root == super::SELF_SIZED_VIEW_BYTES && (u.slot, u.dword) == (0, 0)),
        "self-sized load must also request SELF_SIZED_VIEW_BYTES"
    );

    // Also stays active when the runtime stride table is enabled (`stride_table: true`).
    let cs_table = super::with_options(
        super::Options {
            self_sized_loads_via_memory: true,
        },
        || {
            emit_for(
                &p,
                super::Target {
                    lanes_min: 32,
                    lanes_max: 64,
                    stride_table: true,
                },
            )
            .unwrap()
        },
    );
    assert!(
        cs_table
            .user_data
            .iter()
            .any(|u| u.root == super::SELF_SIZED_RAW_SRV)
    );

    // Negative control: a scalar load whose offset is independent of dword 2
    // does not trigger the self-sized path even when the option is set.
    let mut independent = words;
    independent[7] = s_mov(12, inline(0));
    let cs_indep = super::with_options(
        super::Options {
            self_sized_loads_via_memory: true,
        },
        || {
            let mut p_indep = program(
                &independent,
                vec![param(RootType::Table, 0)],
                vec![entry(ResourceType::Srv, 0), entry(ResourceType::Uav, 8)],
            );
            p_indep.io = StageIo::Cs(ComputeIo {
                rsrc2: 1 << 1,
                threads: [32, 1, 1],
            });
            emit_for(&p_indep, super::Target::ANY).unwrap()
        },
    );
    assert!(
        !cs_indep
            .user_data
            .iter()
            .any(|u| u.root == super::SELF_SIZED_RAW_SRV)
    );

    let _one = crate::emit::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(host) = Host::test_device() else {
        return;
    };
    let (root, pso) = host.compute_pipeline(&cs_alt).unwrap();
    let raw_backing = [0x9999_9999u32, 0x1111_2222, 0x3333_4444];
    let small = [0xdead_beefu32];
    for (data, mh_data, bias, expected) in [
        // Normal read of `size - 8` on an 8-byte view, backed at slot 2 with bias 4 bytes.
        (
            &[0x1111_2222u32, 0x3333_4444][..],
            &raw_backing[..],
            4u32,
            [0x1111_2222u32, 0x3333_4444],
        ),
        // Underflow on a 4-byte view: `size - 8` wraps to 0xFFFF_FFFC; `a + bias` wraps
        // to 0 if unguarded, so both the range check and select keep it 0.
        (&small[..], &raw_backing[..], 4u32, [0u32; 2]),
    ] {
        let mut ud = vec![0; cs_alt.user_data_dwords() as usize];
        let byte_len = std::mem::size_of_val(data) as u32;
        for u in &cs_alt.user_data {
            match u.root {
                super::SELF_SIZED_RAW_SRV => ud[u.index as usize] = (2 << 4) | bias,
                super::SELF_SIZED_VIEW_BYTES => ud[u.index as usize] = byte_len,
                _ => {}
            }
        }
        let views = [
            View::Buffer {
                data,
                stride: Stride::Raw,
            },
            View::RawUav(2),
            View::Buffer {
                data: mh_data,
                stride: Stride::Raw,
            },
        ];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        assert_eq!(out[0][..2], expected, "bias={bias}, len={}", data.len());
    }

    // Structured SRV with `XTP_RAW_STRUCTURED=0`: `s10` is element count (2),
    // `(s10 - 1) * 16` = byte offset 16, and `SELF_SIZED_VIEW_BYTES` = 32 bytes
    // (where `getDimensions(h).x` would be 2 elements and wrongly clamp `16 < 2`).
    let structured_words = [
        s_mov(4, 0),
        s_mov(5, inline(0)),
        smem(2, 8, 4, 0, NULL)[0],
        smem(2, 8, 4, 0, NULL)[1],
        smem(2, 16, 4, 0x20, NULL)[0],
        smem(2, 16, 4, 0x20, NULL)[1],
        WAIT_SCALAR,
        s_sub_u32(12, 10, 1),
        s_lshl(12, 12, 4),
        smem(8, 20, 8, 0, 12)[0],
        smem(8, 20, 8, 0, 12)[1],
        WAIT_SCALAR,
        v_mov(0, 20),
        store(0, 16, 0)[0],
        store(0, 16, 0)[1],
        S_ENDPGM,
    ];
    let mut p_struct = program(
        &structured_words,
        vec![param(RootType::Table, 0)],
        vec![
            UsedResource {
                stride: 16,
                ..entry(ResourceType::Srv, 0)
            },
            entry(ResourceType::Uav, 8),
        ],
    );
    p_struct.io = StageIo::Cs(ComputeIo {
        rsrc2: 1 << 1,
        threads: [32, 1, 1],
    });
    let cs_struct = super::with_options(
        super::Options {
            self_sized_loads_via_memory: true,
        },
        || {
            super::RAW_STRUCTURED_TEST.set(Some(false));
            let r = emit_for(&p_struct, super::Target::ANY);
            super::RAW_STRUCTURED_TEST.set(None);
            r.unwrap()
        },
    );
    let (s_root, s_pso) = host.compute_pipeline(&cs_struct).unwrap();
    let struct_data = [
        0x1111_2222u32,
        0x3333_4444,
        0x5555_6666,
        0x7777_8888,
        0xaaaa_bbbb,
        0xcccc_dddd,
        0xeeee_ffff,
        0x1234_5678,
    ];
    let struct_backing = [
        0x9999_9999u32,
        0x1111_2222,
        0x3333_4444,
        0x5555_6666,
        0x7777_8888,
        0xaaaa_bbbb,
        0xcccc_dddd,
        0xeeee_ffff,
        0x1234_5678,
    ];
    let mut ud = vec![0; cs_struct.user_data_dwords() as usize];
    for u in &cs_struct.user_data {
        match u.root {
            super::SELF_SIZED_RAW_SRV => ud[u.index as usize] = (2 << 4) | 4,
            super::SELF_SIZED_VIEW_BYTES => {
                ud[u.index as usize] = std::mem::size_of_val(&struct_data) as u32;
            }
            _ => {}
        }
    }
    let out = host
        .dispatch(
            &s_root,
            &s_pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[
                    View::Buffer {
                        data: &struct_data,
                        stride: Stride::Structured(16),
                    },
                    View::RawUav(1),
                    View::Buffer {
                        data: &struct_backing,
                        stride: Stride::Raw,
                    },
                ],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap();
    assert_eq!(out[0][0], 0xaaaa_bbbb);
}
