# Merged ES + GS programs: design and first lowering (W10)

Task W10 (5eaf5f2a), 2026-11-25, first phase: the design and the core
lowering in new files. The title is BTP 6, build 1.0.2.1; programs are
named by hash (the runtime's: the first 16 hex digits of the container's
BLAKE3). The records were read from the title's own shader pack (`shaders.rpf`,
read-only) into the session's scratch folder and disassembled with
`xtp shader-info --disasm`; nothing of them is in the repository, and no
translation loop ran on this PC.

## 1. Summary

- **What the 390 are.** 195 GS programs, each alone in a `GS_*` record, and
  195 ES programs, each beside the plain VS in a `VS_*` record (the vertex
  shader compiled for use with a GS). All 390 belong to one effect family,
  the voxelization pass (`VS_VoxelizeGeneric` / `GS_VoxelizeGeneric`, 13
  record names, 195 hashes each). The survey counts each half as "a merged
  ES + GS program (not attempted)" (refusals.md §2 row 2).
- **One shape.** Every GS takes a triangle, emits it again as a strip of
  three vertices (GS EMIT ×3, GS CUT, GS_DONE), on stream 0, with no
  instancing; it projects the triangle on its dominant axis. There are four
  code variants (153, 36, 5 and 1 programs) that differ in registers and in
  how many outputs they write. The ES halves come in three forms. 153 call
  the fetch shader and write a 32-byte item, 5 do the same with a 16-byte
  item, and 37 fetch their vertices themselves with buffer loads, branch
  on the vertex format, and write a 32-byte item. No instanced GS, no
  second stream, no point or line output, no adjacency and no stream-out
  occurs in the title (§3).
- **Decision: a D3D12 vertex shader for the ES, and a D3D12 geometry shader
  for the GS.** The LDS rings become signatures: the ESGS ring becomes the
  VS→GS signature, and the GSVS ring becomes the GS's own outputs
  (`emitStream` and `cutStream`). This replaces shader.md G2's mesh-shader
  plan (§4): the input assembler keeps doing what it already does for
  these vertex shaders, and the GS limits are far away (3 vertices × at
  most 20 scalars, against 1024).
- **Done in this phase** (`crates/xtp-shader/src/emit/lower/geometry.rs`,
  tests in `emit/geometry_tests.rs`):
  - Ring plans resolve every ring access of an ES or a GS from its
    address arithmetic. They number the emits, check the rules that make
    D3D12's emits equal to the console's copy shader, and list the address
    set-up the lowering must leave out. That set-up includes the GS's
    scalar load of its ring base from a console driver table, which the
    host does not have.
  - D3D12 hooks: ES ring write → `storeOutput`; GS ring read →
    `loadInput` of a vertex; GS writes kept until their vertex's
    `emitStream`; cut → `cutStream`.
  - The GS state and runtime information that DXIL and `PSV0` need, and
    D3D12's limits.
  - Tests on hand-assembled programs, straight and branched, and four draws
    on WARP whose GS output is streamed out and read back dword by dword.
- **Phase 2** (§6) wires this into `native.rs`, `io.rs`, `emit/shader.rs`,
  `emit/mod.rs`, `lower/mod.rs` and `lower/lds.rs`, the survey, and the host
  PSO. It needs the stage data fields listed in §3.4, and a way past the
  resource tracker's refusal of the ring-base load.
- **Phase 2, first piece** (§7.2): all 390 halves translate (195 ES as
  vertex shaders, 195 GS as `gs_6_6` geometry shaders) and `dxil.dll`
  accepts every one. No other program's translation changed. A host
  pipeline with a geometry shader runs the ES and the GS. Left: the
  survey's own change, the resource tracker's parameter, the frame-time
  cost on NVIDIA.

## 2. How GFX10 runs ES + GS (as these programs show it)

GFX9 and later merge the export shader and the geometry shader into one
hardware stage. The title's programs use the legacy GS (not NGG: no
`GS_ALLOC_REQ`, no `exp prim`) with both rings in LDS.

### 2.1 One hardware stage, two programs

- The ES program runs first in every wave. Its first instructions set EXEC
  to the ES lanes of the wave: `s_bfe_i32 s, s3, 0x80000` (s3 bits 0-7)
  and `s_bfm_b64 exec`. It ends with `s_barrier`, then `s_setpc_b64` to an
  address that arrived in s[6:7] (the ES saves it on entry and restores it
  before the jump). Each half is a program of its own in the pack: the GS
  code is not after the ES code.
