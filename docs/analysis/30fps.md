# The Series S world at 30 fps: where the CPU preparation goes, and the path to 33 ms

Task 8e0633e9. The Series S world (BTP 6, `xtp run --console series-s`)
runs at 3–4 fps on the person's PC (RTX 4080 SUPER 16 GB, Ryzen 9 7950X3D,
64 GB, Windows 11 26300). AnyKyty, the person's own PS5 layer in
`E:\xtp-refs\AnyPS5`, runs its titles at 30 fps. This document measures
where XTP's CPU preparation of a world frame goes, compares that pipeline
with AnyKyty's, and gives the ordered plan to 33 ms a frame, with numbers
and an owner for each step.

Times are UTC. MB are 10^6 bytes, as the frame line prints them. The
sessions' logs are under `E:\xtp-data\` (paths in §2); none is in the
repository. XTP code is cited at `main` `eeb1f03`. Title code is named by
RVA only.

## 1. Summary

- **A world frame is one thread's work.** The title's submitting thread
  (a priority-15 worker) spends **88 %** of its time inside `Host::run_as`.
  Inside it, that thread does two things: it prepares submissions itself,
  and it waits for earlier ones. The title's 11 recording threads are idle
  94 % of the time, and the title's own code runs 1–2 % of the samples. So
  every millisecond taken out of `run_as` comes off the frame.
- **Session B** (§2): 304.5 ms a frame, 3.3 fps, 2 692 draws a frame. The
  frame line splits it into preparing 150.9 ms, waiting 153.3 ms, GPU
  125.8 ms and copy-back 81.1 ms, with 54.5 ms overlapped.
- **The largest cost is a copy the GPU never needed.** Before each
  submission, `Shadow::writing` takes a snapshot of every range its shaders
  may write: **76.5 ms a frame**, 3 665 snapshots of about 0.6 GB. Those
  snapshots also cause most of the process's page faults (74 000 of a
  median 118 000 a frame), because their memory comes back fresh. After
  the GPU has run, the copy-back compares those bytes (median 637 MB) with
  the host copy (75 ms) and finds 7 MB (median) the GPU actually changed.
  **99 % of the snapshot and of the compare is for bytes nobody wrote.**
- **The waits are the copy-back's too.** The submitting thread waits
  117 ms a frame inside `run_as`, and title threads wait 153 ms a frame in
  all. Most of it is steps of a submission that wait for an earlier one,
  both its GPU run **and its copy-back**:
  - fills of title memory into the list: 77 ms in 3.3 waits;
  - snapshots: 41 ms in 10.8 waits;
  - page-mapping copies, which drain everything: 28 ms;
  - of the time on the submitting thread, 33 ms is spent waiting for the
    completion thread's copy-back (the `finishing` lock) and 30 ms doing
    copy-back itself.
- **"copies 82–145 ms for 20–24 MB" was never a copy cost.** The frame
  line's "copies" counts each step's whole time, so it includes its waits
  for the GPU and counts the refresh flushes it triggers twice (§3). It can
  exceed "preparing". The copying itself takes 4.5 ms (host copies,
  median 35 MB) plus 8.8 ms (8 755 title fills).
- **Top 10 on the submitting thread**, ms a frame (§4.1):
  1. snapshots, 76.5;
  2. waiting for the GPU, 53.5;
  3. waiting for the completion thread's copy-back, 33.4;
  4. copy-back done on this thread, 29.6;
  5. guarding view memory, 13.1;
  6. title fills through the kernel, 8.8;
  7. GetWriteWatch, 6.9;
  8. the write journal, 6.7;
  9. host copy refreshes, 4.5;
  10. step bookkeeping, 4.3 + 4.0.

  Off the critical path:
  - title threads waiting on the guards' lock, 60.5 ms summed;
  - descriptor mirroring on the recording threads, 22.7 ms summed;
  - copy-back on the completion thread, 45.3 ms.
- **AnyKyty does none of the per-frame work that XTP's top 4 are** (§6).
  CPU-written buffers are tracked by page protection, and only the dirty
  pages of a bound range go up. Pages the GPU wrote become no-access, so a
  CPU touch triggers the readback. Images go back at GL2 write-backs and
  flips. Faulted reads are replayed on a second view. XTP's eager, snapshot
  and compare model costs about 230 ms of thread time a frame (140 on the
  submitting thread) where AnyKyty's costs faults only.
- **The plan** (§8):
  1. Drop the snapshots, and the compare after them. Copy back on first
     touch or from a GPU-written map. Start by reusing snapshot memory
     (−40 ms, quick).
  2. Wait only for the GPU, and only where it matters: turn fills of
     GPU-written memory into GPU copies, and stop page-mapping copies from
     draining everything.
  3. Check less title memory: a median 11 GB a frame for 35 MB copied;
     read G16 data in place; stop holding the guards' lock around
     protection changes.
  4. Fewer, cheaper steps.
  5. Then turn on one frame in flight.
  6. Then the GPU: 80–100 ms of work today (126 on the frame line),
     against a budget of 25 ms. The CPU steps alone bring the frame to the
     GPU's time, 8–12 fps; the GPU step is what reaches 30 fps.

  Seven of the steps have no task yet (N1–N7); §9 writes each one up,
  ready for the board.

## 2. Sessions

