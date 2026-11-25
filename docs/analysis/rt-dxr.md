# Ray tracing on DXR: host acceleration structures and inline ray queries in place of the title's traversal loops

Task rtdxr (7ae2c553), 2026-11-25, phase 1 (the design). The title is
BTP 6, build 1.0.2.1, Series S mode. Static analysis only:
RVAs (image base 0x140000000) and behaviour in our own words, no code. Trace
programs are named by their 16-digit hash, as the logs print it; roles come
from rt-standin.md and refusals.md. Disassemblies, scans and notes stay
outside the repository, under `E:\xtp-data\tmp\rtdxr\` (section 12).

The person's decision (2026-11-25 ~10:10): the ray tracing runs on DXR
(RT cores), not on a software emulation of `image_bvh_intersect_ray`.
Their own AnyPS5 already runs RDNA2 traversal loops on host ray queries
(Vulkan); this design starts from it (section 2).

## 1. Summary

- **The starting point is AnyPS5's hardware ray tracing** (section 2).
  It recognises a guest program's traversal loop around each BVH intersection,
  runs a host ray query instead, and hands the code after the loop what
  the walk would have produced. Each result is classed by kind: hit,
  t, word, instance, u, v, node. The meaning of the loop's outputs is
  proven by playing the walk on synthetic worlds against the host answer.
  Its host side rebuilds BLAS/TLAS by walking the guest's own BVH.
  Provenance matters: most of that code came into AnyPS5 with the
  KytyPS5 import (GPL-2.0-only, third parties). Only the person's later
  passes and commits can be ported; the rest is ideas only (section 2.2).
- **XTP gets a cleaner host side than AnyPS5: the title's builder speaks
  DXR** (section 3). The title never builds its structures through
  command-list methods. It calls `XboxBvhBuilder.dll`, a C++ class whose
  7 imported methods take the standard D3D12 build descriptions:
  - triangle geometries with vertex and index buffer addresses, formats,
    counts, stride, an optional transform and an opaque flag;
  - TLAS builds from an instance array;
  - refits (`PERFORM_UPDATE`, source = destination), compaction, copies;
  - BLAS "rehydrated" from the `*_bvh4` packs, still described by their
    geometry.

  XTP can intercept those 7 methods and build host DXR structures from
  the source geometry. It does not need the title's builder, which is
  held today (shadps4.md §3.1), nor the walks over the guest BVH.
- **The 13 trace programs come in two families** (section 4):
  - **A**, six programs (local lights, sun shadows): 32-bit node pointers
    from a base, a top level walked by the whole wave, then the bottom
    level per lane;
  - **B**, seven programs (gathers, reflections, particles, irradiance
    shadow rays): 64-bit node addresses (`image_bvh64_intersect_ray`),
    walked per lane.

  Both keep a short stack in LDS, cap the walk with an iteration
  counter (0xfffe where read), test instance masks, cull through
  instance flags and handle triangle pairs. The gathers probably run alpha tests inside the
  loop (per-candidate memory reads and texture samples between
  intersections).
- **The prototype's program is `32ed072b9f845048`**, the local lights'
  tiled traced shadows (section 4.3). It is the smallest (3,520 bytes),
  has no alpha test and stores one value: the hit's t plus 0.04, or
  FLT_MAX. Its ray is a closest-hit query, opaque, instance mask 0xd5,
  cull per instance flags. `f607780aa8e502d0`, the irradiance cache's two
  shadow rays, reads only "hit or not" after its loops. It is the second
  candidate.
- **Hit mapping is the identity at the API level** (section 6). The host
  builds use the title's own inputs, in its order. So DXR's
  InstanceIndex, InstanceID, mask and flags, GeometryIndex and
  PrimitiveIndex are the ones the title's console builder recorded. What
  the code after a loop reads from guest node memory is either replaced
  by the matching `Committed*` call, or served by guest-format records
  that XTP writes itself. The second is AnyPS5's "HLE TLAS" idea, for
  instance records. A program whose results need a guest node pointer
  for later passes is the only case that needs the title's builder; none
  is known yet.
- **DXIL side** (section 7): DXR 1.1 inline ray queries (opcodes 178 to
  215, checked against DXC 10.0.26100), the shader flag for ray-tracing
  tier 1.1 (bit 25, `SFI0` 0x100000), and the TLAS as an SRV of kind
  16. It is bound as a root SRV at a reserved register, through the
  existing `root_srvs` path, with the host TLAS address set at dispatch.
- **Phase 2 needs files outside this task's owns** (section 10):
  - an HLE module for `XboxBvhBuilder.dll`;
  - one shader-flag line;
  - the module lines.

## 2. AnyPS5's hardware ray tracing (the person's design)

AnyPS5 (`simnJS/AnyPS5`, branch `AnyKyty`; read-only clone
`E:\xtp-refs\AnyPS5`, never built or run) emulates a PS5. Its GPU is
RDNA2 too: same node format, same intersection instruction. Its Kyty
backend (Vulkan, SPIR-V) runs the guest's traversal loops on host ray
queries (`VK_KHR_ray_query`).

### 2.1 What it does

**Shader side** (recompiler IR passes):

- **Recognition.** It finds the BVH intersections of a program and the
  loop around them, from the loop header to its latch.
  - It knows several walk shapes ("forms"): flat walks, walks over inline
    instance records, wave-wide TLAS walks over per-lane BLAS walks
    (handled by "candidate rounds"), lane-mask walks, spilled rays.
  - It reads where the TLAS header comes from: a user-data pointer, or a
    buffer descriptor held in user data (commit `c049aab1`).
- **Results.** The emitted host query returns guest-shaped results, each
  of a kind (`RayQueryResultKind`): `Has`, `T`, `Word`, `InstanceLo`,
  `InstanceHi`, `U`, `V`, `Node`. The loop's exit values become those
  results, and the code after the loop runs unchanged.
- **Proof by play** (`RayQueryResults.cpp`). Which exit value is which
  kind is not guessed. An IR machine plays the guest walk on random
  small worlds: planted instances with their masks and flags, and
  opaque or alpha triangles. It then keeps the hypotheses whose host
  answer gives the same outputs: hit test, closest hit, facing, opacity.
  The source and the instance layout of the TLAS are derived by
  symbolic evaluation (`RayQuerySource.cpp`).
- **Oracle** (`RayQueryOracle.cpp`). Walks that no form recognises keep
  their guest loop, but their intersections are answered from host ray
  queries. Lane-local walks are told apart from those that cross lanes.
  A walk that nothing serves runs with empty box nodes and is logged.
- **Alpha.** Any-hit walks over a wave-wide BLAS are served with the
  alpha test done by the host (commit `8ac5d2c4`).

**Host side** (`rayTracing.h`, `rayTracingSlots.cpp`, `rt_*_walk.comp`):

- At a trace dispatch, the guest TLAS header is read from the program's
  source (`TlasSource`). Compute shaders then walk the guest TLAS and
  each guest BLAS, through buffer device addresses into guest memory.
- Guest triangles become host BLAS "slots", with two geometries:
  opaque, and alpha-tested. An entry table maps each host primitive back
  to its guest triangle record: the entry, its word, its node. A
  per-instance table maps host instances to guest instance nodes.
- The host TLAS is built from the walked instances. Slots are refitted
  or rebuilt as the guest changes them, evicted after 1,800 frames
  unused, and held in a 64 MiB-chunk arena.
- Beside the walks, "HLE TLAS" and "HLE refit" paths follow the PS5's
  own BVH library: its TLAS header is written from the build inputs, and
  its BLAS builds and decompressions are followed (commit `b36cef57`).

**Checks** (`rayTracingCheck.cpp`): oracle dispatches are replayed and
compared, and incomplete host TLASes are set aside as transient.

### 2.2 Provenance

Checked per file with GitHub's history (`gh api …/commits?path=`),
because the local clone is shallow:

| Files | First version | Later authors | In XTP |
|---|---|---|---|
| `RayQueryTraversal.cpp/.h`, `ir/RayQuery.h`, `spirvEmitterRayQuery.cpp`, `spirvEmitterBvh.cpp`, `spirvEmitterTrinityBvh.cpp`, `rayTracing.h/.cpp`, `rayTracingSlots.cpp`, `rt_blas_walk.comp`, `rt_tlas_walk.comp`, `rt_common.inc` | the KytyPS5 import `e6570df9` (2026-11-17). Already substantial then: `RayQueryTraversal.cpp` 232 KB, `rayTracingSlots.cpp` 90 KB. Upstream: KytyPS5/KytyPS5 (Nmzik, Senaxx) and the fork simnJS/KytyPS5-GTA (TheCruZ) | simnJS: about 30 commits on `RayQueryTraversal.cpp` (18–19 Nov). mjnhbg3: one commit on `rayTracingSlots.cpp` (`a6e89640`) | **Ideas only** (GPL-2.0-only, third parties). Only the person's own lines in their commits are portable (anyps5.md) |
| `RayQueryResults.cpp/.h`, `RayQuerySource.cpp/.h`, `RayQueryOracle.cpp/.h`, `spirvEmitterRayQueryShared.h` | added after the import by simnJS | simnJS only | **The person's**: portable with "Ported from simnJS/AnyPS5 (commit) by simnJS" |
| `rayTracingCheck.cpp/.h`, `rayTracingReplay.h` | added by simnJS | simnJS, plus one commit by mjnhbg3 | the person's lines portable, mjnhbg3's ideas only |

XTP is Rust and DXIL, so nothing is copied verbatim anyway. What XTP
takes from the person's passes is their method: result kinds, proof by
play, symbolic source derivation and the oracle fallback. Each port is
credited by commit.

### 2.3 What transposes and what does not

| AnyPS5 (Vulkan, SPIR-V) | XTP (D3D12, DXIL) |
|---|---|
| `OpRayQueryInitializeKHR` / `Proceed` / `GetIntersection*` with a committed-or-candidate operand | DXR 1.1 `TraceRayInline` / `Proceed` / `Candidate*` / `Committed*` (one op per side); same semantics |
| Instance custom index, SBT offset | `InstanceID`, `InstanceContributionToHitGroupIndex` |
| The TLAS taken from a 64-bit address inside the shader (`OpConvertUToAccelerationStructureKHR`) | **Not possible**: a TLAS is a bound resource. XTP reads the title's TLAS address on the CPU at dispatch (from the program's descriptor or user data), finds the host TLAS and binds it as a root SRV |
| Walk shaders that read guest memory through buffer device addresses (`GL_EXT_buffer_reference`) | **No raw pointers in DXIL**. A walk would need XTP's page directory (shadps4.md §3.2), which does not exist yet. XTP builds from the builder's API instead (section 3), so it needs no guest walk |
| Host slots indexed through device-address tables (`slots`, `nodes`) | Structured-buffer tables bound beside the TLAS (root SRVs) |
| The recompiler's own SSA IR and passes | XTP's IR (`ir.rs`: SSA over machine instructions) with a new pass before the resource tracker (section 5.4) |
| Wave64 and wave32, compute and pixel walks | BTP 6's 13 walks are wave32 compute (the family A top level uses `readfirstlane`/`readlane` across 32 lanes) |
| PS5's BVH library HLE ("Base148", "Base460/252" headers) | The Xbox library has a DXR interface: no guest format is needed to build (section 3) |

## 3. Geometry: what the title feeds its builder

### 3.1 A builder library with a DXR interface

The title ships `XboxBvhBuilder.dll`. Its 12 exports are methods of a C++
class `XboxBvhBuilder` with standard D3D12 types in their signatures. The
title imports 7 of them:

| Method | What it takes | Title call sites (function RVA) |
|---|---|---|
| constructor | `ID3D12Device11*`, a flag | `0x2478440` |
| destructor | | `0x248b570` |
| `BuildRaytracingAccelerationStructureBatched2X` | `ID3D12GraphicsCommandList6*`, a count, an array of Xbox batch entries (`D3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2`) | `0x0fe4e50` (4 calls), `0x2490110` (4 calls) |
| `RehydrateRaytracingAccelerationStructureBatched2X` | `ID3D12GraphicsCommandList8*`, a count, standard `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC`s, an array of 64-bit CPU addresses | `0x0ea6c60` (2), `0x2490a20` (2) |
| `CopyRaytracingAccelerationStructure` | list, destination, source, copy mode | `0x0bd96a0`, `0x24908f0` |
| `GetRaytracingAccelerationStructurePrebuildInfo` | standard inputs, an Xbox stream description, the prebuild info out | `0x0075480`, `0x4ee2a00` (BLAS), `0x56ac100` (TLAS) |
| `Set64BitMortonCodes` | a bool | around the builds and prebuild queries |

The builder object is a global: its pointer is at RVA `0x75df0a8`. Each
wrapper exists twice with the same shape (`0x0fe4e50`/`0x2490110`,
`0x0ea6c60`/`0x2490a20`, `0x0bd96a0`/`0x24908f0`). Which one a frame runs
has not been measured.

### 3.2 What the builds pass

- **Batched builds** (`0x0fe4e50`), at most 16 entries a call. Each entry
  is 80 bytes:
  - a standard 48-byte build description:
    - the destination;
    - the inputs: type, flags, count, layout, descriptions;
    - the source, which is the destination when the entry is a refit
      and 0 otherwise;
    - the scratch, carved from one scratch range at 256-byte steps;
  - at +0x30, a count of postbuild descriptions (0 or 1);
  - at +0x38, a pointer to them;
  - 16 bytes of zero.

  The inputs come from a per-structure object of the title:
  - at +0x00, its prebuild info (result size, scratch size, refit scratch
    size);
  - at +0x28, the standard inputs.

  A per-entry byte says whether to refit. A refit ORs `PERFORM_UPDATE`
  (0x20) into the flags and uses the refit scratch size. A new build of
  a structure that allows compaction (flag 2) asks for its compacted size
  as postbuild info, into a readback ring. After each batch, the title
  records a global UAV barrier on the list (`ResourceBarrier`, one UAV
  barrier with no resource).
- **Rehydration** (`0x0ea6c60`) also batches at most 16. Each entry is a
  standard 48-byte description:
  - the destination;
  - the inputs, from the same per-structure object, geometry
    descriptions included;
  - as source, the dehydrated data that streams from the
    `pack/*_bvh4` archives (`core_bvh4.rpf` is 8.3 GB);
  - no scratch.

  The fourth argument is the array of CPU addresses of the same data,
  after the title's own GPU-to-CPU window. An entry is passed only if a
  bit in the dehydrated header is set. The geometry inputs are complete,
  so a host build from them gives the same structure that rehydration
  gives.
- **TLAS prebuild** (`0x56ac100`): type `TOP_LEVEL`, the instance count
  from the low 26 bits of a word of the title. Two bits of the same word
  choose the flags:
  - 0: `PREFER_FAST_BUILD`;
  - 2: `PREFER_FAST_TRACE`;
  - 1 and 3: none.

  The call is guarded by a lock of the title.
- **BLAS prebuild** (`0x4ee2a00`): type `BOTTOM_LEVEL`. Flags come from a
  table of the title, picked by two bits of the same kind of word, ORed
  with two more bits. Each geometry description is the standard 56-byte
  triangle one, filled from a 48-byte mesh record of the title:
  - type: triangles;
  - flags: bit 1 of the record's first word, so `OPAQUE` or none;
  - transform: present when the record holds one; the prebuild passes a
    non-null placeholder;
  - index format: a byte of the record, with `R16_TYPELESS` turned into
    `R16_UINT`;
  - vertex format: another byte of the record;
  - index count and vertex count: two words of the record;
  - index buffer: an address in the record;
  - vertex buffer: an address plus a byte offset, both in the record;
  - stride: a 16-bit field of the record.

**Not measured yet:** which vertex and index formats actually occur,
geometry and instance counts, and how many builds and refits a frame
makes. They are runtime data. The first step of phase 2 logs them from
the intercept (section 10).

### 3.3 Instances, masks, flags

TLAS builds take a standard 64-byte instance array:

- a 3×4 transform;
- `InstanceID:24 | Mask:8`;
- `InstanceContributionToHitGroupIndex:24 | Flags:8`;
- the BLAS address.

The traces read the console's copy of these fields from instance
records in the guest TLAS (section 4.2):

- the descriptor words with ID and mask, flags and the BLAS address;
- the world-to-object 3×4 matrix at +64 in a 128-byte record.

Who writes the instance array has not been read yet. The GPU list L26
("instance fix-ups", refusals.md) suggests compute shaders write it.
If so, the instances must be remapped on the GPU, not read on the CPU
(section 5.2).

### 3.4 Build, refit, compaction per frame

The wrappers show the pattern:

- dynamic BLAS are refitted in place (per-entry update byte);
- new BLAS are built with compaction and compacted through
  `CopyRaytracingAccelerationStructure`;
- static BLAS arrive by rehydration as the world streams;
- the TLAS is rebuilt.

The builder's compute dispatches dropped today ("BVH builder held",
85 to 350 a frame in the run log of 20261125-081233) are the console
library carrying this out.

### 3.5 Where XTP intercepts, and whether the title's builder still runs

- **Intercept the 7 methods** with an HLE module for
  `XboxBvhBuilder.dll`, as `d3d12x.rs` does for `d3d12_xs.dll`. The
  calls carry the title's list, so each host build is recorded on that
  list's host list, in order with the title's other commands. The
  title's own UAV barrier follows each batch.
- **Prebuild sizes**: answer the title with at least what the console
  library asks, by forwarding to the real DLL (a CPU function). The
  title then allocates what it expects. Host sizes are kept apart, in
  host buffers.
- **The title's builder is not run.** Once the traces are served by host
  ray queries, nothing reads the console BVH except code after the loops
  that reads guest records. That case is handled in section 6.
  - Today the builder is held anyway.
  - Not running it saves its 85 to 350 dispatches a frame.
  - It also spares cxr4 the last 22 refusals of its 52 programs.

  The exception is a program whose results need guest node pointers
  (section 6). None is known; one would bring back the console builder
  and AnyPS5's walk-based mapping.

## 4. The trace programs

### 4.1 Two families

Read from the 13 programs' disassemblies. Counts are per program;
"intersections" are the program's BVH intersection instructions.

| Program | Role (rt-standin.md) | Family | Intersections | Top level | Back edges | Iteration cap | Instance mask constant | Texture reads among the intersections | Stores |
|---|---|---|---:|---|---:|---|---|---|---:|
| `32ed072b9f845048` | local lights, tiled traced shadows | A | 2 | wave-wide | 6 | 0xfffe | 0xd5 | none | 1 |
| `559d9f0184e15fe3` | local lights, shadow mask (checkerboard) | A | 2 | wave-wide | 4 | 0xfffe | 0x10 | none | 11 |
| `a1b20bc47e3c9597` | sun shadows past the cascades | A | 8 | wave-wide | 16 | 0xffff? | 0xa0 | 6 | 2 |
| `e8c962358d862e3f` | sun shadows | A | 10 | wave-wide | 20 | 0xfffe | 0xa0 | 6 | 2 |
| `17829ac71aab4e8d` | sun shadows | A | 4 | wave-wide | 12 | 0xffff? | 0xa0 | 1 | 4 |
| `afeea48bbb9913ad` | sun shadows | A | 6 | wave-wide | 18 | 0xfffe | 0xa0 | 1 | 4 |
| `f607780aa8e502d0` | irradiance cache, shadow rays | B | 2 | per lane | 2 | 0xffff (a scalar counter) | not read | none | 1 |
| `caf96a3229145fa6` | particles colliding with the structure | B | 1 | per lane | 35 | not read | not read | none | 14 |
| `f22a76fcb967ee59` | screen probes, rays | B | 5 | per lane | 11 | not read | not read | none | 11 |
| `3592426e3e9142d8` | probes, primary gather | B | 8 | per lane | 48 | 0xfffe | not read | 4 | 33 |
| `b58eb3ca6ee46427` | irradiance cache, gather | B | 8 | per lane | 47 | 0xfffe | not read | 4 | 25 |
| `36f04a4ed0ad1d9b` | transparent surfaces' probes | B | 4 | per lane | 30 | 0xfffe | not read | 4 | 13 |
| `6d7fe7745ab9c6d8` | reflections' visibility buffer | B | 7 | per lane | 86 | not read | not read | 10 | 16 |

Several intersections in one program are several loops: two rays, or the
two levels of family A. A cap with "?" is a compare against that constant
whose role was not read. The mask constant was read in `32ed072b9f845048`.
For the other family A programs it is the constant in the same instance
test. "Texture reads among the intersections" covers everything between
the first and the last intersection: it shows probable alpha tests in
the gathers, but it also counts ray set-up between two loops (sun
shadows). cxr4's note on task d956084e matches the families: family A
starts from a scalar read of the structure's descriptor, family B makes
7 to 9 global loads before its first intersection.

### 4.2 The loop's shape

- **Entry.**
  - **Family A** reads the structure's address from a buffer descriptor
    in a descriptor table (`32ed072b9f845048`: the second descriptor of
    its user-data 5 table), and the root node pointer from the
    structure's header at +0x30. An empty structure (root ≤ 0) skips the
    walk.
  - **Family B** builds 64-bit node addresses from a base and an offset.
- **Stack.** A short stack:
  - **Family A**: the wave-wide top level keeps up to eight entries in
    lanes of one VGPR, then spills to LDS (at most 256);
  - **per lane** (family A's bottom level, all of family B): in LDS, the
    32 lanes interleaved, so one lane's entries are 128 bytes apart
    (from offset 1,024 in `32ed072b9f845048`).

  Box results are four child pointers sorted by distance. Invalid ones
  (all ones) are dropped; the rest are pushed far to near, and the
  nearest is taken next.
- **Instances** (node type 6). The record's descriptor words give:
  - the instance mask, tested against the program's mask (0xd5 in
    `32ed072b9f845048`);
  - the flags: culling and facing;
  - the BLAS address.

  The ray is moved into object space with the record's world-to-object
  matrix. A sign bit selects a translation-only fast path. Family A then
  walks the BLAS per lane (all lanes of the wave share the instance).
- **Triangles** (node types 0 to 3). The hit's t is the numerator over
  the denominator. A hit is accepted when:
  - t is at least 0 and below the current t-max;
  - the facing passes: the sign of the denominator times a factor taken
    from the instance flags.

  An accepted hit lowers t-max (closest hit) and remembers the node. A
  flag in the node's word at +0x30 says whether a second triangle shares
  the node, and the walk visits it next. Procedural (type 7) nodes are
  skipped.
- **Termination.** The stack is empty, or the iteration cap is passed
  (the loop counter against 0xfffe). In `32ed072b9f845048`, passing the
  cap counts as a hit at the current t-max.
- **Any-hit.** No alpha test in `32ed072b9f845048`, `559d9f0184e15fe3`,
  `f607780aa8e502d0`, `caf96a3229145fa6` and `f22a76fcb967ee59`. The
  gathers (`3592426e3e9142d8`, `b58eb3ca6ee46427`, `36f04a4ed0ad1d9b`)
  and the reflections make per-candidate memory reads (geometry data) and
  texture samples inside their loops. Those are probable alpha tests, to
  be confirmed by the results analysis (section 5.3).

### 4.3 The prototype's program: `32ed072b9f845048`

- **Threads.** One thread per ray. A per-tile bit mask (a typed buffer)
  says which threads trace. The others write FLT_MAX.
- **Ray.** A structured buffer of 32-byte records:
  - origin: three floats;
  - t-max: a half float;
  - direction: three half floats, normalised by the program.

  t-min is 0.
- **Query.** Closest hit, no alpha test (all triangles opaque), instance
  mask 0xd5, triangle culling per instance flags, procedural nodes
  skipped.
- **After the loop.** It reads two things: whether a hit was recorded
  (or the cap passed), and the hit's t. It stores t + 0.04 into a UAV,
  or FLT_MAX for a miss, one 32-bit value per thread.
- **In DXR terms:**
  `TraceRayInline(tlas, flags, 0xd5, {origin, 0, direction, t-max})`,
  then `Proceed` until it returns false. No candidate needs a commit:
  the geometry is opaque, or forced so with `RAY_FLAG_FORCE_OPAQUE`.
  The value is `CommittedRayT() + 0.04` on a committed triangle hit,
  FLT_MAX otherwise. The ray's cull flag is still to settle (section 9):
  the walk culls per instance flags, which means the ray itself asks for
  culling.

### 4.4 `f607780aa8e502d0`

Two family B loops (two shadow rays per thread). After each, the only
read is whether a hit was recorded, which gates the light contribution
that follows. One 16-byte typed store at the end.

## 5. Recognising the walk and replacing it (AnyPS5's method on XTP's IR)

### 5.1 Where it runs

`translate_program_for` runs:

1. `ir::build`;
2. `lower::geometry::prepared`, a pass that rewrites the IR;
3. the structurisation;
4. the resource tracker;
5. emission.

A pass `lower::rayquery::prepared` goes right after the geometry one. It
replaces each recognised walk by one synthetic instruction, so the
resource tracker never sees the node reads that refuse today ("an address
not from a root parameter"). The instruction reads the ray's values at
the loop entry and writes the loop's exit values, and the exit block's
phis are rewired to it. The lowering emits the ray query for it.

### 5.2 Gate: the title's hashes, then a proof

Title-specific shortcuts are allowed, so the pass first runs only on the
13 hashes above. Each recognised walk must also pass the proof of
section 5.3, or it is refused as today.

The minimum for each walk is a description of its site. Recognition
derives it; for a program where recognition fails, a hand-written one
can stand in:

- the loop header and the exit block, by offset;
- the registers that hold origin, direction, t-min and t-max at entry;
- the mask and the flags;
- which exit register is which result kind.

Either way it is checked by the proof.

### 5.3 Results and proof (ported from the person's `RayQueryResults`, `RayQuerySource`)

- **Result kinds**, as AnyPS5's: hit, t, word, instance (low, high), u,
  v, node.
- **Play.** XTP plays the walk's IR on synthetic worlds:
  - an interpreter for the instructions of the walk;
  - an RDNA2 intersection model, at translation time on the CPU only,
    never in a shader;
  - a few instances with planted masks and flags, opaque and alpha
    triangles.

  The proof keeps the mapping when the walk's exit values equal the
  host-query hypothesis for every world, and refuses the walk otherwise.
- **Sources.** The TLAS source (which descriptor or user-data dword holds
  it) and the instance record layout come from symbolic evaluation, as
  in `RayQuerySource`. The host binds the TLAS from that source at
  dispatch.
- **Alpha tests.** When a walk's any-hit part reads candidate data (the
  gathers), XTP keeps that code as the body of the `Proceed` loop. Its
  node reads are fed from `Candidate*` values, and the candidate is
  committed when the title's own test passes. AnyPS5 instead moves the
  alpha test to the host (`8ac5d2c4`): ideas for later.
- **Oracle.** For a walk that no form recognises, AnyPS5's oracle keeps
  the guest loop. It needs the guest BVH in memory, which XTP does not
  build. So in XTP an unrecognised walk stays refused, and the program
  is dropped as today. The software emulation (task 2cad4c80) remains
  the last resort, and the person is told if it is ever needed.

### 5.4 What the lowering emits

- One `AllocateRayQuery` per site, with constant flags: `FORCE_OPAQUE`
  when no alpha test was found.
- `TraceRayInline(tlas, flags, mask, ray)` from the entry values. The
  mask and flags come from the proof.
- A `Proceed` loop: empty, or with the kept any-hit body and
  `CommitNonOpaqueTriangleHit`.
- The exit values, by kind:

  | Kind | Value |
  |---|---|
  | hit | `CommittedStatus() == TRIANGLE_HIT` |
  | t | `CommittedRayT()` |
  | u, v | `CommittedTriangleBarycentrics()` |
  | instance | `CommittedInstanceIndex()` / `InstanceID()` (section 6) |
  | word | from `CommittedGeometryIndex()` / `PrimitiveIndex()` |

The wave-wide top level of family A disappears with the loop. So the
spliced program may no longer need a fixed wave size, if nothing else in
it reads lanes.

## 6. Hit mapping

- **Instances.**
  - `CommittedInstanceIndex()` is the index in the title's instance
    array.
  - `InstanceID()`, the mask, the flags and
    `InstanceContributionToHitGroupIndex()` are the title's own 24- and
    8-bit fields, since the host TLAS is built from its array.
  - The guest instance record a walk reads (descriptor words,
    world-to-object matrix) is `CommittedObjectToWorld3x4()` /
    `WorldToObject3x4()`, or the title's array entry at the instance
    index.
- **Geometry and primitive.** The host BLAS is built from the title's
  geometry descriptions, in its order. So `CommittedGeometryIndex()` and
  `CommittedPrimitiveIndex()` are DXR's indices, which the console
  builder also stores (DXR requires both). A "word" kind read from a
  guest triangle record is rebuilt from them. Its bit layout is read
  from the program's own decoding of it, during the proof.
- **Barycentrics.** DXR's (u, v) are relative to the triangle's vertices
  in index-buffer order. The console's triangle pairs may rotate vertex
  order within a node, and the walk's exit code corrects for that. The
  site's exit is taken after that correction, so the replaced value is
  DXR's.
- **t.** `CommittedRayT()` is in world space, the same as the guest's
  (the guest moves the ray into object space without renormalising the
  direction, so t is the same).
- **Node pointers.** Guest node addresses do not exist when the console
  builder does not run. A program whose results go on to later passes as
  node pointers needs one of two things:
  - the console builder plus AnyPS5's walk-based tables (host primitive
    to guest node);
  - or its consumers changed too.

  The proof shows it: a "node" result kind that leaves the program.
  None of the 13 is known to need it. `32ed072b9f845048` and
  `f607780aa8e502d0` need only hit and t.

## 7. The DXIL side

Checked against DXC 10.0.26100 on a reference shader (cs_6_6, not in
the repository):

- **Ops** (`dx.op`, i32 opcode first):
  - `allocateRayQuery` 178 (constant ray flags) → query handle;
  - `rayQuery_TraceRayInline` 179 (query, TLAS handle, ray flags, mask,
    origin xyz, t-min, direction xyz, t-max);
  - `rayQuery_Proceed.i1` 180;
  - `rayQuery_Abort` 181;
  - `rayQuery_CommitNonOpaqueTriangleHit` 182;
  - `rayQuery_StateScalar` (overloads .i32, .f32):
    - `CommittedStatus` 184, `CandidateType` 185;
    - `CandidateTriangleRayT` 199, `CommittedRayT` 200;
    - `CommittedInstanceIndex` 207, `CommittedInstanceID` 208;
    - `CommittedGeometryIndex` 209, `CommittedPrimitiveIndex` 210;
    - candidate twins 201 to 204;
  - `rayQuery_StateVector.f32` with a component:
    - `CommittedTriangleBarycentrics` 194, candidate 193;
    - object ray origin and direction 205/206 and 211/212;
  - `rayQuery_StateMatrix` 186 to 189.
- **Shader flags.** Ray-tracing tier 1.1 is bit 25 of the module's
  shader flags. Its `SFI0` feature bit is 0x100000. Both go beside the
  existing ones in `emit/shader.rs` (`flags`, and the feature mapping in
  the container writer).
- **The TLAS resource.** An SRV whose type is a struct of one i32, of
  resource kind 16 (`RTAccelerationStructure`). Its handle comes from
  `createHandleFromBinding` (217) and `annotateHandle` (216) with
  properties `{16, 0}`. It also needs a PSV0 binding entry.
- **Root slot.** XTP already declares root SRVs as `t<slot>` in
  `ROOT_CBV_SPACE` (`HostShader::root_srvs`), and the pipeline code turns
  each into a root SRV parameter. The TLAS takes a reserved register
  (`t240`, beyond any title root slot), pushed through the same list. At
  dispatch, `draw.rs` binds the host TLAS address there (`ComputeRoot::Srv`)
  instead of looking up a title root view. With `bounded_root_srvs()` on,
  that parameter is a one-view table instead, and the dispatch writes an
  acceleration-structure view into it.
- **Why not the descriptor heap.** A heap-indexed TLAS would need the
  host view at the title's own descriptor index. XTP's descriptor mirror
  rewrites that slot as a raw buffer view, at times that do not follow
  the command list. A query through a non-structure view is undefined.
  A root SRV is captured when the command is recorded.

## 8. Host structures in XTP

Per title structure address, as VIctory #702 keys them (section 11):

- **BLAS**: a host buffer sized from the host's prebuild info. The build
  uses the title's inputs, with addresses turned into host ones:
  - vertex, index and transform buffers through the shadow copies
    (`shadow::find`, `refresh`, `gpu`);
  - formats checked against DXR's list.

  A refit is `PERFORM_UPDATE` with source = destination when the title's
  flags allowed updates. A compaction copy becomes a host compaction
  copy. A rehydration becomes a full host build from the same inputs.
- **TLAS**: built each time the title builds it, from its instance
  array. Each instance's BLAS address is rewritten to the host BLAS
  address.
  - If the array is written by the GPU (section 3.3), a small compute
    pass rewrites it on the host GPU, after the title's writers in the
    same list, through a sorted table (title address to host address)
    uploaded at the build.
  - If it is written by the CPU, the rewrite is done at submission.
- **Scratch**: one pooled scratch buffer per list, grown on demand, not
  one per build.
- **Lookup at dispatch**: the program's TLAS source gives the title
  address (section 5.3), which gives the host TLAS. A dispatch whose
  host TLAS does not exist yet is dropped and logged (`rt: no host
  TLAS`), as dropped traces are now.
- **Guest-format records**, only when a proven result reads them (AnyPS5's
  "HLE TLAS" idea): the title's instance records written in its TLAS
  memory from the same instance array.

## 9. Costs and risks

- **Builds per frame.** Refits of dynamic BLAS and one TLAS rebuild are
  cheap on RT cores (well under a millisecond for thousands of
  instances). Rehydration becomes full host builds when the world
  streams in. Those spikes are spread over frames with priority to what
  a trace reads, as AnyPS5 queues its slots, and compacted.
- **Memory (Series S mode).** The title's own structure memory is title
  memory, and the host never mirrors it while nothing on the host GPU
  reads it. Host structures are extra VRAM: BLAS before compaction are
  about 1 to 2 times their triangle data, compaction about halves them.
  The real numbers come from the intercept's log (section 10). An
  eviction of host BLAS unused for a long time, as AnyPS5 does at 1,800
  frames, bounds it.
- **Vertex formats.** Host DXR 1.1 accepts a fixed list of vertex
  formats:
  - float32: `R32G32_FLOAT`, `R32G32B32_FLOAT`;
  - float16: `R16G16_FLOAT`, `R16G16B16A16_FLOAT`;
  - 16-bit normalised: `R16G16_SNORM`, `R16G16B16A16_SNORM`,
    `R16G16B16A16_UNORM`, `R16G16_UNORM`;
  - 10-bit: `R10G10B10A2_UNORM`;
  - 8-bit: `R8G8B8A8_UNORM`, `R8G8_UNORM`, `R8G8B8A8_SNORM`,
    `R8G8_SNORM`.

  A format outside it needs a conversion pass into a float buffer.
  `R16_TYPELESS` indices are already turned into `R16_UINT` by the
  title.
- **Culling.** The host applies DXR's instance-flag semantics
  (`TRIANGLE_CULL_DISABLE`, `FRONT_COUNTERCLOCKWISE`) to the title's
  flags. The ray's own cull flag, and the console's sign convention for
  the denominator, must agree with them. The proof (section 5.3) checks
  facing with planted back-facing triangles.
- **The iteration cap.** The guest's cap counts as a hit at t-max. The
  hardware has no cap, so a pathological walk that hits it on the
  console sees a clean answer on the host. That is an accepted
  difference.
- **Root signature size.** One more root SRV takes two dwords of the 64.
- **WARP.** WARP supports DXR 1.1 inline ray queries, for tests.
- **What stays emulated: nothing planned.** A walk that no form and no
  hand description can serve stays refused (dropped). Emulation stays
  parked unless the person asks.

## 10. Phase 2: the prototype on `32ed072b9f845048`

Steps:

1. **Intercept and log.** An HLE module for `XboxBvhBuilder.dll` records
   every call (kinds, counts, formats, flags, sizes) and forwards to the
   real DLL, with nothing built yet. One session gives section 3's
   missing numbers.
2. **Host structures** (`crates/xtp-gpu/src/rt/`):
   - the address table, builds, refits, copies, the instance rewrite and
     the TLAS lookup;
   - WARP tests: one triangle, one instance; a refit; a compaction.
3. **The splice** (`emit/lower/rayquery.rs`): recognition for family A
   shadows, the proof by play for hit and t, and the DXIL ops above.
   Tests in `emit/rayquery_tests.rs`, on a synthetic program written for
   the test, never the title's.
4. **The binding**: the reserved root SRV, and the TLAS lookup at
   dispatch in `draw.rs`.
5. **A WARP end-to-end test**: a synthetic program with a walk of
   family A's shape, translated, with its TLAS built by `rt`. Its
   output equals the expected t + 0.04 and FLT_MAX values.
6. **One PC session** through `queue-session.ps1 -Measure 40 -Limit 120`:
   `32ed072b9f845048`'s output buffer, from the pass dump, non-zero and
   plausible (finite t + 0.04 where shadows fall, FLT_MAX elsewhere).

**Files outside this task's owns** that phase 2 needs:

| File | Change | Why |
|---|---|---|
| `crates/xtp-hle/src/modules/bvhbuilder.rs` (new) and one line in `crates/xtp-hle/src/modules/mod.rs` | the 7 methods' HLE, calling `xtp_gpu::rt` | the intercept (section 3.5) |
| `crates/xtp-gpu/src/lib.rs` | `mod rt;` | the module line |
| `crates/xtp-shader/src/emit/shader.rs` | the tier 1.1 shader flag and its `SFI0` bit (two lines) | section 7 |
| `crates/xtp-shader/src/emit/mod.rs` (shared) | the `prepared` call, a `HostShader` field for the TLAS source, the test module line | section 5.1 |
| `crates/xtp-shader/src/emit/lower/mod.rs` (shared) | `mod rayquery;`, the synthetic instruction's dispatch | section 5.4 |
| `crates/xtp-gpu/src/draw.rs` (shared) | the TLAS root SRV at dispatch | section 7 |
| possibly the PSV0 binding code (`emit/container.rs`, held by cxw6c) | a binding entry of kind 16 | section 7 |

The rest of `32ed072b9f845048` (its ray buffer, mask buffer and output
UAV through descriptor tables) should translate as other compute
programs do. That is checked with `xtp translate --validate` on the
container once the splice exists.

## 11. Other prior art

- **VIctory #702** (Marvisak/VIctory, GPL-3.0; commits `0237eca`,
  `0672d75` by wrothgaril), a DXR 1.1 backend: 948 lines,
  `raytracing.rs`. It has:
  - a table of host structures keyed by title address;
  - BLAS geometry addresses turned into shadow addresses, with refresh;
  - TLAS instances rewritten to host BLAS addresses;
  - postbuild info, copies, and structure SRVs in the descriptor
    translation;
  - WARP-skipping tests.

  It fits XTP's host side, and phase 2 can take it with credit after
  these changes:
  - its instance rewrite reads the instance array on the CPU at
    submission, which is stale if the title's GPU writes it;
  - it allocates a scratch buffer per build;
  - it hooks command-list methods this title never calls;
  - it has no rehydration;
  - its structure SRVs in descriptor heaps are exposed to the mirror
    race of section 7.
- **xcl #2184 to #2189** (rison1337, `24206a657`, GPL-2.0-only: ideas
  only). They build native BLAS/TLAS from the builder's exports and
  bound a shadow traversal with RayQuery, with differential GPU tests
  (640 hardware queries) but no image measurement. It is a second view
  that agrees with section 3.5's intercept.

## 12. Method and limits

- **The builder interface**:
  - the DLL's exports: `xtp inspect --exports`;
  - the title's imports: `xtp inspect --imports`;
  - the call sites: a scan of RIP-relative references to the import
    slots (`E:\xtp-data\re\scripts\import_refs.py`);
  - the wrappers, read with Capstone (`E:\xtp-data\tmp\rtdxr\*.asm`).
- **The trace programs.** The containers saved by task rtdark
  (`E:\xtp-data\re\out\rtdark\containers\`) were disassembled with
  `xtp shader-info --disasm` (`E:\xtp-data\tmp\rtdxr\dis\`). The table
  of section 4.1 comes from a script counting:
  - intersections and their form;
  - back edges;
  - caps and masks by their constants;
  - texture reads and stores.

  `32ed072b9f845048` and `f607780aa8e502d0` were read in full; the
  others only through those counts.
- **AnyPS5.** Read in the read-only clone, its history through GitHub's
  API. Never built or run.
- **Not done:**
  - runtime measurement of the builder calls;
  - the instance array's writer;
  - each gather's exit values: the proof of section 5.3 gives them;
  - the cull sign convention.
