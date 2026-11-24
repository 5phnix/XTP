# The world frame and the loading frames on main: where the time goes

Where the 5 to 50 s of a loading frame and the seconds of a "world frame"
go on today's `main` (`488f2b4`), measured on the person's PC (RTX 4080
SUPER 16 GB, driver 621.01, Ryzen 9 7950X3D, 64 GB, Windows 11 26300),
with every thread of the process sampled every 0.5 to 1 s. It
replaces the ranking of `run-world.md` (measured at `2d0257e`) for the
current code and points each cost at the task that removes it. Times are
UTC; GiB are 2^30 bytes, as VidMm's counters print them. Logs and samples
are under `E:\xtp-data\` (paths per session in section 2); none is in the
repository. Title code is named by RVA (build blake3 `b99f876b74d4a77f…`);
XTP code by function and line at `488f2b4`.

## 1. Summary

- **Nothing computes.** During the world load, of about 125 threads, 18
  samples in 52 316 are running code, and the process's 3D engine use stays
  under 1 %. Every busy thread is waiting for the video memory manager
  (VidMm) to admit a new allocation, which past ~9.5 GiB resident takes
  about **0.3 to 0.5 s per allocation, whatever its size** (`stall.md`):
  0.29 to 0.48 s in the three sessions, during the load and in the world.
- **A loading frame** lasts as long as the title's loading thread (start
  `0x2215ea0`, priority 7) takes to get to the next one, and that thread
  spends **97 % of its time in VidMm waits that XTP causes**. They are 70 %
  `residency::make_resident` of each host texture and 25 %
  `Host::shared_buffer`, the host copy of each title buffer used for the
  first time. Timed in session B: the 15 loading frames longer than 1 s
  last 146.8 s, of which 140.6 s are that thread's creations (326
  `make_resident`, median 304 ms; 86 `shared_buffer`, median 348 ms). No
  loading frame is shorter than about 1.3 s, because each one makes 4 to 5
  new buffer copies and, in diagnostics builds, a probe readback buffer.
- **"Outside submissions"** is the frame time minus XTP's submission
  preparation and the time title threads wait for submissions. It holds
  everything else: the title's own code and waits, and every HLE and D3D12
  call made from title threads outside `Host::run` (resource creation and
  its residency wait, host copies of buffers, recording, pipeline waits).
  In a loading frame it is 95 to 99 % of the frame, and nearly all of it is
  the loading thread's VidMm waits. The "on the GPU" part (0.3 to 4 s per
  frame for 2 submissions of 2.5 lists, 2 to 3 ms in the front end) is the
  queue waiting, not the GPU working.
- **The "world frames" at 1 to 7 draws are not world frames.** They are the
  3 to 7 transition frames presented right after the title's in-game flags
  flip, built like the loading screen (2 submissions of 2.5 lists, 0 to 37
  drops, 0 ms of pipeline waits). After them **no frame is presented at
  all**: 43 s in T2, 53 s in A, 132 s in B, each time until the session
  ends. The first real world frame is still being recorded. The title's
  recording threads wait on XTP's lazy creations: B timed 314 of them in
  those 132 s, 304 s of thread time (`shared_buffer` 170 s, `make_resident`
  66 s, `upload_buffer` 55 s). Or they wait on XTP locks held through such
  a wait: `shadow::find` and `HostHeap::mirror`, 46 to 52 % of their busy
  samples. No draw is skipped; the title waits inside XTP.
- **In Series S mode the same code reaches the world in about 10 s of
  loading** (session C): the title's smaller budgets keep the process at
  8.19 GiB resident, under the threshold. Only 7 creations take 20 ms or
  more, and the first world frame is submitted at once: 2.6 fps, 39
  submissions, 79 draws, 4 711 dropped. So the PC's ceiling, combined with
  XTP's way of creating resources, explains the 1 to 7 draws. The title
  then crashes about 7 s into the world, right after its FSR 4 provider
  fails to deserialize its pipelines in XTP (pipeline state slot `0x58` not
  implemented). The link between the two is not established.
- **Top costs**, with one root for both: one VidMm admission per resource
  that XTP creates on a title thread. Pooling host textures (tex-pool,
  450ec9ba) removes about 70 % of a loading frame. Pooling XTP's own
  buffers and making them outside its locks removes the rest of the load
  and unblocks the world frame. Keeping the process under the threshold
  (b70e1cc4) removes the driver's share too. The ranking with owners is in
  section 8.

## 2. Sessions

| Session | Start | Binary | Options | Front end | Loading | World flags | After them |
|---|---|---|---|---|---|---|---|
| T2 (intro-skip) | 15:47:10 | intro-skip's worktree: `main` `5b51ddd` + its watchdog follow-ups, diag | `XTP_WATCHDOG_HOOK=1`, autopilot, golden, `xtp_gpu::resource=debug` | to +24 s | +24 to +184 s (intro-skip's states), 160 s; VidMm jump at 15:47:44–48 | 15:50:17 | 3 frames (1, 7, 2 draws), then none until 15:51:14 |
| A (this task) | 16:09:02 | `main` `488f2b4`, diag (sha256 `cb1b529f…`) | as T2, plus `XTP_STACKS` every 0.5 s from +5 to +340 s (613 dumps) | 30 fps to 16:09:33 | VidMm jump at 16:09:44 to the world flags at 16:13:16: 212 s, about 25 frames | 16:13:16 | 3 frames (11, 1, 1 draws), then none until 16:14:16 |
| B (this task) | 16:25:33 | A's binary + the timing patch (sha256 `e949deb0…`) | as A, `XTP_STACKS` every 1 s (336 dumps), `xtp_gpu::frame` and `xtp_gpu::shadow` debug, `--measure 150` | 30 fps to 16:26:05 | VidMm jump at 16:26:10 to the world flags at 16:28:48: 158 s, 15 frames over 1 s | 16:28:48 (+191 s) | 7 frames in 18 s, then none until 16:31:18 (132 s) |
| C (this task) | 16:33:29 | B's binary, `xtp run --console series-s` | as B | 30 fps to 16:33:59 | 16:34:01 to 16:34:10.6: about 9.6 s | 16:34:10.6 (+41 s) | a world frame at 2.6 fps, 79 draws; the title crashes at 16:34:17 (section 6) |

- T2: log `E:\xtp-data\cache\play\20261124-154712\run.log`, VidMm and GPU
  counters `E:\xtp-data\runs\stall-T2-watchdog-hook`.
- A: log `E:\xtp-data\cache\play\20261124-160904\run.log` (stacks
  included), counters `E:\xtp-data\runs\stall-frame-prof-A`. The session
  hit `session.ps1`'s hard stop at 360 s while the title was releasing its
  video memory, after the autopilot's 60 s in the world.
- B: log `E:\xtp-data\cache\play\20261124-162537\run.log`, counters
  `E:\xtp-data\runs\stall-frame-prof-B`, patch
  `E:\xtp-data\runs\frame-prof-B-timing.patch`. Hard stop at 360 s, at
  the end of the autopilot's 150 s.
- C: log `E:\xtp-data\cache\play\20261124-163332\run.log`, counters
  `E:\xtp-data\runs\stall-frame-prof-C-seriesS`; the title crashed
  (section 6).

All start from the golden save with the autopilot (`cargo xtask play
--measure <s>` through `session.ps1`, under `measure.lock`), with no other
game session running and no display change. The PC ran other workers'
builds on the farm and the Hangar app (14.7 % of the 3D engine on average
in A, 20 % in T2).

In every session VidMm's local segment jumps from 5.5 to 9.5 GiB resident
at the start of the load and then grows by one allocation per 0.29 to
0.48 s (A: 8 545 → 9 052 resident allocations in 210 s, 9.54 → 9.98 GiB,
free pages 6.13 → 5.69 GiB; T2: 8 417 → 8 699 in 135 s; B: 8 492 → 9 020
in 151 s during the load, then 9 029 → 9 333 in 131 s in the world, up to
10.08 GiB). That is the admission rate `stall.md` measured with a probe
outside XTP and the title. Session C (Series S) stays under the threshold.

## 3. What the frame line measures

The 5-second frame line (`host.rs`, `Meter::frame`) divides the time since
the last line by the frames presented in it, and splits a frame into:

- **preparing**: `Host::run_as` up to `ExecuteCommandLists`, without its
  waits: deferred steps, shadow refreshes (copies), texture refreshes;
- **on the GPU**: measured by the completion thread (`xtp-gpu-finisher`)
  from each submission (or the end of the one before) until it is seen
  done. It is not taken off the frame;
- **copying back**: GPU-written ranges written back to title memory;
- **waiting for them**: time title threads spend in `Host::waiting`
  (finishing earlier submissions, `settle`, the `drain` that `present`
  runs after `Present`), summed over threads;
- **outside submissions** = frame − preparing − waiting, at least 0.

So "outside submissions" is everything a title thread does that is neither
preparing a submission nor waiting for one: the title's own code and its
waits on its own events, every HLE call (files, DirectStorage, memory), and
every D3D12-side call XTP makes from a title thread outside `Host::run`.
That includes resource creation at an address with its residency wait
(`create_at_address` → `hand_out_resource` → `host_texture` →
`residency::make_resident`), the lazy host copies of title buffers
(`shadow::find` → `Host::shared_buffer`), command list recording
(`draw::State::bind`, uploads, budgets), pipeline waits, and in diagnostics
builds the present probe's readback buffer. None of these is timed on its
own today. The only creation that logs its time, "a host texture was slow
to make" (from 100 ms), times `CreateCommittedResource` alone, which is
fast since the textures are created not resident. Its residency wait right
after is logged only when it fails. That is why T2 saw "only 2 slow
creations" in a load where each texture waited 0.3 to 0.5 s.

## 4. A loading frame

### 4.1 The frames

Session A, the loading screen after the VidMm jump (frame lines, ms per
frame):

| Line | ms/frame | preparing | on the GPU | waiting | outside | submissions/frame | draws/frame |
|---|---:|---:|---:|---:|---:|---:|---:|
| 16:10:03 | 6 591 | 2.1 | 3 112 | 781 | 5 808 | 2.0 | 12 |
| 16:10:18 | 15 919 | 2.4 | 2 041 | 400 | 15 516 | 2.0 | 12 |
| 16:10:53 | 31 537 | 1.7 | 1 290 | 487 | 31 048 | 2.0 | 11 |
| 16:11:43 | 49 716 | 2.4 | 735 | 288 | 49 425 | 2.0 | 12 |
| 16:12:06 | 23 404 | 2.4 | 12 | 1 | 23 400 | 2.0 | 12 |
| 16:12:34 | 28 056 | 6.4 | 1 129 | 1 126 | 26 923 | 2.0 | 11 |
| 16:12:57 | 9 516 | 2.4 | 580 | 571 | 8 943 | 2.0 | 12 |

T2's loading frames have the same shape: 5.6 to 44.4 s, outside 5.1 to
40.5 s, on the GPU 0.16 to 3.97 s, 2 submissions of 2.5 lists, 11 or 12
draws. In the front end the same two submissions took 2.4 to 3.1 ms on the
GPU at 30 fps.

### 4.2 Per thread

Session A, 420 dumps from 16:09:46 to 16:13:15 (0.5 s each; a share of a
thread's samples is a share of its wall time):

| Thread | Share | Where (XTP frame, title caller) |
|---|---:|---|
| **Loading thread** (title worker `0x2215ea0`, priority 7, the one that creates the streamed textures and records the loading screen's copies) | 69.5 % (≈ 146 s) | VidMm: `residency::enqueue` (residency.rs:131) ← `make_resident` ← `resource::resident` (resource.rs:1117) ← `hand_out_resource` ← `create_at_address` (resource.rs:1552) ← title `0x193dffe` |
| | 25.5 % (≈ 54 s) | VidMm: `Host::shared_buffer` (host.rs:2839, `CreateCommittedResource` of a CUSTOM write-back heap in system memory) ← `shadow::find` (shadow.rs:500) ← `shadow::copy` ← `list::copy_buffer_region` ← title `0x4f4600` |
| | 1.7 % | releases: `object::release_boxed` (object.rs:138) → `DestroyAllocation2` |
| | 2.4 % | XTP locks (`shared_buffer`, `hand_out_resource`), title waits |
| **Presenting thread** (title worker, priority 15: `ExecuteCommandLists` from `0x24821e3`, `PresentX` from `0x24824c7`) | 85.0 % | title wait `0x56a0d` ← `0x4e7f358` (waits for its next frame) |
| | 9.0 % (≈ 19 s) | VidMm: `capture::probe` (capture.rs:622) → `CreateCommittedResource` of a readback buffer → `MakeResident_Worker` → `PerNodePagingOperation::Wait`, in `Host::present`, **diagnostics builds only** |
| | 2.6 % + 1.7 % | `present`'s `drain` (`wait_gpu`, host.rs:1257) and the rest of `present` |
| | 1.2 % | the NVIDIA driver waiting on the CPU inside `ExecuteCommandLists` (`run_as`, host.rs:1110) |
| **title-main** | 83.1 % | sleep in its frame wait loop `0x23cf729` (the function of the title's watchdog) ← frame loop `0x8126a7` |
| | 16.9 % | title waits `0x509898d`, `0x5df645` |
| 13 title workers, priority 8 | 98.4 % idle | 1.0 % `make_resident` (← `create_at_address`, title `0x193e2db`), 0.6 % waiting for the `shadow::register` lock |
| 8 title workers priority 5, the other priority-15 workers, Windows.Gaming.Input | idle or sleeping | |
| `xtp-gpu-finisher` | 92.9 % idle | 7.1 % waiting for the GPU |
| 32 `dstorage`, 8 `xtp-pipelines`, 4 D3D background, 7 NVIDIA | 100 % idle | |

Running code anywhere in the window: 15 samples in title code and 3 in XTP
(Opus decoding), out of 52 316 thread samples (124–125 threads per dump).

Read against the frames: the loading thread alternates long runs of
`make_resident` (15 to 40 s, one texture per 0.45 s) with short runs of
`shared_buffer` (2.5 to 5 s), and **every present comes right after a
`shared_buffer` run** (19 of 19 present episodes have one in the 6 s
before), when that thread records the loading screen's copies.
title-main sleeps in its frame wait meanwhile, and the presenting thread
waits for the frame. A loading frame therefore lasts as long as the
textures the loading thread creates before it records the next frame, at
about 0.45 s each, plus the host copies of the buffers that frame copies,
plus the probe's readback buffer (diagnostics). Session A had 316 texture
creations at an address in the load (`xtp_gpu::resource` debug lines;
median gap 0.45 s, p90 0.92 s, 207 s in all) and 44 buffers. T2 had 349
creations in 145 s (median gap 0.28 s).

Session B times the same waits instead of sampling them. It runs the
same binary plus an experimental patch, not delivered
(`E:\xtp-data\runs\frame-prof-B-timing.patch`): a log line for each
`shared_buffer`, `upload_buffer`, host texture `CreateCommittedResource`,
`make_resident` and probe readback creation of 20 ms or more, with its
thread, and a line per present (`xtp_gpu::frame=debug`). From the VidMm
jump (16:26:10) to the world flags (16:28:48), the loading thread logs 417
such creations, 146.7 s in all, of 158 s: `make_resident` 326 (median
304 ms, p90 574 ms, at most 4.8 s), `shared_buffer` 86 (median 348 ms),
host texture creation 5. **The 15 loading frames longer than 1 s last
146.8 s in all, and the loading thread's creations that end in them take
140.6 s (96 %).** The longest frame, 63.2 s, holds 59.9 s of them. Every
loading frame also has 4 to 5 `shared_buffer` (1 to 4 s) and one probe
readback (0.3 to 1.2 s, 16 in all, 10.6 s), so **no loading frame is
shorter than about 1.3 s even when no texture is created**. These are the
host copies of the buffers the title uses that frame for the first time.

What is not a cost here, with its numbers: the write watch and the shadow
copies (0.0 to 0.1 ms and 0.2 to 0.5 MB per frame, `copystat` lines),
DirectStorage and file I/O (32 threads idle in all 420 dumps), pipeline
creation (0.0 ms/frame of pipeline waits, 0 being made, the 8 pipeline
threads idle), guard faults (0 to 4 per frame), title CPU (under 0.1 % of
samples).

### 4.3 "On the GPU" in a loading frame

The loading screen's two submissions take 0.01 to 3.1 s "on the GPU" (A)
and 0.16 to 3.97 s (T2), against 2 to 3 ms in the front end, while the
process's 3D engine stays under the sampler's 1 % from the VidMm jump on
(A: last sample above 1 % at 16:09:43; T2 at 15:47:48). The queue is
therefore waiting, not running. The likely wait is VidMm's paging queue:
each submission waits for the residency operations queued before it (the
textures and buffers made resident in the meantime). The present thread
pays it as "waiting for them" through `present`'s `drain`. It goes away
with the per-resource admissions, not on its own.

## 5. The world frame

### 5.1 What the frame lines show

| Session | Line | ms/frame | submissions/frame | draws/frame | dropped |
|---|---|---:|---:|---:|---:|
| T2 | 15:50:20 | 2 498 | 2.0 | 1 | 0 |
| T2 | 15:50:26 | 3 990 | 2.0 | 7 | 37 |
| T2 | 15:50:31 | 5 447 | 2.0 | 2 | 0 |
| A | 16:13:17 | 4 727 | 2.0 | 11 | 9 |
| A | 16:13:23 | 3 129 | 2.0 | 1 | 0 |

These are the last frames the title presents. T2 has no frame line after
15:50:31 (43 s, the title ended by the autopilot at 15:51:14), A none after
16:13:23 (53 s). The frame line is written on a present, so no frame was
presented at all. The title's watchdog would have ended T2 a second time
at 15:51:03 (`XTP_WATCHDOG_HOOK` kept it running).

They have the loading screen's structure: 2 submissions of about 2.5 lists,
the same 11-12 draws, 0 ms of pipeline waits and 0 being made. The drops
(0 to 37 per frame) are pipelines the host refused: compute
`13dfdd3b1136e407` and mesh `d4111a6e6ea49d91` with pixel
`c0a7fe77b1862e10` in A (436 mesh pipelines refused so far for 64-lane
waves; wave64-wiring, reverted from `main` at `4394656`, wires them). So **the 1 to 7 draws are the fade
out of the loading screen, not a world frame with draws missing**. The first
world frame, with its thousands of draws, is never submitted while the
session lasts.

### 5.2 Per thread

Session A, 104 dumps from 16:13:24 to 16:14:15, after the last present:

| Thread | Share | Where |
|---|---:|---|
| **11 title recording threads** (title workers `0x2215ea0`, priority 6) | 62.4 % idle | job waits (`0x56a0d`, `0x22511f`, `0x41ec1a9`, `0x41eb52b`) |
| active 430 samples, about 4 threads busy at any time: | 46 % of active | XTP locks: `shadow::find` (shadow.rs:484, the global `BUFFERS` mutex) 87, `descriptor::host::HostHeap::mirror` (descriptor/host.rs:418, the heap's `made` mutex) 71, `shadow::existing` 14, other 28 |
| | 33 % | VidMm: `Host::shared_buffer` ← `HostList::budget` (host.rs:2000, a 256-byte budget word per list, from `indirect::bound` ← `execute_indirect`) 59; ← `shadow::find` (shadow.rs:493/500, from `descriptor::host::buffer_srv`/`buffer_uav` ← `make` ← `State::bind`, and from `shadow::copy`) 84 |
| | 7 % | VidMm in the driver: inside `Dispatch` (draw.rs:655) 22, `indirect::bound` (indirect.rs:175) 7, `ClearDepthStencilView` 1 |
| | 7 % | VidMm: `Host::upload_buffer` (host.rs:2428) ← `texture::upload` (texture.rs:121, `draw::readable`) 20, ← `HostList::scratch` (host.rs:1978) 10 |
| | 6 % | VidMm: `make_resident` ← `texture_at` (resource.rs:1715, textures placed in title memory) |
| **title-main** | 96.2 % | title wait `0x56a0d` ← `0x23cf735`: waits for the frame being rendered |
| **Presenting thread** | 86.5 % | title wait `0x56a0d` ← `0x4e7f358`: waits for lists to submit |
| | 8.7 % | the driver waiting on the CPU inside a D3D12 call |
| **Loading thread** | 71.8 % idle | 13.7 % `shared_buffer` ← `shadow::copy` (title `0x4f4600`), 1.7 % each `make_resident` (`texture_at`), `upload_buffer` |
| 13 workers priority 8, 8 priority 5, dstorage, pipelines, finisher | idle | |

The two locks turn one VidMm wait into a wait for every recording thread:

- `shadow::find` holds `BUFFERS` (every title buffer's entry) while it
  calls `Host::shared_buffer` for a buffer's first host copy, about 0.4 s.
  Every other `find`, `existing` and `register` (resource creation at an
  address) waits for it.
- `HostHeap::mirror` holds the heap's `made` mutex while it makes a view;
  a buffer view goes through `buffer_srv`/`buffer_uav` → `shadow::find`,
  so a view of a new buffer holds `made` through that wait too, and every
  descriptor copy (`descriptor::copy_simple`) and bind waits behind it.

Session B (timed, `--measure 150`) gives the world 2.5 minutes. The title
presents 7 frames in the 18 s after the world flags (16:28:48), the last
at 16:29:06, and **none in the 132 s that follow**, until the session ends
at 16:31:18. In those 132 s the recording threads log 314 creations of
20 ms or more, 304 s of thread time over 7 threads (2.3 waiting at any
time on average):

| Creation | Count | Sum | Median | p90 |
|---|---:|---:|---:|---:|
| `shared_buffer` (host copies on first use, budget words) | 169 | 170.4 s | 836 ms | 2 049 ms |
| `make_resident` (`texture_at`) | 80 | 66.2 s | 695 ms | 1 387 ms |
| `upload_buffer` (texture uploads, scratch) | 49 | 54.6 s | 905 ms | 2 309 ms |
| host texture `CreateCommittedResource` | 16 | 12.4 s | 818 ms | 1 202 ms |

VidMm admits 304 allocations in the same 131 s (9 029 → 9 333 resident,
10.01 → 10.07 GiB): 0.43 s each, the same order as in the load. Each call
waits about twice as long as in the load (median 0.7–0.9 s against 0.3 s)
because the threads queue for the same admissions. 118 new title buffers
got a host copy after the world flags (78.7 MB, median 4 KiB, each rounded
to a 64 KiB committed resource). The stacks of B (131 dumps, 1 s)
split the recording threads' active samples like A's: XTP locks 52 %
(`shadow::find` alone 32 %), `shared_buffer` 24 %, `make_resident` 10 %,
the driver 7 %, `upload_buffer` 7 %.

So the world frame is recorded at the pace of VidMm admissions, serialised
by those locks, and title-main and the presenting thread wait for it. The
title is not slow and no draw is skipped: its threads are blocked inside
XTP.

## 6. The same load in Series S mode

Session C runs B's binary with `xtp run --console series-s`, passed by a
second experimental patch to `xtask play` (`XTP_PLAY_XTP_ARGS`, not
delivered: `E:\xtp-data\runs\frame-prof-C-xtask-args.patch`). The title
then takes the Series S budgets. Log
`E:\xtp-data\cache\play\20261124-163332\run.log`, counters
`E:\xtp-data\runs\stall-frame-prof-C-seriesS`, crash record
`crashes.log` in the run's data folder.

| | Series X (T2, A, B) | Series S (C) |
|---|---|---|
| Start of the load → world flags | 160 s, 212 s, 158 s | **about 9.6 s** (16:34:01 → 16:34:10.6, +41 s after the start) |
| VidMm local resident, highest | 9.7, 10.0, 10.1 GiB | **8.19 GiB** (16:34:12), 7.48 GiB free: under the ~9.5 GiB threshold |
| Host textures alive, highest | 3 542 textures, 8 841 MiB (T2) | 3 659 textures, **6 945 MiB** |
| Creations of 20 ms or more | 417 in the load and 314 in the world (B) | **7 in the whole session**, at most 23.6 ms |
| After the world flags | 3 to 7 transition frames (1 to 11 draws), then no frame for 43 to 132 s | **a world frame**: 2.6 fps, 384 ms/frame, 39 submissions of 5.6 lists, 79 draws, 1 196 dropped without a host pipeline (205 being made), 3 515 indirect executions dropped |

The world frame line (16:34:14) splits the 384 ms into preparing 126 ms
(copies 71.6 ms, 17.9 MB), on the GPU 188 ms, copying back 152 ms, waiting
281 ms and outside submissions 0. Pipeline waits are 0.4 ms graphics and
4.9 ms compute. That is the shape run-world measured at `2d0257e` in the
first seconds of a world, with pipelines still being made.

**The title crashes about 7 s into the world** (16:34:17), with an access
violation in a title worker thread at `0x29268aa`, reading
`0xffffffffffffffff` (stack `0x2215ea0` → `0x221c29c` → `0x4eaa05a` →
`0x63e78` → `0xf6afc3` → `0xf6b07b`). Its crash reporter then tries to
upload a 35 MB minidump, which XTP's network policy blocks. Just before,
the title had loaded `amd_fidelityfx_upscaler_xbox.dll`, which it uses only
on a Series S (`dark-world.md` §3). The FSR 4 provider logged
"DeserializeComputePipelineStateX FAILED hr=0x80004001 … falling back to
NewBE" 37 times, while XTP logged "method not implemented object=pipeline
state slot=0x58". That the crash comes from this path is not established:
it is in the title's code, not in the FidelityFX DLL.

Reading: **with the same XTP code, staying under the threshold is enough
for the world frame to be submitted at once.** On the PC, the 1 to 7 draws
come from the VidMm ceiling combined with XTP's one admission per resource,
not from the title or from skipped draws. Series S is a candidate stopgap
for the stall until tex-pool lands, if the crash above is fixed first.

## 7. Since run-world

run-world (`2d0257e`) ranked the world frame's 750–900 ms as shadow
refreshes 40–45 %, GPU 25–35 %, copy-back 11–14 %. On Series X none of it
can be measured today, since no world frame is submitted. Series S's only
world frame line has the same shape. The shadow refresh cost (W1, task
w1-shadow 74bc23b5) comes back once recording is unblocked; today the
loading screen copies 0.2 to 0.7 MB per frame in 0.1 ms. What changed in
between is the world load itself (`stall.md`): past ~9.5 GiB resident,
every allocation waits 0.3 to 0.5 s. dark-world's not-resident creation,
landed on `main`, moved that wait from `CreateCommittedResource` to
`make_resident` without shortening it.

## 8. Top costs, fixes and owners

All costs below are waits for one VidMm admission per resource that XTP
creates on a title thread, or locks held through such a wait. Each one
disappears if XTP stops creating resources one at a time on title threads
(pooling, done ahead of need on its own thread), or if the process stays
under the threshold (section 6).

### 8.1 Loading frame (A: 212 s of load; B: 158 s, timed)

| # | Cost | Measured | Fix | Owner |
|---|---|---|---|---|
| 1 | `make_resident` of each host texture, on the title's loading thread | A: 69.5 % of that thread, ≈ 146 s, 316 textures 0.45 s apart. B: 326 waits, 121 s, median 304 ms, up to 4.8 s | Host textures placed in large heaps reserved ahead of need on a thread of XTP's: one admission per heap, no residency wait per texture on a title thread | **tex-pool 450ec9ba** |
| 2 | `Host::shared_buffer`: one committed system-memory resource (at least 64 KiB) per title buffer's host copy, made on first use under `BUFFERS` | A: 25.5 %, ≈ 54 s. B: 86 waits, 32.6 s, median 348 ms; 441 new host copies in the load (median 4 KiB); 4 to 5 per frame, 1 to 4 s, even in frames without textures | Suballocate host copies of title buffers from a few large pre-reserved heaps (one admission per heap); make the copy outside `BUFFERS` | host.rs, shadow.rs: **w1-shadow 74bc23b5** (owns both), with tex-pool's allocator; or a follow-up task after w1-shadow |
| 3 | The present probe's readback buffer, made on every present (diagnostics builds, which every play session uses) | A: 9 % of the presenting thread, ≈ 19 s, 0.7 to 1.5 s per frame. B: 16 waits, 10.6 s | One readback buffer per presenting thread | **done** on `main` after session A: `9c8522f` |
| 4 | The queue waiting for paging: "on the GPU" 0.3 to 4 s per frame for 5 lists, drained by `present` | 0.7 to 3.1 s per frame in A's frame lines, with the 3D engine under 1 % | Goes with 1 and 2 (no new resident allocation per frame); `present`'s `drain` can end once titles wait on their own fences | tex-pool; host.rs (w1-shadow) for the drain |
| 5 | Releases (`DestroyAllocation2`) and the driver's own CPU waits in `ExecuteCommandLists` | A: ≈ 4 s and ≈ 2.5 s | Fewer resources (1, 2), and staying under the threshold | **b70e1cc4** (low VRAM: host textures made on first GPU read, optional top-mip drop). With 6.9 GiB of host textures against 8.3 to 8.8 GiB, Series S already keeps the process under the threshold (section 6) |

### 8.2 World frame (A: none submitted in 53 s; B: none in 132 s)

| # | Cost | Share of the recording threads' busy samples (A, B) | Timed (B, 132 s) | Fix | Owner |
|---|---|---:|---|---|---|
| 1 | XTP locks held through a VidMm wait: `BUFFERS` in `shadow::find`, `made` in `HostHeap::mirror` | 46 %, 52 % | (the time behind 2) | Never hold a lock across a D3D12 creation: create the host copy outside the lock (insert a placeholder, unlock, create, relock), and make buffer views outside `made` | shadow.rs: **w1-shadow 74bc23b5**; descriptor/host.rs: **nvidia-fix 1dba664f** (owns descriptor/) or a follow-up |
| 2 | `Host::shared_buffer` during recording: host copies on first use (118 new ones), and a 256-byte budget buffer per list (`HostList::budget`) | 33 %, 24 % | 169 waits, 170 s, median 836 ms | As 8.1 #2; budget words from one shared, pre-made buffer | host.rs: **w1-shadow** or a follow-up |
| 3 | `make_resident` of textures placed in title memory (`texture_at`), and host texture creation | 6 %, 10 % | 80 waits, 66 s; 16 creations, 12 s | As 8.1 #1 | **tex-pool 450ec9ba** |
| 4 | `Host::upload_buffer` during recording: one committed upload buffer per texture upload and per large scratch | 7 %, 7 % | 49 waits, 55 s | An upload ring made at device creation (run-world's W3) | host.rs, texture.rs: **w1-shadow** or a follow-up (W3) |
| 5 | The driver's own CPU waits inside `Dispatch`, `ExecuteIndirect`, `ClearDepthStencilView` | 7 %, 7 % | not timed | Not in XTP's hands, except by keeping the process under ~9.5 GiB resident | **b70e1cc4** |

Besides: Series S crashes in the world (section 6). Implementing the
pipeline state's slot `0x58` (FSR 4's `DeserializeComputePipelineStateX`),
or making the FSR 4 provider fail cleanly, has no owner today (new task,
S). It would make Series S usable as a stopgap.

### 8.3 What the other tasks change

shader-masks (ef9bc5ad), shader-images (24cfee9a), nvidia-fix (1dba664f,
apart from the `made` lock above), #334 (1661c7ac) and wave64-wiring
(f1b74488, reverted from `main` at `4394656`) change what the world frame
draws, not how long it takes today. Their effect shows once the world frame
is submitted, as in Series S, where 1 196 draws are dropped without a host
pipeline and 3 515 indirect executions are dropped per frame, against 79
drawn. Then W1 (shadow refreshes), the copy-back and the GPU time come back
as run-world measured them.

## 9. Method

- Stacks: XTP's own sampler, `XTP_STACKS=<seconds>,…`
  (`crates/xtp-loader/src/stacks.rs`): every listed second, each thread is
  suspended alone and unwound in process, and its frames are printed as
  `module+offset` into the run log. Up to 40 frames; the title image shows
  as `xtp-image-<hash>.dll`. A dump costs a few ms.
- Symbols: `xtp.exe` offsets with `cdb -z <target>\diag\xtp.exe` against
  the build's PDB (`ln` with `.lines -e`, at the return address minus one),
  system DLLs against Microsoft's public symbols
  (`srv*E:\tools\symbols*https://msdl.microsoft.com/download/symbols`).
  NVIDIA's DLLs have no public symbols and are left as offsets.