| Session | When | Log | Binary and settings | World | Result |
|---|---|---|---|---|---|
| **A** | 20:57:52–20:59:11 | `E:\xtp-data\cache\play\20261124-205756\run.log`, counters `E:\xtp-data\runs\stall-fps30-A` | diag of `main` `eeb1f03` + the local timing patch (§10, not delivered), sha256 `af10cc4f…`; `XTP_PREP_SAMPLE=1` (the 1 kHz sampler), `XTP_STACKS` every 0.5 s from +50 s | from +26 s | **the title stopped** 22 s into the world: an `int3` in a title worker at title+0x22106e0 (thread start 0x2215ea0), exit 0x80000003. Three world frame lines and one 20 s sampler window, 8 327 samples in `run_as` |
| **B** | 21:00:41–21:03:31 | `E:\xtp-data\cache\play\20261124-210044\run.log`, counters `E:\xtp-data\runs\stall-fps30-B` | the same binary, **without the sampler**; `XTP_STACKS` every 0.5 s from +40 to +150 s (201 dumps in the world) | from +20 s, 120 s | exit 0, no device removal; **21 world frame lines** (≥ 100 draws a frame) |

- Both: Series S, `XTP_SKIP_INTRO=1`, `XTP_WATCHDOG_HOOK=1`,
  `XTP_FRAME_LINE=5`. Run with the autopilot from golden data through
  `E:\tools\stall-probe\session.ps1` under `measure.lock`, with a 180 s hard
  stop, announced on the task.
- **A's stop.** That address is in no other session in
  `E:\xtp-data\cache\play`. In A, title threads waited 172 ms a frame for
  the guards' lock, against 60 ms in B. The sampler suspends the submitting
  thread for tens of microseconds at a time (not measured), sometimes while
  that thread holds the lock, so it lengthens the waits of every faulting
  title thread. A timing assertion in the title is the likely reading, but
  nothing establishes it. B, without the sampler, ran its full 120 s. A's
  numbers are used only for the sampler's shares (§4.2), which agree with
  B's timers.
- **Baseline.** 18 earlier Series S sessions, 243 world frame lines, run
  by other tasks between 17:14 and 19:48 on older `main`. Medians: 266 ms
  a frame, preparing 126, "copies" 87, textures 25.5 (before gpu-prof's
  fix), GPU 107, copy-back 76, waiting 128. The same session counters
  (copystat) per frame:

  | Counter | Median |
  |---|---|
  | title copies | 3 658, 2.8 MB |
  | refreshes | 4 704, 2.8 GB declared |
  | updates | 1 472, over 6.9 GB checked |
  | copied | 19.9 MB, in 1.8 ms |
  | write watch | 1 599 askings, 9.5 ms |
  | guards | 6.2 ms |
  | page faults | 94 000 |
  | copy-back | 630 MB compared, 5.4 MB read |

  Across those lines, preparing follows the other measured times
  (correlation with the watch time 0.71, the guard time 0.63, page faults
  0.65). It does not follow the volumes: GB checked 0.12, refreshes 0.04,
  copies −0.04. The same work goes slower or faster with the PC's load.
  Script: `E:\xtp-data\runs\fps30\existing.py`, `corr.py`, `agg.py`.
- **Timer overhead.** B's timers enter and leave about 95 000 phases a
  frame, and seven phase kinds also read the process's page-fault count.
  That is an estimated 5–10 ms a frame on the submitting thread. B's frame
  (305 ms, preparing 151) lies within the spread of the unpatched sessions
  of the same hour: 20261124-204819 ran at 382 ms, preparing 181, with
  2 040 draws a frame.
