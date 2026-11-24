# GPU layer: VIctory (XTP's base) against xcl

Analysis of the console's D3D12.X on the host's D3D12: command lists,
descriptors, memory, synchronisation and presentation. It states what XTP
keeps from VIctory, what it rewrites after xcl's approach, and in which order.

- XTP is VIctory imported at `ffe310e`, GPL-3.0-or-later. Crate names below
  are the XTP ones (`xtp-gpu` = VIctory's `victory-gpu`, and so on).
- xcl is GPL-2.0-only and is a **reference for behaviour only**. Nothing here is,
  or may become, a copy of its code. xcl items are named by file and function so
  a worker can read what they do and write XTP's own version.
- Out of scope, covered by other reports: the shader compiler (RDNA2 → HLSL →
  DXIL), the loader, HLE outside D3D12.X, texture formats and detiling, and tooling.

## 1. Summary

Both layers record commands the same way: the title's inline packet room is
kept at zero, so every command arrives as a method call. They differ in
memory and in time.

- **VIctory keeps two memories and one timeline.**
  - Title memory is host RAM the GPU cannot see. Buffers get host copies
    ("shadows"), refreshed by the CPU before each submission and copied back
    after it.
  - Buffer copies in a list run on the CPU *before* that submission's GPU
    work.
  - Every submission is waited for, on one host queue.
- **xcl makes title memory GPU memory.**
  - Console addresses are window plus offset into imported RAM, or into video
    memory that mirrors the title's pages tile by tile.
  - Copies and memory writes run on the GPU, in order.
  - Submission is asynchronous. Fences, waits and bottom-of-pipe writes follow
    host fences, with timeouts and deadlock breaking.

**Most likely causes of VIctory's symptoms**, all visible in its own code
(section 6):
1. **Dark world.** GPU results reach their consumers late or never:
   - copies run before the GPU work they follow;
   - resources reached through bindless heaps keep their first upload;
   - bindless UAV writes never go back.

   Work is also dropped (untranslated shaders, the pipeline wait budget, some
   indirect signatures).
2. **0.5–1.8 s per frame.** Up to 500 ms of pipeline waits per title frame,
   plus fully serial submissions with CPU refreshes and copy-backs around
   each.
3. **GPU hang.** Each title submission runs to completion as one host
   submission, close to Windows' 2 s TDR limit.

**Plan.**
- *Keep* VIctory's object model, recording, pipelines, indirect clamping and
  presentation.
- *Rewrite* memory and synchronisation after xcl, in three phases:
  1. ordering fixes in the current model;
  2. asynchronous submission;
  3. GPU-visible title memory.
- *Caveat:* xcl is the better architecture, not a fix proven on this world
  (section 8).

## 2. Scope and files

### XTP / VIctory: `crates/xtp-gpu/src` (15.8 k lines)

| File | Lines | Role |
|---|---|---|
| `object.rs`, `device.rs`, `dxgi.rs`, `console.rs` | 384+532+319+50 | console object header, device vtable (PC order + console slots, newer layout), DXGI adapter/output, Series X/S choice |
| `list.rs` | 1449 | allocators and lists; every console list method; queries, memory writes, waits, copies |
| `draw.rs` | 1217 | per-list draw state, bound to the host list at each draw/dispatch |
| `queue.rs`, `fence.rs`, `frame.rs` | 547+173+226 | queues (execute, signal, PresentX), fences (CPU values), frame clock |
| `host.rs` | 2055 | the host device, its single direct queue, submission, deferred copy steps, swap chain, DRED |
| `resource.rs` | 1844 | heaps, committed and placed resources, host textures per address |
| `shadow.rs`, `watch.rs` | 499+381 | host copies of title buffers; write watch and page-guard dirty tracking |
| `texture.rs` | 344 | CPU detiling of title-written textures into staging, then upload |
| `descriptor.rs`, `descriptor/host.rs` | 793+976 | console descriptor heaps (title memory) and their host mirrors in title slot order |
| `root.rs`, `signature.rs`, `sampler.rs` | 324+94+119 | root signatures, command signatures, `CreateSamplerX` |
| `pipeline.rs`, `pipeline/{host,make,library}.rs` | 624+667+238+254 | pipeline states; host PSOs made by workers; on-disk library and preload list |
| `indirect.rs`, `primitive.rs` | 217+231 | GPU clamp of indirect arguments; rectangle lists |
| `query.rs`, `shader_cache.rs` | 72+359 | query heaps (no results); translated-shader disk cache |
| `capture.rs`, `copystat.rs` | 541+147 | diagnostics only (frame PNGs, copy statistics) |

Outside the crate: `crates/xtp-hle/src/modules/xmem.rs` (title GPU memory),
`modules/d3d12x.rs` (exports), `crates/xtp-formats/src/texture.rs` (detiling);
`docs/architecture.md`, `README.md` (status, warning, debug variables).

### xcl (reference): `E:/xtp-refs/xcl/crates/xcl/src`

| File | Lines | Role |
|---|---|---|
| `hle/d3d12x/real.rs` | 5767 | real backend: device init, vtable adoption, `open_uma`, queue dispatch, `q_execute_command_lists`, `stop_if_device_removed`, `end_title` |
| `hle/d3d12x/list_x.rs` | 5873 | console list methods: memory writes and waits (`wait_memory`), `kickoff_x`, predication, queries, barriers, sync graph |
| `hle/d3d12x/title_list.rs`, `console_list.rs` | 444+990 | title list wrapper (no room, forwarding thunks); opt-in compute list recorded in a buffer and replayed |
| `hle/d3d12x/bind.rs` | 1283 | root arguments kept per list; `bind_abi` binds the translated-shader ABI per draw |
| `hle/d3d12x/guard.rs`, `layout.rs`, `hle/d3d12_layout.rs` | 342+59+113 | caller-routed thunks; generated interface table (from `scripts/gen_d3d12_layout.py` over the MIT DirectX-Headers) |
| `hle/d3d12x/pso_async.rs` | 469 | stand-in PSOs, background/lazy builds, preload, `resolve_wait` |
| `hle/d3d12x/device_x.rs`, `dxgi_device.rs` | 1163+209 | console device extensions, queue/list type mapping, view formats |
| `hle/d3d12x/fence_x.rs`, `gpu/fence.rs` | 178+231 | title-polled fence cells; waits with timeouts and removal detection |
| `hle/d3d12x/present.rs`, `frame.rs` | 1156+630 | `present_x`/`present_now`, swap chain; console frame clock |
| `gpu/uma.rs`, `hle/d3d12x/sparse.rs`, `vram.rs` | 944+2208+1035 | UMA windows; reserved-buffer windows following title commits; GPU-owned textures on shared 64 KiB pages |
| `hle/d3d12x/console_heap.rs`, `gpu/desc.rs` | 601+697 | console heaps in UMA plus one shared host heap; T#/V#/S# decoding |
| `hle/d3d12x/title_textures.rs`, `gpu/texture.rs`, `gpu/tiling.rs` | 1998+1060+816 | title-placed textures: write watch, hash skip, CPU detiling, batched upload |
| `hle/d3d12x/cpu_writes.rs`, `alloc_pool.rs`, `late_release.rs` | 247+447+211 | CPU-side bottom-of-pipe writes after a fence; allocator rotation; deferred release |
| `gpu/adapter.rs`, `gpu/agility.rs`, `host.rs` | 160+361+148 | adapter choice, Agility SDK loading, host start-up threads |
| `scripts/gen_tiling.py` | 110 | GFX10 swizzle tables from Mesa addrlib (formats report) |

Docs: `README.md`, `AGENTS.md`, `HANDOFF.md` (GPU findings, around lines
560–700 and 1600–1700) and xcl's design notes (`docs/specs/`) §5.5a, §5.7, §9.

## 3. The two designs in one page

**VIctory.**
- Lists record onto a host command list as the title calls methods
  (`list.rs`, `draw.rs`).
- Each draw binds our root signature, the host copies of the buffers and
  descriptors it is known to read, and texture transitions.
- Anything touching title memory becomes a step that `HostList::run_deferred`
  runs at submission, before the lists execute: buffer copies,
  `CopyMemoryToMemoryX`, fills, shadow refreshes of dirty 64 KiB pages
  (`watch.rs`), texture detiling.
- `Host::execute` submits on the single direct queue and blocks on its fence.
- After the GPU, shader-written ranges are compared with a snapshot and copied
  back (`Shadow::gpu_writes`/`copy_back`). Then bottom-of-pipe writes land.
- Fence signals complete at once, and GPU waits (`wait_value`, queue `Wait`)
  are no-ops.

**xcl.**
- The same interception, by thunks forwarding to an "adopted" host list.
  Console extensions live in `list_x.rs`.
- Title GPU memory sits in 4 GiB windows:
  - window 3 is committed RAM imported with `OpenExistingHeapFromAddress`
    (`real.rs:open_uma`);
  - windows 0–2 are reserved buffers whose 64 KiB tiles follow the title's
    commits onto video-memory pools. CPU-changed 64-byte chunks are uploaded
    before each submit, and the title queue GPU-waits for the upload
    (`sparse::before_execute`).
- GPU writes come back only where the CPU reads them
  (`readback_after_execute`).
- Translated shaders reach memory through the windows bound as root UAVs, and
  read raw console descriptors from memory (`bind::bind_abi`,
  `console_heap.rs`).
- Submission is asynchronous, split before unsatisfied memory waits
  (`execute_with_waits_on`). Fence cells the title polls follow host
  completion (`fence_x`).
- Waits time out (2 s CPU polls, a 5 s GPU watchdog), cross-queue cycles are
  broken (`break_cycles_before_title_signal`), and the process stops on device
  removal (`stop_if_device_removed`).

## 4. Capability matrix

States: ✅ works for the title, ◐ partial or shortcut, ✗ missing.

| Sub-area | VIctory (XTP today) | xcl | Tests, VIctory | Tests, xcl |
|---|---|---|---|---|
| Object model, vtables, newer layouts | ✅ hand-written tables, PC order plus console slots, newer layout by creation interface | ✅ adopted host vtables, generated interface table, caller-routed thunks, layout auto-detect | `device.rs` 6, `list.rs` (inline fallback), `queue.rs` slot tests | `layout`, `guard` 4, `title_list`, `real` slot test |
| Recording (inline room) | ✅ limits zero, every command a call | ✅ same; opt-in record/replay for compute lists | `every_inline_command_falls_back_to_the_table` | `console_list_objects_have_no_room…`, `console_list` 4 |
| Root signatures and arguments | ✅ host root signature from translated bindings (root constants, root views, heaps) | ✅ console RTS0 layout into SGPRs plus table in UMA | `root.rs` 3 | `console_list` layout tests, `bind` tests |
| Draws, rect lists, mesh | ✅ VS/PS, mesh pipelines, rect lists on CPU | ◐ VS/PS, rect lists; two-pass mesh not wired into draws | `primitive.rs` 4 | `bind` rect tests |
| Indirect execution | ◐ GPU clamp plus per-list budget; root-changing signatures dropped; indirect mesh dropped | ◐ offsets adjusted, no clamp; root-changing signatures with translated PSOs skipped | `argument_clamp_builds` | `execute_indirect_with_a_pipeline_still_building_is_skipped` |
| Barriers and states | ◐ whole-resource transitions from tracked state; enhanced barriers become one UAV barrier | ◐ title's states passed through and masked; UAV barriers become global | – | `list_x` barrier cases |
| Copies (buffer, memory, fill) | ✗ **run on the CPU before the submission's GPU work** | ✅ GPU copies in list order (UMA) | – | `list_x` fill, copy tests |
| Memory writes (Write32/64, timestamps) | ◐ CPU after the submission; timestamps are CPU time | ✅ `WriteBufferImmediate` in UMA, else CPU after a fence (`cpu_writes`); real timestamps | `timestamps_and_values_are_written` | `writes_land_after_the_queue`, `timestamps_outside_unified_memory…` |
| GPU memory waits (`WaitXBitValueX`) | ✗ no-op, sound only while everything is synchronous | ✅ dropped if satisfied in-list, split, bridged to host fence waits, CPU poll with timeout | – | 9 deadlock and cycle tests in `list_x` |
| Title memory to GPU | ◐ shadows refreshed from dirty pages, only for ranges known per draw | ✅ windows: imported RAM, plus VRAM tiles with dirty-chunk upload | `watch.rs` 3, `host.rs` 3 | `uma`, `sparse` (`only_changed_chunks_are_sent`…) |
| GPU writes to title memory | ◐ snapshot-compare copy-back of declared ranges; bindless UAV writes never | ◐ only for list memory operations and mapped reads | – | `readback_u64_…` |
| Descriptor heaps | ✅ host heap in title slot order, refreshed on view creation and copies, and when a table read finds a slot changed | ✅ raw console descriptors in UMA, one shared host heap allocated in 256-slot pages | `descriptor.rs` 4, `descriptor/host.rs` 3 | `desc` 5+, `console_heap` |
| Bindless (directly indexed) resources | ✗ buffers keep their first copy; textures not refreshed; UAV writes not copied back | ✅ in place (memory is shared) | – | – |
| Textures written by the title | ◐ whole-texture CPU detile on any dirty page; hash in physical-page views; plane 0 only; only table-read textures | ◐ write watch of all registered textures per submit, hash skip, CPU detile, batched upload | `a_texture_is_checked_once…`, `buffer_data_reaches_the_host_texture` | `gpu_mirror_upload_follows_cpu_writes…`, `placed_title_textures_share_vram…` |
| Render targets, aliasing | ◐ one host texture per (address, description); aliases do not share memory; never released | ✅ GPU-owned textures as reserved resources on shared 64 KiB pages, so aliases share memory | `committed_and_placed_resources_map_at_their_gpu_address` | `vram` sweep tests, `streamed_targets_stay_bounded` |
| sRGB views of UNORM resources | ✗ read without conversion | ✅ view formats chosen per use (`pc_shader_view_format`) | – | – |
| Queries | ✗ fixed answers (occlusion "visible", statistics 0) | ✅ real occlusion (nested emulated), real timestamps | – | `nested_occlusion_queries_end_the_outer_one_early` |
| Depth bounds, predication, depth bias X | ✗ depth bounds dropped; predication ignored | ✅ emulated | – | predication e2e, depth-bias tests |
| Pipelines | ✅ workers, 500 ms wait per frame then dropped; disk library; preload list | ✅ stand-ins, background/lazy builds, preload; draws wait up to a cap | `pipeline/*` 6 | `pso_async`, `retail_pipelines_built…` (data-gated) |
| Queues | ◐ every console queue onto one host direct queue | ✅ console types map to host types (DMA as copy) | `console_queues_are_created_as_on_pc` | queue tests in `list_x` |
| Submission and CPU/GPU overlap | ✗ every submission waited for | ✅ asynchronous; `KickoffX` mid-list | – | `execute_with_waits_splits_batches…` |
| Fences | ◐ CPU values, signals complete at once | ✅ host fences drive polled cells; waits with timeouts | `signals_set_waiting_events` | `the_cell_follows_the_fence`, `gpu::fence` 5 |
| Resource and allocator lifetime | ✅ trivially safe (synchronous) | ✅ `late_release`, `alloc_pool` (needed once asynchronous) | – | `a_held_resource_goes…`, `a_recording_outlives…` |
| Hang protection | ◐ indirect clamp, shader loop limit (2^20 per lane), DRED, WARP mode, abort on removal | ◐ loop guard (4 M), wait timeouts, watchdog, stop on removal; no clamp | `argument_clamp_builds` | deadlock tests |
| Presentation | ◐ plane 0 copied to a 2-buffer flip swap chain sized to it; no scaling, no colour space | ◐ same copy; stretch scaling, 100 ms copy wait, fallback before the first PresentX; no colour space | – | `window_position_modes`, frame-dump tests |
| Frame pacing | ✅ clock per interval, one event per kind per tick | ✅ timer thread; every event type due fires; console intervals only | `waits_end_on_ticks_and_count_frames` | `frame` 4 |
| Adapter, Agility SDK | ◐ default adapter, WARP for diagnostics | ✅ adapter choice, Agility SDK loading | `warp_adapter_accepts_only_software` | `adapter` 3, `agility` 6 |
| GPU test harness | ◐ a few host-GPU unit tests, no skip policy | ✅ `runner::skip_test`, WARP, `XCL_REQUIRE_GPU`, e2e test title presents | – | e2e `test_title_runs…`, `…root_signature_under_the_debug_layer` |

Test counts: VIctory's GPU crate has about 54 `#[test]` functions, almost all
CPU-only. xcl's README counts 370+ tests overall, many of them GPU tests that
skip on missing hardware. Neither side has an image-level regression test
for the world.

## 5. Verdict per sub-area

| Sub-area | Verdict | Why |
|---|---|---|
| Object model, vtables, DXGI, console model | **Keep VIctory** | Reaches BTP 6's world. xcl's generated table is useful for naming slots in logs; optional (backlog G21). |
| Recording via method calls | **Keep VIctory** | Same as xcl. xcl's record/replay list buys nothing for this title. |
| Root signature and draw binding | **Keep VIctory** | Tied to VIctory's shader ABI (shader report). xcl's raw-descriptor ABI only together with a shader ABI change. |
| Pipelines (workers, library, preload) | **Keep VIctory**, retune the wait | Already off-thread with a disk library. Change the drop-after-budget policy (G12). |
| Indirect clamping | **Keep VIctory** | xcl has no clamp; VIctory's protects against garbage arguments. Extend to root-changing signatures and indirect mesh (G13). |
| Mesh, rect lists, primitive types | **Keep VIctory** | Works in the world; xcl's mesh path is not wired. |
| Copies, fills, memory writes, in-list ordering | **Rewrite after xcl** | Must run on the GPU in list order (G3, G4; final form in G18). |
| Bindless resource coherence | **Rewrite after xcl** | Interim: dirty refresh of everything bound heaps reach (G5, G6). Final: shared memory (G16–G18). |
| Title memory model | **Rewrite after xcl** | Windows of imported RAM plus VRAM tiles following commits. Phased, behind a switch (G15–G19). |
| Render targets and aliasing | **Rewrite after xcl** | GPU-owned textures on shared pages (G19). |
| Submission, fences, waits, queues | **Rewrite after xcl** | Asynchronous submission, fences from host completion, GPU waits with timeouts and cycle breaking, real queue types (G8–G11). |
| Lifetimes (allocators, release) | **Rewrite after xcl** | Needed as soon as submission is asynchronous (G9). |
| Queries, depth bounds, predication, view formats | **Rewrite after xcl** | Small, self-contained (G7, G14). |
| Textures written by the title | **Keep VIctory**, widen | Extend refresh to bindless textures and to every registered texture per submission, as xcl does (G6). Detiling stays in `xtp-formats`. |
| Presentation | **Keep VIctory**, then new work | Same design as xcl. Scaling, window size, latency and HDR colour space are new work: xcl has none (G20). |
| Frame pacing | **Keep VIctory** | Equivalent behaviour. |
| Device-removed handling, diagnostics | **Keep VIctory** | On a par with xcl (DRED report naming allocations), plus per-list GPU timing and pipeline skipping. |
| Adapter, Agility SDK | **Rewrite after xcl** (small) | Needed for Windows 10 SM 6.6, and to pick a GPU (G22). |
| GPU tests | **New work, after xcl's pattern** | A GPU/WARP test helper with skip reporting is the base of every backlog item's acceptance (G1). |

## 6. Diagnosis

*Measured* means stated in README or in the code's own comments.
*Hypothesis* means what the code makes likely. G2 turns hypotheses into
numbers.

### 6.1 The dark world

The world renders, so pipelines, targets and presentation work. Missing light
means GPU-produced data does not reach the passes that consume it.

1. **Copies run out of order (high).**
   - `copy_buffer_region`, buffer `copy_resource` and `copy_memory`
     (`CopyMemoryToMemoryX`) in `list.rs` are CPU steps, run by
     `HostList::run_deferred` before the host lists execute.
   - So a dispatch writing A, then a copy A→B in the same submission, copies
     A's *old* title bytes; A's GPU result comes back only afterwards.
   - Engines move luminance and exposure reductions, light and tile lists,
     culling output and indirect arguments exactly this way. Stale exposure
     or light lists give a dark scene.
   - `write32`/`write64` land only after the whole submission, so commands
     later in the list never see a counter or predicate written mid-list.
   - xcl does all of these on the GPU in list order.
2. **Bindless resources are stale (high).** VIctory's own shortcuts say so:
   - buffers read through directly indexed heaps keep their first copy
     (`shadow.rs`);
   - such textures are never refreshed (`texture.rs`);
   - UAV writes through them never reach title memory (`descriptor/host.rs`
     `buffer_uav`).

   World shaders index heaps directly, so per-frame constants, streamed
   textures and GPU-written buffers read as zero or as first seen. With xcl's
   shared memory the question does not arise.
3. **Work is dropped (high; counted in the frame log).**
   - Untranslated shaders. README: vertex/pixel 10,617 of 12,346 translate,
     compute only 1,643 of 2,539, so a third of compute is missing.
   - Pipelines not ready within the 500 ms per-frame budget
     (`pipeline/make.rs` `BUDGET`).
   - Indirect signatures that change root arguments, and indirect mesh
     dispatches.

   One lost lighting or exposure pass is enough to darken the frame. The
   shader share is the shader report's; the GPU-layer share is G12 and G13.
4. **Render-target aliasing (medium).**
   - `resource.rs` keeps one host texture per (address, description), so
     aliases do not share memory.
   - A pass that writes one alias and reads another reads a different host
     texture.
   - xcl's `vram.rs` puts GPU-owned textures on shared 64 KiB pages, so
     aliases see each other's writes.
5. **Smaller ones.**
   - sRGB views read undecoded (`descriptor/host.rs` `fits`).
   - Depth bounds dropped (`pipeline/host.rs`): too much light, not too
     little.
   - Fixed query answers (`list.rs` `resolve_queries`).
   - Depth and stencil planes of title-written textures not uploaded.

**xcl's history** (HANDOFF) records no gamma or HDR cause. Its black frames
came from:
- per-heap descriptor indices (fixed with one shared heap);
- heaps that did not fit its windows;
- cold shader caches drawing stand-ins;
- an untranslated vertex-buffer address, which put NaN in every target and
  then hung the GPU.

### 6.2 0.5–1.8 s per frame

Measured: the README range. The `Meter` line in `host.rs` already splits CPU
preparation, GPU wait and copy-back, and counts dropped draws, but no world
run's numbers are recorded (G2). Structural costs, in likely order:

1. **Pipeline waits.** `pipeline/make.rs` blocks recording up to 500 ms per
   title frame while pipelines are made. With 1,200+ first-use pipelines in
   the world, that is a 0.5 s floor. xcl caps waits and preloads the last
   run's pipelines; VIctory also preloads (`pipeline/library.rs`). Whether a
   second run is faster is unknown.
2. **Serialised submission.** `Host::execute` waits for every submission, and
   all console queues share one host direct queue. The CPU and GPU frames add
   up instead of overlapping, and every submission pays a full drain.
3. **CPU work around each submission.**
   - Shadow refreshes through `ReadProcessMemory` (`copy_mapped`); a 174 MB
     buffer is read through many ranges.
   - Whole-memory hashing of physical-page textures.
   - Whole-texture detiling on any dirty page.
   - Snapshots plus compare-and-copy of every declared GPU-written range.

   xcl avoids most of it: no copy-back unless the CPU reads, and uploads of
   changed 64-byte chunks only. It measured about 0.8 ms of scanning per
   submit.
4. **GPU time (unknown).** Possible sources:
   - loops running to the 2^20-per-lane limit (the reason for README's
     `VICTORY_SHADER_LOOP_LIMIT`);
   - huge but in-budget indirect records;
   - work over the stale data of 6.1.

   `VICTORY_GPU_TIMING` names the slowest commands.

### 6.3 GPU hang and driver reset

- One title `ExecuteCommandLists` is one host submission, run to completion,
  and Windows resets the driver at about 2 s. If most of a 1.8 s frame's GPU
  work sits in one submission, a slightly heavier frame or one long dispatch
  crosses the limit. That fits the warning and the hang appearing only in the
  world.
- VIctory already bounds garbage indirect arguments (`indirect.rs`) and
  shader loops, but 2^20 iterations per lane over many lanes can still take
  seconds.
- xcl adds smaller submissions (chunks, splits before waits, `KickoffX`) and
  a hard stop on device removal. VIctory also aborts, through `fail`.
- xcl's worst hang came from an untranslated GPU address. The same risk exists
  in VIctory for any title address that reaches a shader untranslated, such as
  a pointer built from a constant; shared memory makes such addresses valid.
- Fix order:
  1. end the serial CPU+GPU frame (G8);
  2. bound submissions (G10, G11);
  3. remove the stale data that drives garbage work (G3–G6);
  4. GPU-visible title memory (G15–G18).

## 7. Porting backlog

Ordered: an item depends only on earlier ones. Each is one worker session (a
few hundred changed lines at most). "xcl" names where to read the behaviour,
never code to copy. Paths are under `crates/xtp-gpu/src` unless another crate
is named.

### Phase 0: measure and test

**G1. GPU test helper** (S; depends: none).
A helper creating a host device (hardware, or WARP with `XTP_TEST_ADAPTER=warp`)
that prints `SKIPPED <test>: <reason>` without one and can be made strict.
- Touches: `host.rs` (test constructor), new `testgpu.rs` (cfg(test)).
- xcl: `gpu/recomp/runner.rs` `skip_test`; README "Tests" (`XCL_REQUIRE_GPU`).
- Test: `buffer_data_reaches_the_host_texture` runs through it on WARP, and
  reports a skip without a device.

**G2. Frame budget breakdown** (S; none).
The 5 s log line adds pipeline-wait ms per frame (`make.rs`), submissions per
frame, lists per submission and the longest submission. One world run is
recorded in `docs/analysis/gpu-measure.md`.
- Touches: `host.rs` (`Meter`, `execute`), `pipeline/make.rs`.
- xcl: the counters idea of `report.json` (`real.rs`); nothing to port.
- Test: unit test of the meter's arithmetic. The doc states which cause of 6.2
  dominates.

### Phase 1: ordering and coherence in the current model

**G3. Buffer copies on the GPU, in list order** (M; G1).
When a side of `copy_buffer_region`, buffer `copy_resource` or `copy_memory`
is in a shadow, record a host `CopyBufferRegion` between shadows and declare
the destination GPU-written. The CPU step stays only when neither side has a
shadow.
- Touches: `list.rs`, `shadow.rs`, `host.rs` (`Step`).
- xcl: `list_x.rs` CopyMemoryToMemory and Fill (GPU operations inside the windows).
- Test (GPU): dispatch writes A → copy A→B → dispatch copies B to C, in one
  submission; C holds the first dispatch's output.

**G4. In-list memory writes visible to later commands** (S; G1).
`write32`/`write64` inside a shadow also become a host `WriteBufferImmediate`
at that point of the list. The CPU write after the submission stays.
- Touches: `list.rs` (`defer_write`), `shadow.rs`.
- xcl: `list_x.rs` bottom-of-pipe writes (marker in/out), `cpu_writes.rs`.
- Test (GPU): write64, then a dispatch copying the word, gets the value.

**G5. Bindless buffers kept up to date and written back** (M; G3).
Each heap keeps the set of shadows its buffer views reach, updated at view
creation and copy. A submission using directly indexed heaps refreshes the
dirty pages of those shadows. Buffer UAVs in bound heaps are declared
GPU-written.
- Touches: `descriptor/host.rs`, `draw.rs`, `shadow.rs`.
- xcl: `sparse::before_execute` (dirty scan per submit), `console_heap.rs`.
- Test: unit test that a heap-only view refreshes after a title write. GPU
  test that a UAV write through a heap index reaches title memory.

**G6. Bindless and all registered textures refreshed** (M; G5).
Each submission checks the write watch of every title-written texture a bound
heap reaches, not only table-read ones. It skips the upload when a content
hash matches the last one, and uploads depth and stencil planes.
- Touches: `texture.rs`, `descriptor/host.rs`, `host.rs` (`Batch`).
- xcl: `title_textures.rs` `refresh_once`, `take_written`, hash skip
  (`XCL_TEXTURE_SKIP_UNCHANGED`).
- Test: a texture reached only through a heap is due after a title write, and
  not after an unchanged rewrite.

**G7. View formats and depth bounds** (S; G1).
- sRGB and other same-family views read with conversion (castable formats, or
  typeless host resources).
- Depth bounds kept in host pipelines (stream subobject) and set by
  `SetDepthBoundsX`.
- Touches: `resource.rs`, `descriptor/host.rs` (`fits`), `pipeline/host.rs`, `list.rs`.
- xcl: `device_x.rs` `pc_shader_view_format`, `list_x.rs` `set_depth_bounds_x`.
- Test (GPU): an sRGB view of a UNORM-created texture samples decoded values.
  A pipeline test shows depth bounds reach the host description.

### Phase 2: asynchronous submission

**G8. Submissions not waited for** (M; G3, G4).
`Host::execute` signals a host fence per submission and returns. A completion
thread, in submission order, does the rest when the fence passes: console
fence signals, bottom-of-pipe writes, copy-backs and timestamps.
`Fence::completed` and the title's polled values follow it.
- Touches: `host.rs`, `queue.rs`, `fence.rs`, `list.rs`.
- xcl: `fence_x.rs` (polled cell refreshed by a thread), `cpu_writes.rs`,
  `gpu/fence.rs` (timeouts, removal detection).
- Test (GPU): a title fence value moves only after the GPU work, and a CPU wait
  on it ends. Menus still at 30 fps.

**G9. Lifetimes under asynchronous submission** (M; G8).
- Host lists, allocators, uploads, scratch and kept objects live until their
  fence passes.
- A title allocator reset with work in flight swaps in a spare.
- A title's last release is held until the queues pass it.
- Touches: `host.rs` (`HostList`, `Deferred`), `list.rs`, `resource.rs`, `object.rs`.
- xcl: `alloc_pool.rs` `title_reset_in`, `late_release.rs` `hold`.
- Test (GPU): a recording outlives its allocator's reset, and a released
  resource survives until its submission completes, with a clean debug layer.

**G10. Console queues on host queues, and GPU waits** (M; G9).
- Compute and DMA queues get host queues of their type.
- Queue `Signal` and `Wait` become host fence operations.
- `wait_value` (WaitXBitValueX) is:
  - dropped when an earlier write in the list satisfies it;
  - otherwise split after the write it depends on;
  - otherwise a host fence wait, or a logged CPU poll with a timeout.
- A watchdog releases waits pending over 5 s.
- Touches: `queue.rs`, `list.rs`, `fence.rs`, `host.rs`.
- xcl: `device_x.rs` `pc_queue_desc`; `list_x.rs` `wait_memory`,
  `try_split_for_wait`, `execute_with_waits_on`, `break_cycles_before_title_signal`
  and the sync watchdog. Its Wait64 finding: what follows the value is not a mask.
- Test: CPU tests of the wait rules (satisfied, split, timeout). A GPU test of a
  cross-queue wait that neither deadlocks nor runs early.

**G11. Bounded submissions** (S; G8).
- A title submission is split into host submissions of bounded recorded work
  (draws, dispatches, indirect records).
- `KickoffX` is honoured mid-list.
- Submissions over 500 ms are logged with their pipelines.
- Touches: `host.rs`, `list.rs`.
- xcl: `list_x.rs` `kickoff_x`, chunking in `execute_with_waits_on`.
- Test: unit test of the split planner. A world run (WARP first) has no
  submission over the limit and no driver reset.

**G12. Pipeline wait policy** (S; G2).
Replaces the fixed 500 ms per-frame budget:
- compute pipelines whose output later passes read are waited for;
- graphics ones are dropped, or drawn with a stand-in, past a measured cap;
- the preload list starts at device creation.
- Touches: `pipeline/make.rs`, `pipeline/library.rs`, `draw.rs`.
- xcl: `pso_async.rs` `resolve_wait` (compute waited, graphics capped by
  `XCL_PSO_WAIT_MS`), preload of `used-pipelines.txt`.
- Test: unit tests of the policy. On a second run, G2's pipeline wait stays
  under 50 ms per frame.

**G13. Indirect: root-changing signatures and indirect mesh** (M; G1).
- Signatures whose records change root constants or views are expanded into
  our root layout.
- Indirect mesh dispatches go through the clamp.
- Clamp and budget stay.
- Touches: `indirect.rs`, `signature.rs`, `list.rs`, `draw.rs`.
- xcl: `device_x.rs` `create_command_signature_x`, `bind.rs` `execute_indirect`.
  xcl skips the translated root-changing case, so the expansion is new work.
- Test (GPU): a root-constant argument reaches the draw; garbage mesh counts
  are clamped.

**G14. Real queries** (M; G8).
- Occlusion and binary queries use host heaps; nested ones end the outer one
  early, which resolves visible.
- Timestamps come from host queries, with frequency and calibration passed
  through.
- Pipeline statistics are passed through.
- `SetPredicationBufferX` becomes host predication.
- Touches: `query.rs`, `list.rs`, `queue.rs`.
- xcl: `list_x.rs` `begin_query_x`, `end_query_x`, `resolve_query_data_x`, `timestamp`, `set_predication_buffer_x`.
- Test (GPU): an occluded draw counts 0; nested queries resolve; timestamps
  increase across submissions.

### Phase 3: GPU-visible title memory

Behind `XTP_UMA=1` until G19 passes; the shadow path stays as the fallback.

**G15. Importing title memory: feasibility** (S; G1).
Can `OpenExistingHeapFromAddress` import both memory kinds `xtp-hle` xmem
makes? Those are write-watched reservations and physical-page views (try
`OpenExistingHeapFromFileMapping` for the latter). At what cost in commit, the
4 GiB limit and open time?
- Touches: new `uma.rs`; `crates/xtp-hle/src/modules/xmem.rs` (allocation flags, only if needed).
- xcl: `real.rs` `open_uma`, `check_uma_import`; partial-commit experiments in
  `gpu/recomp/tests.rs`.
- Test (GPU): a CPU canary, copied on the GPU, reads back for each memory
  kind. Findings go in `docs/analysis/gpu-measure.md`.

**G16. Coherent window for readback and small buffers** (M; G8, G15).
Buffers the title maps for reading, and small per-frame uploads (constants,
arguments), become placed buffers over imported memory. They lose their
shadows, and descriptors point at window base plus offset.
- Touches: `uma.rs`, `shadow.rs`, `resource.rs`, `descriptor/host.rs`.
- xcl: `real.rs` `place_console_buffer`, `sanitize_coherent_buffer`;
  `gpu/uma.rs` `window_of`.
- Test: the G3–G5 tests pass with those shadows gone. G2's copy MB per frame drop.

**G17a. VRAM windows following commits** (M; G15).
One reserved host buffer per title memory window. Its 64 KiB tiles map onto
pooled default heaps on xmem commit and unmap on decommit.
- Touches: new `sparse.rs`, `uma.rs`; commit hook in `crates/xtp-hle/src/modules/xmem.rs`.
- xcl: `sparse.rs` `init`, `committed`, `decommitted`.
- Test (GPU): a decommitted tile reads zero; a mapped tile keeps its data across a remap.

**G17b. Dirty-chunk uploads and readback** (M; G17a, G10).
Before each submission, changed chunks of write-watched pages (per-chunk hash)
go up through a staging ring on our copy queue, and the title queue GPU-waits
for it. CPU reads of GPU-written memory use explicit readback (mapped reads,
list memory operations).
- Touches: `sparse.rs`, `host.rs`, `watch.rs`.
- xcl: `sparse.rs` `before_execute`, `changed_chunks`, `readback_after_execute`;
  HANDOFF's per-submit scan costs.
- Test: unit tests "only changed chunks are sent" and the ring planner. A GPU
  test: a CPU write before a submission is seen by the GPU.

**G18. Shadows retired** (M; G16, G17b).
With the windows on, buffers are placed buffers in them. Copies, refreshes,
snapshots and copy-backs go. Copies, fills and memory writes are plain GPU
operations.
- Touches: `shadow.rs` (behind the switch), `draw.rs`, `list.rs`, `descriptor/host.rs`.
- xcl: `real.rs` `uma_va_of`, `uma_window_buffer`; `bind.rs` `buffer_delta`
  (64 KiB-aligned placement plus offset).
- Test: the `xtp-gpu` GPU suite passes with `XTP_UMA=1`. Menus at 30 fps; G2's
  copy-back near zero.

**G19. GPU-owned textures on shared pages** (M; G17a).
- Render targets, depth and UAV textures in title memory become reserved
  resources on the window's 64 KiB pages, so aliases share memory.
- Idle pages are freed after a delay.
- Textures with metadata planes count as GPU-owned.
- Touches: `resource.rs`, `sparse.rs`, `texture.rs`.
- xcl: `vram.rs` `place`, `place_on_pages` (idle and cool-down rules);
  `title_textures.rs` `note_gpu_write`, `retire_overlapping`.
- Test (GPU): two aliased targets at one address see each other's clear;
  streamed targets stay bounded.

### Independent items

**G20. Presentation** (S; G8).
- Stretch scaling, with the swap chain sized to the window.
- A waitable object and frame latency.
- `SetColorSpace1` for 10-bit (HDR10 when asked) and FP16 (scRGB).
- A bounded presenting copy.
- Planes beyond the first composited, or logged.
- Touches: `host.rs` (`Presenter`), `queue.rs` (`present`).
- xcl: `present.rs` `ensure_swapchain`, `present_now` (100 ms bounded wait).
  The colour space is new work.
- Test: format and colour-space choice per title format; a second plane is logged.

**G21. Interface names for logs** (S; none).
Our own generator over the MIT DirectX-Headers emits method names per
interface, so missing-method logs name the PC method.
- Touches: `xtask` (new subcommand), `device.rs`, `list.rs`.
- xcl: `scripts/gen_d3d12_layout.py` (what it extracts), `hle/d3d12x/layout.rs`.
- Test: known slots get their names.

**G22. Adapter choice and Agility SDK** (S; none).
- `XTP_ADAPTER` (index or name).
- The Agility SDK loaded from beside the executable or a configured folder, so
  Windows 10 gets SM 6.6.
- Preflight messages for mesh shaders, SM 6.6 and tiled resources tier 2 (G17).
- Touches: `host.rs`, new `adapter.rs`, `crates/xtp-core` (config key).
- xcl: `gpu/adapter.rs`, `gpu/agility.rs` (`CreateDeviceFactory` path), `preflight.rs`.
- Test: adapter matching; a missing SDK folder falls back to the inbox runtime.

**Order:** (G1, G2) → (G3, G4) → (G5, G6, G7) → (G8, G9) → (G10, G11, G12) →
(G13, G14) → G15 → G16 → (G17a, G17b) → G18 → G19, with G20–G22 alongside.
A world re-test after G7 shows how much of the darkness was ordering and
staleness; one after G12 does the same for speed and hangs.

## 8. Risks and open questions

**Risks.**
- **xcl has not been shown on this world.** The "newer retail game" in its
  README, an RPF title, stops after its menus. Its UMA and synchronisation
  are proven on Minecraft, Fortnite and XDefiant. Judge each phase by XTP's
  own numbers (G2).
- **Asynchronous submission exposes hidden races.** Today every fence completes
  at once and nothing is in flight, which hides wrong lifetimes and wrong
  waits. Expect new hangs during G8–G10, and keep a synchronous switch for
  bisecting.
- **Memory import limits (G15).**
  - `OpenExistingHeapFromAddress` needs a fully committed allocation of at
    most 4 GiB (xcl measured both), so commit charge grows. README already
    warns of commit pressure, and xcl runs about 15 GB committed in menus.
  - xmem's physical-page section views may not import at all.
- **Bandwidth.** Imported RAM is read over PCIe. Vertex and texture traffic
  there would be slow, which is why xcl mirrors most memory in VRAM. VRAM
  capacity (12 GB on the test RX 6700 XT, against the Series X title budget)
  may not hold the title's pools: tiles must follow commits, not
  reservations.
- **Shader ABI coupling.** Full UMA as xcl does it (raw console descriptors
  and addresses in shaders) needs the shader compiler's ABI to change. The
  backlog keeps VIctory's ABI: host descriptors pointing at window addresses
  (G16–G18). Any ABI change must be agreed with the shader report's backlog.
- **Clamps and budgets cut legitimate work.** The indirect bounds, the
  submission limits (G11) and the pipeline policy (G12) can drop real work.
  Each needs its counter in the frame log.
- **Licence.** Behaviour may be reimplemented, code may not be copied (xcl is
  GPL-2.0-only, XTP GPL-3.0-or-later). Generated tables must come from XTP's
  own generators over the MIT sources (DirectX-Headers, Mesa addrlib), never
  from xcl's generated files. Reviewers should check new GPU files against xcl
  for copied structure.

**Open questions.**
1. How do 0.5–1.8 s split between pipeline waits, CPU preparation, GPU time
   and copy-back on a world frame? Answered by G2.
2. How many draws and dispatches per world frame are dropped, and for which
   reason (no translation, budget, signature)? The log exists; the numbers
   are not recorded.
3. Does BTP 6 present more than one plane (for example HUD and world), given
   that only plane 0 is shown? The opening credits render and the scene after them
   does not.
4. Which resources does BTP 6's world read through directly indexed heaps
   versus tables? This decides how much G5 and G6 alone recover.
5. Can xmem's physical-page views be imported (G15)? If not, which pools does
   the title place there?
6. Is the 2 s TDR crossed by one long dispatch or by the sum of a submission?
   `VICTORY_GPU_TIMING` on WARP can tell; this decides between G11 and
   shader-side fixes.
7. Does the title read GPU results on the CPU (auto-exposure, occlusion,
   readback buffers) at a fixed frame latency? That sets how strictly G8 must
   order completion writes.
