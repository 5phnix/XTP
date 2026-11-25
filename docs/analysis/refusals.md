# Refusals: every program of the pack, by class

Task census (7d248d0b), 2026-11-25, analysis only. The title is BTP 6,
build 1.0.2.1; programs are named by hash only (the runtime's hash: the
first 16 hex digits of the BLAKE3 of the container, as the logs print it).
Runs, logs, dumps and survey files stay under `E:\xtp-data\`, none in the
repository. Times are local (UTC+1) unless a log line says otherwise.

The person's goal, as the orchestrator relayed it at 01:20: **every
program of the pack translates**, whatever it draws; not only the world's,
and not in order of image impact. "Runs in the world" stays as a column,
for information. The census of the world's refusals by image impact (the
first brief) is kept in sections 6 to 8.

## 1. Summary

- **2,265 of the pack's 26,698 programs do not translate on a 32-lane host**
  (`--host-lanes 32-32`, the L4 and the RTX 4080): 423 compute, 555 mesh,
  843 pixel, 54 vertex, and the 390 halves of merged ES + GS pairs, which
  the translator does not attempt. They fall into 24 classes (section 2).
  The orchestrator's 2,101 matches no survey found here; 1,875 are refused
  when the ES + GS pairs are left out.
- **Five classes hold 86 % of them:** "a scalar memory read as data" (998,
  every stage), merged ES + GS (390), "a lane mask read as data" (281
  graphics programs, 5 compute), "a structured buffer of unknown stride"
  (204 compute and pixel) and "a descriptor read only as data" (72).
- **One of the five is already solved behind a switch:** with
  `XTP_STRIDE_TABLE=1` (stride's port of VIctory #193 and #704, on `main`)
  178 of the 204 unknown-stride programs translate and validate; the 26
  others move on to lane-mask classes. Turning it on needs one session
  first (section 8).
- **Methods and files per class (section 3), and twelve new tasks beside
  meshdata (section 4):** six can start now with no file in common
  (masks, wave64, pixel inputs, scalar gaps, the stride table, validator
  rejections); four wait for a holder (rawfix's `smem.rs` and
  `descriptor.rs`, n6b's `flow.rs`, rects' `pipeline/host.rs`);
  `resource.rs` goes to meshdata, then per-lane descriptors, then title
  memory by address, in that order.
- **D3D12 makes no class impossible to translate, but five things need a
  stand-in that is exact only under conditions (section 5):** raw GPU
  addresses (no pointers in DXIL), descriptor bits (opaque in D3D12),
  64-lane waves on 32-lane hosts, 64-lane masks in pixel and vertex
  programs (no group-shared memory to fetch the other half), and mesh
  shader output limits (53 mesh programs exceed 32 KiB of outputs).
- **Not in the pack, counted apart:** 34 compute programs ("an address in
  an SGPR that is no root parameter") come from the system library that
  builds the ray tracing structures (the BVH builder the title loads), not
  from the title's pack; refused in every world session, invisible to the
  pack survey (W12).
- **The world, for information (section 6):** the G-buffer's missing
  street geometry is mostly "a descriptor read from the first lane of
  computed VGPRs" (foliage, distant LODs, generic meshes) and lane masks
  (characters), more than "scalar memory read as data", which costs the
  characters' shadows; instance culling is refused in every view
  ("unknown stride"); about 365 ray-tracing resolves a frame wait in the
  heavy pipeline queue (not refusals).
- **Pick 12 leaves the 23 "groups of more than 64 threads" programs because
  each uses `v_mbcnt`, `v_readfirstlane` or a mask read as a number,
  which its conditions exclude (section 7).**

## 2. The census

Survey: `xtp shader-survey <pack> --host-lanes 32-32 --json <file>` on the
cloud L4. The translator on `main` (1c61fa1, after bee231f) is that of
bacfcc6: `crates/xtp-shader` changed since only by `build.rs`,
`fingerprint.rs` and `lib.rs` (the sources' fingerprint for the shader
cache's key: a constant and a test module), and `apps/xtp/src/survey.rs`
not at all. The survey of 1c61fa1 on the L4 gives 24,433 translated
and 2,265 refused, with the same outputs hash (`9255acc3…`) and the same
causes as present's survey of bacfcc6 (section 9); the per-program lines
below come from the latter's JSON (`D:\xtp-data\present-after2.json` on
the VM), read before mine had run. Example hashes come from joining the survey's records with
precook's list of the pack's records (`E:\xtp-data\precook\list-b.txt`:
record, hash); world programs first. "Seen in the world": programs of the
class that the world sessions of section 6 dropped (their builds were a
little older, so a few have moved class since).

| # | Refusal (survey cause) | Item | Programs | CS | MS | PS | VS | ES | GS | Examples | Seen in the world |
|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---|---:|
| 1 | a scalar memory read as data | R1 | 998 | 66 | 383 | 516 | 33 |  |  | `910b4be059bfb28f`, `321d411b20a35ef9`, `4e35b816f2f91d68` | 28 |
| 2 | a merged ES + GS program (not attempted) | G2 | 390 |  |  |  |  | 195 | 195 | `595056121e4b69d2`, `a1430adfd0332446`, `4017ab4a141874cd` | 0 |
| 3 | a lane mask read as data | L4 | 281 |  | 101 | 169 | 11 |  |  | `ef5409053e14aa82`, `093c64546c55554e`, `a402e650d0acbc92` | 6 |
| 4 | a structured buffer of unknown stride | R3 | 204 | 202 |  | 2 |  |  |  | `e30970f1727063fd`, `55679a0547649d0b`, `17af15c4015cb4b6` | 42 |
| 5 | a descriptor read only as data | R2 | 72 | 3 | 5 | 63 | 1 |  |  | `6b6db9f1a526daeb`, `2fd01fca16993001`, `ae8d95a27b954ab3` | 2 |
| 6 | an address not from a root parameter | R4 | 64 | 64 |  |  |  |  |  | `caf96a3229145fa6`, `f607780aa8e502d0`, `6d7fe7745ab9c6d8` | 9 |
| 7 | console wave64 lanes in a group of more than 64 threads | L2 | 43 | 43 |  |  |  |  |  | `b22a16a178976e1b`, `a1bf269d5b19f025`, `5c6224a6bc3a4066` | 4 |
| 8 | a descriptor chosen by control flow | R4 | 41 | 1 | 3 | 37 |  |  |  | `f4fd42dc10ebd74d`, `efce39ec51c443c0`, `925d917ce78662eb` | 0 |
| 9 | a descriptor read from the first lane of computed VGPRs | R4 | 38 |  | 34 |  | 4 |  |  | `adc5c689b0552cc2`, `903776ba469c8bac`, `89a622617919c92a` | 3 |
| 10 | the first half of an interpolation | P2 | 28 |  |  | 28 |  |  |  | `5ddcaa08d4a51215`, `464ea474374b8ecd`, `38bf0814e721a1b3` | 0 |
| 11 | a lane mask holding lanes elsewhere read as data | M2 | 20 | 20 |  |  |  |  |  | `3bc3bfd1cec35a7f`, `ffa98fdac87e91cb`, `04f90cb19f940759` | 0 |
| 12 | a waterfall loop whose compares do not select the access's lanes | R4 | 15 |  | 12 |  | 3 |  |  | `a91084c089f1d02f`, `ffef6fc85b3c948e`, `4499fa2ae03d0c81` | 0 |
| 13 | a hardware SGPR input | S3 | 15 |  |  | 15 |  |  |  | `f0296ce9fbe481cb`, `c1dacd75698a5db8`, `aafcf40b30b9df18` | 0 |
| 14 | a constant lane mask | M1 | 13 |  |  | 13 |  |  |  | `4afdc95a666116d7`, `b84ed91212991569`, `2a12fab309f26d20` | 0 |
| 15 | a first-lane read of a value lanes may hold differently (wave64 mesh group) | — | 11 |  | 11 |  |  |  |  | `45f081396c2c59ba`, `c89489afd89ee7f0`, `0bd4d80ad23b106b` | 1 |
| 16 | a descriptor partly joined by control flow | R4 | 8 |  | 6 |  | 2 |  |  | `fe660f2d6d57de76`, `9087520fdbab22ca`, `5c7da08ced013cdf` | 1 |
| 17 | a lane mask read as data | M2 | 5 | 5 |  |  |  |  |  | `90a0692af4320129`, `82a2ad6d07417d36`, `9ffcaff533995118` | 2 |
| 18 | v_cvt_f32_f64 | S3 | 5 | 5 |  |  |  |  |  | `6871d6e4b3ac9a10`, `5dc3b25986622949`, `6f225b641c5b0708` | 0 |
| 19 | a lane-mask join of an untracked mask | M1 | 5 | 5 |  |  |  |  |  | `c309eb1b2234e63c`, `de0d2103f217616e`, `2c55789a32b5aec7` | 0 |
| 20 | a whole-wave section of a wave64 on a host without 64-lane waves | L1 | 4 | 4 |  |  |  |  |  | `e18931c72af42e46`, `53c21e950d143cc7`, `25328ea424d08c00` | 1 |
| 21 | a whole-wave section step the simulation does not follow | M2 | 2 | 2 |  |  |  |  |  | `526c84818961beb6`, `697f2629f7dc5be0` | 0 |
| 22 | v_permlanex16_b32 | S3 | 1 | 1 |  |  |  |  |  | `a929109552ce1e84` | 0 |
| 23 | an address with an untracked high half | — | 1 | 1 |  |  |  |  |  | `aab1b3c7248ee5de` | 1 |
| 24 | a descriptor's address or format read as data | R2 | 1 | 1 |  |  |  |  |  | `11eeb27c2d15980d` | 1 |
| | **all** | | **2265** | 423 | 555 | 843 | 54 | 195 | 195 | | 101 |

**Counted apart: 34 programs outside the pack.** "An address in an SGPR
that is no root parameter" (compute, 34 programs) comes only from the
system library that builds the ray tracing structures (the BVH builder
the title loads beside its executable), not from the title's pack: none
of the 34 hashes is among the pack's records (precook's list), and the
survey never meets the class. They run in every world session (the BVH
list, section 6.2; about 250 dispatches a frame with the impostor
transforms) and are W12's.

## 3. Method and files, class by class

"Fork" is fxceless's VIctory fork (GPL-3.0, its owner's commits portable
with credit; private study `E:\xtp-data\upstream\fork-fxceless.md`).
"shadPS4" is what task 04f38afd is studying (its report is not in yet:
the entries say what its source is known to do, to be confirmed there).
"Ours" means no outside design to take. Files are under
`crates/xtp-shader/src/` unless a path says otherwise; the refusal's own
site is the first file.

| # | Refusal | Method (source) | Files | Task (§4) |
|---:|---|---|---|---|
| 1 | a scalar memory read as data | fork `dead` and `view_address` (578d459, portable); shader.md R1 (sort by where the address comes from; a root CBV or SRV address read raw from that root view); shadPS4: user data read through pointers flattened on the host before the draw (04f38afd to confirm) | `resource.rs`, `resource/`, `emit/lower/smem.rs` | meshdata (in progress; smem.rs after rawfix) |
| 2 | a merged ES + GS program (not attempted) | our own (shader.md G1 + G2: the ES then the GS in one mesh-shader group, the ESGS ring in group-shared memory, emit and cut as strip outputs); xcl for behaviour only | `native.rs`, `io.rs`, `emit/shader.rs`, new `emit/lower/geometry.rs`, `emit/mod.rs`, `apps/xtp/src/survey.rs`, `crates/xtp-gpu/src/pipeline/host.rs` | W10 |
| 3 | a lane mask read as data | the fork's unspill (parked masks) is in (819ef9b); the rest is ours: shader.md M1 (keep the mask as a number, each lane tests its own bit by its console lane), and for mesh programs a group-shared exchange of the two halves' ballots (mesh groups have group-shared memory); pixel and vertex programs have none (§5) | `emit/lower/mask.rs`, `emit/lower/compare.rs`, `structure.rs` | W1 |
| 4 | a structured buffer of unknown stride | VIctory #193 and #704, ported by stride (54ab53d), behind `XTP_STRIDE_TABLE=1`: with it 178 of the 204 translate and validate, 26 move on to lane-mask classes (18 "lane mask read as data", 7 "lane-mask join of an untracked mask", 1 "lanes elsewhere") | `apps/xtp/src/survey.rs` (count with the table), `crates/xtp-gpu/src/device.rs` (the default) | W5, after one session (§8) |
| 5 | a descriptor read only as data | fork `view_address` with `USER_VIEW_ADDRESS` (the host passes a view's address as a user-data dword); shader.md R2 (the title's descriptor bytes bound as one raw view); D3D12 limit in §5 | `emit/lower/smem.rs` (`descriptor_data`), `emit/host.rs`, `crates/xtp-gpu/src/draw.rs` (shared) | W7 |
| 6 | an address not from a root parameter | fork `by_address` (title memory by address through a page directory, fork study §3.3); shadPS4 reads such memory through buffer device addresses, which D3D12 lacks (§5) | `resource.rs`, new `emit/lower/memory.rs`, new `crates/xtp-gpu/src/memory.rs`, `draw.rs` (shared) | W12 |
| 7 | console wave64 lanes in a group of more than 64 threads | pick 12 (ours, landed) extended to `v_mbcnt`, `s_bcnt1` of a 64-lane compare and `v_readfirstlane` through its exchange, then loops (§7); the fork's exact forms (`wave_base`, `exchange_pair`, 578d459 + 6a2d9f9) for groups whose waves are not consecutive | `emit/lower/lanes.rs`, `emit/lower/mod.rs`, `emit/wave.rs`, `emit/split_tests.rs` | W2 |
| 8 | a descriptor chosen by control flow | fork `descriptor_phi` (nested joins); shader.md R4's fallback: one non-uniform access whose index each lane reads | `emit/lower/descriptor.rs` | W8 |
| 9 | a descriptor read from the first lane of computed VGPRs | fork `lane_copies` (a descriptor the compiler keeps per lane in VGPRs is its load; 578d459) | `resource.rs` | W11 |
| 10 | the first half of an interpolation | fork `Interp::Weighted`, `eval_channel`, `per_vertex` (each half of an interpolation evaluated on its own, 578d459); shader.md P2 (SM 6.1 `GetAttributeAtVertex`) | `emit/lower/input.rs`, `io.rs` | W3 |
| 11 | a lane mask holding lanes elsewhere read as data | the fork's exact wave64 forms (`MASK_ELSEWHERE`); ours: inactive lanes count as 0 bits (shader.md M2, part in 69a2c56) | `emit/lower/mask.rs` | W1 |
| 12 | a waterfall loop whose compares do not select the access's lanes | ours (the waterfall tracer of `resource.rs`); shader.md R4 | `resource.rs` | W11 |
| 13 | a hardware SGPR input | ours (shader.md S3) | `emit/lower/input.rs` | W3 |
| 14 | a constant lane mask | ours (shader.md M1) | `emit/lower/mask.rs` | W1 |
| 15 | a first-lane read of a value lanes may hold differently (wave64 mesh group) | ours: the mesh group's group-shared memory carries the console wave's first lane to both halves | `emit/lower/mesh.rs` | W2 |
| 16 | a descriptor partly joined by control flow | fork `descriptor_phi` and `only_edge` (present ported `only_edge`/`anywhere`, dd11271) | `resource.rs` | W11 |
| 17 | a lane mask read as data | the fork's unspill (parked masks) is in (819ef9b); the rest is ours: shader.md M1 (keep the mask as a number, each lane tests its own bit by its console lane), and for mesh programs a group-shared exchange of the two halves' ballots (mesh groups have group-shared memory); pixel and vertex programs have none (§5) | `emit/lower/mask.rs`, `emit/lower/compare.rs`, `structure.rs` | W1 |
| 18 | v_cvt_f32_f64 | ours (shader.md S3; D3D12 doubles, `DoublePrecisionFloatShaderOps`) | `emit/lower/valu.rs` | W4 |
| 19 | a lane-mask join of an untracked mask | ours (shader.md M1) | `emit/lower/flow.rs` | W9, after n6b |
| 20 | a whole-wave section of a wave64 on a host without 64-lane waves | ours (shader.md L1) | `emit/lower/mod.rs`, `emit/lower/lanes.rs` | W2 |
| 21 | a whole-wave section step the simulation does not follow | ours (shader.md M2) | `emit/lower/lanes.rs` | W2 |
| 22 | v_permlanex16_b32 | ours (shader.md S3) | `emit/lower/lanes.rs` | W2 |
| 23 | an address with an untracked high half | fork: the high half followed through `only_edge` (its `live`, not ported; world-black.md §9.5) | `resource.rs` | W11 |
| 24 | a descriptor's address or format read as data | fork `view_address` (`USER_VIEW_ADDRESS`); D3D12 limit in §5 | `emit/lower/smem.rs` | W7 |

## 4. Tasks that can run side by side

Each task below owns its files; no two tasks of one wave share a file.
Files are under `crates/xtp-shader/src/` unless a path says otherwise.
Each task also adds its own `<name>_tests.rs` (a new file, and its `mod`
line in `emit/mod.rs` or `resource.rs` as a mechanical fix). Each one's
acceptance: its classes' programs translate and validate on 32-lane hosts
(`xtp translate --validate --pipelines --host-lanes 32-32` on them), a
survey before and after on the L4 with no translation lost or changed
outside the class, and the workspace check.

Held today (board, 01:30): meshdata `resource.rs`, `resource/`,
`emit/smem_data_tests.rs`; rawfix `emit/lower/{buffer,descriptor,smem}.rs`,
`emit/raw_tests.rs`; n6b `emit/lower/flow.rs`; the shadow-sampler task
(053c743d, todo) `emit/lower/descriptor.rs`, `emit/sampler_tests.rs`;
rects `crates/xtp-gpu/src/pipeline/host.rs`, `emit/rect_gs.rs`.

### Wave 1: now, no file held by anyone

| Task | Classes | Files |
|---|---|---|
| **W1 masks** | lane mask read as data (all stages), lanes elsewhere read as data, constant lane mask | `emit/lower/mask.rs`, `emit/lower/compare.rs`, `structure.rs`, `ir/unspill.rs` |
| **W2 wave64** | groups of more than 64 threads, whole-wave sections and their steps, the wave64 mesh first-lane read, `v_permlanex16_b32` | `emit/lower/lanes.rs`, `emit/lower/mesh.rs`, `emit/wave.rs`, `emit/lower/mod.rs` (the refusal sites at 590–610), `emit/split_tests.rs` |
| **W3 inputs** | first half of an interpolation, computed barycentrics, barycentrics read directly, hardware SGPR input, conditional export | `emit/lower/input.rs`, `emit/lower/export.rs`, `io.rs` |
| **W4 scalar gaps** | `v_cvt_f32_f64`, `s_getpc_b64` | `emit/lower/valu.rs`, `emit/lower/salu.rs` |
| **W5 stride on** | structured buffer of unknown stride | `apps/xtp/src/survey.rs` (a `--stride-table` switch, so the survey counts what the table translates), `crates/xtp-gpu/src/device.rs` (on by default once the session of §8 passes) |
| **W6 validator** | the validator's refusals (a survey with `--validate` first) | `emit/container.rs`, `emit/shader.rs`, `emit/validator.rs` |

### Wave 2: after a holder delivers

| Task | Classes | Files | After |
|---|---|---|---|
| **meshdata** (in progress) | scalar memory read as data, all stages | `resource.rs`, `resource/`, then `emit/lower/smem.rs` | its own; smem.rs after rawfix |
| **W7 descriptor bits** | descriptor read only as data, a descriptor's address or format read as data, a constant buffer's descriptor read as data | `emit/lower/smem.rs` (`descriptor_data`), `emit/host.rs`; the host's user-data dword in `crates/xtp-gpu/src/draw.rs` (shared: add lines only) | rawfix (smem.rs), then meshdata's smem.rs part |
| **W8 chosen descriptors** | a descriptor chosen by control flow | `emit/lower/descriptor.rs` | rawfix, then 053c743d (both hold descriptor.rs) |
| **W9 untracked joins** | a lane-mask join of an untracked mask | `emit/lower/flow.rs` | n6b (flow.rs) |
| **W10 ES + GS** | merged ES + GS (shader.md G1, G2) | `native.rs`, `io.rs`, `emit/shader.rs`, a new `emit/lower/geometry.rs`, `emit/mod.rs` (the entry point), `apps/xtp/src/survey.rs` (it skips the pairs today), `crates/xtp-gpu/src/pipeline/host.rs` | W3 (io.rs), W5 (survey.rs), W6 (shader.rs), rects (pipeline/host.rs) |

### Wave 3: the `resource.rs` line

`resource.rs` is one file and three tasks need it, in this order:

1. **meshdata** (scalar memory read as data): in progress.
2. **W11 per-lane descriptors**: a descriptor read from the first lane of
   computed VGPRs (the fork's `lane_copies`), a waterfall loop whose
   compares do not select the access's lanes, a descriptor partly joined
   by control flow, a descriptor neither loaded nor built, an address with
   an untracked high half. Files: `resource.rs`, `resource/`.
3. **W12 title memory by address**: an address not from a root parameter
   (and from a phi or a first lane, where any is left), an address in an
   SGPR that is no root parameter (the BVH builder's 34, from the system
   library and not the pack: count them from a session's log).
   Files: `resource.rs` (the `by_address` arm), a new `emit/lower/memory.rs`,
   and on the host a new `crates/xtp-gpu/src/memory.rs` (the page
   directory) plus its user-data lines in `draw.rs` (shared).

`flow.rs` has one task waiting on it (W9, after n6b).

## 5. What D3D12 cannot do, and what standing in for it costs

No class above is impossible to translate. Five things have no direct
D3D12 equivalent; each needs a stand-in that is exact only under the
condition given.

1. **Raw GPU addresses** ("an address not from a root parameter", 64; the
   system BVH builder's 34, outside the pack). DXIL has no pointer: no buffer
   device address as Vulkan has, on which shadPS4 builds its reads of
   arbitrary memory. The stand-in maps title memory into views the shader
   indexes: the fork's page directory (`by_address`, fork study §3.3),
   pages without a host copy reading 0 and faulting for the next frame.
   Exact only for pages XTP keeps current; the first frame that reaches a
   new page reads 0 there.
2. **Descriptor bits** ("a descriptor read only as data" 72, "a
   descriptor's address or format read as data" 1). A D3D12 descriptor is
   opaque: no base address, format or tiling to read. Sizes are rebuilt
   already (`descriptor_data`); for the rest the fork passes a made-up
   address as user data (`view_address`). Exact when the program compares
   or offsets the address; not when it reads memory from it (then it is
   case 1).
3. **64-lane waves on a 32-lane host** (every wave64 class). D3D12 has no
   `WaveSize(64)` where `WaveLaneCountMax` is 32 (NVIDIA). Compute and mesh
   programs can rebuild a console wave from two host waves through
   group-shared memory (pick 12's form, mesh64's); the cost is a barrier
   per exchange, and a form per pattern with its proof.
4. **Pixel and vertex programs that read a 64-lane mask as a number**
   (part of "a lane mask read as data": 169 pixel, 11 vertex). These
   stages have no group-shared memory and no way to choose their wave
   size, so the other 32 lanes of a console wave are out of reach. Exact
   only where the number's use does not depend on them: a parked mask
   (unspill, in), a lane testing its own bit (M1), a count that only
   decides "any" or "all" where both halves agree. Where a program really
   counts lanes across the halves (a quad or coverage count over 64
   lanes), no D3D12 pixel shader can do it; such programs would need
   another pass (shader.md L4 does it for mesh programs, not for pixels).
   W1 must count how many are of that kind before promising them.
5. **Mesh shader limits.** A D3D12 mesh shader group has at most 256
   vertices, 256 primitives and 32 KiB of vertex plus primitive outputs.
   In stride's validated survey (84ff9e4 with the table, every
   translation checked by `dxil.dll`), **53 mesh programs** were rejected
   for "vertex plus primitive output size is greater than 32768". They
   translate and do not run. Packing outputs tighter (16-bit attributes,
   outputs no pixel program reads) may bring some under; the others need
   their group split in two (W6 measures which).

The validator's other 105 rejections in that survey are ours to fix
(signature parts that do not match the module: 51; interpolation modes
packed into one row: 54), W6.

Merged ES + GS (390) is not on this list: D3D12 has geometry and mesh
shaders, and shader.md's G2 lowers the pair into one mesh shader.

## 6. The world, for information

The "runs in the world" column of section 2 comes from three Series S
sessions and their per-pass views; this section says what those show. It no
longer sets the order of the work (section 4 does), but it says where each
class costs the image today.

### 6.1 Sessions read

| Session | Build | Switches | What it gives |
|---|---|---|---|
| dispatchx-a, `E:\xtp-data\cache\play\20261125-000534` | dispatchx's branch on `main` 99b6376 (23:41): **without** mesh-fix's `code_join`/`code_lane` (e1b75d7, on `main` with fa9dd78 at 23:59), stride's (54ab53d) or present's fixes (bd0cc66) | `XTP_GPU_TIMESTAMPS=1` only: no `XTP_RAW_STRUCTURED`, no `XTP_SKIP_PIPELINES` | 171 refused programs; 11 frame reports in the world (60 s, 3 to 11 fps) with the drops per program |
| present-1, `…\20261124-234706` and `…\20261124-234824` | bacfcc6 (present's build: pick 12 and the descriptor fix) | `XTP_RAW_STRUCTURED=1`, `XTP_SKIP_PIPELINES=577248728544d74b`, stride table off | 180 refused (both runs); per-pass views of frames 677, 687, 697, 683, 693, 703 (`E:\xtp-data\runs\present-1`) |
| stride-3e, `…\20261124-231449` | stride's 62f8765 | as present-1 | 157 refused; per-pass views of frames 710, 730, 749, 750 (`E:\xtp-data\runs\stride-3e`) |

The brief called dispatchx-a "present's build plus the image switches"; its
`run.log` and `E:\xtp-data\runs\stall-dispatchx-a\env.txt` say otherwise
(above). Its refusal list is therefore older than `main`.

**The eight "address from a phi" and two "first lane of a per-lane value"
refusals of dispatchx-a** (`098fc28e44347a00`, `21d27f785da6d2b1`,
`77a7af30c430cd16`, `982f3cccd5c04294`, `9aa4a785e0800cda`,
`b3e7781d53ff8326`, `d9978c76c746d809`, `f7c4f227a2a4c4b2`;
`5ed77e9e8f084dea`, `0a5d4f9047b3dce4`), which mesh-wave64.md §9.2 gives
as translated since e1b75d7:
- **offline they translate**: none of the ten is among the refused
  records of the `main` translator's survey (bacfcc6, section 2), and
  present-1 and stride-3e, whose builds have e1b75d7, log none of them;
- **the session's build did not have e1b75d7**: dispatchx's commits
  (9e1390b, 93d7b5d, d9bd860, b1ad979) sit on 99b6376, older than fa9dd78;
  `git merge-base --is-ancestor e1b75d7 <commit>` is false for each;
- **not a stale cache**: the session's shader cache served nothing
  ("shaders from the cache and translated so far hits=0 misses=1226"), and
  each of the ten refusals took 6 to 14 ms, a translation made in the
  session: the refusals are that build's own. Precook's list of 00:03
  (`list-b.txt`) refuses them too; which commit its build was on was not
  checked.

`2da4beb386509b77` is refused there for the same reason (no bd0cc66).

How a dropped op was tied to its program: the per-pass view lists each
dropped op with its pipeline's address; the session's `first bind` lines
give each pipeline's programs (every address resolved, none unknown). The
per-pass view puts a dropped op in the pass of the list its thread last
recorded for (world-black.md §3), so the pass is the right list and the
neighbouring work, and the dropped op's own targets are those of that pass
only when it is a draw. The frame reports print 40 lines (7 or so are the
per-cause totals), so a program dropped less than about 4 times a frame is
cut from them: the means below are lower bounds, and the per-pass views
fill the gap.

### 6.2 The frame's lists, and what is dropped in each

Frame 677 of present-1 (85 lists), with the roles read from the programs
that run in each list (named by hash only; the roles are ours):

| Lists | Role | Targets | Dropped there (per frame) |
|---|---|---|---|
| L1–L13 | instance culling, one list per view (13 views) | buffers (instance lists, indirect arguments) | the batch-culling programs of props, trees, tree impostors, simple meshes, distant LODs, grass, props not culled: `17af15c4015cb4b6` ×13, `55679a0547649d0b` ×13, `89d08e561897d4e5`, `42f717f1b4d18e94`, `1c4b3c0547e3e6fa`, `9209777e8face9dd`, `fdd56d97a806bcca` (each "structured buffer of unknown stride") |
| L15 | depth prepass and motion | D32S8 1280×720 (DXGI 19), R16G16F 1280×720 | nothing |
| L21 | a compute graph (matrix products) | buffers | `b22a16a178976e1b` ×42 (wave64, group > 64) |
| L25 | character skinning | vertex buffers | `36e010b53acffd77` ×9, `2bba29bbd7c019bc` (scalar memory read as data) |
| L26 | ray tracing structures: skinning for the BVH, instance fix-ups, the BVH builder | buffers | 633 of 669 ops: the BVH builder's 34 programs ("an address in an SGPR that is no root parameter", not in the pack: the system library that builds the ray tracing structures) and `e30970f1727063fd` ×85 (impostor transforms, "unknown stride") |
| L30 | G-buffer, opaque meshes and characters | RGBA8 ×4 (DXGI 28), RGBA8 typeless (27), R16G16F (34), D32S8, all 1280×720 | 70 of 223 in P293: characters' mesh programs `ef5409053e14aa82` ×40, `093c64546c55554e` ×12 (lane mask read as data), `5bd95fdec765c0d6` ×10, `f841ad74ec6e7367` ×3, `c3ba22ec6fc5dd4a` (scalar memory read as data), pixel `a402e650d0acbc92` ×5 (lane mask) |
| L31 | G-buffer, foliage, distant LODs and generic meshes | the same G-buffer (P279 of frame 683) | 137 of 147: `adc5c689b0552cc2` ×35, `903776ba469c8bac` ×27, `89a622617919c92a` ×24 (a descriptor read from the first lane of computed VGPRs), vertex `4e35b816f2f91d68` ×24 and `b3ba0ef9ee352720` (scalar memory read as data), `fe660f2d6d57de76` ×13 (descriptor partly joined), `45f081396c2c59ba` ×13 (wave64 mesh first-lane read) |
| L32–L37 | G-buffer (terrain, opaque meshes) | the same | nothing |
| L39 | GPU particles | buffers | `caf96a3229145fa6` ×27 (particle collision against the ray tracing structure: "an address not from a root parameter") |
| L40 | G-buffer (more meshes) | the same | `9aff566ca0fa7c98` ×6, `c0f0db84d228f441` ×5, pixel `e819d0b31c213166` ×2 (scalar memory read as data) |
| L46–L55 | shadow maps: 1536², 768², 512×384, 256² | D16 (DXGI 53) | skinned depth `910b4be059bfb28f` ×136 and `321d411b20a35ef9` ×104 in this frame (scalar memory read as data): characters cast no shadow |
| L56 | puddle detection | R32F 64×64, D16 64×64 | pixel `6b6db9f1a526daeb` ×7, `2fd01fca16993001` (descriptor read only as data) |
| L67 | alpha prepass, scattering | 1280×720 R8 / R32F | pixel `97175c2c1a214a2f`, `a1bf269d5b19f025` (wave64, group > 64) |
| L68, L77 | lighting: scattering, irradiance cache, screen probes, ray-traced reflections, sky | 640×360 R11G11B10F, 1280×720 RGBA16F | `6d7fe7745ab9c6d8` (address not from a root parameter); about 280 to 370 ray-tracing resolve dispatches "being made" (below) |
| L69, L72 | forward and transparent | RGBA16F 1280×720 + depth | `5c6224a6bc3a4066` (wave64, group > 64), pixel `73644c6a0b7b5b35` (scalar memory read as data) |
| L70–L85 | colour pyramid (`2da4beb386509b77` runs), temporal, depth of field, bloom, grading, tone mapping, UI | | nothing refused |

What it means for the image:

- **The G-buffer and the depth lose the street's foliage, distant LODs and
  generic meshes (L31) and the characters (L30), not only the "scalar
  memory read as data" class.** In L31 the three heaviest are "a
  descriptor read from the first lane of computed VGPRs" (mesh, the fork's
  `lane_copies`, §3.2 of the fork study); no task holds that class.
  meshdata's brief (d687ecdb) names the 18 "scalar memory read as data"
  mesh programs; in the G-buffer those are the characters (L30, L40) and
  two vertex programs of L31. Its skinned-depth pair (`910b4be059bfb28f`,
  `321d411b20a35ef9`) is in the shadow maps, not in the G-buffer.
- **Instance culling is refused in every view.** The seven batch-culling
  programs stop at "a structured buffer of unknown stride". With
  `XTP_STRIDE_TABLE=1` alone, worldlight-16 saw the material resolve's
  G-buffer target cover 100 % of the screen instead of about 20 %
  (world-black.md §6.2); the depth prepass (L15), which drops nothing,
  holds only 8 to 11 % of the screen. Whether the culling's missing
  outputs are why the G-buffer lists that run whole (L32–L37: 810 ops)
  add no depth (15.2 % before and after them) was not traced op by op.
- **Ray tracing and the BVH:** the 34 builder programs and the impostor
  transforms are the largest drop by count (about 250 a frame) and the
  least visible: buffers only, read by the ray-traced lighting.
- **"Being made (heavy)", 365 indirect dispatches a frame in dispatchx-a,
  is not a refusal.** In the per-pass views those dispatches are the
  ray-traced hit-shading ("resolve") programs of every material, in the
  lighting lists (L68, L77): 276 of the 279 not-refused drops of frame
  683, 374 of 379 in stride-3e's frame 730. They are heavy pipelines (1 to
  14 s each to make) queued for the 2 heavy workers ("waiting 191 to 203
  heavy" in every pool line of the session). A precook of the compute
  pipelines empties that queue before a session (`xtp precook`).

### 6.3 Ranked: the refused programs that run in the world

Order: G-buffer and its inputs, lighting, shadows, post-processing, then
particles and simulation, then ray tracing and the BVH. "Per frame" is the
mean over dispatchx-a's 11 world reports (a lower bound, see 6.1), then the
mean per dumped frame of present-1 and stride-3e, with how many of the 11
reports and 10 dumped frames show it. "Covered by": meshdata (d687ecdb),
stride's table (54ab53d, landed, behind `XTP_STRIDE_TABLE=1`), mesh-fix's
e1b75d7 (on `main`), or none, with the task of section 4 that would take
it. present's pick 12 (bd0cc66) covers none of them (section 7); rawfix
(ccf43697) changes how raw views are read, not what translates, apart
from the descriptor loads beside an `s_buffer_load` it now classifies
(refused before as "a scalar memory read as data", its comment of 01:00;
how many of the 998 that is was not measured); heaptex (8e968756) and rects (7fead5db) fix host-side work
(textures picked by heap index, the rectangles' fourth corner) and cover
no refusal here. Pass and target: the list and outputs of section 6.2.

| # | Program | Refusal | Per frame (dispatchx-a; dumps) | Pass | Writes | Covered by |
|---:|---|---|---|---|---|---|
| 1 | compute `55679a0547649d0b` | a structured buffer of unknown stride | 7.8 (8/11); 7.9 (9/10) | instance culling, every view (L1–L13) | buffers: instance lists, indirect arguments for the G-buffer and shadow draws | stride table (landed, switch off): W5 |
| 2 | compute `17af15c4015cb4b6` | a structured buffer of unknown stride | 8.0 (9/11); 6.8 (9/10) | instance culling, every view (L1–L13) | buffers: instance lists, indirect arguments for the G-buffer and shadow draws | stride table (landed, switch off): W5 |
| 3 | compute `1c4b3c0547e3e6fa` | a structured buffer of unknown stride | below the cut; 1.0 (9/10) | instance culling, every view (L1–L13) | buffers: instance lists, indirect arguments for the G-buffer and shadow draws | stride table (landed, switch off): W5 |
| 4 | compute `42f717f1b4d18e94` | a structured buffer of unknown stride | below the cut; 1.0 (9/10) | instance culling, every view (L1–L13) | buffers: instance lists, indirect arguments for the G-buffer and shadow draws | stride table (landed, switch off): W5 |
| 5 | compute `89d08e561897d4e5` | a structured buffer of unknown stride | below the cut; 1.0 (4/10) | instance culling, every view (L1–L13) | buffers: instance lists, indirect arguments for the G-buffer and shadow draws | stride table (landed, switch off): W5 |
| 6 | compute `9209777e8face9dd` | a structured buffer of unknown stride | below the cut; 1.0 (9/10) | instance culling, every view (L1–L13) | buffers: instance lists, indirect arguments for the G-buffer and shadow draws | stride table (landed, switch off): W5 |
| 7 | compute `fdd56d97a806bcca` | a structured buffer of unknown stride | below the cut; 1.0 (7/10) | instance culling, every view (L1–L13) | buffers: instance lists, indirect arguments for the G-buffer and shadow draws | stride table (landed, switch off): W5 |
| 8 | mesh `adc5c689b0552cc2` | a descriptor read from the first lane of computed VGPRs | 31.5 (8/11); 28.8 (6/10) | G-buffer: foliage (L31) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W11 |
| 9 | mesh `ef5409053e14aa82` | a lane mask read as data | 19.2 (8/11); 28.3 (6/10) | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W1 |
| 10 | mesh `903776ba469c8bac` | a descriptor read from the first lane of computed VGPRs | 16.4 (8/11); 16.7 (6/10) | G-buffer: meshes (L31, L40) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W11 |
| 11 | vertex `4e35b816f2f91d68` | a scalar memory read as data | 19.7 (8/11); 12.7 (9/10) | G-buffer: distant LODs (L31) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata (class; its acceptance is the mesh ones) |
| 12 | mesh `45f081396c2c59ba` | a first-lane read of a value lanes may hold differently, where lanes of a wave64 mesh group branch differently, on a host without 64-lane waves | 13.8 (10/11); 11.4 (9/10) | G-buffer: distant LODs (L31) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W2 |
| 13 | mesh `89a622617919c92a` | a descriptor read from the first lane of computed VGPRs | 10.5 (8/11); 10.9 (9/10) | G-buffer: foliage (L31) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W11 |
| 14 | vertex `b3ba0ef9ee352720` | a scalar memory read as data | 10.5 (5/11); 5.5 (4/10) | G-buffer: distant LODs (L31) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata (class; its acceptance is the mesh ones) |
| 15 | mesh `093c64546c55554e` | a lane mask read as data | 5.8 (5/11); 8.5 (6/10) | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W1 |
| 16 | mesh `5bd95fdec765c0d6` | a scalar memory read as data | 5.3 (5/11); 7.0 (6/10) | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata |
| 17 | mesh `fe660f2d6d57de76` | a descriptor partly joined by control flow | 3.2 (4/11); 5.7 (9/10) | G-buffer: meshes (L31, L40) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W11 |
| 18 | pixel `a402e650d0acbc92` | a lane mask read as data | 2.5 (3/11); 3.8 (6/10) | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W1 |
| 19 | mesh `9aff566ca0fa7c98` | a scalar memory read as data | below the cut; 4.8 (4/10) | G-buffer: meshes (L31, L40) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata |
| 20 | mesh `c0f0db84d228f441` | a scalar memory read as data | 0.4 (1/11); 3.0 (9/10) | G-buffer: meshes (L31, L40) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata |
| 21 | mesh `4ee6e34d8e008c63` | a scalar memory read as data | 3.2 (2/11); not in the dumps | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata |
| 22 | pixel `e819d0b31c213166` | a scalar memory read as data | 1.0 (1/11); 1.7 (3/10) | G-buffer: meshes (L31, L40) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata (class; its acceptance is the mesh ones) |
| 23 | mesh `f841ad74ec6e7367` | a scalar memory read as data | below the cut; 2.2 (6/10) | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata |
| 24 | pixel `46395a33fdf2a3fa` | a lane mask read as data | below the cut; 2.0 (1/10) | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W1 |
| 25 | mesh `0979c5189155cf3f` | a scalar memory read as data | 1.7 (2/11); not in the dumps | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata |
| 26 | mesh `09a88636ff0d843c` | a lane mask read as data | below the cut; 1.0 (1/10) | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W1 |
| 27 | mesh `235e56869c1ea883` | a scalar memory read as data | 1.0 (1/11); not in the dumps | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata |
| 28 | mesh `c3ba22ec6fc5dd4a` | a scalar memory read as data | below the cut; 1.0 (3/10) | G-buffer: meshes (L31, L40) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | meshdata |
| 29 | pixel `a223e485dd8247a3` | a lane mask read as data | 0.7 (1/11); not in the dumps | G-buffer: characters (L30) | G-buffer 1280×720: RGBA8 (28) ×4, RGBA8 typeless (27), R16G16F (34), D32S8 (19) | none: W1 |
| 30 | compute `36e010b53acffd77` | a scalar memory read as data | 6.0 (6/11); 6.1 (9/10) | character skinning (L25) | vertex buffers the G-buffer and shadows draw | meshdata (class; its acceptance is the mesh ones) |
| 31 | compute `2bba29bbd7c019bc` | a scalar memory read as data | 8.4 (3/11); 2.0 (6/10) | character skinning (L25) | vertex buffers the G-buffer and shadows draw | meshdata (class; its acceptance is the mesh ones) |
| 32 | compute `a31407f77c16d796` | a scalar memory read as data | below the cut; 1.0 (1/10) | character skinning (L25) | vertex buffers the G-buffer and shadows draw | meshdata (class; its acceptance is the mesh ones) |
| 33 | compute `a1bf269d5b19f025` | console wave64 lanes in a group of more than 64 threads on a host without 64-lane waves | below the cut; 2.0 (9/10) | lighting: scattering (L67, L68) | 1280×720 and 640×360 light targets | none: W2 (pick 12 does not take it, §7) |
| 34 | compute `11eeb27c2d15980d` | a descriptor's address or format read as data | below the cut; 1.0 (7/10) | lighting (L68) | light lists, 1280×720 RGBA16F scene colour | none: W7 |
| 35 | compute `e18931c72af42e46` | a whole-wave section of a wave64 on a host without 64-lane waves | below the cut; 1.0 (7/10) | lighting (L68) | light lists, 1280×720 RGBA16F scene colour | none: W2 |
| 36 | mesh `910b4be059bfb28f` | a scalar memory read as data | 90.5 (11/11); 62.4 (9/10) | shadow maps (L46–L55) | D16 (53) 1536², 768², 512×384, 256² | meshdata |
| 37 | mesh `321d411b20a35ef9` | a scalar memory read as data | 43.2 (11/11); 33.4 (9/10) | shadow maps (L46–L55) | D16 (53) 1536², 768², 512×384, 256² | meshdata |
| 38 | pixel `6b6db9f1a526daeb` | a descriptor read only as data | 5.0 (8/11); 7.7 (9/10) | puddle map (L56) | R32F 64×64, D16 64×64 | none: W7 |
| 39 | pixel `73644c6a0b7b5b35` | a scalar memory read as data | below the cut; 2.0 (4/10) | transparency (L67–L72) | 1280×720 RGBA16F, tiles | meshdata (class; its acceptance is the mesh ones) |
| 40 | compute `2da4beb386509b77` | a descriptor neither loaded nor built | below the cut; 1.0 (2/10) | transparency (L67–L72) | 1280×720 RGBA16F, tiles | none: W11 |
| 41 | pixel `2fd01fca16993001` | a descriptor read only as data | below the cut; 1.0 (9/10) | puddle map (L56) | R32F 64×64, D16 64×64 | none: W7 |
| 42 | compute `5c6224a6bc3a4066` | console wave64 lanes in a group of more than 64 threads on a host without 64-lane waves | below the cut; 1.0 (6/10) | transparency (L67–L72) | 1280×720 RGBA16F, tiles | none: W2 (pick 12 does not take it, §7) |
| 43 | pixel `97175c2c1a214a2f` | a scalar memory read as data | below the cut; 1.0 (4/10) | transparency (L67–L72) | 1280×720 RGBA16F, tiles | meshdata (class; its acceptance is the mesh ones) |
| 44 | compute `caf96a3229145fa6` | an address not from a root parameter | 44.5 (10/11); 28.3 (9/10) | particles: collision against the BVH (L39) | particle buffers | none: W12 |
| 45 | compute `b22a16a178976e1b` | console wave64 lanes in a group of more than 64 threads on a host without 64-lane waves | 18.3 (9/11); 29.8 (8/10) | compute graph, matrix products (L21) | buffers | none: W2 (pick 12 does not take it, §7) |
| 46 | compute `098fc28e44347a00` | an address from a phi | 3.5 (6/11); not in the dumps | hair strands | strand buffers | fa9dd78 on `main` (translates in present-1) |
| 47 | compute `f7c4f227a2a4c4b2` | an address from a phi | 3.3 (6/11); not in the dumps | hair strands | strand buffers | fa9dd78 on `main` (translates in present-1) |
| 48 | compute `b3e7781d53ff8326` | an address from a phi | 2.8 (5/11); not in the dumps | hair strands | strand buffers | fa9dd78 on `main` (translates in present-1) |
| 49 | compute `02384a60fda6f2d0` | console wave64 lanes in a group of more than 64 threads on a host without 64-lane waves | below the cut; 1.0 (9/10) | VFX memory (L14) | buffers | none: W2 (pick 12 does not take it, §7) |
| 50 | compute `f607780aa8e502d0` | an address not from a root parameter | below the cut; 2.0 (7/10) | ray-traced lighting and shadows (L68, L77) | 640×360 and probe targets | none: W12 |
| 51 | compute `6d7fe7745ab9c6d8` | an address not from a root parameter | below the cut; 1.8 (9/10) | ray-traced lighting and shadows (L68, L77) | 640×360 and probe targets | none: W12 |
| 52 | compute `32ed072b9f845048` | an address not from a root parameter | below the cut; 1.0 (7/10) | ray-traced lighting and shadows (L68, L77) | 640×360 and probe targets | none: W12 |
| 53 | compute `3592426e3e9142d8` | an address not from a root parameter | below the cut; 1.0 (7/10) | ray-traced lighting and shadows (L68, L77) | 640×360 and probe targets | none: W12 |
| 54 | compute `36f04a4ed0ad1d9b` | an address not from a root parameter | below the cut; 1.0 (7/10) | ray-traced lighting and shadows (L68, L77) | 640×360 and probe targets | none: W12 |
| 55 | compute `559d9f0184e15fe3` | an address not from a root parameter | below the cut; 1.0 (6/10) | ray-traced lighting and shadows (L68, L77) | 640×360 and probe targets | none: W12 |
| 56 | compute `aab1b3c7248ee5de` | a descriptor neither loaded nor built | below the cut; 1.0 (7/10) | ray-traced lighting and shadows (L68, L77) | 640×360 and probe targets | none: W11 |
| 57 | compute `b58eb3ca6ee46427` | an address not from a root parameter | below the cut; 1.0 (7/10) | ray-traced lighting and shadows (L68, L77) | 640×360 and probe targets | none: W12 |
| 58 | compute `f22a76fcb967ee59` | an address not from a root parameter | below the cut; 1.0 (7/10) | ray-traced lighting and shadows (L68, L77) | 640×360 and probe targets | none: W12 |
| 59 | compute `535f8a954d8ec638` | a structured buffer of unknown stride | 3.2 (6/11); 5.7 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 60 | compute `3e7e04c03727b40c` | a structured buffer of unknown stride | below the cut; 6.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 61 | compute `df1c34cb6d9dbcfa` | a structured buffer of unknown stride | 0.4 (1/11); 3.6 (5/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 62 | compute `46539424133f9b74` | a structured buffer of unknown stride | 0.6 (2/11); 2.7 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 63 | compute `02a4ed10c682ac3a` | a structured buffer of unknown stride | 0.3 (1/11); 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 64 | compute `29be03ccaa4e353a` | a structured buffer of unknown stride | 0.3 (1/11); 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 65 | compute `2a5cb98cf0749a82` | a structured buffer of unknown stride | 0.3 (1/11); 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 66 | compute `2ec1dd27e95fdd16` | a structured buffer of unknown stride | 0.3 (1/11); 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 67 | compute `2f12d0f639fb7ed0` | a structured buffer of unknown stride | 0.3 (1/11); 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 68 | compute `34121970d8a2e435` | a structured buffer of unknown stride | 0.3 (1/11); 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 69 | compute `355f182eb23a2ccf` | a structured buffer of unknown stride | 0.3 (1/11); 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 70 | compute `571bf6d258f65ed0` | a structured buffer of unknown stride | 0.3 (1/11); 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 71 | compute `8f8dd4e691330c74` | a structured buffer of unknown stride | 0.3 (1/11); 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 72 | compute `479975ca9ff7e370` | a structured buffer of unknown stride | below the cut; 3.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 73 | compute `adc76ddaa3a13d20` | a structured buffer of unknown stride | below the cut; 3.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 74 | compute `d84c0ca92452e526` | a structured buffer of unknown stride | below the cut; 3.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 75 | compute `e0ba27015903a5f3` | a structured buffer of unknown stride | below the cut; 3.0 (6/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 76 | compute `e90b8dcb8b4cfc6b` | a structured buffer of unknown stride | below the cut; 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 77 | compute `efc770e7f2c27b03` | a structured buffer of unknown stride | below the cut; 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 78 | compute `f6da51317fcef92e` | a structured buffer of unknown stride | below the cut; 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 79 | compute `f91ef42e5b381cb9` | a structured buffer of unknown stride | below the cut; 2.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 80 | compute `06989306fb3b54c2` | a structured buffer of unknown stride | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 81 | compute `072f5fe96efd27e1` | a scalar memory read as data | below the cut; 2.0 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | meshdata (class; its acceptance is the mesh ones) |
| 82 | compute `0b3d156291158496` | a structured buffer of unknown stride | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 83 | compute `16622000e3a30bf0` | a structured buffer of unknown stride | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 84 | compute `203f78efece5bbdf` | a scalar memory read as data | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | meshdata (class; its acceptance is the mesh ones) |
| 85 | compute `3fa1fda35402c93f` | a scalar memory read as data | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | meshdata (class; its acceptance is the mesh ones) |
| 86 | compute `52ab310c43b9f99c` | a scalar memory read as data | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | meshdata (class; its acceptance is the mesh ones) |
| 87 | compute `649850fad69f9228` | a structured buffer of unknown stride | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 88 | compute `74930ddd74e36464` | a structured buffer of unknown stride | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 89 | compute `a1266cc7391ae0d6` | a scalar memory read as data | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | meshdata (class; its acceptance is the mesh ones) |
| 90 | compute `a4df980a1c65fdbe` | a scalar memory read as data | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | meshdata (class; its acceptance is the mesh ones) |
| 91 | compute `a62731e90739f195` | a scalar memory read as data | below the cut; 2.0 (1/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | meshdata (class; its acceptance is the mesh ones) |
| 92 | compute `ad1cf51b8a6c7096` | a structured buffer of unknown stride | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 93 | compute `c3cb00c582d26394` | a structured buffer of unknown stride | below the cut; 2.0 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 94 | compute `de4e3af66d68d980` | a structured buffer of unknown stride | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 95 | compute `dffafa6052629c85` | a structured buffer of unknown stride | below the cut; 2.0 (3/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 96 | compute `05ef21a2132db12d` | a structured buffer of unknown stride | below the cut; 1.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 97 | compute `1c02f8131c508c75` | a scalar memory read as data | below the cut; 1.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | meshdata (class; its acceptance is the mesh ones) |
| 98 | compute `7f05ddab771799a9` | a structured buffer of unknown stride | below the cut; 1.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 99 | compute `d2c52fcd936985b4` | a scalar memory read as data | below the cut; 1.9 (7/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | meshdata (class; its acceptance is the mesh ones) |
| 100 | compute `d5ef4f2bb16d9fa3` | a structured buffer of unknown stride | below the cut; 1.8 (5/10) | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 101 | compute `ad17f5c4f7acac25` | a structured buffer of unknown stride | 0.3 (1/11); not in the dumps | ray-traced hit shading (L68, L77) | 640×360 R11G11B10F (26) | stride table (landed, switch off): W5 |
| 102 | compute `e30970f1727063fd` | a structured buffer of unknown stride | 108.4 (11/11); 82.2 (9/10) | BVH: impostor instances (L26) | buffers | stride table (landed, switch off): W5 |
| 103 | compute `3b90165a77d95cdf` | an address in an SGPR that is no root parameter | 50.2 (11/11); 45.9 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 104 | compute `f6e47efa50226beb` | an address in an SGPR that is no root parameter | 22.7 (10/11); 27.8 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 105 | compute `caa78b8ca62bc3d6` | an address in an SGPR that is no root parameter | 12.6 (6/11); 23.3 (6/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 106 | compute `5552dac6a2b7a1fd` | an address in an SGPR that is no root parameter | 12.7 (11/11); 12.8 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 107 | compute `198f6dfd2b81bcea` | an address in an SGPR that is no root parameter | 3.1 (4/11); 16.3 (6/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 108 | compute `5062bc7a48e842f8` | an address in an SGPR that is no root parameter | 9.0 (9/11); 6.6 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 109 | compute `5279ae36796c31fc` | an address in an SGPR that is no root parameter | 9.0 (9/11); 6.6 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 110 | compute `c42a31951201f9a0` | an address in an SGPR that is no root parameter | 9.0 (9/11); 6.6 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 111 | compute `162216b9cfd91aaf` | an address in an SGPR that is no root parameter | 7.0 (8/11); 7.8 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 112 | compute `28918b7150a7aad5` | an address in an SGPR that is no root parameter | 5.5 (4/11); 9.2 (6/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 113 | compute `3df629ffc3d0a2a6` | an address in an SGPR that is no root parameter | 6.5 (7/11); 7.8 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 114 | compute `6f6a359d1166bb27` | an address in an SGPR that is no root parameter | 6.2 (6/11); 7.8 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 115 | compute `f28a33a89eaa2caa` | an address in an SGPR that is no root parameter | 6.2 (6/11); 7.8 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 116 | compute `4b5aedc0e96b0770` | an address in an SGPR that is no root parameter | 6.1 (6/11); 7.7 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 117 | compute `a63e2dfc25ba3155` | an address in an SGPR that is no root parameter | 3.2 (6/11); 6.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 118 | compute `bb55c842c391589f` | an address in an SGPR that is no root parameter | 0.6 (1/11); 8.0 (1/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 119 | compute `cb9647e77864cb3a` | an address in an SGPR that is no root parameter | below the cut; 7.8 (6/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 120 | compute `7bb5b1b91d283a28` | an address in an SGPR that is no root parameter | 1.8 (4/11); 5.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 121 | compute `a197f50cd8481541` | an address in an SGPR that is no root parameter | 1.8 (4/11); 5.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 122 | compute `c85822de2e41cfaa` | an address in an SGPR that is no root parameter | 1.8 (4/11); 5.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 123 | compute `190fbf0ebe040169` | an address in an SGPR that is no root parameter | 1.3 (3/11); 5.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 124 | compute `4ddd6f9702fab2a7` | an address in an SGPR that is no root parameter | 1.3 (2/11); 5.0 (6/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 125 | compute `5e68767498c57eb6` | an address in an SGPR that is no root parameter | 1.3 (2/11); 5.0 (6/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 126 | compute `14394a322f6dd5c2` | an address in an SGPR that is no root parameter | below the cut; 4.0 (1/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 127 | compute `e63d22c4def9ba6a` | an address in an SGPR that is no root parameter | below the cut; 4.0 (1/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 128 | compute `e95858837a9aa22d` | an address in an SGPR that is no root parameter | below the cut; 4.0 (1/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 129 | compute `f6eb6498409ca974` | an address in an SGPR that is no root parameter | below the cut; 4.0 (1/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 130 | compute `3cea4534bbc15632` | an address in an SGPR that is no root parameter | below the cut; 2.1 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 131 | compute `894c91654720595f` | an address in an SGPR that is no root parameter | below the cut; 2.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 132 | compute `9205d2655bf8cc88` | an address in an SGPR that is no root parameter | below the cut; 2.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 133 | compute `35e51bb27a463294` | an address in an SGPR that is no root parameter | below the cut; 1.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 134 | compute `5c09eb42b7a3c362` | an address in an SGPR that is no root parameter | below the cut; 1.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 135 | compute `82a2ad6d07417d36` | a lane mask read as data | below the cut; 1.0 (6/10) | BVH: skinning (L26, L38) | BVH vertex buffers | none: W1 |
| 136 | compute `90a0692af4320129` | a lane mask read as data | below the cut; 1.0 (3/10) | BVH: skinning (L26, L38) | BVH vertex buffers | none: W1 |
| 137 | compute `f94e5d8bf4e1cf05` | an address in an SGPR that is no root parameter | below the cut; 1.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |
| 138 | compute `ffd93c8a029ff016` | an address in an SGPR that is no root parameter | below the cut; 1.0 (9/10) | BVH builder, the system library (L26) | acceleration structures | none: W12 |

## 7. Pick 12 and the 23 compute programs still refused for "groups of more than 64 threads"

- **The census session did not have pick 12.** dispatchx-a was built on
  `main` 99b6376, before bd0cc66 landed (6.1). Its 23 refusals of this
  class are the pre-pick-12 count.
- **With pick 12 the same 23 remain.** present-1 (bacfcc6, pick 12 in) logs
  the same 23 hashes in this class, refused at the same place. Pick 12
  translated one program that stopped at this class in the world,
  `2da4beb386509b77`, once its descriptor fix let it reach the class; in
  the pack survey it gained that one and no other (world-black.md §9.5:
  24,432 → 24,433).
- **Why: each of the 23 fails a hard exclusion of `group_reads_meet`**
  (`crates/xtp-shader/src/emit/lower/lanes.rs`), read from their native
  code (`xtp shader-info --disasm` on the 23 containers taken from the
  pack by their records, outside the repository):

| Programs | What they do (our words) | `v_mbcnt` | `v_readfirstlane` | mask read as a number (`s_bcnt1`, `s_ff1`) | loop | whole-wave section | permute | `v_writelane` |
|---|---|:-:|:-:|:-:|:-:|:-:|:-:|:-:|
| 16: `191ecaed66eea3b1`, `1d44a723bdebbf27`, `240cb516c1af0bc3`, `31d703207b51549a`, `65de113e23fa4fdd`, `804b342cb4b9d1e9`, `ab5455fc6abb804f` (no loop); `466ec41d3f9d24c0`, `56f3bf2d9388e5a7`, `abd7a22106898c66`, `b4b0427232707598`, `d7db57731ada919b` (loop); `922e1d0ee1e19a85`, `be6ce5d849d51127` (loop, whole-wave, permute); `ce79e9eba7ae0348`, `fa0bd10a35bfef3c` (loop, `v_writelane`) | compute shadow rendering (cascades); none uses LDS or `s_barrier` | all | all | all | 9 of 16 | 2 | 2 | 2 |
| 3: `0c028454b58543f0`, `b8f3f7979d6bbc50`, `eae3498e6553afa8` | shadow tile classification | all | all | all | 2 of 3 | 1 | 1 | |
| `02384a60fda6f2d0` | VFX memory defragmentation (runs in the world) | yes | yes | yes | yes | | | |
| `5c6224a6bc3a4066` | transparency depth tiles (runs in the world) | yes | | yes | | yes | yes | |
| `a1bf269d5b19f025` | scattering: sorting ray tiles (runs in the world) | yes | yes | | yes | yes | yes | |
| `b22a16a178976e1b` | compute graph, matrix transposes (runs in the world) | yes | | | | yes | yes | |

  What they share: `v_mbcnt_lo/hi` over a full mask (each lane's index in
  its console wave), `s_bcnt1` of a 64-lane compare (how many of the
  console wave's lanes pass it) and `v_readfirstlane`; none of the 23 has
  an atomic. In the split form a lane's console index is known from its
  thread id; the count and the first lane need the other half's ballot or
  value, which is the group-shared exchange pick 12 already makes for
  `v_readlane` at a fixed lane. So the next step is pick 12's form taking
  these three reads (same exchange, same barriers), first for the seven
  shadow programs without a loop, then loops (a barrier inside a loop
  that every lane of the group runs the same number of times).
- **Only 4 of the 23 were dispatched in the world sessions** (the last
  four rows); the 19 shadow programs were not seen running (another
  shadow path or quality setting).

## 8. Is `XTP_STRIDE_TABLE=1` safe to turn on in the image sessions now?

**Not shown yet: one session first.** What is known:

| Session | Build | Switches | Result |
|---|---|---|---|
| worldlight-12 to -14 | integration, before a644480 | table and `XTP_RUN_TIME_VIEWS=1` | the world's first frame never completes (the run-time views, worldlight-17) |
| worldlight-16 (`20261124-221222`) | integration a8f280b, before a644480 | table alone | frames at 0.2 fps; the material resolve's target covers 100 % of the screen instead of ~20 %; "no free raw view slots" 24 s into the world, after which reads through the table read zeros |
| stride-1 (`20261124-224115`) | 1177717 | table and run-time views | 0.1 fps; one culling submission 11.7 s on the GPU |
| stride-2, stride-2b (`…-225023`, `…-225243`) | 600e7f7, before a644480 | table and `XTP_RAW_STRUCTURED=1` | 2.1 fps median (stride-3a, the same without the table: 4.3); the raw view pool ran out 4 to 5 s into the world, zeros after |
| — | `main` with a644480 (in since 54ab53d) | table | **never run**: stride-3e and present-1 ran with the table off |

No session with the table on removed the device or hung the GPU. a644480
rewrites a slot's raw view in place, so the pool that ran out in every
session above should no longer run out, but no session has checked it.
What turning it on gains is large: dispatchx-a's 30 "unknown stride"
programs, the seven instance-culling programs among them, and
`e30970f1727063fd` (108 to 144 dispatches a frame); in the pack, 178
programs that translate and validate with it (section 3).

Before the person's image sessions use it, one Series S session (the L4
or the PC, 150 s) with `XTP_STRIDE_TABLE=1`, `XTP_RAW_STRUCTURED=1`,
`XTP_SKIP_PIPELINES=577248728544d74b`, `XTP_D3D12_DRED=1` and a fresh
precook (the table changes the translation key, so every program is
translated again otherwise): pass if no "no free raw view slots" line, no
device removal, the frame rate within the stride-3a range, and the
culling programs and `e30970f1727063fd` gone from the drops. Then it can
be the image sessions' default.

## 9. How this was made

- **Pack census.** present's survey of bacfcc6 on the L4 (run
  `20261125-003910-cargo`: `cargo run --release -p xtp --locked --
  shader-survey <pack> --host-lanes 32-32 --json D:\xtp-data\present-after2.json`),
  its refused lines read from the VM with
  `tools\cloud\invoke-vm.ps1 -Command "Select-String -Path D:\xtp-data\present-after2.json -Pattern '""item""'"`
  (2,265 lines), grouped by cause and stage, records joined to hashes
  through `E:\xtp-data\precook\list-b.txt`.
- **The survey of `main` 1c61fa1:** `tools\cloud\run-remote.ps1 -Cargo
  'run --release -p xtp --locked -- shader-survey <pack> --host-lanes
  32-32 --json D:\xtp-data\census-1c61fa1.json' -Local -Repo E:\XTP
  -LocalRev main`, sent at 01:25, run at 01:50 (it waited for the VM's
  build lock): 24,433 of 26,698 translated, 2,265 failed, outputs
  `9255acc3…`, 294 s; the same outputs hash as bacfcc6's survey and the
  same table of causes line for line (`diff` of the two logs' "causes"
  sections: none).
- **The world.** The three sessions' `run.log` (refusals, `first bind`,
  the frame reports' drop lines) and the per-pass views of section 6.1.
  Roles were read from the programs that run in each list; program names
  stay out of this repository (CONTRIBUTING.md, "Writing about titles").
- **The 23 wave64 programs (section 7).** Their containers taken from the
  pack at their records (a raw inflate and a BLAKE3 check of each, every
  hash matching), then `xtp shader-info --disasm` on them, locally, with
  no session running and no `measure.lock`; the scan for the conditions
  of `group_reads_meet` is a text scan of the listing (backward branches,
  `s_barrier`, LDS, lane reads, `v_mbcnt`, `v_readfirstlane`, permutes,
  `v_writelane`, `s_bcnt1`/`s_ff1`, saves of EXEC with all lanes set).
  Containers and scripts stay in the session's scratch folder.
- **Not done:** a survey of `main` with `--validate` (the validator figures
  of section 5 are stride's, on 84ff9e4 with the table on); a survey with
  the stride table on for `main` (survey.rs has no switch for it: W5);
  per-program checks of the BVH builder's 34 (they are not in the pack).