- **What changed on `main` since the baseline.** gpu-prof's texture fix
  is in (textures 0.7 ms a frame instead of 23–29), and so are frames in
  flight (off by default), the indirect budget (VIctory #1510) and
  mesh64's 3D (2 692 draws a frame instead of about 400).

## 3. What the frame line's "preparing" and "copies" count

`Meter::frame` in `host.rs` prints "N preparing: copies X ms Y MB, textures
…". The pieces mean the following:

- **Preparing** is `Host::run_as`, from after the `submitted` lock to
  before `ExecuteCommandLists`. The time this thread spends in
  `Host::waiting` is taken off (`host.rs:1188-1245`). `ExecuteCommandLists`
  itself (2.2 ms a frame in B), the timeline push and the lock wait are
  outside it.
- **Copies** is `PREPARING[0]`, which two places add to:
  - each `Copy` or `Fill` step's **whole** time (`host.rs:2436`), its
    `Batch::settle` waits for earlier submissions included;
  - each `Batch::flush` (`host.rs:2553-2560`). A `Copy` or `Run` step
    calls `title_written()`, which flushes the pending refreshes, so that
    flush is counted **twice**.

  That is why "copies" can be larger than "preparing" (145.7 against
  124.5 ms at 18:13:40, run 181140), and why it moved with the waiting.
  The MB are bytes copied, and they are real: title copies plus refresh
  copies.
- **Textures** is `Staging::refresh`.
- **Copy-back** is `Host::complete`, on whichever thread finishes a
  submission: the completion thread, or a title thread inside a wait.

So "20 MB of copies taking 80–145 ms" (200 MB/s) was never a copy rate.
The host copies run at about 8 GB/s (§4.1, row 9), and the 8 755 small
fills at under 1 GB/s through the kernel (row 6); the rest is waits, the
double count and the checks around the copies. The frame line deserves a
fix. A small one: count steps without their waits and flushes once, and
print the snapshot time apart (§9, N7).

## 4. The preparation, measured

### 4.1 By phase (timers, session B)

These are means per frame over B's 21 world lines. "Exclusive" means
without nested phases. The page faults column is the process's count
taken over the phase, so it includes other threads' faults while the phase
ran. The submitting thread is the only one inside `run_as`.

| # | Phase (submitting thread, in `run_as`) | ms/frame, exclusive | Calls/frame | What it is |
|---:|---|---:|---:|---|
| 1 | **Snapshots of GPU-written ranges** (`Shadow::writing`, `shadow.rs:316`; `Before::cover`, `:142`) | **76.5** | 3 665 | The host copy's bytes of every range the submission's shaders may write, copied into pooled buffers before the submission. 73 700 page faults over these calls, about 0.3 GB of fresh pages a frame. The pool keeps at most 16 buffers and frees the rest (`give_back`, `:224`; `NtFreeVirtualMemory` under it in A's samples), so large snapshots land in new memory |
| 2 | Waiting for the GPU (`Host::wait_gpu`, under `Batch::settle*`) | 53.5 | — | Steps waiting for an earlier submission that writes or reads the same title memory (§5) |
| 3 | Waiting for the completion thread (`Timeline::finish`, `host/timeline.rs:254-255`, the `finishing` lock) | 33.4 | 46 | A settle's `finish_reached` waits while the completion thread copies back the submission it is finishing |
| 4 | Copy-back on this thread (`Host::complete` → `Shadow::copy_back`, `shadow.rs:378`) | 29.6 | 7.4 | The submissions this thread finishes itself in a wait: comparing snapshots with host copies (`memcmp`) |
| 5 | Guarding view memory (`watch::guard`, `watch.rs:277`) | 13.1 | 1 236 | A walk of every 64 KiB block of the asked range, with one to four set lookups each (about 200 000 blocks a frame), then `VirtualProtect`: 620–3 800 protection changes a frame (median 2 300) |
| 6 | Title fills and copies (`copy_mapped` → `ReadProcessMemory`) | 8.8 | 8 755 | A median 5.4 MB, about 1 µs a call, almost all of it kernel entry |
| 7 | `GetWriteWatch` (with reset) | 6.9 | 2 698 | A median 11 GB asked a frame (1.4–15.5), a few hundred 64 KiB pages found written |
| 8 | Write journal (`watch::writes`, `watch.rs:114`) | 6.7 | 2 471 | The `BTreeMap` of pages' last writes: inserting what the kernel found, then collecting the asked range |
| 9 | Host copy refreshes (`Shadow::copy`) | 4.5 | 490 | A median 35 MB a frame (15–48) copied at about 8 GB/s: the work that is actually needed |
| 10 | Step bookkeeping: the step loop 4.3, `GpuWrite` steps 4.0, `settle` and `conflict` 2.7, `update`, `copy_stale` and `flush` 3.2, `ExecuteCommandLists` 2.2, `Pending::of` and push 1.3 | 17.7 | about 22 000 steps | 8 745 fills, 9 260 refreshes and 3 665 GPU-write declarations a frame; includes most of the timers' own cost |
| — | Textures (`Staging::refresh`) | 0.7 | 277 | gpu-prof's fix: 23–29 ms before |

The other threads, outside `run_as`, ms a frame summed over threads:

| Phase | ms/frame | Calls/frame | Threads |
|---|---:|---:|---|
| Waiting for the guards' lock (`watch::guards`, `watch.rs:231`) | **60.5** | 1 852 | Title threads in the guard-fault handler (76–473 guard faults a frame) and `watch::writing` (DirectStorage, file reads), behind the submitting thread's `guard` walks and protection changes |
| Copy-back on the completion thread | 45.3 | 25 | `xtp-gpu-finisher`, plus 47.3 ms waiting for the `finishing` lock while the submitting thread holds it |
| Descriptor mirroring (`HostHeap::mirror`, `descriptor/host.rs:745`) | 22.7 (+1.7 in the `made` lock) | 23 026 | Recording threads |
| `shadow::find` (+ `BUFFERS` lock) | 14.1 (+10.0) | 77 159 | Recording threads. The lock is rarely contended now: 0.13 µs a call, mostly the timers' cost |
| Waits outside submissions (the present, flips) | 15.3 | 2.1 | The submitting thread at the present |

Copy-back in all: 29.6 + 45.3 = 74.9 ms a frame, against 81.1 on the frame
line.

### 4.2 By sample (session A, 1 kHz on the thread in `run_as`)

Of 8 327 samples (20 s of world), the share of each phase at sample time,
and the first XTP function under the leaf:

| Share | Where |
|---:|---|
| 33.8 % | `Shadow::writing`: `memcpy` (`memcpy_avx_ermsb_amd`), page faults included (they run in the kernel under the same user address) |
| 16.3 % | `Mutex::lock_contended` in `Timeline::finish`: waiting for the completion thread's copy-back |
| 12.6 % | `Shadow::copy_back`: `memcmp` 10.3 %, run by this thread |
| 12.7 % | `watch::writes` and `guard`: `NtGetWriteWatch` 3.9 %, `NtProtectVirtualMemory` 3.1 %, the journal 2.0 %, collecting the range 0.8 % |
| 10.5 % | `Host::wait_gpu`: `NtWaitForSingleObject` |
| 5.8 % | `Shadow::copy` (`xtp_gpu_try_copy_moving`) |
| 3.4 % | `copy_mapped` (`NtReadVirtualMemory`) |
| 1.7 % | `ExecuteCommandLists` (NVIDIA's driver, `D3D12Core` validation) |
| 1.6 % | `give_back`: `NtFreeVirtualMemory` of snapshot buffers |

The sampler and the timers agree on the order and on the shares.
Symbolised stacks: `E:\xtp-data\runs\fps30\A-report.txt`.

### 4.3 The threads (session B, 201 dumps)

| Thread group | Idle or sleeping | XTP | Title running |
|---|---:|---:|---:|
| **The submitting thread** (title worker, priority 15, `ExecuteCommandLists` from title+0x24821e3) | 1 % | **in `run_as` 88 %**: snapshots 20 %, GPU waits 13 %, `finishing` lock 11 %, copy-back 9 %, fills 7 %, write watch 6 %, copies 5 %; `frame::wait` 8.5 % (the 30 Hz flip tick) | — |
| 11 recording threads (priority 6) | 94.4 % | 2.6 % running (binds, views, `CopyTextureRegion`), 2.3 % locks (`defer_fill`, `descriptor::record`, `defer_refresh`) | 0.4 % |
| 13 workers, priority 8 | 97.4 % | 0.4 % (the guard handler's lock) | 2.0 % |
| 8 workers, priority 5 | 98.2 % | 1.6 % | 0.2 % |
| `xtp-gpu-finisher` | 43.8 % | GPU waits 26 %, copy-back 15 %, `finishing` lock 11 % | — |
| 16 `xtp-pipelines` | 36.8 % | translations and driver compiles, 61 % | — |

The title asks for little CPU of its own; it waits for its submitting
thread. In the menus the same title runs at 30 fps with 0.3 ms of
preparation a frame (A's 20:58:14 line). Tables:
`E:\xtp-data\runs\fps30\B-cats.txt`.

### 4.4 The items the brief named

- **Shadow refreshes.** 2 162 updates a frame cover a median 11 GB of ranges
  asked of the write watch (1.4–15.5 GB; about 4 MB per asking), of which a
  few hundred 64 KiB pages are found written and a median 35 MB copied. The
  cost per update is 15.5 µs: `GetWriteWatch` 2.6 µs, the journal 2.7 µs,
  the guard walk 10.6 µs per guarded asking, and a page walk (`copy_stale`,
  about 200 000 pages a frame) of under 1 µs. XTP has no separate
  `ResetWriteWatch` call: the reset is a flag of `GetWriteWatch`.
- **Guard faults and protection changes.** 76–473 guard faults (median 309)
  and 620–3 800 protection changes (median 2 300) a frame. A fault costs
  little; what costs is the lock. The submitting thread holds `GUARDS`
  through each walk and its `VirtualProtect` calls, and every faulting title
  thread waits for it: 60.5 ms a frame.
- **Descriptor mirroring.** 23 026 calls and 22.7 ms a frame, on the
  recording threads, which are idle 94 % of the time. It is not on the
  frame's critical path today.
- **Texture refresh.** 0.7 ms a frame since gpu-prof's fix.
- **Barriers and recording.** Recording runs on the title's recording
  threads, in parallel and mostly idle. `ExecuteCommandLists` takes
  2.2 ms a frame. On the GPU, "between commands" (copies, barriers,
  clears) was 10.1 ms a frame before the 3D (`gpu-frame.md` §4.1).
- **Lock contention.**
  - `finishing`: 33.4 ms on the submitting thread, 47.3 ms on the
    completion thread.
  - `GUARDS`: 60.5 ms on title threads.
  - `made`: 1.7 ms.
  - `BUFFERS`: 77 000 acquisitions, rarely contended.
  - `submitted`: 0.
- **The title's own threads.** Idle 94–98 %, running 0.4–2 % (§4.3).

## 5. Waiting, by cause (session B)

From the diagnostics line "waiting for submissions, by where it was asked",
means over B's 21 world lines, summed over title threads (153.3 ms a
frame):

| Where it waits (`main` `eeb1f03`) | ms/frame | Waits/frame | Why |
|---|---:|---:|---|
| a `Fill` step (`host.rs:2369`) | **77.1** | 3.3 | It copies title memory into the list's upload memory. An earlier submission's GPU work writes that memory, so the step waits for that submission to run **and be copied back**: 23 ms a wait |
| a `GpuWrite` step's snapshot (`host.rs:2408`) | **41.0** | 10.8 | The snapshot is read from the host copy, which an earlier submission's shaders may still be writing |
| the title's page-mapping copies (`queue.rs:405`, `copy_page_mappings`) | **28.3** | 1.6 | They drain every submission in flight |
| finishing at submission (`host.rs:1211`) | 3.8 | 20.9 | Copy-back of what the GPU finished |
| the present and flips (`host.rs:1367`) | 1.6 | 0.3 | The present's `drain`, with frames in flight off |
| a refresh (`host.rs:2382`) | 1.5 | 0.2 | |

This is the chain gpu-prof found (`gpu-frame.md` §6). A submission's steps
wait for the one before to have run on the GPU and been copied back,
because in the world nearly every submission touches what the one before
wrote. So preparing, GPU and copy-back run one after another, and the
frame is about their sum. One frame in flight cannot help while it lasts:
inflight measured 0.0–0.6 ms of the waiting at the present
(`frames-in-flight.md` §5).

## 6. AnyKyty's frame pipeline, against XTP

AnyKyty is `E:\xtp-refs\AnyPS5` at `7de61e3451`, read but never built or
run. Paths below are under `core/kyty/graphics/host_gpu/` unless said
otherwise. The read-only copy is a one-commit clone, so the authorship
below is `anyps5.md` §2 and §7's, not re-derived here.

- **Portable** means the person's own commit, ported with credit
  ("Ported from simnJS/AnyPS5 (<commit>)").
- **Ideas** means KytyPS5's or a contributor's code, GPL-2.0-only,
  reimplemented and never copied.

The page-protection caches (`regionManager.h`, `memoryTracker.h`,
`pageManager.cpp`, `renderer/cache/bufferCache.cpp`, `textureCache.cpp`)
come from KytyPS5, with the person's additions inside `ANYPS5_KYTY`
blocks where noted.

| AnyKyty mechanism | How it works there | XTP today | Gap, ms a frame (session B) | XTP owner |
|---|---|---|---|---|
| **Buffer and texture caches that upload only CPU-dirty ranges** (ideas) | CPU writes are found by **page protection**: 4 KiB pages, a fault handler, no hashing. Per draw, only the dirty 4 KiB runs **inside the bound range** go up, through a staging ring (`bufferCache.cpp:1544-1613`). Protection is re-armed at upload, and neighbouring pages share one protection call (`pageManager.cpp:210-333`). A clean texture is skipped without reading its memory (`textureCache.cpp:1977-1999`), and a dirty one is detiled on the GPU | The write watch and 64 KiB guards are asked **over every declared range at every submission**: a median 11 GB a frame for 35 MB copied. Textures have been guarded since gpu-prof (done) | about **30** on the submitting thread (guards 13.1 + `GetWriteWatch` 6.9 + journal 6.7 + walks and updates 3.2), plus **60.5** of title threads waiting for `GUARDS` | **G16 ce8bcd0a** (small CPU-written data in place, so no refresh at all) and new **N4** (§9) for the rest |
| **Readbacks only at write-back events and flips; faulted reads replayed on a second view** (portable for the image write-back at GL2 events `f55d92ea53` and the replay `e36fbe6707`, `6cb0cb2a4a`, `0832a8bc2e`, `7c5fc8900b`; the lazy buffer readback is taken as ideas, `anyps5.md` does not attribute it) | A written buffer binding marks its pages GPU-dirty and **no-access**. A CPU read faults and triggers a read-ahead readback of up to 256 KiB on the readback thread; a CPU write first reads back (`bufferCache.cpp:434-443`, `760-891`). Images go back at `ACQUIRE_MEM`/`RELEASE_MEM` with GL2 write-back and at flips, only the bytes that changed. A faulting read of bytes no GPU write is near (64-byte lines) is replayed on the unprotected view (`Faults.cpp:90-172`). **There is no snapshot and no compare of untouched memory** | Every declared shader-written range is snapshotted before each submission and compared after it (the three-way merge, `502e91e`, ported from AnyKyty's image path), eagerly, at every submission | **about 230 of thread time** (140 on the submitting thread): snapshots 76.5 + copy-back 74.9 + the `finishing` lock waits 33.4 (submitting thread) and 47.3 (completion thread), plus the snapshot settles (41 of waiting) | **lazyback 8c035076** (copy back on first touch; its census is ready) and new **N1** (find the GPU's bytes without snapshots) |
| **64 KiB write-fault blocks** (portable, `88eb2573eb`, `da2947c57d`) | A write fault in command-buffer memory opens its whole 64 KiB block: one fault per block instead of per page | **Has it.** Guard blocks are 64 KiB (`watch.rs`, `BLOCK = PAGE`), and the write watch is folded into 64 KiB | 0 on the faults (76–473 a frame); what costs is the lock around them (row 1) | — |
| **Flips completed at submission, at most one frame ahead** (portable, `7b85afc1aa`, `4328d9d796`) | A flip completes on its own thread once its command buffer is submitted and the previous flip's work is done | **Has it, off by default** (`XTP_FRAMES_IN_FLIGHT=1`, task inflight, landed). It measured no gain while the waits sit inside submissions | 0 today. Later, the overlap of GPU and CPU: the frame becomes the larger of the two, not their sum | **inflight f1d37c5d** (done); turning it on is step 6 of §8 |
| **Idle submit** (portable, `a63804469a`) | When the GPU runs out of work, the open command buffer is submitted after the next draw or dispatch | Not applicable as such: the title decides its submissions (`ExecuteCommandLists`), and the GPU idles because of the chain (§5), not because of batching | — | — |
| **Deferred labels** (portable, `33fe9d39eb`, `842fd4f3d4`) | A label next to GPU-written bytes, but not on them, is deferred instead of draining the GPU; a deferred label waits for the newest pending occlusion dump only | Conflicts are tested per range (`Timeline::conflict`, `host/timeline.rs:151`) on whole declared ranges, labels included. A fill near GPU-written bytes waits for that GPU work **and** its copy-back | part of the **77.1** (fills) + **28.3** (page-mapping drains) of waiting | new **N2** and **N3** (§9) |
| Three-way merge on copy-back (portable, `65a1b01db4`) | Write back only the bytes the GPU changed and the title did not | **Has it** (`502e91e`) | — | copyback 78d58a42 (done) |
| Command memory in place (portable, `5d64eeb574`, off there) | Guest command memory imported, no upload | G15b done, G16 open | — (AnyKyty measured it 8 % slower and left it off: G16 should measure the same trade) | G16 ce8bcd0a |

How the other tasks the brief named fit:

| Task | State | What it gives, measured or expected |
|---|---|---|
| inflight f1d37c5d | done, off by default | 0 today; the overlap once the waits are gone |
| lazyback 8c035076 | doing (census committed, waiting for its session) | copy-back 75 → a few ms if most GPU writes are never read (xcl #1640 measured 96 % elsewhere). The snapshots stay unless its design drops them too (N1) |
| gpu-prof 522e7cb5 | done | textures 23–29 → 0.7 ms of CPU, 12.4 → 1.8 ms of GPU |
| G16 ce8bcd0a | todo (waits for G15b's follow-ups and #334) | refreshes of small CPU-written data (constants, descriptors, indirect arguments) gone: part of the 30 ms of checks, and fewer steps |
| indirect 5b249871 | done (VIctory #1507, #1527, #1510) | the indirect budget no longer wraps; the clamp cost was 8.7 ms of GPU a frame before it |

## 7. The budget at 33 ms

| Stage | Today (session B, a frame) | Target | Note |
|---|---:|---:|---|
| CPU preparation (the submitting thread in `run_as`, waits excluded) | about 145 (frame line 150.9) | **≤ 15** | the copies that must stay: about 5 ms for 35–48 MB; `ExecuteCommandLists` 2 |
| Copy-back | 81.1 | **≤ 5** | |
| GPU | 125.8 (frame line; 76–91 ms of timestamped work before the 3D, `gpu-frame.md`) | **≤ 25** | |
| Waiting on the critical path | 117 on the submitting thread (153 summed) | **about 0** | waits only where the title's data truly depends on the GPU |
| Overlap | 54.5 | **all** | one frame in flight: the CPU prepares N+1 while the GPU runs N and the completion thread copies N back |

At the target, the frame is max(15 + 2, 25, 5) plus the flip, about
25–33 ms: GPU-bound, as AnyKyty's frames are.

## 8. The plan, in order

The gains below are each step measured or estimated alone, on session B.
Removing one cost can reveal the next, so the sums are an order of
magnitude, not a promise. The frame column is a rough estimate of the
frame after each step. It cannot go below the GPU's work, about 80–100 ms
a frame with the 3D on (the frame line's 126 ms counts the completion
thread's wake-ups too, `gpu-frame.md` §4). **So the CPU steps 1–5 bring
the frame to the GPU's time, 8–12 fps; step 7, the GPU, is what reaches
30 fps.**

| Step | What | Gain | Frame after (estimate) | Owner |
|---:|---|---|---:|---|
| 1a | **Snapshot memory that does not fault**: snapshots in buffers kept for good (one arena per submission in flight, grown, never freed), not a pool of 16 that frees the rest | about **−40 ms** of preparation (estimate): 74 000 page faults, about half of the 76.5 ms. Small, local to `shadow.rs` | 265 | **lazyback 8c035076** holds `shadow.rs` (a commit in its task), or new **N1** right after it |
| 1b | **No snapshots and no full compare**: find the bytes the GPU wrote without copying 0.6 GB before and comparing it after (N1) | **−76.5** of preparation (1a's 40 included), **−75** of copy-back, **−33** of `finishing` waits, most of the **41** of snapshot settles | 140–160 | new **N1**, with **lazyback 8c035076** |
| 1c | **Copy-back on first touch** (lazyback): GPU-written pages copied back when the CPU touches them, or when the title waits for them | copy-back **75 → ≤ 5** where lazyback's census shows the bytes are never read | (in 1b) | **lazyback 8c035076** |
| 2 | **Fills of GPU-written title memory become GPU copies** (N2): when a fill's source lies in a range an earlier submission's GPU writes, copy from the host copy on the GPU, in queue order, instead of waiting for the GPU and the copy-back, then copying on the CPU | **−77** of waiting (3.3 waits of 23 ms a frame) | 110–130: the GPU's time now bounds it | new **N2** (`host.rs`, `shadow.rs`) |
| 3 | **Page-mapping copies wait only for what they remap** (N3; `gpu-frame.md` §8 #7) | **−28** of waiting | 100–125 (GPU-bound) | new **N3** (`queue.rs`, held by worldlight c19d640b now) |
| 4 | **Check less title memory** (N4 and G16): G16 reads constants, descriptors and indirect arguments in place. N4 asks the write watch and the guards only over what a step reads (AnyKyty's bound range), keeps the journal per host copy instead of one `BTreeMap` of pages, and changes protections outside the `GUARDS` lock | about **−25** of preparation (of 30), **−60** of title-thread waits | 100–125 (GPU-bound; CPU about 30) | **G16 ce8bcd0a**, new **N4** (`watch.rs`) |
| 5 | **Fewer, cheaper steps** (N5): fills read title memory with plain loads and a fault handler (VIctory #1010, 796 → 81 ns a fill) instead of `ReadProcessMemory`; steps merged per list and range (22 000 a frame) | about **−15** of preparation (fills 8.8 → 1, bookkeeping 17.7 → 5) | 100–125 (GPU-bound; CPU about 15) | new **N5** (`host.rs`, `shadow.rs`) |
| 6 | **One frame in flight on by default**: once 1–3 have taken the waits out of the submissions, the present's drain becomes the wait (`frames-in-flight.md` §5.3) | the overlap: the frame becomes max(CPU, GPU) instead of their sum | about the GPU's work alone: 80–100 | follow-up of **inflight f1d37c5d** (a switch and a session, no new code) |
| 7 | **The GPU to 25 ms** (N6): measure the 3D world's GPU frame again with `XTP_GPU_TIMESTAMPS=1`. Before the 3D it was 76–91 ms: compute 47 ms, of which `compute 3a79195f06d3ed85` alone was 16 ms; the clamps 8.7 ms; the texture copies 12.4 → 1.8 ms (done); between commands 10 ms | 126 → 25, by program | **33** | new **N6**; the translator tasks for the programs it names |
| 8 | **The frame line tells the truth** (N7): "copies" without waits and with each flush once; snapshots, guards and the write watch on their own; per-step counts | measurement only | — | new **N7** (`host.rs`, `copystat.rs`) |

Two things fall outside the plan and stay out of the critical path:

- descriptor mirroring (22.7 ms) and `shadow::find` (14 ms) on the
  recording threads, which are idle 94 % of the time;
- the 30 Hz flip tick (`frame::wait`, 8.5 % of the submitting thread's
  samples, about 26 ms a frame at 3 fps): the title waits for the next
  tick at every flip, and at 30 fps that tick is the frame's own pace.

Two risks:

- **Step 1b is a design task.** The three-way merge needs the bytes from
  before the GPU's work, wherever the title wrote the same pages
  meanwhile. Snapshotting everything is how XTP gets them today; N1 must
  get them only where needed.
- **Every step moves more of the frame onto the GPU.** With the waits
  gone, the device removals still under investigation (task 873bbb7a,
  `frames-in-flight.md` §5.2) will show sooner.

## 9. Steps with no task yet, ready for the board

**N1. Copy-back without snapshots: find the GPU's bytes without copying
0.6 GB a frame.**

- *Goal.* Today `Shadow::writing` (`shadow.rs:316`) snapshots every declared
  shader-written range before each submission: 76.5 ms a frame, 3 665
  snapshots, 74 000 page faults. `Shadow::copy_back` (`:378`) then compares
  a median 637 MB for a median 7 MB the GPU changed: 75 ms. Get the same
  result, every byte the GPU wrote and only it, through the three-way merge,
  at a cost in proportion to what the GPU writes.
- *Candidates, to measure.*
  1. Keep snapshot memory for good (no fresh pages): the quick half.
  2. A GPU-written map. Translated stores mark the 4 KiB or 64 KiB blocks
     they write in a per-host-copy byte map (plain stores of 1). The
     copy-back compares only marked blocks. The snapshot is needed only
     where the title wrote the same pages during the GPU's run. In view
     memory the guard handler sees that write first and can save the block
     before letting it through; write-watched memory has no such hook, so
     it would keep a snapshot of its marked blocks or be guarded too.
  3. AnyKyty's ownership. GPU-written pages become no-access, so the title
     cannot write them unseen and no snapshot is needed, at the price of a
     fault, and a wait, when the title writes GPU-owned pages. It fits
     lazyback's model.
- *Files.* `crates/xtp-gpu/src/shadow.rs`; `host.rs` (the `GpuWrite`
  step); for candidate 2, `crates/xtp-shader/src/emit/lower/buffer.rs` and
  the root signature (a map per UAV).
- *Acceptance.*
  - Snapshot plus compare at most 5 ms a frame in a Series S world
    session.
  - WARP tests: a title write during the GPU's run is kept; a GPU write
    goes back; overlapping declared ranges are merged once.
  - No new page faults per frame.

**N2. Fills of GPU-written title memory run on the GPU.**

- *Goal.* A `Fill` step (`host.rs:2369`) whose source an earlier
  submission's GPU work writes waits for that submission to run and be
  copied back: 77 ms a frame in 3.3 waits. Instead, record a GPU copy from
  the host copy into the list's upload memory or a scratch buffer, in
  queue order, and keep the CPU fill for memory the GPU does not own.
- *Files.* `crates/xtp-gpu/src/host.rs` (`Step::Fill`, `defer_fill`,
  `Batch::settle`); `shadow.rs` (finding the host copy of the source).
- *Acceptance.*
  - The "waiting by where it was asked" line shows the fill site under
    5 ms a frame in a Series S world session.
  - WARP test: a fill of bytes an earlier submission's shader wrote reads
    the shader's value.

**N3. Page-mapping copies wait only for what they remap.**

- *Goal.* `copy_page_mappings` (`queue.rs:389-405`) drains every
  submission in flight: 28 ms a frame in B, 14 ms in gpu-prof's A2. Wait
  only for submissions whose GPU work touches the remapped pages: a
  timeline conflict on the range.
- *Files.* `crates/xtp-gpu/src/queue.rs`, `host/timeline.rs`.
- *Acceptance.*
  - That site under 3 ms a frame.
  - WARP test: a remap waits for a submission writing the old pages and
    not for one elsewhere.

**N4. The write watch and the guards: ask less, and never hold `GUARDS`
across protection changes.**

- *Goal.* 2 698 askings a frame cover a median 11 GB for 35 MB copied: 30 ms
  on the submitting thread. While `guard` (`watch.rs:277`) walks blocks and
  changes protections under `GUARDS`, title threads in the fault handler
  wait 60 ms a frame summed.
- *What to change.*
  - Ask over the range a step reads (AnyKyty's bound range) rather than
    the whole declared view.
  - Keep the journal per host copy, as a bitmap of pages, not one
    `BTreeMap`.
  - Collect the blocks to protect under the lock, change protections
    outside it, and let the fault handler use a lock-free fast path for an
    open block (AnyKyty, `regionManager.h:297-337`: ideas).
- *Files.* `crates/xtp-gpu/src/watch.rs`; `shadow.rs` (`update`,
  `copy_stale`).
- *Acceptance.*
  - Write watch plus journal plus guards at most 5 ms a frame.
  - `GUARDS` waits on title threads at most 5 ms a frame.
  - The guard tests of `watch.rs` stay green, aliases included (#1122).

**N5. Fewer, cheaper steps.**

- *Goal.* 22 000 steps a frame (8 745 fills, 9 260 refreshes, 3 665 GPU
  writes) cost 17.7 ms of bookkeeping, and the 8 755 fills 8.8 ms through
  `ReadProcessMemory`.
- *What to change.*
  - Read fills with plain loads and a vectored handler for a page that
    faults (VIctory #1010; take it with credit after a critical review).
  - Merge neighbouring fills and refreshes of a list at record time.
- *Files.* `crates/xtp-gpu/src/host.rs` (`copy_mapped`, the `defer_*`
  functions); `shadow.rs`.
- *Acceptance.* Fills plus bookkeeping at most 5 ms a frame, with the
  unmapped-page tests of `copy_mapped` green.

**N6. The 3D world's GPU frame, by pass and program.**

- *Goal.* gpu-prof's timestamps (`XTP_GPU_TIMESTAMPS=1`) were taken before
  the 3D (125 draws a frame). The world now draws 2 692 a frame. Measure
  again on current `main`, rank passes and programs, and give each of the
  top 5 an owner (translator, `draw.rs`, indirect) toward 25 ms of GPU a
  frame.
- *Files.* `docs/analysis/gpu-frame.md` (a new section); no code unless a
  fix is in the task's own files.
- *Acceptance.* GPU ms a frame by kind and by program, for two Series S
  world sessions.

**N7. The frame line counts each part once.**

- *Goal.* "copies" includes waits and double-counts flushes (§3).
  - Time `Copy` and `Fill` steps without their settles.
  - Count each flush once.
  - Print snapshots, guards plus write watch, and fills on their own.
  - Print the steps per frame.
  - Keep "preparing" as it is.
- *Files.* `crates/xtp-gpu/src/host.rs` (`run_deferred`, `Batch::flush`,
  `Meter::frame`), `copystat.rs`.
- *Acceptance.* On a Series S session, preparing ≈ the sum of the printed
  parts within 10 %, and "copies" never above "preparing".

## 10. Method

- **Timers.** A local patch, not delivered:
  `E:\xtp-data\runs\fps30\prep-timing-main.patch` (on `eeb1f03`); the new
  module is also at `E:\xtp-data\runs\fps30\prepprof.rs`.
  - Nested phase timers (a thread-local stack; inclusive, exclusive and
    count, inside `run_as` or outside), placed in `Host::run_as`, the
    deferred steps by kind, `Batch::settle*` and the timeline's conflict
    searches, `Batch::flush`, `Shadow::update`, `copy_stale`, `copy`,
    `writing`, `copying` and `stale`, `watch::writes` (journal lock,
    `GetWriteWatch`, journal), `guard`, `guards()`, `Staging::refresh`,
    `HostHeap::mirror` and its lock, `shadow::buffers()` and `find`,
    `Host::waiting`, `finish_reached`, the copy-back in
    `Host::complete`, `Pending::of`, `ExecuteCommandLists` and the push.
  - Seven phase kinds also take the process's page-fault count.
  - Logged with the frame line (diagnostics builds).
- **Sampler** (`XTP_PREP_SAMPLE=1`, session A only). A
  time-critical thread wakes every millisecond. When a thread is inside
  `run_as`, it suspends it, reads its context, unwinds up to 28 frames
  (`RtlVirtualUnwind`), resumes it, and notes the phase it was in. It
  reached about 650 samples a second. Stacks are logged every 20 s and
  symbolised with `cdb -z` against the build's PDB and Microsoft's
  symbols.
- **Stacks of every thread.** `XTP_STACKS` every 0.5 s, categorised with
  `E:\xtp-data\runs\frame-prof-tools\cats.py` (world-frame.md's tools).
- **Scripts.** `E:\xtp-data\runs\fps30\prep.py` (`phases`, `stacks`,
  `report`), `existing.py`, `corr.py`, `agg.py`. Outputs: `B-phases.txt`,
  `A-report.txt`, `B-cats.txt`, `existing.txt`.
- **AnyKyty.** Read-only code reading at `7de61e3451` (`bufferCache.cpp`,
  `textureCache.cpp`, `regionManager.h`, `memoryTracker.h`,
  `pageManager.cpp`, `renderContext.cpp`, `Faults.cpp`); authorship from
  `anyps5.md`. Nothing of it was built, run or copied.
- **Not done.**
  - ETW sampling: WPR needs an elevated shell, which this session did not
    have.
  - A second session with the sampler, to tell whether it caused A's stop.
  - GPU timestamps with the 3D on (N6).
