# AnyPS5 (AnyKyty): what XTP can reuse

What the person's AnyPS5, on its `AnyKyty` branch (PS5 titles as native
programs, the Kyty graphics core on Vulkan, NVIDIA and AMD), does about
the five questions of the task, whether it beats XTP, how XTP would reuse
it, the XTP task it maps to, and who wrote it.

- **Read:** the read-only copy `E:\xtp-refs\AnyPS5` at `7de61e3451`
  (branch `AnyKyty`; a shallow clone of one commit). Nothing of
  it was built or run.
- **History:** a blobless clone of `github.com/simnJS/AnyPS5`, branch
  `AnyKyty` (49,975 commits), made in the session's scratchpad for
  attribution only, plus GitHub API reads of `KytyPS5/KytyPS5` and of the
  person's KytyPS5 fork (simnJS).
- **Paths:** below, paths without a prefix are under `core/kyty/graphics/`
  of AnyPS5; `shim/` is `core/libs/prx/libSceAgcDriver/KytyShim/`. Line
  numbers are at `7de61e3451`. Commit hashes are AnyPS5's (10 characters),
  on `AnyKyty` unless said otherwise.
- **No code is copied here.** Mechanisms are described in our own words.
  The title XTP runs is called BTP 6; no PS5 title is named.

## 1. Summary