- Each sample gets a category from its frames: a VidMm wait
  (`NtGdiDdDDIWaitForSynchronizationObjectFromCpu`, or the fence wait in
  `residency::enqueue`) named after the first XTP frame above it; a
  contended XTP mutex (`Mutex::lock_contended`) named after the XTP
  function that takes it; idle (a wait with no XTP frame, or a condition
  variable); a sleep; running code.
- Frame lines, `copystat` lines and resource debug lines come from the same
  run logs. VidMm counters come from `kmt_adapter.py` (every 1 s) and engine
  use from `gpu_sampler.ps1` (every 2 to 3 s, values under 1 % not
  written), both started by `E:\tools\stall-probe\session.ps1`.
- Experimental patches, in this task's worktree only and not delivered:
  `E:\xtp-data\runs\frame-prof-B-timing.patch` (a log line per creation of
  20 ms or more, B and C) and `frame-prof-C-xtask-args.patch`
  (`XTP_PLAY_XTP_ARGS`, extra `xtp run` arguments for `xtask play`, C).
- The scripts that parse, symbolise, categorise and time the dumps and
  logs, the session launchers and the symbol tables of A and B are in
  `E:\xtp-data\runs\frame-prof-tools\` (`stacks.py parse|symbols|report`,
  `cats.py`, `timeline.py`, `vidmmlog.py`, `frames.py`, `vidmm.py`).
- The schedule was one dump per 0.5 s in A and one per 1 s in B and C,
  from +5 to +340 s.
