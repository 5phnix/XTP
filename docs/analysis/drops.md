# Series S world: draws and indirect executions dropped, by cause

Why the Series S world frame ran 79 draws and dropped 1,196 draws and
3,515 indirect executions per frame (frame-prof's session C,
`world-frame.md` §6), what each cause is, whose it is, what is fixed here,
and the drops per frame before and after. Measured on the person's PC
(RTX 4080 SUPER, 32-lane waves, 16 GB; Ryzen 9 7950X3D, 32 logical
processors) on 2026-11-24 in Series S mode (`--console series-s`,
`XTP_SKIP_INTRO=1`, `XTP_WATCHDOG_HOOK=1`, autopilot from the golden save).
The title is BTP 6. Times are UTC. Logs are under `E:\xtp-data\`, none in
the repository (§7).

## 1. Summary

- **Most drops are refused wave64 mesh pipelines** (VIctory #578: every
  mesh program of the title is written for 64-lane waves, the RTX 4080 has
  32): 1,590 to 3,451 drops a frame, 27 to 62 % of all, nearly all of them
  indirect mesh dispatches (`ExecuteIndirect` kind 10). Running them is the
  translator's (task mesh64). Here they are now refused before their
  translation, which took a quarter of the pipeline workers' translations
  and was thrown away.
- **Pipelines still being made** come next in the world's first minute
  (660 to 3,943 a frame): the eight workers spent most of their time on 16
  compute programs the translator refuses only after 20 to 28 s each (lane
  masks read as data, 135 of 203 s of translation in session B). With 16
  workers, 48 % more translations finish in the world's first minute; the
  queue still takes about 80 s to drain, held by those programs
  (translator: masks-fix).
- **Shaders that do not translate** (170 to 992 a frame, mostly direct
  dispatches): the translator's (lane masks, unknown strides: VIctory #193
  and #704, which also need `descriptor/`).
- **Indirect rectangle lists**, 130 to 256 a frame in every world frame
  before, are now drawn (VIctory #346, ported): **0** dropped after. The
  title's rectangles make their corners from the vertex id, which counts
  from 0 in each record on the console as on the host (§5.1).
- The frame report now gives every drop a cause, where it was recorded
  (draw, dispatch, mesh dispatch, indirect kind …) and why, with each
  cause's total first (§2).

| Per frame (Series S world) | before: A | after: B | after: C′ (B + 16 workers) |
|---|---:|---:|---:|
| draws run | 42 | 270 | 339 |
| dropped: direct + indirect | 2,217 + 3,617 = 5,834 | 1,238 + 1,815 = 3,053 | 2,353 + 3,212 = 5,566 |
| of which refused wave64 mesh | 1,590 | 1,700 | 3,451 |
| being made | 3,943 | 660 | 1,122 |
| do not translate | 170 | 691 | 992 |
| indirect rectangles | 130 | **0** | **0** |
| indexed rectangles | 0.7 | 0.8 | 1.0 |

Read the columns with §5.4: the per-frame counts follow how far into the
world the reports fall and how many draws the title records, not only the
fixes. What the fixes change by themselves: rectangles 130–256 → 0, mesh
translations 362 → 0, translations finished per minute +48 %.

## 2. What the frame line counted, and what it did not say

The frame line's two drop counts come from two counters:

- "dropped N without a host pipeline": `draw::DROPPED`, the direct draws,
  mesh dispatches and dispatches recorded but not run on the host;
- "and N indirect": `list::INDIRECT`, the `ExecuteIndirect` calls not run.

In diagnostics builds a line per reason followed, "dropped N/frame without
a host pipeline: <reason>". Before this task those lines:

- named a refused pipeline by its shaders only ("refused [mesh …, pixel
  …]"), not by the host's reason;
- merged direct and indirect drops;
- showed only the first 40 reasons (in session C they add up to 3,504 of
  the 4,711 drops per frame);
- said nothing for a direct draw whose `prepare` failed with a host
  pipeline bound, and only "prepare failed (compute false, mesh false)"
  for an indirect one (run-world's W2).

Session C's world frame line (16:34:14, 2.6 fps), its 40 lines summed by
kind:

| Reason (session C, `main` `488f2b4`) | Drops per frame (of the first 40 lines) |
|---|---:|
| refused mesh pipelines (wave64 mesh programs, VIctory #578) | ≥ 2,473 |
| being made | 566 |
| indirect kind 0: prepare failed (compute false, mesh false) | 256 |
| shaders that do not translate | ≥ 167 |
| refused compute pipelines (64-lane waves) | ≥ 42 |
| not shown (past the 40th line) | 1,207 |
| **all** (1,196 + 3,515) | **4,711** |

Now (`pipeline::take_drops`, `draw::Unready`) each drop has a cause: where
it was recorded and why, as "indirect kind 10 (mesh dispatch): refused,
mesh shader is a mesh program written for 64-lane waves (…)", "dispatch:
being made", "indirect kind 0 (draw): rectangles …". The report lists every
cause's total first ("by cause: …", lines that add up to every drop), then
the causes' details by shaders. Refused pipelines keep the host's reason
(`pipeline::host::take_refusal`); `prepare`, `bind` and the root SRV tables
return theirs ("no dynamic slots for its tables", "a root SRV without a
view heap", "index buffer outside title buffers", …). Translations and
host pipelines that take a worker 100 ms or more log their time.

## 3. Before

### 3.1 By cause (session A)

Session A: `main` at `07684c8` with the counters of §2 and nothing else of
this task (the refusal of §5.2 turned off in that build only). One world
frame report (17:58:10, 0.1 fps, 42 draws): the title crashed into its
known Series S crash soon after (task series-s), and a second try (A2)
crashed at the world's entry, before any world frame.

| Cause (session A) | direct | indirect | all |
|---|---:|---:|---:|
| being made | 1,812 (dispatch 1,390, mesh dispatch 416) | 2,132 (kind 10: 1,727; kind 2: 341) | 3,943 |
| refused, wave64 mesh program | 340 | 1,250 (kind 10) | 1,590 |
| does not translate | 65 | 105 | 170 |
| rectangles, no vertex inputs | | 130 (kind 0) | 130 |
| indexed rectangles | | 0.7 (kind 1) | 0.7 |
| **all** | **2,217** | **3,617** | **5,834** |

### 3.2 Over 150 s of world (series-s's run 5)

series-s's run `20261124-173913` held 150 s in the world. Its build has
the old report (first 40 reasons, summed by kind, so lower bounds). In
its 26 world reports the indirect rectangles were 256 a frame in 19 and
128 to 512 in the others. "Being made" went from 206 to 1,321 a frame in the
first 75 s and to none after (queue at 147–207 pipelines, empty 78 s after
the world's entry). From then on: refused wave64 mesh 294–2,884 and shaders
that do not translate 712–2,121 a frame. Of its 1,491 translated shaders,
362 were wave64 mesh programs, all refused once translated.

## 4. The causes, and whose they are

| Cause | What it is | Whose | Done here |
|---|---|---|---|
| Refused wave64 mesh pipelines | Every mesh program of the title is written for 64-lane waves; on a 32-lane host (NVIDIA) VIctory #578 refuses them (`XTP_WAVE64_MESH=allow` lets through those without known hazards). Mostly indirect mesh dispatches (kind 10). | The translator: running wave64 mesh programs as 32-lane waves (task mesh64, 0a31f0cb). | Refused before their translation (§5.2). |
| Being made | The pipeline is queued or being made by a worker: its shaders are translated (up to 28 s for the largest refusals), then compiled by the driver. | The GPU layer (`pipeline/make.rs`); what holds the workers is the translator's (below). | Fewer translations to wait behind (§5.2), 16 workers (§5.3), timings logged. |
| Shaders that do not translate | Lane masks read as data (the slowest, 20 to 28 s each), "a structured buffer of unknown stride", `image_gather4_l`, "a scalar memory read as data", addresses not from a root parameter. | The translator (masks-fix; VIctory #1225 and #1231 for first-lane code-table addresses; #193 and #704 for unknown strides, which also need `descriptor/`). | Counted by cause, named by shader in the details. |
| Refused wave64 compute pipelines | Compute programs the translator could only emit as `WaveSize(64)`. Not among session A–C′'s causes. | The translator (wave64 stack). | — |
| Indirect rectangle lists | `ExecuteIndirect` kind 0 with the console's rectangle-list topology: rectangles are expanded on the CPU for direct draws, and an indirect draw's vertex counts are on the GPU. | The GPU layer. | Expanded on the GPU (§5.1, VIctory #346). |
| Indexed rectangle lists | Under one a frame. | The GPU layer. | — (still dropped, with their own reason) |
| FidelityFX (FSR 4) programs | 20 programs refused with "no DXIL part". | The FidelityFX task (HLE). | — |

No drop in sessions A to C′ came from the descriptor tables ("no dynamic
slots"), so VIctory #1126 (tables given back after a rejected bind) is not
needed for these drops today.

## 5. What changed, and after

### 5.1 Indirect rectangle lists run (VIctory #346)

`ExecuteIndirect` kind 0 with the rectangle-list topology now runs
(`rectangles.rs`, ported from VIctory #346 by BastienRodz, with
fuzeofficlal's halfway-to-even half rounding on both paths): after the
argument clamp a pass rewrites each record into the list's own arguments
(the title's count respected; records past their vertex buffers or without
instances empty; `rects * 6` vertices allocated in the list's scratch), a
pass per per-vertex slot writes `(v0, v1, v2, v0, v2, v3)` with
`v3 = v0 + v2 - v1` as the direct path's `primitive::expand_rects` does,
and `ExecuteIndirect` reads only the list's buffers. Changed in the port:
the list's memory is the draw state's, pooled (no `host.rs` change); the
allocation counter is zeroed in the list before each draw
(`WriteBufferImmediate`); vertex buffers are read 4 bytes short of their
host copy's end; the fourth corner is `precise`. A WARP test runs the whole
expansion twice in one list on title buffers and compares it byte for byte
with the CPU's; it fails with the corner computed as `a + f + b`, and with
the counter not reset (36 vertices instead of 18).

What the title's rectangles are: session A counted them all under "no
vertex inputs", and session B logged at least 10 such pipelines in its
first second of world (`[vertex 167501e87c356551, pixel 9727fc3c51b2f11c]`, …): their
corners come from the vertex id. Since drops-fix (`d80e707`) those draw
each record as the title wrote it: 3 vertices a rectangle, from the
record's start vertex, on the indirect path and on the direct one. They
drew 6 a rectangle from vertex 0 before, twice the title's vertices. Their
vertex ids count from 0 in each record, as on the console (§5.1.1).
shortcut: the fourth corner, which the console makes from the vertex
shader's outputs, is not made; a vertex shader that reads its id past 2
draws whatever it computes for it. The pipelines are logged once each
("indirect rectangles without vertex inputs"). Making the fourth corner is
left to do: VIctory #255's remap (the vertex shader run for the three
corners), or a geometry stage as VIctory #1020 adds.

#### 5.1.1 Where the start vertex and start instance arrive

Under D3D12, `SV_VertexID` leaves out the draw's start vertex and
`SV_InstanceID` its start instance. A WARP test draws a record
`(3, 1, 6, 0)` with ids 0 to 2: cell 0 of four, not cell 2
(`draw::tests::indirect_rectangles_without_vertex_inputs_draw_their_first_triangles`).
The console does the same for the title's programs, so XTP adds neither
start to the ids. A start added by the translator would draw that record
in cell 2, where the console draws cell 0. Checked on 2026-11-24:

- **The hardware.** On AMD GPUs the vertex id (v0 of a hardware VS) and the
  instance id (v3) count from 0. The start vertex and start instance reach
  a program only in user SGPRs. For a direct draw the driver writes them
  there: `DRAW_INDEX_AUTO` has a vertex count and no start. For an indirect
  draw the command processor writes each record's start vertex and start
  instance into the two user SGPRs that the `DRAW_INDIRECT` packet names.
  A shader that needs a start adds that SGPR itself. The public reference
  is Mesa's RADV (`main` `6c103024`, 2026-11-24):
  - `ac_nir.c` sets `vertex_id_zero_based`, and nothing sets
    `instance_id_includes_base_index`;
  - `nir_lower_system_values.c` therefore makes the API's vertex index
    `load_vertex_id_zero_base + load_first_vertex`, and its instance index
    `load_instance_id + load_base_instance`;
  - `ac_nir_lower_intrinsics_to_args.c` reads the first of each pair from
    the vertex id and instance id VGPRs, and the second from the
    `base_vertex` and `start_instance` arguments, which `radv_shader_args.c`
    declares as user data (SGPRs);
  - `radv_cmd_buffer.c` writes those SGPRs for direct draws, and names them
    in `DRAW_INDIRECT` (`vertex_offset_reg`, `start_instance_reg`) for
    indirect ones.
- **The title's vertex programs read no such SGPR.** `xtp shader-info
  --disasm` lists 2,041 containers with a native VS program in
  `shaders.rpf`. Of those programs, 1,424 read v0 and 494 read v3. Their
  root parameters start at user SGPR 8; s0 to s7 are the console runtime's.
  - 632 programs read a user SGPR outside their root parameters:
    - 630 read only `s[4:5]`, with `s_swappc_b64`: the call into the fetch
      shader the runtime makes from the input layout (on the host the input
      assembler fetches the attributes, start applied);
    - 1 reads `s7` in an exec mask (`s_and_b64 exec, s[6:7], s[4:5]`);
    - 1 is the filter's mistake: the carry out of `v_subrev_co_ci_u32` is a
      destination.
  - None adds a runtime SGPR to v0 or v3, and the translator refuses no VS
    program for "a hardware SGPR input". So the start never reaches their
    code: v0 and v3 count from 0, as D3D12's ids do.
- **The title passes its own offsets as root constants.** `VS_ImGui` fetches
  its vertices itself and adds root constant `s11` (root parameter 2) to v0
  (`v_add_nc_u32 v0, s11, v0`). `VS_TiledLighting`, whose rectangles take
  their tile and corner from v0, reads root constant `s10` alongside it.

The listings and the filter that counted them are in
`E:\xtp-data\tmp\vstart\` (`vs_ids.py`, `vs-ids.txt`, `vs-dump.txt`), not in
the repository. Nothing of the translation changes, so a shader survey
before and after this check is the same by construction.

### 5.2 Wave64 mesh programs refused before their translation

A pipeline whose mesh program the host refuses for its 64-lane waves is
refused from the console program alone (`Needs::of_native`), with the
reason the translated check gives (a test checks both agree in every case;
a WARP test that neither the mesh nor the pixel shader of such a pipeline
is translated). Session B and C′ translated no mesh program, against 362 of
1,491 shaders in series-s's run. When mesh64 lets these programs run, the
refusal follows `Needs::mesh_reason` and stops by itself.

### 5.3 Sixteen pipeline workers

What the workers spent their time on in session B (8 workers, translation
times now logged):

| Translations (session B) | count | seconds | median | slowest |
|---|---:|---:|---:|---:|
| compute, refused by the translator | 88 | 145.2 | 1 ms | 27.9 s |
| of which lane masks read as data | 16 | 134.8 | | 27.9 s |
| compute, translated | 552 | 52.2 | 7 ms | 1.4 s |
| pixel | 353 | 5.2 | 16 ms | 39 ms |
| vertex | 161 | 0.3 | 1 ms | 11 ms |
| mesh | 0 | 0 | | |

In session C's stacks all eight workers were translating while 200
pipelines waited; in session B's, 8 to 10 of the process's 129 threads
were running, on 32 logical processors. The workers are now half the
logical processors up to 16 (`pipeline::make::MAX_WORKERS`; 8 before).
Session C′ (16 workers) against B (8), same build otherwise, same
pipeline cache:

| Translations finished | B (8 workers) | C′ (16 workers) |
|---|---:|---:|
| last 10 s of the load (−20 to −10 s) | 482 | 934 |
| world's first 60 s | 153 | 226 |
| time of each one | | +37 % (same shaders) |
| queue at the world reports | 199 → 238 (crash at +67 s) | 126 (+57 s), 7 (+82 s), 2 |

The queue drains about as late as in series-s's run (78 s): what is left
in it in the world is mostly the 16 slow refusals, which only the
translator can shorten (or a separate set of workers for known-slow
programs, as OCE does: `docs/upstream/watch.md` item 43; not done).

### 5.4 How to read the before and after

- The reports fall at different times into the world: A's only one at
  about +38 s of a very slow first world frame; B's from +33 to +65 s; C′'s
  from +57 to +93 s. "Being made" falls as the queue drains, and the
  refusals grow with the scene the title draws (C′ got furthest).
- More draws run after (42 → 270 and 339 a frame), so more of the title's
  passes reach pipelines that are refused or being made.
- Frames are very slow in sessions A to C′ (5 to 25 s): 3 to 17 million
  page faults and 1.1 to 2.2 GB of refreshes per frame, against 120,000 to
  260,000 page faults in series-s's runs of the same hour. That is outside this task's files (`shadow.rs`, `host.rs`,
  `resource/`; W1 landed on `main` since, `6a51f7b`) and was reported on the
  task.

## 6. What is left, and to whom

| Left | Drops per frame (B / C′) | Owner |
|---|---:|---|
| Wave64 mesh programs on 32-lane hosts | 1,700 / 3,451 | mesh64 (0a31f0cb) |
| Slow translator refusals holding the workers (lane masks) | most of "being made", 432 / 336 indirect | masks-fix; a slow lane in `make.rs` if it stays |
| Shaders that do not translate | 691 / 992 | translator tasks (VIctory #193 + #704 need `descriptor/` too) |
| Rectangles whose corners come from the vertex id (drawn, fourth corner not made; their vertex ids are the console's, §5.1.1) | 0 dropped | the fourth corner: VIctory #255's remap or #1020's geometry stage, not taken yet |
| Indexed rectangle lists | 1 | GPU layer, not done |
| FidelityFX programs without DXIL | — | FidelityFX task |

## 7. Method

- Sessions with `E:\tools\stall-probe\session.ps1` under `measure.lock`,
  one at a time, Series S through frame-prof's tooling patch
  (`XTP_PLAY_XTP_ARGS=--console series-s`, not delivered), `-Measure 150`
  (A: 60), `XTP_STACKS` every 10 s from +50 s. Runs: A
  `20261124-175709` (log lost when its cache was reset; its report is
  quoted above), B `20261124-180110`, A2 `20261124-180344`, C′
  `20261124-180617`, kept in `E:\xtp-data\drops\runs\`; counters in
  `E:\xtp-data\runs\stall-drops-{A,B,A2,C}\`; binaries in
  `E:\xtp-data\drops\bin-{A,B,C}\`.
- Each session had a cache of its own (`E:\xtp-data\drops\xtp-*.toml`),
  copied from one snapshot of the pipeline library and early list (1,348
  pipelines) and with no translated shaders, as for any new build, so that
  no session's pipelines warmed another's.
- `E:\xtp-data\drops\tools\causes.py` sums the "by cause" lines per report;
  `making.py` counts pipelines made and translation seconds per 10 s.
- Session C and series-s's run were read from their logs
  (`E:\xtp-data\cache\play\20261124-163332`, `…-173913`) with the old
  report: their per-reason lines are summed by kind and are lower bounds.