| # | Pick | Maps to | Licence |
|---|---|---|---|
| 1 | **Lane pairs**: run one console wave64 as one 32-lane host wave, each host thread holding console lanes *i* and *i* + 32. Ballots, lane reads and branches stay inside one host wave, `s_barrier` stays a group barrier, LDS is shared inside one host wave, and mesh outputs are written once after the program. This answers the four mesh64 hazards and steps 7–8 of `wave64-on-wave32.md` | mesh64 (0a31f0cb); wave64 steps 7–8 | **Ideas only** (KytyPS5, GPL-2.0-only) |
| 2 | Cheaper exchanges between two halves for one-wave groups: up to four cross-lane operations per barrier, alternating slots (one barrier per exchange instead of two), no exchange for operands proven uniform, the group flattened to one dimension. Also a rule for the uniformity proof: reads of memory the program writes are not uniform once the wave is split | wave64 console flow; mesh64's proof | **Ideas only** unless a contributor (raghav314) agrees (§2) |
| 3 | A run-time self-check of the wave64 lowering: a sampled dispatch runs twice, in the split form and in the lane-pair form, and the outputs are compared on the GPU | wave64 stack (a5327214 follow-up) | **Portable** (the person's own) |
| 4 | Copy-back as a three-way merge: write back only the bytes the GPU changed and the title did not touch since the readback was recorded | copy-back (78d58a42) | **Portable** (the person's own, a 61-line header) |
| 5 | One frame in flight: complete a flip once its command buffer is submitted, not once the GPU has run it, with the title never more than one frame ahead | one frame in flight (f1d37c5d) | **Portable** (the person's own) |
| 6 | Pixel interpolation with computed barycentrics through the host's barycentrics and per-vertex attributes (D3D12: `SV_Barycentrics` and `GetAttributeAtVertex`, Shader Model 6.1) | translator: the 248 programs of `shader.md` §4 | **Ideas only** (KytyPS5) |
| 7 | Every buffer bound as a raw, byte-addressed buffer: no structured stride to know | unknown strides (0b01ce59, VIctory #193, #704) | **Ideas only** (KytyPS5) |
| 8 | Fuzzing the translator, with every output checked by the validator | new translator task | **Portable** (the person's own) |
| 9 | In-run A/B: settings alternate at flips within one session | XTP's measurements (any task) | **Portable** (the person's own) |
| 10 | Retiling of readbacks in device-local memory, then one copy to the host; detiling on the GPU | Series S world frame (522e7cb5) | **Portable** for the readback path (the person's own); GPU tilers **ideas only** |

The licence column needs §2: most of AnyPS5's graphics core is not the
person's, so "portable" applies only to the person's own commits.

## 2. Provenance and licence

### 2.1 Who wrote the graphics core

| Layer | Where in AnyPS5 | Authors | Licence |
|---|---|---|---|
| Kyty | the origin of KytyPS5 | InoriRus | MIT (`core/kyty/LICENSES/Kyty-MIT.txt`) |
| KytyPS5 | the bulk of `core/kyty/`, imported in `e6570df909` (2026-11-17), comments stripped | Nmzik and about a hundred contributors (`KytyPS5/KytyPS5`) | **GPL-2.0-only** (its README says `GPL-2.0-only`) |
| Fork edits | in the same import: "KytyPS5 fork src/...@40ddebbe" | `40ddebbe` is a commit by TheCruZ, a KytyPS5 contributor, in the person's KytyPS5 fork (simnJS) | GPL-2.0-only (KytyPS5's) |
| AnyKyty | commits on `core/kyty/` and `shim/` after the import, mostly under `#if defined(ANYPS5_KYTY)` or in new files | the person: 281 commits on `core/kyty/`, 247 on `shim/` | the person's; AnyPS5 is distributed as GPL-2.0 |
| Contributors | same places | 16 people, 95 commits on `core/kyty/` (14 people, 92 on `shim/`): mjnhbg3, Celegans12, raghav314, Jan Šlégl, Victor, trvxoo-glitch and others | GPL-2.0 (AnyPS5's README: "version 2"; no "or later", no CLA) |
| shadPS4 | `host_gpu/shaders/fault_buffer_process.comp` | shadPS4 Emulator Project | GPL-2.0-or-later (SPDX header) |

The person's commits were found by the author identity of the person's
GitHub account. Some "Origin" hashes in AnyPS5's
`docs/dev/THIRD_PARTY_KYTY.md` are not on `AnyKyty` but exist on GitHub
(`499dfc3f39`, `08c666a175` …); the same changes are on `AnyKyty` under
other hashes (`7b85afc1aa`, `9279408f46` …), which this report cites.

### 2.2 What that means for XTP (GPL-3.0-or-later)

The orchestrator's note says the person wrote or heavily changed
everything in AnyPS5 except Kyty. The history says otherwise for the
graphics core: Kyty's code reached AnyPS5 through KytyPS5, which is
third-party and GPL-2.0-only, and the import commit carries it as it was.
The person's work is the AnyKyty layer on top of it.

- **Portable** (with credit, "Ported from simnJS/AnyPS5 (<commit>)"):
  a commit or file by the person alone. A person's commit that edits a
  KytyPS5 function makes only the person's lines portable: a port takes
  the person's logic, rewritten in Rust and for D3D12 anyway, never the
  KytyPS5 lines around it.
- **Ask**: code by one of the 16 contributors. GPL-2.0 without "or later",
  so ideas only unless that contributor agrees.
- **Ideas only**: KytyPS5 code (GPL-2.0-only).
- **MIT**: code that is Kyty's own could go in with its MIT notice, but it
  should be taken from `InoriRus/Kyty` itself, not through KytyPS5. Kyty
  was a PS4 emulator; nothing below needs it.
- **GPL-2.0-or-later**: `fault_buffer_process.comp` (shadPS4) can go in
  with its notice.

Each recommendation below carries one of these labels.

## 3. Wave64 on 32-lane NVIDIA hosts

### 3.1 What AnyPS5 does

A wave64 compute or mesh program on a device whose subgroups are 32
(`graphicContext.h:114`, "no wave64 compute": subgroup size not 64 and no
subgroup size control) takes one of three forms.

**Lane-local** (KytyPS5). A program that never depends on its lanes
(`frontend/translate/LaneLocal.cpp:1020`, `IsWaveLaneLocal`) runs one
console lane per host thread, in whatever waves the host forms
(`frontend/translate/Translate.cpp:1023-1026`). This is XTP's per-lane
form.

**Lane pairs** (KytyPS5; the default for everything else). One console
wave is one 32-wide host subgroup, and every host thread runs two console
lanes.

- *Group shape.* The group is flattened to one dimension with 32 host
  threads per console wave: a 128-lane console group (two waves) becomes 64
  host threads (`shader/recompiler/backend/spirv/spirvEmitterModule.cpp:1114-1130`;
  mesh: `host_gpu/renderer/pipeline/pipelineCache.cpp:2482-2492`).
- *Lanes.* Host thread *t* holds console lanes *t* + (*t* & ~31) and that
  plus 32, so host subgroup *k* is console wave *k*
  (`backend/spirv/spirvEmitterHelpers.cpp:143-157`). The console's local and
  global ids are rebuilt from that flat index
  (`backend/spirv/spirvEmitterFlow.cpp:24-45`). A partial last wave starts
  with the missing lanes out of EXEC (`frontend/translate/Translate.cpp:1158-1170`).
- *Registers.* The emitter keeps two value contexts, one per half, and
  emits every vector instruction for both (`backend/spirv/spirvEmitterProgram.cpp:1807-1815`).
  Scalars exist once. The person added a fixed-point analysis that also
  emits once the vector operations whose operands, EXEC included, are the
  same in both halves (`spirvEmitterProgram.cpp:712`, `FindPairSharedValues`,
  used at 1884-1886; `fb866cc5d8`, checked by `shim/tests/PairCodegen.cpp`).
- *Masks.* A ballot is two host ballots, one per half, packed as the low
  and high dwords of the console mask (`spirvEmitterProgram.cpp:1502-1526`).
  A lane's own bit of a mask held as a number picks the dword by lane ≥ 32
  (`spirvEmitterHelpers.cpp:184-223`). EXEC is a value: the entry sets it
  per lane and its two dwords from a ballot (`Translate.cpp:1181-1189`).
- *Lane reads.* `v_readlane` and `ds_bpermute`-style reads shuffle both
  halves' registers to host lane *l* & 31 and keep the one of the half that
  owns *l* (`spirvEmitterProgram.cpp:1547-1572`). `v_readfirstlane` finds
  the first set bit of the 64-bit EXEC ballot and reads that lane
  (`spirvEmitterProgram.cpp:1528-1545`, `spirvEmitterFlow.cpp:853-867`).
  Whole-wave reductions combine the two halves in the thread, then reduce
  over the subgroup (`spirvEmitterFlow.cpp:869-881`).
- *Branches.* The program follows the console's scalar branches. An
  `s_cbranch_execz` or `vccnz` test combines both halves' predicates in the
  thread, then ballots over the subgroup (`spirvEmitterFlow.cpp:809-837`).
  Every thread of a host wave takes the same branch, because the wave is
  the console wave.
- *Barriers.* `s_barrier` is a group barrier. All host waves reach it
  where all console waves reach it, the same contract as on the console.
  No barrier sits where only one half of a console wave goes.
- *LDS.* Lanes that share LDS inside one console wave are threads of one
  host subgroup. Before each LDS access the emitter places a
  subgroup-scope control barrier with workgroup memory semantics
  (`backend/spirv/spirvEmitterMemory.cpp:14-23`, profile flag
  `lds_lane_sync`), never a group barrier. That flag is set for compute
  only; mesh programs rely on the subgroup running in step.
- *Mesh.* The console program runs as a function. Each thread keeps its
  two lanes' vertex outputs in private arrays indexed by half
  (`backend/spirv/spirvEmitterMesh.cpp:81-88`). The console's output
  allocation (GS_ALLOC_REQ) is stored to group memory by host thread 0
  (`spirvEmitterMesh.cpp:124-142`). After the function returns: one group
  barrier, one `SetMeshOutputsEXT` with the stored counts, then every
  thread writes both halves' vertices (`spirvEmitterMesh.cpp:144-175`).

**Split** (created by a contributor, raghav314, `74fca2387c`, `d8e15aa496`,
`1720b90478`; widened by the person, `930a414301`, `87e8c52fde`,
`dc0107761a`, `30f7d8edb9`, `b49d9e21f7`). For compute only. The program
is emitted as if the host had 64-wide subgroups, then a SPIR-V-to-SPIR-V
pass (`shader/recompiler/backend/spirv/SpirvWaveSplit.cpp`, 1,664 lines,
called from `shader/recompiler/ShaderRecompiler.cpp:839-864`) rewrites it
for two 32-wide subgroups per console wave. A program the pass refuses
falls back to lane pairs, with the reason logged.

- *Why it exists.* Lane-pair modules are about twice as long, and the
  commit that added the split reports NVIDIA's driver building them much
  more slowly than in proportion (`74fca2387c`).
- *Eligible.* Compute, the group whole waves of 64 to 1,024 threads
  (`SpirvWaveSplit.cpp:274-285`), no compute derivatives, no workgroup-size
  or subgroup-layout builtin read, LDS in group memory.
- *Lanes.* The console lane is `SubgroupId × 32 + SubgroupLocalInvocationId`
  (Vulkan full subgroups), and the console's ids are rebuilt from it; the
  group is flattened to (threads, 1, 1) (`SpirvWaveSplit.cpp:1151-1184`, 1592).
  The host's way of forming waves does not matter.
- *Divergence analysis* (`SpirvWaveSplit.cpp:665-831`): a fixed point over
  values, variables, blocks and the phis of merges. It treats the workgroup
  id as uniform and the wave index (local index ÷ 64) as uniform. It treats
  every buffer read as divergent when the module writes buffers, and every
  image read when it writes images (`573-595`; `d8e15aa496`): once the wave
  is split, each half reads at its own time, while the console reads a
  wave-uniform address once.
- *Refusals* (`835-856`, `891-984`): a subgroup operation or barrier in
  divergent flow, lanes leaving early, LDS written in a divergent loop,
  LDS read and written in one divergent region (unless all its accesses
  are no-return atomics of one kind per word, `1720b90478`).
- *Exchange* (`1228-1285`): cross-lane operations whose operands are
  divergent go through an exchange area in group memory. Up to four
  operations of one block share one barrier. The area has two slots used
  in turn, so one barrier per exchange suffices. Operations on uniform
  operands need no exchange: a ballot of a uniform predicate is all or
  nothing, a broadcast of a uniform value is the value, a reduction is
  computed (sum = value × 64, xor = 0, …) (`1287-1293`, `1348-1362`).
- *Same-word stores* (`1295-1308`, `87e8c52fde`): when every lane stores a
  different value to one LDS word, the value of console lane 63 is
  exchanged and stored, as on RDNA.
- *Groups of several waves* (`930a414301`): each console wave has its own
  exchange area; its two halves meet on a per-wave arrival counter in group
  memory, one lane spinning on an atomic load (`1056-1102`), not on a group
  barrier. The guest `s_barrier` stays a group barrier. The person then
  kept such groups in lane pairs unless the split pays, which it does only
  with at most 512 threads and a loop carrying 48 or more values, or with
  same-word stores (`997-1004`, `30f7d8edb9`).

**Checks** (the person's). `KYTY_WAVE_SPLIT_CHECK=N` plays one direct
dispatch in N of every split program twice: the split module, its outputs
saved and the previous contents restored, then the lane-pair module, and
counts on the GPU the output dwords that differ. Every other check runs the
lane-pair module twice instead, as a witness of what the program leaves to
the order of its atomics (`host_gpu/renderer/waveSplitCheck.cpp`, 348
lines, `6f15827dac`, `3e3fe2d82b`; shared capture and compare pass
`host_gpu/renderer/checkOutputs.cpp`, `cd5b7d080b`).

### 3.2 Compared with XTP

| | XTP console flow (`wave64-on-wave32.md`) | mesh64's design (0a31f0cb) | AnyPS5 lane pairs | AnyPS5 split |
|---|---|---|---|---|
| Host waves per console wave | two | one or two, whatever the host forms | **one** | two |
| Groups | one console wave (≤ 64 threads); more stops | mesh 128-lane groups | **any** whole or partial waves, compute and mesh | 64 to 1,024 threads, compute |
| `v_readfirstlane` | exchanged | proven uniform, else refused | first bit of the 64-bit EXEC, no proof needed | exchanged, or nothing if uniform |
| Branch on EXEC/VCC | ballot + two group barriers per branch | must be uniform at group level | ballot in one host wave, no barrier | must be uniform, else lane pairs |
| `s_barrier` | group barrier, reached because the group is one console wave | hazard 2 (a half may skip it): refused | group barrier, reached as on the console | group barrier |
| LDS in one console wave | group barrier before each access | hazard 3: refused | subgroup barrier before each access (compute) | barriers around shared writes |
| Mesh output counts | n/a | hazard 4: to be set once | set once after the program, then both halves write | n/a |
| Uniformity analysis | none needed | three levels on XTP's IR | none needed (masks are values) | divergence analysis on SPIR-V |
| Cost | ballot and two barriers per branch, one barrier per LDS access | none when proven | twice the vector registers per thread, half the threads, longer modules | barriers per exchange batch |

For groups of several console waves and for the title's mesh programs,
lane pairs are what XTP lacks: no form XTP has, or mesh64 plans, runs them
all, and lane pairs need no proof. For one-wave compute groups, XTP's
console flow and AnyPS5's split do the same job; the split is cheaper per
branch and per exchange.

### 3.3 How XTP would reuse it

**Lane pairs in XTP's lowering** (ideas only: reimplement).

1. *When.* On a host whose waves are fixed at 32 (D3D12 `WaveLaneCountMin`
   = `Max` = 32), for a console wave64 program that is not lane-local:
   mesh programs first, then compute groups of several console waves
   (steps 7–8 of `wave64-on-wave32.md`). Keep the native form where the
   host runs 64-lane waves.
2. *Shape.* `numthreads(32 × waves, 1, 1)`. The console lane of host thread
   *t*, half *h*, is *t* + (*t* & ~31) + 32*h*; rebuild the console's 3-D
   ids from it. A one-dimensional group also removes the risk
   `wave64-on-wave32.md` notes of NVIDIA forming 3-D waves from
   non-consecutive ids (its 1-D groups were all exact).
3. *Values.* The console flow's structure already gives each vector
   instruction its EXEC bit. Emit each vector instruction twice, one SSA
   value per half, each guarded by its half's EXEC bit; scalars once. Emit
   once what does not depend on the half (the person's `fb866cc5d8` is the
   model; portable). This also covers mesh64's "integer divisions of scalar
   values done in VALU".
4. *Cross-lane.* Ballot = (`WaveActiveBallot(p0).x`,
   `WaveActiveBallot(p1).x`). `v_readlane l` = `WaveReadLaneAt` of the half
   that owns *l*, at *l* & 31 (*l* is uniform, so the choice of half is
   too). `v_readfirstlane` = lowest set bit of the 64-bit EXEC. `v_mbcnt`
   counts the bits below the console lane. DPP rows and `ds_swizzle` groups
   stay inside a half. `ds_bpermute` and `ds_permute` with 64-lane
   addresses: two reads and a select. Reductions: combine the two halves in
   the thread, then one wave reduction.
5. *Branches and barriers.* The console flow's `s_cbranch_*` stay; an EXEC
   or VCC test ORs (or ANDs) both halves' predicates before the ballot.
   `s_barrier` = `GroupMemoryBarrierWithGroupSync`, as on the console.
6. *LDS.* D3D12 has no subgroup-scope barrier. In this form the host wave
   stays converged (all branches are wave-uniform), so a memory fence
   (`GroupMemoryBarrier`, no group sync) between dependent LDS accesses of
   one console wave should do; **measure it** with XTP's
   `lds_written_and_read_in_different_masked_blocks` in lane-pair form on
   the RTX 4080 SUPER and on WARP before relying on it.
7. *Mesh.* Run the console body; keep each half's vertex and primitive
   outputs in per-thread arrays; let console lane 0 store the
   GS_ALLOC_REQ counts to groupshared memory; after the body, one
   `GroupMemoryBarrierWithGroupSync`, one `SetMeshOutputCounts` (uniform,
   once, as D3D12 requires), then each thread writes both halves' outputs.
   A 128-lane console group is 64 host threads, within D3D12's 128.
8. *Costs to measure.* Register pressure (twice the vector values),
   translation time and the driver's compile time (AnyPS5 met the latter;
   XTP's pipeline workers already wait up to 28 s on some translations,
   `drops.md`), and frame time against the 1.7 fps world.

**Improvements to the console flow from the split** (ideas only, unless
raghav314 agrees for `SpirvWaveSplit.cpp`, which is that contributor's
file with the person's later changes):

- batch up to four exchanges of one block behind one barrier;
- alternate two exchange slots, so one barrier per exchange replaces two;
- no exchange for a ballot, broadcast or reduction of a value proven
  uniform across the console wave;
- console lane from the wave's place, not from the host's flat id layout.

**For mesh64's uniformity proof** (idea): AnyPS5 found that a load whose
address is uniform is not uniform in value once the two halves run at
different times, when the program writes that memory (`d8e15aa496`).
`wave64-on-wave32.md` already lists this as a known risk; the proof should
count a load from memory the program writes as non-uniform.

**What not to take:** the split's per-wave spin on an arrival counter.
`wave64-on-wave32.md` already dropped a partner wait because D3D12 does
not promise that the other half runs meanwhile, and AnyPS5 itself keeps
most multi-wave groups in lane pairs.

**Self-check** (portable, the person's): an `XTP_WAVE64_CHECK=N` that runs
a sampled direct dispatch in two forms (split or console flow against lane
pairs, later lane pairs against native on AMD) with outputs saved,
restored and compared by a compute pass, plus a run of the same form twice
as witness of atomic order. XTP's own tests compare against the Radeon iGPU
offline only.

**XTP tasks:** mesh64 (0a31f0cb) for the mesh form; a new task for
compute groups of several waves (steps 7–8); a new task for the self-check.

## 4. NVIDIA device removals (TDR)

### 4.1 What AnyPS5 does

- **Device robustness** (KytyPS5): `robustBufferAccess`, `robustImageAccess`
  and `nullDescriptor` are required or enabled, and `robustBufferAccess2`
  when its alignment is 1 (`presentation/window/vulkanWindow.cpp:228-297`,
  `622-623`, `653`, `707`, `720-723`). Without it, every storage buffer
  access checks its index against the view's length in the shader
  (`backend/spirv/spirvEmitterMemoryHelpers.cpp:410-424`).
- **Device addresses** (KytyPS5, shadPS4's design): flat and global
  accesses go through a page table of guest pages. A missing page records a
  fault in a fault buffer, which a compute pass turns into requests for the
  host, and yields an address of 0 instead of a wild one
  (`backend/spirv/spirvEmitterMemory.cpp:1931-1960`, `2022-2060`;
  `host_gpu/shaders/fault_buffer_process.comp`, shadPS4, GPL-2.0-or-later).
- **Descriptors chosen at run time** (KytyPS5): a key is looked up in a
  sorted table with a binary search of a fixed number of steps; no match
  gives a default resource. The index used is never outside the table
  (`spirvEmitterMemoryHelpers.cpp:79-124`).
- **Empty buffers** (the person, `c5f18b4156`): a slot whose V# has zero
  records reads zeros and drops writes, as the console does
  (`shader/recompiler/ir/passes/ResourceMaterialization.cpp:2278`).
- **Translator robustness** (the person): libFuzzer harnesses on the
  recompiler and the PM4 parser, every recompiled module checked by
  `spirv-val` (`shim/fuzz/`, `0d9874cb21`, `b56f328189`, `be51eaee75`);
  shaders that would give invalid SPIR-V are refused (`0eabf7ecb4`); a draw
  or dispatch whose shader the recompiler stops on is skipped with one log
  line (`KYTY_FALLBACKS`, `99d4953400`).
- **Loop limits:** none. A console loop runs on the host as long as it
  runs.

### 4.2 Compared with XTP, and what to take

- XTP is ahead on loops: `LOOP_LIMIT` stops a loop after 2^14 iterations
  because of the TDR watchdog (`crates/xtp-shader/src/emit/lower/mod.rs:306-312`).
- D3D12 bounds-checks accesses through descriptors itself; root
  descriptors and heap indices are not checked. The pattern worth taking
  (ideas only): **a heap index formed at run time is always inside the
  table**, by a bounded lookup with a default slot, for XTP's per-lane heap
  indices (`emit/lower/descriptor.rs`, waterfall and per-lane handles).
- The largest TDR risk in XTP's queue is mesh64's hazard 2 (a barrier one
  half may skip). Lane pairs (§3.3) remove it by construction.
- **Fuzzing with validation** (portable, the person's approach): a
  `cargo fuzz` target on `xtp-shader` that feeds random GFX10 words to the
  translator and checks each module with dxil.dll. A new translator task.
- Flat and global addresses through a page table with fault recording:
  ideas only, but `fault_buffer_process.comp` itself is GPL-2.0-or-later
  and could be ported with its notice if XTP takes that design (it would go
  with UMA, G15b/G16).
- VIctory #334 (1661c7ac, the NVIDIA DEVICE_HUNG fix through buffer
  states): nothing in AnyPS5 maps to it; Vulkan's barriers are explicit.

## 5. RDNA2 shader translation coverage

XTP decodes every instruction of the title's pack (`shader.md` §4); what it
refuses are constructs. Below, XTP's refusal buckets (`shader.md` §4, the
first blocker per program, and `drops.md` §1 and §4 for the world's
sessions), and what AnyPS5 does with the same construct. Several buckets
have shrunk since `shader.md` was measured (#180, #539, #563 and others are
ported).

| XTP refusal (programs in `shader.md` §4) | AnyPS5 | Reuse | Licence | Task |
|---|---|---|---|---|
| Lane masks and wave layout (927): selects on untracked masks, masks read as data | Masks are values everywhere: EXEC, VCC and compares give 64-bit masks (two dwords), and a lane's bit is extracted when a mask selects (`Translate.cpp:1181-1189`, `spirvEmitterHelpers.cpp:184-223`). Nothing needs tracking | XTP's console flow already treats masks as numbers. Lower a program the per-lane form refuses for its masks through the console flow instead of stopping, on every host | Ideas | masks-fix (68af0b3f) follow-up |
| Scalar memory read as data (776), descriptors read as data (396, 92) | A CPU "SRT walker" evaluates, at each draw, the chain of scalar loads from user data (pointers, descriptors, constants), reads guest memory, and writes a flat table the shader reads instead (`shader/recompiler/ir/passes/SrtWalker.h`, `SrtWalker.cpp`, `ResourceTracking.cpp`). The person binds a stable flat table as a uniform buffer and reads scalar loads of read-only data through uniform views (`521ca7e58f`, `f749c16ef2`); words in pages the GPU writes are filled later in command order. What the walker cannot evaluate goes through device addresses (§4) | For XTP (root signatures, unlike the PS5's tables), the part that transfers is the per-draw CPU evaluation of pointer chains rooted at root constants into a small constant buffer, for the mesh and vertex programs `run-world.md` lists | Ideas (walker); portable (the person's uniform binding) | new translator + GPU task (run-world's R1, R2) |
| Structured buffer of unknown stride (148) | Every buffer is a raw, byte-addressed storage buffer; formats and strides are applied in the shader (`BufferFormat.h`, `spirvEmitterMemoryHelpers.cpp:426` on) | Use `ByteAddressBuffer` views where the stride is unknown | Ideas | 0b01ce59 (#193, #704) |
| Per-lane descriptor cases (105) | Per-lane keys go through the bounded lookup of §4 | Same as §4 | Ideas | descriptor follow-up |
| 64-bit image atomics (659) | 64-bit image atomics (`Int64ImageEXT`, `backend/spirv/spirvEmitterAnalysis.cpp:124`) | Since ported in XTP (#563) | — | — |
| Biased sample with 16-bit coordinates, `image_sample_d_cl`, `image_gather4_l` (153; gather4_l still in `drops.md`) | The MIMG table decodes every sample and gather variant as flags: lod, bias, derivatives, compare, offset, level zero, lod clamp, A16, and gather's `_l`, `_b`, `_cl` (`frontend/decode/ImageOps.cpp:79-245`, flags `ShaderDecoder.h:797-810`) | Implement the flag combinations from AMD's RDNA2 ISA reference (public); AnyPS5's table is a check list | Ideas | shader-images (84a6a313) follow-up |
| Interpolation with computed barycentrics, barycentrics read directly (248) | Barycentrics are host builtins (`BaryCoordKHR`) and parameters are fetched per vertex (`PerVertexKHR`), so `v_interp_p1`/`p2` work with any i, j (`spirvEmitterModule.cpp:683`, `804`; `Translate.cpp:1415-1424`) | D3D12: `SV_Barycentrics` and `GetAttributeAtVertex` (SM 6.1, NVIDIA and AMD) | Ideas | translator, new task |
| Packed input half (166), hardware SGPR input, conditional export (25) | Not checked in depth | — | — | — |
| LDS bytes and halves (21) | `ds_write_b8`, `_b16`, `ds_read_u8`, `_u16`, `ds_write_b8_d16_hi` are translated (`frontend/decode/MemoryOps.cpp`) | ISA reference | Ideas | — |
| `v_permlanex16_b32`, `v_cvt_f32_f64`, `s_getpc_b64` (12) | Translated | ISA reference | Ideas | — |
| Compute `sample` with implicit derivatives (12 at `WaveSize(64)`) | Compute derivatives (`VK_KHR_compute_shader_derivatives`), quads from a 2 × N layout; wave64 ones stay in lane pairs (`spirvEmitterModule.cpp:1121-1127`, `dc0107761a`) | SM 6.6 compute derivatives; XTP's shader-images fix computes quad gradients on 2-D groups | Ideas | 84a6a313 |
| Merged ES + GS (390) | No geometry stage: AnyPS5's stages are vertex, pixel, compute, mesh, local, hull and domain (`shader/shader.h:25-34`); PS5 titles use primitive (mesh) shaders | Nothing to take | — | — |
| FidelityFX programs ("no DXIL part") | n/a | HLE | — | 7cf93cdb |

## 6. Texture tiling, formats, BC

### 6.1 What AnyPS5 does (KytyPS5, except the readback path)

- The PS5's tile modes (linear, standard 256 B, 4 KiB, 64 KiB, PRT,
  render target, depth, with 3-D variants) map to block families
  (`guest_gpu/tile.h:20-30`, `guest_gpu/tile.cpp:247-262`). The CPU side
  computes layouts, mip tails, block offsets and the block XOR
  (`tile.cpp:654-710`), and DCC, CMASK and depth sizes (`tile.h:107-121`).
- Tiling and detiling run **on the GPU**: one compute shader per block
  family (`host_gpu/shaders/gpu_tiler_*.comp` and `.inc`, 523 lines in
  all), one texel per thread and 64 per group, elements of one byte, two
  bytes or whole dwords, an optional color transform on the way
  (`gpu_tiler_color.inc`), d16 promote and demote for depth.
- **Readbacks** (the person, `9279408f46`): a tiled image going back to
  guest memory was retiled by the shader straight into the host-visible
  readback buffer, a few bytes at a time across PCIe. Now the range is
  copied to a device-local scratch buffer, tiled there, and copied back
  with one buffer copy (`host_gpu/renderer/image/tiler.cpp:38`,
  `KYTY_TILE_VIA_VRAM`).
- BC formats are native on the host; the only BC rule is that
  block-compressed textures get no DCC metadata (a KytyPS5 fix).
  `guest_gpu/gpu_format.cpp` is the PS5's format table.

### 6.2 Compared with XTP, and what to take

- XTP is ahead for its console: XOR modes with the Series X and Series S
  pipe configurations, from addrlib's patterns, are in `xtp-formats`
  (3cba2a96). The PS5's pipe configuration differs, so AnyPS5's equations
  do not apply.
- **Detiling on the GPU** (ideas only): XTP detiles on the CPU. The world
  frame uploads 100+ MB of textures a frame (522e7cb5). Uploading the tiled
  bytes as they are and detiling in a compute pass per subresource, with
  XTP's pattern tables as its input, would take that work off the CPU.
- **Retile readbacks in VRAM, then one copy** (portable, the person's): for
  any render target XTP copies back to title memory in a console layout.
- Formats and BC: nothing to take.

## 7. Memory

### 7.1 What AnyPS5 does

All of the following are the person's, unless said otherwise.

- **Copy-back as a three-way merge** (`65a1b01db4`;
  `host_gpu/renderer/cache/compareWriteBack.h`, 61 lines, the function at
  line 16; used in `textureCache.cpp:3298-3320` and `3338-3391`). When a
  readback is recorded, the guest bytes of its footprint are saved. When
  the GPU is done, guest memory is read again, and in 64-byte steps only
  the bytes where the GPU result differs from the saved copy **and** guest
  memory still holds the saved copy are written. A byte the title changed
  meanwhile is kept and reported. Before, the whole footprint was written,
  putting back old padding, unused mips and memory reused for other data.
- **Images copied back at GL2 write-backs** (`f55d92ea53`,
  `textureCache.cpp:159`): queued readbacks land at each
  `ACQUIRE_MEM`/`RELEASE_MEM` with GL2 write-back and at each flip, their
  pages read-protected until then.
- **Faulted reads replayed** (`e36fbe6707`, `6cb0cb2a4a`, Windows
  `0832a8bc2e`, `7c5fc8900b`; `shim/src/Faults.cpp:86`, `101`): a title read
  that faults on a page the GPU owns, on bytes no GPU write is near, is
  replayed on an unprotected second view of the same memory instead of
  waiting for a readback; after a readback, the read is replayed on the
  copied bytes and the page is released without waiting. Instructions that
  write memory are never replayed.
- **Write-fault blocks** (`88eb2573eb`, `da2947c57d`): a write fault in
  command-buffer memory opens its whole 64 KiB block, so a command buffer
  filled page by page faults once per upload instead of once per page.
- **Command memory in place** (`5d64eeb574`, experimental, off by default;
  `host_gpu/renderer/cache/inPlaceMemory.cpp`): guest direct memory is
  imported as host memory (`VK_EXT_external_memory_host`, 256 MiB chunks)
  and command-memory buffers are bound to it, never protected nor
  uploaded. The price: the host must follow the GPU (labels written at
  completion, one frame in flight, a visibility barrier ending every
  submission). The person measured it slower than the default and left it
  off.
- **Flips completed at submission** (`7b85afc1aa`,
  `host_gpu/renderer/commandScheduler.cpp:316-326`, `DeferFlipCompletion`):
  a flip completes on a thread of its own once its command buffer is
  submitted and the previous flip's work is done, at most two frames in
  flight; the present blit waits for the copy on the GPU.
- **Suspend points** (`54fe441bb6`, `4328d9d796`,
  `guest_gpu/graphicsRun.cpp:348-355`): up to eight in flight, but the title
  never more than one frame ahead of the GPU. With eight frames ahead, one
  title rewrote vertex tables the command processor had not read yet.
- **Labels** (`33fe9d39eb`, `842fd4f3d4`, `graphicsRun.cpp:2799`, `2970`):
  a deferred label waits for the newest pending occlusion dump, not its
  own command buffer; a label next to GPU-written bytes, but not on them,
  is deferred instead of draining the GPU.
- **Idle submit** (`a63804469a`): when the GPU has run out of work, the open
  command buffer is submitted after the next draw or dispatch.
- **In-run A/B** (`2eadda9b42`, `core/kyty/common/runtimeSwitches.cpp:271`,
  `KYTY_AB`): listed settings alternate at flips within one session, with
  settling time and per-phase frame counts.
- **Allocation** (KytyPS5): VulkanMemoryAllocator sub-allocates; a
  buffer allocation the budget refuses is retried once without the budget
  (a KytyPS5 fix). On Linux, direct memory is backed by huge pages on a
  private tmpfs (raghav314, `afaf3ef8b4`), and pages the title keeps
  writing can stop faulting through userfaultfd (`b0994febfe`, off by
  default: it cost more than it saved).

### 7.2 Compared with XTP, and what to take

| AnyPS5 | XTP today | Take? | Licence | Task |
|---|---|---|---|---|
| Three-way merge on copy-back | copy-back writes the GPU's bytes; overlapping shadow ranges in one submission can put older title bytes over newer ones | **Yes**: snapshot at record, compare at landing, write only the bytes the GPU changed and the title did not | **Portable** (`65a1b01db4`, a pure function over four byte ranges) | 78d58a42 |
| Flip completed at submission, at most one frame ahead | every submission drained at each present | **Yes**, with AnyPS5's lesson: bound how far ahead the title runs to one frame | **Portable** (`7b85afc1aa`, `4328d9d796`) | f1d37c5d |
| Faulted reads replayed on a second view | guards on title views; copy-back per submission | Idea for lazy copy-back: keep GPU-written pages read-guarded, serve a faulted read from the view XTP keeps (formats F3) | **Portable** (the person's) | 78d58a42 follow-up |
| Command memory in place | G15b (heaps over title memory) done; G16 (read in place) open | Use AnyPS5's result as a warning: in place forces the host to follow the GPU, and it measured slower; G16 should measure the same trade | **Portable** | ce8bcd0a |
| Retiling in VRAM, then one copy | CPU copies | See §6.2 | **Portable** | 522e7cb5 |
| Labels deferred around GPU writes, idle submit | G8 asynchronous submission | Later, once the world frame is GPU-bound: look for drains caused by fences sharing a page with GPU-written data | **Portable** | 522e7cb5 |
| In-run A/B | sessions compared one against another | **Yes** for measurements: same session, same scene, settings switched at presents | **Portable** (`2eadda9b42`) | tooling |
| VMA sub-allocation | heaps reserved ahead of need (450ec9ba, 74bc23b5) | No: XTP now does the same for D3D12's VidMm | — | — |
| Write-fault blocks, userfaultfd, tmpfs huge pages | write watch in 64 KiB units on Windows | No | — | — |

Frame pacing proper (vblank rate, FPS limit) is AnyPS5's VideoOut, outside
the graphics core, and has nothing for XTP.

## 8. Method

- Provenance: `git log` over each file of interest in the blobless clone,
  each commit classed as the import (`e6570df909`), the person (by the
  identity of the person's GitHub account) or a contributor; the
  licences from `LICENSE`, `README.md`, `docs/dev/THIRD_PARTY_KYTY.md`,
  SPDX headers, and KytyPS5's README and licence (GitHub API).
- Mechanisms: read in the source at `7de61e3451`, with the commit messages
  of the wave split, the checks and the memory changes.
- XTP side: `docs/wave64-on-wave32.md`, the mesh64 task (0a31f0cb),
  `docs/analysis/shader.md`, `drops.md`, `formats.md`, `stall.md`,
  `world-frame.md` and `crates/xtp-shader/src/emit/lower/`.
  `gpu-frame.md` does not exist; `gpu.md` and `gpu-measure.md` were the
  nearest and had nothing that changes §7.
- Not done: running AnyPS5, measuring anything, and reading AnyPS5's ray
  tracing, tessellation and pipeline-library code, which map to no XTP
  question here.