- The GS program sets EXEC to its own lanes from s3 bits 8-15, one lane
  per primitive, and M0 to s3 bits 16-23 (the GS wave id that the GS
  messages need).
- System SGPRs come first and user data starts at s8 (`HwStage::user_sgpr`
  already says so). s2 is a per-wave offset that the GS adds to its ring
  base. s3 is the merged wave info: lane counts in bits 0-7 and 8-15, the
  wave id in 16-23, the wave's index in the group in 24-27.
- VGPRs: the GS inputs come first and the ES keeps them for the GS half.
  v0 holds the ESGS offsets of input vertices 0 and 1 in its 16-bit
  halves, and v1 those of vertex 2 (and 3). In the GFX9 layout v2 is the
  primitive id, v3 the invocation id and v4 vertices 4-5, but none of the
  title's GS reads them. The ES's vertex id is v5: the self-fetching ES
  programs multiply it by their stride. Its attributes land in the VGPRs
  the VS-layout stage data names (v9 and up where the plain VS has v4 and
  up).

### 2.2 The ESGS ring

Each ES lane writes its vertex's item into LDS at `(wave × 64 + lane) ×
item bytes`. The title's ES programs compute it as
`v_mad_u32_u24 v, lane, item, (s3[24:27] << 11)`, the lane being
`v_mbcnt_lo` then `v_mbcnt_hi` of all lanes. Item dword `4r + c` holds row
`r`, column `c` of the ES's output signature (`OSG1`): position in dwords
0-3, the next row in 4-7. The GS reads dword `d` of input vertex `k` at
`4 × offset_k + 4d`, the offset being in dwords. It computes that address
as `(v0 << 2) & 0x3fffc` for vertex 0, with an SDWA `WORD_1` select of
v0 for vertex 1, and from v1 for vertex 2.

### 2.3 GS invocations and the GSVS ring

One GS lane runs per input primitive. Vertex `n` that the lane makes goes
to LDS at `base + lane × item + n × vertex bytes + 4 (4r + c)`, row `r`,
column `c` of the GS's output signature. In the title the base is a dword
loaded from user data 0-1 (a driver table, offset 0xb8), shifted left by 8,
plus s2. The item is the most vertices times the vertex size, for example
3 × 80 bytes. With a 20-dword vertex, rows 0-4 are the position, a uint, a
float4, a float2 and a float3, exactly the `OSG1` rows.

### 2.4 Messages, vertex count, streams

- `s_sendmsg`, message 2 (GS) with op 2 is EMIT, op 1 is CUT and op 3 is
  EMIT_CUT; bits 8-9 carry the stream. Message 3 is GS_DONE. M0 must hold
  the GS wave id.
- The vertex an emit completes is the compiler's own count: the title's
  code writes vertex n's slots with constant offsets, then emits. The
  hardware drops emits past the GS's maximum (`VGT_GS_MAX_VERT_OUT`).
- After GS_DONE the console runs a copy shader, made by its driver and not
  in the pack, which reads each emitted vertex back from the ring and
  exports it. So what the console draws is the ring's content when the GS
  ends, in the order and strips the messages gave.
- Up to four streams, one rasterized. The title uses stream 0 only, so the
  ring layout of the other streams (where stream s's vertices start in the
  item) cannot be checked against any program.

## 3. The title's programs

### 3.1 Census

| | Records | Hashes | Programs |
|---|---:|---:|---|
| `GS_VoxelizeGeneric_*` | 195 | 195 | one GS each |
| `VS_VoxelizeGeneric_*` | 195 | 195 | a VS and an ES each |

The pairing is the title's PSO (a VS record and a GS record), not the record
name: 13 names cover 195 hashes on each side. With the design below each
half translates alone, so the translator never needs the pairing. The host
only has to take the ES when the PSO has a GS (§6).

### 3.2 Eight programs surveyed

Chosen to cover every code variant of the GS and every ES form.

| Hash | Half | Shape |
|---|---|---|
| `d82a0a5a22471c77` | GS (variant A, 153) | triangle in; 3 × EMIT, CUT, DONE; 20-dword vertices, 5 output rows; reads item dwords 0-2 and 4-5 |
| `03861c67cb9ceb8d` | GS (variant B, 36) | as A; a different user-data SGPR (s21) holds the axis constant |
| `57bc3001951308d7` | GS (variant D, 1) | as A, with s20 |
| `31d74f060dce8087` | GS (variant C, 5) | position-only input (`ISG1` 1 element), 4 output rows, 16-dword vertices, 48-dword item |
| `595056121e4b69d2` | ES, fetch shader (153) | `s_swappc_b64` to the fetch shader, position transformed, item 32 bytes (position, texcoord) |
| `4017ab4a141874cd` | ES, fetch shader (5) | item 16 bytes (position only), for variant C |
| `01b16737c7da6b94` | ES, self-fetch (37) | `s_setreg` flat scratch at entry; a 16_16 snorm or raw-dword texcoord chosen by a branch on the vertex format (`s_cbranch_scc1`); item 32 bytes |
| `12da3b5580f73ed4` | ES, self-fetch | the longest (101 instructions): `buffer_load_dwordx3` position, the same branch |

Every GS's `PSV0` says the same thing: input primitive triangle (3),
output topology triangle strip (5), stream mask 1, a position output, and
at most 3 vertices. A survey of all 195 GS programs found one message
sequence only.

### 3.3 Shapes the title does not have

No GS reads an invocation id or a primitive id (none is instanced), and
none emits on streams 1-3. There is no point or line output, no adjacency
input and no stream-out (the voxelization writes through the pixel
shader's UAVs). The first lowering therefore covers exactly the title's
shape and refuses the rest with a reason (§5.3), rather than guessing a
layout that no program can confirm.

### 3.4 Stage data found (for `native.rs`, G1)

Offsets are from the end of the record (E = record + 0x54), as in
`native.rs`'s notes.

- **ES**: the VS layout. Inputs at E+0x24, 5-byte entries, count at
  E+0xc4; their VGPRs are v9 and up where the VS variant has v4 and up.
  Outputs at E+0xc8, 6-byte entries, count at E+0x188: entry n is item row
  n (position first, usage 0). E+0x1ac holds 8 for a 32-byte item (4 in
  the plain VS): the item in dwords, to be confirmed on a 16-byte one.
- **GS**: outputs at E+0x148, 6-byte entries as the VS's (usage, `OSG1`
  row, parameter...), count at E+0x208. E+0x27c is the GSVS item in dwords
  (60, or 48 for variant C), E+0x284 the most vertices (3), E+0x28c the
  vertex in dwords (20, or 16). E+0x280 is 1 in every program: the instance
  count or the stream count, to be confirmed when a program has another
  value (none does). The record's LDS size (+0x4c) is 0: the console driver
  sizes the rings at link time.

## 4. The D3D12 target: vertex shader + geometry shader

### 4.1 The two options

1. **A DXIL vertex shader for the ES and a DXIL geometry shader for the
   GS.**
   - The input assembler fetches the vertices and assembles the
     primitives, as for every translated VS today: the fetch-shader ES
     programs get their attributes through `VsInput` VGPRs, and the
     self-fetching ones read their buffers as the VS variants already do.
   - The ESGS ring becomes the VS's outputs and the GS's inputs (the ES's
     `OSG1` and the GS's `ISG1`, which the console linked by semantic, as
     D3D12 does).
   - The GSVS ring becomes the GS's outputs. The copy shader disappears:
     D3D12 rasterizes the GS's stream 0 itself.
2. **A compute or mesh emulation**: the ES and then the GS in one
   mesh-shader group, the rings in group-shared memory (shader.md G2). The
   mesh shader has no input assembler, so the translation would need:
   - to read the index buffer itself (16 and 32 bits, primitive restart,
     base vertex, start index, instances);
   - to emulate the fetch shader's formats from the title's input layout,
     which only the host knows at PSO creation;
   - and the host would have to turn `DrawIndexedInstanced` and its
     indirect forms into `DispatchMesh`.
   The console's vertex reuse would also be lost or rebuilt.

### 4.2 Why option 1

- **Limits.** D3D12 allows a GS invocation 1024 output scalars (most
  vertices × scalars per vertex), 1024 vertices, 32 registers per input
  and per output vertex, and 32 instances. The title's GS outputs 3 × 14
  scalars (3 × 20 counting whole rows); its inputs use 2 rows. Nothing is
  near a limit; `GsShape::check` refuses what would be.
- **Stream-out.** D3D12's GS feeds stream-out directly (four streams, one
  rasterized, and every stream a point list once there are several). The
  title uses none. The tests of this phase use stream-out as their oracle
  (§5.4).
- **NVIDIA performance.** GS cost on NVIDIA comes from amplification and
  large `maxvertexcount × outputs` (occupancy). This GS amplifies nothing
  (one triangle in, one out, 3 vertices) and is the standard voxelization
  GS of PC engines; RDNA drivers compile a D3D12 GS as NGG. The cost is
  the GS stage on the voxelization draws only. It is to be measured in
  phase 2 (frame time with the pass on and off), not assumed.
- **What the translator keeps.** Both halves stay per-lane programs. No
  group-shared memory, no barrier, no cross-lane exchange: `v_mbcnt` only
  computes ring addresses, which the plans resolve and the lowering leaves
  out. On 32-lane hosts the wave64 halves need nothing more than the VS
  and PS programs need today.
- **What the mesh form would cost.** An input assembler in the shader and
  a host draw-path rewrite, both coupled to the title's input layouts and
  index buffers, for the same 390 programs and no gain.

The mesh emulation stays the fallback for a GS beyond D3D12's limits, and
none exists in the title. If it is ever built, the review note relayed for
this task applies to it: in a merged single program, the ES's indirect
jump must end its block and give the GS entry a predecessor, in both the
native and the split-wave forms.

## 5. The lowering (`emit/lower/geometry.rs`)

### 5.1 Ring addresses as bit fields

Every address is evaluated as `constant + Σ coefficient × field`, exact
modulo 2^32. A field is bits `lo..lo+width` of an input register, of a
scalar load's result (uniform), or of the lane (`v_mbcnt_lo` plus
`v_mbcnt_hi` of all lanes).

- Shifts and masks are exact where the terms do not share bits, which is
  how the 16-bit offset halves of v0 and v1 come out of
  `(v0 << 2) & 0x3fffc`, `v0 >> 16 << 2` or an SDWA word select.
- The rules cover `s_mov`, `s_add`, `s_sub`, `s_mul`, `s_and`, `s_lshl`,
  `s_lshr`, `s_bfe_u32`, `v_mov`, `v_add_nc`, `v_sub_nc`, `v_add3`,
  `v_lshlrev`, `v_lshrrev`, `v_lshl_add`, `v_and`, `v_mul_lo`,
  `v_mul_u32_u24`, `v_mad_u32_u24`, `v_bfe_u32`, `v_mbcnt` and the SDWA
  source selects.
- Anything else, a phi included, leaves the address unresolved, and the
  program is refused.

An address is one of two kinds:

- **An input vertex's item:** four times one vertex-offset half, plus a
  constant. That is a GS ring read.
- **This lane's item:** stride × lane, plus a part uniform across lanes
  that every access of the ring must share, plus a constant below the
  stride. That is an ES ring write, or a GS ring write (stride = the GSVS
  item).

### 5.2 Plans

- `es_plan`: each ES ring write's register dwords and their item dwords,
  and the item size.
- `gs_plan`: each ESGS read's register dwords and their input vertex and
  item dword, and each GSVS write's register dwords and their vertex and
  dword.
  - Emits are numbered by the emits before them, which must be the same
    on every path; a loop around an emit or a branch that emits on one
    side only is refused.
  - For each emit, the plan lists which write holds each dword of its
    vertex (`GsPlan::emits`).
- Both list the instructions whose every result only feeds ring addresses,
  or M0 for the messages (`address_only`): the lowering leaves them out.
  This matters for the GS's ring base: it is a scalar load from a console
  driver table, which the resource tracker refuses today ("an address in an
  SGPR that is no root parameter").

The rules that make D3D12's emits give what the copy shader reads:

- No write after its vertex's emit. The copy shader would read the later
  value; D3D12 has already taken the earlier one.
- On the paths to an emit, each dword of its vertex is written at most
  once, by an instruction that dominates the emit. So a vertex may be
  written and emitted on either side of a branch, but not written in a
  branch and emitted after the join (that would need a phi of ring values).
- At most the declared vertices; nothing after GS_DONE; stream 0 only.
- Every emit and cut under one lane mask (EXEC), the same value on every
  path to it. The IR places phis only where a register is live, and a
  message reads M0 alone, so after a join that nothing reads EXEC after,
  no phi joins the two sides' masks: the mask is then followed back on
  every path, and an emit whose paths bring two masks is refused (Codex
  review of phase 1's fixes; before, the first predecessor's mask was
  taken).

### 5.3 D3D12 hooks and refusals

- `write_item`: an ES ring write of item dword `d` is `storeOutput` of the
  ES's `OSG1` row `d / 4`, column `d % 4` (none for padding dwords).
- `read_vertex`: a GS ring read is `loadInput` of that row and column of
  the GS's `ISG1` for input vertex `k`, as a dword.
- `Vertices`: GS ring writes are kept by write and stored at their
  vertex's emit, then `emitStream(0)`. A cut is `cutStream(0)`, an
  EMIT_CUT both; GS_DONE emits nothing.
- `GsShape`: the shape from `PSV0` (input primitive, output topology, most
  vertices) and from the stage data (vertex and item dwords, instances).
  It gives DXIL's geometry state (entry property tag 1), the `PSV0`
  geometry runtime information, and D3D12's limits.

Refusal reasons, each its own text, survey-countable:

- an LDS access the rings do not explain;
- an ES item past 32 rows;
- a GSVS item of several streams; an emit or cut on another stream;
- a message a GS does not send;
- an emit whose vertex depends on the path; a message after GS_DONE;
- too many emits;
- an output written after its emit, twice, or on some paths only;
- a read of a ring dword the input signature does not hold;
- beyond 1024 vertices or 1024 scalars, 32 rows or 32 instances;
- a boolean element.

### 5.4 Tests (`emit/geometry_tests.rs`)

The programs are hand-assembled GFX10, written for these tests in the
title's form (no title code): the ES's item at its lane after its wave's
base, the GS's vertex offsets in halves of v0 and v1, its ring base loaded
then shifted and offset.

- **Plans**:
  - every read, write and emit of a 3-vertex GS resolved, and exactly its
    14 set-up instructions left out (the base's load among them);
  - the ES's item (48 bytes);
  - a branched ES, written for this task after the review note on
    indirect jumps: writes on both sides of a branch and after the join.
    Its `s_setpc_b64` ends the last block with no successor, the join has
    both sides as predecessors, and the plan takes every write;
  - a branched GS that writes and emits vertex 0 on both sides: each emit
    takes its own side's writes. Emitting on one side only is refused, as
    is writing on both sides and emitting after the join;
  - every refusal of §5.3, and D3D12's limits.
- **WARP draws**, with stream-out as the oracle and every dword of every
  vertex compared:
  - three emits, a cut and GS_DONE over two triangles: six vertices, in
    emit order, each from the input vertex it names;
  - a cut after two vertices, then three more: one triangle;
  - four vertices in one strip (`maxvertexcount` 4, the last an EMIT_CUT):
    two triangles, the second turned as the first;
  - the pair, both halves from console programs: the ES as the vertex
    shader, through `write_item`, then the GS.

A small walker stands in for the lowering in these tests: it calls the
hooks at the plan's sites, and lowers nothing but `v_mov_b32`. It panics on
any other instruction outside `address_only`, so the tests also prove the
set-up list complete for these programs.

Results: §7.

## 6. Phase 2: wiring, file by file

The plan as phase 1 wrote it. What its first piece did, item by item,
and what is left, is in §7.

1. **`native.rs` (G1):**
   - read the ES stage data (the VS layout, §3.4) into `StageIo::Es` with
     its inputs and its item rows;
   - read the GS stage data into `StageIo::Gs` (its outputs, vertex and
     item dwords, the E+0x280 field), with `GsShape`'s input primitive,
     topology and most vertices from the container's `PSV0`.
2. **`io.rs` (cxinputs holds it):**
   - ES inputs: the vertex id in v5, the instance id (v8 in the GFX9
     layout, to be confirmed), the attributes at their `VsInput` VGPRs,
     and the merged wave info in s3, whose ES lane count makes EXEC (as
     `MergedWaveInfo` does for mesh programs);
   - GS inputs: the offsets of `VERTEX_OFFSETS`, the primitive and
     invocation ids, s3's GS lane count and wave id.
   - Neither half has exports.
3. **`emit/shader.rs` (w6valid holds it):**
   - a geometry kind: `gs_6_6`, program kind 2, the entry's tag 1 from
     `GsShape::state`, and `PSV0` with `GsShape::runtime_info`;
   - `ISG1` the GS container's, `OSG1` with stream 0.
   The test frame `finish_gs` in `geometry_tests.rs` is the minimal form
   of it.
4. **`emit/lower/mod.rs` and `lower/lds.rs`** (one arm each, plus the
   file's line in `lower/mod.rs`'s list):
   - in ES and GS programs, the sites of the plan go to `geometry.rs`, and
     the `address_only` sites are skipped;
   - `s_sendmsg` GS goes to `Vertices`;
   - the ES's `s_barrier` is dropped, and its `Exit::Indirect` ends the
     vertex shader.
5. **`resource.rs`, or its caller in `emit/mod.rs`:** the scalar loads in
   `address_only` must not be tracked. The GS's ring base is the console
   driver's and the host has none. meshdata holds `resource.rs`; the entry
   point can pass the plan's sites to `resource::track` instead.
6. **`emit/mod.rs` (the entry point):** `translate_program_for` accepts
   `HwStage::Es`, translated as a vertex shader, and `HwStage::Gs`, as a
   geometry shader; `HostShader` gets the geometry stage.
7. **`apps/xtp/src/survey.rs`:** attempt both halves instead of "not
   attempted"; a survey before and after on the L4, none lost.
8. **`crates/xtp-gpu/src/pipeline/host.rs` (rects holds it):** a PSO with a
   GS takes the VS record's ES program as its VS, and the translated GS as
   its GS.

Then measure the voxelization pass's cost on NVIDIA (§4.2).

## 7. Results and checks

### 7.1 Phase 1

- `cargo-slot.ps1 test -p xtp-shader --locked geometry` (the task's check),
  2026-11-25:
  - 10 passed on the build farm (xtp-farm-2), and again on this PC with
    `--nocapture`, the farm being busy then;
  - no test was skipped: the WARP device and `dxil.dll` were there, and
    every geometry and vertex shader made validated;
  - a first farm run lost one GPU test to an access violation, two GPU
    tests running at once on WARP. The tests now take the crate's
    `ONE_AT_A_TIME` lock, as the other GPU tests do.
- `cargo-slot.ps1 clippy -p xtp-shader --all-targets --locked -- -D
  warnings`, and the same with `--all-features`: clean.
- The workspace check (`xtp-check.ps1`): run by Hangar on delivery.
- Survey: unchanged by this phase, since nothing is wired in yet. The
  pairs are still "not attempted" until phase 2's `survey.rs` and entry
  point.

### 7.2 Phase 2, first piece (task 5baa47be, 2026-11-25)

What §6 asked for, item by item:

1. **`native.rs`**: `StageIo::Es` (the VS layout and the item in dwords)
   and `StageIo::Gs` (outputs, item, the E+0x280 count, most vertices,
   vertex). A half whose stage data does not read keeps no I/O, so the
   plain VS beside an ES still translates. The translation refuses that
   half ("whose stage data the translation does not read").
2. **`io.rs`**: the GS half's system VGPRs v0 to v4 in both halves, the
   ES's vertex id in v5 and its attributes at the stage data's VGPRs. No
   export in either half. s3 is not raised:
   - `geometry::drop_lane_prologue` removes each half's EXEC set-up from
     s3 (`s_bfe_i32`, `s_bfm_b64 exec`, `s_cmp_eq_u32 64`,
     `s_cselect_b64 exec`). Its readers then take the entry's EXEC: every
     host invocation is one of the half's lanes.
   - Its other readers only compute ring addresses or M0.
   - `Io::geometry` carries the GS's shape from `PSV0`. `emit_for` sets
     it.
3. **`emit/shader.rs`**: the geometry kind, made by `Shader::finish` when
   `Shader::geometry` is set:
   - program kind 2, `gs` 6.6;
   - the entry's tag 1 from `GsShape::state`;
   - `PSV0` with `GsShape::runtime_info`;
   - the view-id state of four streams.

   The phase-1 WARP draws now go through it (their test frame is gone).
4. **`lower/mod.rs`** (ES/GS arms only). A field and three calls:
   `merged_start` after the inputs, `merged_inst` before the dispatch by
   encoding, `merged_end` before the block's exit. The hooks themselves
   are in `geometry.rs`:
   - set-up sites are skipped;
   - ring reads are `loadInput` under the read's lane guard;
   - ES ring writes are `storeOutput` under the write's guard;
   - GS writes are kept until their vertex's emit;
   - emits and cuts run under the lanes' EXEC where some lane outside it
     gets there;
   - the halves' `s_barrier` and the ES's `s_setpc_b64` are dropped, and
     the ES's indirect exit returns.

   `lds.rs` needs nothing: every LDS access of a half is a plan site.
   16-bit ring rows are refused.
5. **Resources**: a workaround, not the `resource.rs` change §6 asks
   for:
   - `geometry::tracked` gives `resource::track` a copy where the
     set-up's scalar loads are an `s_nop` with the same results. The GS's
     ring base is read from the console driver's table (user data 0-1,
     offset 0xb8 in the title), which no root parameter holds.
   - Sites and values stay the same, and the lowering never reads those
     loads.
   - A plan's refusal is reported at the resources' step.
   - A parameter of `resource::track` should replace it.
6. **`emit/mod.rs`**: `translate_for` takes a GS program. The new
   `translate_es_for` takes a vertex shader container's ES (the first
   program would be the ES, so `translate_for` keeps the plain VS).
   `translate_program_for` removes the lane set-up after `ir::build` and
   tracks the copy above.
7. **Survey**: `survey.rs` was another task's. The farm surveyed main
   (e6f17bb) and this branch with a one-line survey change that is not in
   the repository: ES and GS go to the translated arm. Both runs used
   `shader-survey --validate --host-lanes 32-32` on the same pack:

   | | before | after |
   |---|---:|---:|
   | translated | 25725 of 26698 (96.4 %) | 26115 (97.8 %) |
   | ES / GS | 0 / 0 of 195 / 195 | 195 / 195 |
   | `dxil.dll` accepts | 25654 | 26044 |
   | failed | 973 | 583 |

   - Every other program has the same outcome, the same output hash and
     the same validator verdict in both runs: none lost, none changed.
   - The 195 GS give 4 distinct translations, one per code variant of
     §3.2. The 195 ES give 11.
8. **Host PSO** (`pipeline/host.rs`): a geometry stage is taken, but not
   beside a mesh stage, nor with the rectangles' own geometry stage.
   - The vertex stage runs the vertex shader container's ES and the
     geometry stage the GS (`merged_half`).
   - Each half is translated once per container, with its own console
     program's resources. `pipeline.rs`'s `Shader::native` reads only VS,
     PS, MS and CS programs, so a GS container would bind no tables.
   - `Stage::translated` gives these translations, so draws bind each
     half's tables and user data.
   - `shader_cache::host_es_shader` keeps the ES under a key of its own.
9. **NVIDIA cost**: not measured yet (it needs a game session).

Also fixed, from the Codex review of phase 1's fixes: an emit's lane mask
after a join without a phi (§5.2).

Checks:

- `cargo-slot.ps1 test -p xtp-shader --locked` (merged with main
  e6f17bb): 391 passed, 37 ignored, none skipped. The WARP draws ran and
  `dxil.dll` validated. `geometry_pair_translates_and_streams_out` takes
  the pair through `translate_program` and compares every streamed-out
  dword. The join test fails on the previous `live_at`.
- `cargo-slot.ps1 test -p xtp-gpu --locked -- pipeline::host
  shader_cache`: 28 passed, among them
  `a_pipeline_with_a_geometry_shader_runs_the_export_shader` on WARP.
- `clippy -p xtp-shader` and `-p xtp-gpu`, `--all-targets -D warnings`:
  clean.

## 8. Open points

- E+0x280 of the GS stage data (1 everywhere): the instance or stream
  count.
- The ES instance id VGPR (v8 in the GFX9 layout): no title ES reads it.
- Streams 1-3 and their ring layout: none in the title.
- GS performance on NVIDIA: measure in phase 2.
- The survey's own change (ES and GS attempted) and a parameter of
  `resource::track` for the sites to leave out, in place of
  `geometry::tracked` (§7.2, items 5 and 7).
- `pipeline.rs`'s `Shader::native` reads no GS program: the host makes
  the GS's translation with its resources itself (`merged_half`).
- A validated translation is not yet a correct one on the title's own
  programs: the pair has run on WARP only as hand-assembled programs. The
  first game session with the voxelization pass checks the real ones
  (pipelines made, the pass's output).
