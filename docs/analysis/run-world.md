# The running game in the world: frame time, missing image, hangs

What keeps Grand Theft Auto VI's first mission from 30 fps on the person's
machine (RTX 4080 SUPER 16 GB, Ryzen 9 7950X3D, 64 GB, Windows 11
26300), measured on XTP `main` at `2d0257e` with the diagnostics build
(`cargo build --profile diag -p xtp --features diagnostics`), on
2026-11-24. It answers `gpu.md` open questions 1, 2, 6 and part of 3 and 7,
and ranks the causes against the backlogs of `gpu.md`, `shader.md`,
`formats.md` and `hle.md`.

How the runs were made is in `run-keys.md` (golden save, `play.ps1`,
`XTP_KEYS`). Logs and captures are under `E:\xtp-data\runs\` and
`E:\xtp-data\run1.clean.log`; none is in the repository. Title code is
named by RVA (relative to `game_scarlett_master.exe`, build blake3
`b99f876b74d4a77f…`), shaders by their 16-digit hash.

## 1. Summary

- **A world frame takes 750 to 900 ms (median), 1.1 to 1.3 fps**, against a
  33 ms budget. It is spent, in order: CPU preparation of submissions
  (45-55 %, of which shadow copies 35-40 %), GPU execution (25-35 %),
  copy-back of GPU-written memory (11-14 %), the rest (title CPU,
  presentation, pipeline waits) 4-12 %. Everything is serial: each frame
  makes 21 to 40 host submissions (26 to 29 on average) and waits for each.
- **The copies are slow, not big**: 200-225 MB per frame move at
  0.6-0.8 GB/s. Behind them, the write watch checks 6.6 to 7.7 GB of
  declared ranges per frame (97 % unchanged), and one 182 MB buffer is
  refreshed in every submission.
- **About 40 % of the world's draws and dispatches never run**: 1 100 to
  1 700 dropped per frame (run means) against 1 800 to 2 400 drawn. Untranslated
  shaders are 57 % of the drops (one translation error alone,
  "an address in an SGPR that is no root parameter", 28 %), pipelines
  still being made 23 % (first 15-55 s only), indirect executions whose
  preparation fails 15 %, pipelines the NVIDIA driver refuses 5 %.
- **Every presented world frame is black** (mean level 0.0-0.3 of 255),
  title card included, although thousands of draws run. On this NVIDIA
  machine the likeliest extra cause is the refused 64-lane compute passes
  (section 6); the world's render targets could not be dumped yet.
- **The GPU hangs**: three of the five world sessions that lasted over
  35 s ended in a Windows driver reset (TDR) 38 to 64 s into the world,
  always on a submission of 8 to 30 lists that ran 4.8 to 5.3 s; two of
  them on the same pass (31 pipelines in common). Timed list by list, a
  single list takes up to 3.2 s.
- **What the title waits on**: its submission thread sits in XTP's
  `Host::execute` (refreshing shadows, then waiting for the GPU); its main
  thread waits in a title wait (RVA `0x23cf735`) for the frame behind it.
  The title is not the bottleneck: at most 20-40 ms of a frame are not
  XTP's.

The ranked list with shares and backlog items is section 9.

## 2. Runs

| Run | Start (UTC) | From | Options | World at | Time in world | End |
|---|---|---|---|---|---|---|
| `run1` (orchestrator) | 12:58:57 | empty data folder | captures (powers of two) | +68 s | 77 s | stopped by `timeout`, no reset |
| `A-fresh` | 13:10:35 | empty data folder | stacks (not reached) | +97 s | 38 s | **TDR**: submission of 30 lists, 5 169 ms |
| `B-save` | 13:14:09 | save | `XTP_D3D12_DRED=1`, captures, stacks at +100 and +130 s | +80 s | 64 s | **TDR**: 8 lists, 4 837 ms; DRED report |
| `play-test1` | 13:20:07 | golden | captures, memory scan | +74 s | 61 s | **TDR**: 10 lists, 5 296 ms |
| `play-C` | 13:25:38 | golden | `XTP_GPU_TIMING=1`, captures, `XTP_WATCH` | +53 s | 65 s | the title crashes itself (section 7.3) |
| `play-D` | 13:29:31 | golden | `XTP_GPU_TIMING=1` + `XTP_D3D12_DRED=1` | not reached: loading screen at 25 s per frame | – | stopped at +170 s |
| `play-E` | 13:33:03 | golden | `XTP_SHADER_LOOP_LIMIT=1024` (cold shader cache), 4 `rustc` running | +186 s | 24 s | stopped at +210 s |
| `play-E2` | 13:37:53 | golden | same, warm cache; first version of the no-focus launcher | – | – | the process vanished at +24 s, on the profile screen, with no log line and no crash record |
| `play-F` | 13:39:38 | golden | render-target dumps every 10-25 frames, 2 `rustc` | not reached: loading from +116 s, no frame presented after +122 s | – | the title crashes itself at +155 s (section 7.3) |

"Clean" timing runs are `run1`, `A-fresh` and `play-test1` (captures
cost a few ms per captured frame). `B-save` carries DRED's per-command
notes (about +10 to +20 % frame time, section 8). `play-C`, `play-D` and `play-E`
are diagnostic runs whose frame times are not comparable.

All numbers below come from XTP's own 5-second log lines (`xtp_gpu::host`
frame line, `xtp_gpu::copystat`, the per-reason drop lines), parsed by a
script kept outside the repository; "world" windows are those after the
first world frame.

## 3. Frame-time breakdown

### 3.1 Per run

Medians over the world windows (ms per frame; MB per frame):

| Run (windows) | Frame | Preparing | of which copies | textures | other | GPU | Copy-back | Not accounted |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `run1` (13) | 755 | 365 | 283 (224 MB) | 21 (105 MB) | 61 | 204 | 92 (4.6 MB) | 31 |
| `A-fresh` (5) | 829 | 460 | 344 (205 MB) | 21 (113 MB) | 95 | 293 | 121 (2.9 MB) | 103 |
| `B-save` (10, DRED) | 905 | 442 | 339 (198 MB) | 28 (131 MB) | 75 | 295 | 94 (4.7 MB) | 51 |
| `play-test1` (7) | 1 703 | 421 | 305 (208 MB) | 27 (138 MB) | 89 | 544 | 93 (3.2 MB) | 601 |

`play-test1` is the outlier: two of its windows hold 2 s of GPU time per
frame and 5.5 to 9.2 s frames, just before its TDR. Ranges seen in single
windows: preparing 107-760 ms, GPU 63-750 ms (2 070 ms in `play-test1`),
copy-back 6-157 ms.

"Not accounted" is the frame minus preparing, GPU and copy-back: the
title's own CPU work that does not overlap our submission, pipeline waits,
recording and presentation. It is 9-40 ms in steady windows and up to
300-400 ms in the first windows of a world, while pipelines are made (3.5).

Shares of a median frame (pooled `run1`, `A-fresh`, `B-save`, ≈ 830 ms):
preparing ≈ 51 % (copies ≈ 39 %, textures ≈ 3 %, other ≈ 9 %), GPU
≈ 32 %, copy-back ≈ 12 %, not accounted ≈ 5 %.

### 3.2 Where the preparation goes

Averages per world frame (`xtp_gpu::copystat`):

| | `run1` | `A-fresh` | `B-save` |
|---|---:|---:|---:|
| Host submissions | 29.3 | 25.7 | 28.1 |
| Shadow refreshes (ranges) | 7 483 | 7 957 | 8 286 |
| Bytes of those ranges | 6.3 GB | 7.3 GB | 7.4 GB |
| Write-watch queries, time | 1 024, 29 ms | 1 112, 35 ms | 1 178, 95 ms |
| Watched bytes checked | 6.6 GB | 7.3 GB | 7.7 GB |
| Page faults (process) | 171 k | 400 k | 241 k |

- About 97 % of the checked bytes are unchanged; 200-225 MB per frame are
  copied, in 280-345 ms: **0.6-0.8 GB/s**, where an in-process copy runs
  at 10 GB/s or more (estimate, not measured here: 20-30 ms for that
  volume). The cost is the copy path (`ReadProcessMemory` over thousands of
  ranges, `gpu.md` 6.2; 160-300 guard faults and 170-400 k page faults per
  frame), not the volume. `XTP_COPY_MODE=plain` (copies without the guard)
  is the switch to time it on a quiet machine.
- The range that dominates every copy line is one 182 MB title buffer
  (0xae00000 bytes of mapped memory, the "large buffer copied to the host"
  of the log), refreshed 23 times per frame: once per submission. Four
  other ranges of 26-108 MB follow (each 3-9 times per frame).
- "Other" preparation (60-95 ms) is the write-watch queries, the per-list
  deferred steps and the snapshots taken for copy-back.

### 3.3 GPU

- Median 200-300 ms per frame, single windows 63-750 ms, and 2 s in the
  windows before a TDR.
- With `XTP_GPU_TIMING=1` (each list submitted and waited for on its own,
  run `play-C`), 71 lists over 50 ms were logged in 65 s of world; the
  slowest took 864, 1 285, 1 572, 2 072 and 3 202 ms. **One list alone can
  exceed Windows' 2 s limit**: splitting submissions (`gpu.md` G11) bounds
  the sum, not this. The commands inside were not named: without
  `XTP_D3D12_DRED=1` the notes are empty (`commands=0`), and with both
  switches (`play-D`) the loading screen already ran at 25 s per frame and
  the world was never reached.
- The shader loop limit test (`XTP_SHADER_LOOP_LIMIT=1024`, `play-E`)
  recompiled every pipeline (its own cache entries) on a machine where four
  `rustc` were running; it reached the world only at +186 s and its 24 s
  there carry no usable frame line. The repeat with a warm cache
  (`play-E2`) died on the profile screen (section 2). Section 10 lists it
  as still open.

### 3.4 Copy-back

90-120 ms per frame for 3-8 MB of changed bytes: snapshots and
compare-and-copy of every range declared GPU-written, once per submission.
The volume needs a few ms; the per-submission scanning is the cost.

### 3.5 Pipeline waits

Not logged as time (`gpu.md` G2 is not done); visible as the draws dropped
"being made" and as the "not accounted" time of the same windows:

| Run | Library preloaded at start | Draws per frame dropped "being made", by window | Gone after |
|---|---|---|---|
| `run1` | 0 pipelines (first run) | 1 154, 863, 775, 662, 609, 540, 462, 299, 113, 0 | ≈ 55 s |
| `A-fresh` | 1 054 started early | 163, 158, 127, 0 | ≈ 20 s |
| `B-save` | 1 133 started early | 151, 141, 25, 1, 0 | ≈ 18 s |
| `play-test1` | 1 256 started early | 170, 54, 0 | ≈ 15 s |

The "not accounted" time of those first windows is 120-410 ms (bounded by
the 500 ms per-frame budget of `pipeline/make.rs`), against 20-40 ms
later. With the library kept on disk, pipeline waits cost the first
15-20 s of a world and nothing afterwards: `gpu.md` open question
"whether a second run is faster" is answered yes.

### 3.6 The title's own CPU time

After the pipeline warm-up, 9-40 ms per frame are not XTP's submission
work. Section 8 shows the title's threads waiting for XTP. The title's CPU
side is not what keeps the world from 30 fps today.

## 4. Dropped draws and dispatches

Average per world frame over 35 samples of four runs (`run1`, `A-fresh`,
`B-save`, `play-test1`; each sample is the drop list logged every 5 s,
which covers the frames of the last second):

| Cause | Per frame | Share of drops |
|---|---:|---:|
| Shader does not translate | 1 318 | 57 % |
| Pipeline still being made (first 15-55 s only) | 542 | 23 % |
| Indirect execution: preparation failed | 340 | 15 % |
| Pipeline refused by the NVIDIA driver | 115 | 5 % |

Over the same windows 1 770 to 2 430 draws per frame were drawn: about
40 % of the recorded work is dropped. Indirect: kind 0 (draw) 215 per
frame, kind 10 (mesh dispatch) 108, kind 2 (dispatch) 14, kind 1 (indexed)
1; all are "prepare failed" with a host pipeline present, so neither the
argument clamp nor the signature is the cause; `draw.rs` `prepare` /
`prepare_for` return false (tables, vertex buffers or topology).

### 4.1 Untranslated shaders by translation error

405 to 435 shaders per run fail to translate (442 distinct over four
runs), nearly all of them first used in the world. Weighted by the draws
they drop per frame:

| Translation error | Draws per frame | `shader.md` item |
|---|---:|---|
| an address in an SGPR that is no root parameter | 637 | R4 (gain on the pack 206 programs, but 48 % of the world's untranslated drops) |
| a scalar memory read as data | 233 | R1, then R2 |
| a structured buffer of unknown stride | 210 | R3 |
| `image_gather4_l` | 76 | S1 |
| a descriptor read from the first lane of computed VGPRs | 35 | R4 |
| a lane-mask join of an untracked mask | 29 | M1 |
| `image_sample_d_cl` | 25 | S1 |
| barycentrics read directly | 18 | P2 |
| a lane mask read as data | 16 | M1, M2 |
| a whole-wave section step the simulation does not follow | 14 | M1 (S3 for `v_permlanex16`) |
| a 64-bit image atomic | 10 | I1 |
| an address not from a root parameter / from a phi | 8 | R4 |
| a descriptor read only as data | 5 | R2 |
| others (select on an untracked mask, partial joins, lanes elsewhere) | 3 | M1, M2, R4 |

By draws dropped, the order of the shader backlog for this world is
**R4 → R1/R2 → R3 → S1 → M1**, not the pack order (M1 first by program
count). The heaviest mask errors in the pack (select on an untracked
mask: 91 shaders logged; lanes elsewhere read as data: 77) are compute
programs the world barely dispatches (under 2 draws per frame together).

### 4.2 By shader

The 30 heaviest, with the runs (of 4) in which they appear:

| Draws/frame | Runs | Stage, hash | Cause |
|---:|---:|---|---|
| 176 | 4 | compute `e30970f1727063fd` | structured buffer of unknown stride |
| 143 | 4 | mesh `910b4be059bfb28f` | scalar memory read as data |
| 128 | 4 | compute `3b90165a77d95cdf` | address in an SGPR that is no root parameter |
| 100 | 4 | compute `f6e47efa50226beb` | address in an SGPR … |
| 87 | 4 | compute `caa78b8ca62bc3d6` | address in an SGPR … |
| 61 | 4 | mesh `321d411b20a35ef9` | scalar memory read as data |
| 41 | 4 | compute `28918b7150a7aad5` | address in an SGPR … |
| 36 | 4 | compute `5552dac6a2b7a1fd` | address in an SGPR … |
| 36 | 4 | compute `c9da71da40af2e28` | refused by the NVIDIA driver |
| 32 | 4 | compute `3df629ffc3d0a2a6` | address in an SGPR … |
| 32 | 4 | compute `162216b9cfd91aaf` | address in an SGPR … |
| 31 | 4 | compute `f28a33a89eaa2caa` | address in an SGPR … |
| 31 | 4 | compute `6f6a359d1166bb27` | address in an SGPR … |
| 30 | 4 | compute `4b5aedc0e96b0770` | address in an SGPR … |
| 23 | 4 | compute `87bf9e3f2bf2f052` | `image_gather4_l` |
| 23 | 4 | pixel `7bdefdfde461f6e3` | `image_sample_d_cl` |
| 23 | 4 | compute `764ca63daec9c616` | `image_gather4_l` |
| 22 | 4 | compute `f8f50ab85e7b10bb` | refused by the NVIDIA driver |
| 22 | 4 | compute `660814153af43185` | refused by the NVIDIA driver |
| 21 | 4 | mesh `adc5c689b0552cc2` | descriptor from the first lane of computed VGPRs |
| 19 | 4 | compute `198f6dfd2b81bcea` | address in an SGPR … |
| 18 | 4 | pixel `2d0cace65bc05bb2` | barycentrics read directly |
| 16 | 4 | vertex `4e35b816f2f91d68` | scalar memory read as data |
| 15 | 4 | compute `4ddd6f9702fab2a7` | address in an SGPR … |
| 15 | 4 | pixel `b37565055f2f7d3b` | lane-mask join of an untracked mask |
| 15 | 4 | compute `a25ed0e3d4d521ba` | refused by the NVIDIA driver |
| 14 | 4 | compute `b22a16a178976e1b` | whole-wave section step |
| 14 | 4 | compute `2561af9577c47289` | lane-mask join of an untracked mask |
| 13 | 4 | compute `5e68767498c57eb6` | address in an SGPR … |
| 12 | 4 | mesh `ef5409053e14aa82` | lane mask read as data |

Thirteen compute shaders with the SGPR-address error drop 25 to 130
dispatches each, every frame, in every run: one fix of that error brings
back a whole family of passes.

## 5. NVIDIA-specific refusals

- **118 compute pipelines refused** at creation (`CreateComputePipelineState`:
  "The parameter is incorrect", `E_INVALIDARG`): 113 to 118 per run, 118
  distinct over four runs; 115 dispatches per frame dropped (section 4).
  All are compute shaders that translate. The translator declares `WaveSize` equal to the console's
  wave (64 for wave64 programs) for cross-lane code, masks read as numbers
  and one-wave groups with LDS (`emit/lower.rs`); this GPU runs 32-lane
  waves only. Fix: `shader.md` L1 (one console wave per group as two
  32-lane halves), then L2. On NVIDIA L1 is not "late": it is the only way
  these passes run.
- **Two textures refused** at creation (`resource.rs`, "no host texture",
  `E_INVALIDARG`), created at boot, both 2560×1440 (the world's internal
  resolution at a 4K output): DXGI format 86 (`B5G5R5A1_UNORM`) with render
  target and unordered access, and format 115 (`B4G4R4A4_UNORM`) with render
  target. The driver refuses both (D3D12 makes these uses optional for
  these formats). Every view of them is null: writes are lost, reads
  return zero. No backlog item covers
  it (`formats.md` has no host-format fallback); new item **W4** (section 9).
- One view refused: an unordered-access view of a typeless format
  (`R16_TYPELESS`, format 53), "view the host does not allow". Part of
  `gpu.md` G7 (view formats).
- The CBV/SRV/UAV heap is capped at 1 000 000 descriptors (the title asks
  for 1 245 184). The diagnostic for a title slot past the cap never fired
  in any run: the world does not use those slots so far.

## 6. Black frames and the dark scene

- Every presented world frame captured is black: mean level 0.0 to 0.3 of
  255, maximum 0 to 2 (runs `play-test1`, `play-C`, `B-save`). The loading
  screen before them is fine (mean ≈ 84). XTP's own black-frame warning
  fires at the switch from the loading screen to the world.
- The first world frames draw 900 to 5 900 draws each (frame lines of
  `play-test1` and `play-C`) and still present black, so the scene is not
  missing for lack of draws: the chain that turns it into the presented
  image outputs zero.
- The README reports, on an AMD RX 6700 XT, that the mission's title card
  renders and the scene behind it is dark. Here not even the title card
  shows. What differs on this machine is NVIDIA-only: 118 compute
  pipelines refused for their 64-lane waves and two 2560×1440 targets that
  do not exist on the host (section 5). Exposure and luminance reductions,
  and the passes that compose the final image, are typically cross-lane
  compute, exactly what the wave-size refusal removes. Hypothesis, not
  proven: on NVIDIA a refused compute pass in the final chain blacks the
  whole frame, on top of the darkness `gpu.md` 6.1 explains (copies out of
  order, stale bindless data) that AMD shows as well.
- The render targets of a world frame were not dumped. Run `play-F`, set
  to dump every 25th frame, stopped on the loading screen before the world
  (section 7.3); the machine's rules now allow dumps of a few chosen frames
  only, and the frame number at which the world starts varies from about
  1 180 to 2 160 between runs, so a frame list cannot be aimed in advance.
  New item **W5** (section 9.4) dumps by state instead of by number. The
  front end's targets (profile screen: two 3840×2160 colour targets and a
  depth target) were dumped and match the presented frame.

## 7. GPU hangs

### 7.1 What happened

| Run | In the world | Submission | Pipelines in it |
|---|---|---|---|
| `A-fresh` | +38 s | 30 lists, 5 169 ms, then `DXGI_ERROR_DEVICE_HUNG` | 83: 14 compute, 68 mesh/pixel pairs, 1 vertex/pixel; none in common with the two below |
| `B-save` | +64 s | 8 lists, 4 837 ms | 33: 13 compute (`0c0380d9ec648626`, `0ccc14877cbec21e`, `1c8165626a581de4`, `54bcc8478432ba15`, `565cc0b38920b229`, `5af5bbc0c3267799`, `738350c1a01b713a`, …) and mesh/pixel pairs |
| `play-test1` | +61 s | 10 lists, 5 296 ms | 36, of which 31 are `B-save`'s |
| `run1` | – | no reset in 77 s | |

Each reset blanked the screens for a moment; the title exited with
`host GPU failure` in its log and in `crashes.log`.

### 7.2 Where (DRED, `B-save`)

One list stopped at command 590 of 714, inside a run of mesh dispatches
(commands 549 to 685 between two barriers) of pipeline
`[mesh 51ff151f75cd5759, pixel 7fe75645c406cac6]`. Two of its heap views
(slots 327 880 and 883 099, user-data 8 and 9) are null, so the shader
reads zeros where it expects data. The same pipeline draws without trouble
from the first world frame on in every run, so the hang depends on the data
of that frame, not on the shader alone.

Reading:
- The hang is a long-running GPU command, not only a long submission:
  with lists timed alone, one list takes up to 3.2 s (3.3). Bounded
  submissions (`gpu.md` G11) are necessary, not sufficient.
- Candidates for the long command, from the evidence: loops in translated
  shaders that only end at the 2^20-per-lane limit when fed zeros or stale
  counts (null views here; stale GPU-written data in `gpu.md` 6.1), and
  indirect counts read from stale memory. The loop-limit run that would
  separate them is still to do on an idle machine (section 10).

### 7.3 The title stops itself when its frames stall

Twice the title crashed the same way: a write to address 0 at RVA
`0x23cf683` with `rax`, `rbx` and `rbp` all 0, in a thread started by the
title's thread entry (`0x2215ea0` → `0x2215c35`). Each time its crash
reporter wrote a 32-35 MB minidump to `T:\` (the run's data copy) and
tried to upload it to its crash service; XTP's network policy blocked the
connection.

- `play-C` (`XTP_GPU_TIMING=1`): world frames of 7.8 s, then 17.5 s; the
  crash came 65 s into the world.
- `play-F`: on the loading screen, no frame presented from +122 s on (the
  last frame line, at 30 fps, is at 13:41:39); the crash came 35 s later.
  Why the presentation stopped is not known (no warning in the log).

A store to address 0 with every register cleared is how engines crash on
purpose, and the code sits in the same RVA range (`0x23cf0xx`-`0x23cf7xx`)
as the main thread's frame wait (section 8). Reading: a stall detector
that stops the title when no frame completes for a while. With 9 s frames
(`play-test1`) it did not fire, so its threshold lies between about 9 and
35 s without a frame. Once frames are normal it does not matter; for
diagnostic runs it caps how slow a frame may be.

## 8. What the title waits on

Every thread's stack was taken twice in the world (`XTP_STACKS`, run
`B-save`, +100 s and +130 s; XTP frames named from the build's PDB):

- **The submission thread** (title priority 15, calling
  `ExecuteCommandLists` from RVA `0x24821e3`) is inside XTP both times:
  first in `Host::execute` → `Shadow::update` → `watch::writes`
  (`GetWriteWatch`), then in `Host::execute` waiting for the host fence.
  The title's frame is gated on our serial submission.
- **The recording threads** (priority 8) were busy at +100 s, four of
  them, all inside XTP (at +130 s none was): two in `draw::State::reads` (reading title memory to build
  DRED's per-command note, a cost of the DRED switch only), one in
  `indirect::bound` → `HostList::budget` → `Host::shared_buffer` creating a
  D3D12 buffer, one in `draw::State::bind` → `HostList::scratch` →
  `Host::upload_buffer` creating a D3D12 upload buffer. Buffer creation in
  the middle of recording is a kernel call per new scratch chunk.
- **The title's main thread** (`title-main`) waits, both times, in the
  title's wait wrapper (`0x56a0d`, over `WaitForSingleObject`) called from
  `0x23cf735`, under the frame loop (`0x8126a7` ← `0x53ec040`). Reading:
  it waits for the frame being rendered before it starts the next one,
  the usual update/render hand-off. In neither sample does it wait inside
  XTP's HLE.
- 60-odd other title threads (workers started at `0x2215ea0`) are idle in
  waits; the 32 `dstorage` and 8 pipeline threads of XTP are idle too.

So the title waits on its render frame, and the render frame waits on
XTP's synchronous submission (preparation, GPU, copy-back). Nothing in
the two samples points at a title-side wait on a GPU value XTP never
writes. `gpu.md` question 7 (does the title read GPU results at a fixed
frame latency?) stays open: two samples cannot show it.

## 9. Ranked causes and their fixes

### 9.1 Frame time (target 33 ms; median today ≈ 830 ms)

| # | Cause | Share of the frame | Backlog |
|---|---|---|---|
| 1 | Shadow refresh on the CPU before each submission: 200-225 MB copied at 0.6-0.8 GB/s through `ReadProcessMemory`, 6-8 GB of watched ranges checked, the same 182 MB buffer refreshed in every one of ~28 submissions | 40-45 % (330-400 ms) | short term **W1** (new): refresh each shadow range at most once per frame unless the write watch says it changed since, and copy committed pages with a plain copy instead of `ReadProcessMemory` (`XTP_COPY_MODE=plain` already exists for timing); then `gpu.md` G16–G18 (GPU-visible title memory) and `formats.md` F6 (journal of written pages) remove it |
| 2 | GPU execution | 25-35 % (200-300 ms), 2 s spikes | measure first (section 10: commands, loop limit); `gpu.md` G3–G6 (stale data that drives loops and indirect counts), G11 (bounded submissions), G13 (indirect) |
| 3 | Copy-back of GPU-written ranges, once per submission | 11-14 % (90-120 ms) | `gpu.md` G8 (asynchronous completion) then G16 (coherent readback window); xcl copies back only what the CPU reads |
| 4 | Other preparation: write-watch queries, deferred steps, snapshots | 8-11 % (60-95 ms) | W1, G8 |
| 5 | Serial submissions (26-37 per frame, each drained) | inside 1-4 | `gpu.md` G8, G10 |
| 6 | Pipeline waits | first 15-20 s of a world only (≤ 400 ms per frame then), 0 after | `gpu.md` G12; the on-disk library already works |
| 7 | Title CPU, recording, presentation | 3-5 % (20-40 ms) | none needed yet; recording's buffer creations (section 8) → **W3** (new): pre-allocate scratch and shared buffers outside recording |

Even with 1, 3, 4 and 5 gone, the GPU's 200-300 ms remain: 30 fps needs
both the memory rewrite and the GPU time explained (section 10).

### 9.2 Missing image

| # | Cause | Share | Backlog |
|---|---|---|---|
| 1 | Untranslated shaders | 1 318 draws/frame, 57 % of drops (74 % after warm-up) | `shader.md` R4 (637), R1+R2 (233), R3 (210), S1 (101), M1/M2 (60), P2 (18), I1 (10) |
| 2 | Black presented frames | 100 % of the picture | section 6: `shader.md` L1 first on NVIDIA (refused cross-lane compute), `gpu.md` G3–G6 (ordering, stale bindless data), W4; W5 to see which target goes black |
| 3 | Indirect preparation fails | 340 draws/frame, 15 % | **W2** (new): find why `prepare`/`prepare_for` refuse kind 0 and kind 10 with a host pipeline bound (log the reason per refusal), then `gpu.md` G13 |
| 4 | Pipelines being made | 542 draws/frame early, 0 after 15-55 s | `gpu.md` G12 |
| 5 | NVIDIA wave size | 118 pipelines, 115 dispatches/frame, 5 % | `shader.md` L1, L2 (to move earlier on NVIDIA) |
| 6 | NVIDIA formats 86 and 115 (2560×1440 targets) | 2 render targets with every write lost | **W4** (new): create such resources in a supported format (`R16_UINT` or `R8G8B8A8_UNORM`) and convert in views and shaders, or alias them as typeless 16-bit |
| 7 | Typeless UAV view (format 53) | 1 view | `gpu.md` G7 |

### 9.3 Hangs

| # | Cause | Effect | Backlog |
|---|---|---|---|
| 1 | One list running longer than 2 s (up to 3.2 s timed alone), in mesh passes fed null views | TDR in 3 of 5 world sessions, 38-64 s in | `gpu.md` G11 (bounded submissions) plus whatever section 10 finds (loops, stale data: G3–G6); W2 for the null views |
| 2 | No frame for about 17-35 s (very slow diagnostic frames, or a stalled presentation as in `play-F`) | the title stops itself (section 7.3) | none once frames are normal; keep diagnostic frames under ~9 s; the `play-F` stall itself is unexplained |

### 9.4 New items

- **W1. One refresh per range per frame, by plain copy** (S). In
  `shadow.rs`, a range refreshed earlier in the frame and not written since
  (write watch) is skipped; committed pages are copied with `copy_nonoverlapping`
  instead of `ReadProcessMemory`. Test: `copystat` refreshes per frame fall
  from ~8 000 to the number of distinct ranges; copies above 5 GB/s.
- **W2. Indirect preparation refusals logged and fixed** (S). `draw.rs`
  `prepare` / `prepare_for` return a reason (no tables, no vertex buffer,
  rectangles, root layout) that the drop line shows; then fix the top one.
- **W3. Scratch and shared buffers made before recording** (S). Pools in
  `host.rs` filled at device creation and between frames, so recording
  never creates a D3D12 resource.
- **W4. Host formats the GPU lacks** (M). At resource creation, check
  `CheckFeatureSupport(FORMAT_SUPPORT)` for the requested bind flags; for
  `B5G5R5A1_UNORM` and `B4G4R4A4_UNORM` without render target or typed UAV
  support, create the resource as 16-bit `R16_UINT` and pack and unpack in
  translated shaders (or as `R8G8B8A8_UNORM` with a conversion pass when
  the title copies it out). Touches `resource.rs`, `descriptor/host.rs`,
  the shader ABI for typed stores.
- **W5. Dump the render targets of the n-th world frame** (S, diagnostics).
  `XTP_DUMP_WORLD=<n>,…` arms `capture.rs`'s target dump for the n-th
  frame after the title's "not in game" word (`run-keys.md` section 5,
  RVA `0x9e58138`) falls to 0, instead of absolute frame numbers that vary
  by a thousand between runs. Needed for section 6 under the machine's
  "few chosen frames" rule.

## 10. Still open

- **Which commands are slow on the GPU.** `XTP_GPU_TIMING` + `XTP_D3D12_DRED`
  is too slow to reach the world; a lighter note (pipeline and kind only,
  no memory reads) under `XTP_GPU_TIMING` alone would name them.
- **Loop limit.** Repeat `XTP_SHADER_LOOP_LIMIT=1024` with its cache warm and
  no compilation running, and compare GPU time and resets with section 3.3.
- **Render-target content of the black frames**: which target goes black
  first, with W5, on a quiet machine (or the cloud VM).
- **Why the presentation stopped** on the loading screen of `play-F`
  (section 7.3) and why `play-E2`'s process vanished at +24 s with no log
  line (it ran under the first version of the no-focus launcher, without a
  standard input; the later version gives it an empty one and records the
  exit code).
- **A "player controllable" flag** (`run-keys.md` section 5), once the
  world shows an image.
