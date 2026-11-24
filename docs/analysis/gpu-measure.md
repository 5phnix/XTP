# GPU frame measurement

How to read where a world frame goes, from the frame-rate line the GPU layer
logs (backlog G2 in [gpu.md](gpu.md) §7), and what the first ordering fixes
(G3, G4) and asynchronous submission (G8) change in it. The run-analysis
worker fills in section 6 from world runs; everything else here is what each
number means.

## 1. The frame-rate line

`xtp_gpu::host` logs it every 5 s, at `INFO`:

```
1.2 fps, 866.8 ms/frame (505.0 preparing: copies 340.2 ms 273.9 MB, textures
23.9 ms 133.6 MB; 193.2 on the GPU; 156.7 copying back 7.8 MB; {waiting}
waiting for them, {overlap} overlapped; {outside} outside submissions),
{submissions} submissions/frame of {lists} lists, the
longest {ms} ms ({gpu ms} on the GPU, {lists} lists), buffer copies {n} on the
GPU {MB} MB and {n} on the CPU {MB} MB, {n} in-list writes on the GPU, 2644
draws/frame, dropped 2215 without a host pipeline (0 being made) and 3046
indirect
```

(The numbers are a run1 world frame's; the fields in braces are new and have
no world value yet.)

The counters restart every second, when the window caption is updated. The
line shows the second before it, per presented frame, not five seconds of
averages.

| Field | What it counts | Where |
|---|---|---|
| `fps`, `ms/frame` | Presented frames per second, and wall time per frame. | `Meter::frame` |
| `preparing` | CPU time from the start of each submission until its lists go to the host queue: the submission-time steps of every list (title memory copies into host copies, copies made on the CPU, texture refreshes, snapshots of shader-written ranges) and the state preludes. Since G8, without the waits for earlier submissions inside it (those count in `waiting`). | `Host::run_as` |
| `copies … ms … MB` | Of `preparing`: copies of title memory, meaning refreshes of host copies (merged per submission), buffer copies left on the CPU, and fills of upload memory. Time and bytes actually copied. | `run_deferred`, `Batch::flush` |
| `textures … ms … MB` | Of `preparing`: texture refreshes (bytes hashed or detiled). | `texture.rs` |
| `on the GPU` | Per submission, from its submission (or the end of the one before it, if later) until it was seen done: the GPU's busy time, idle gaps left out. Seen done means its fence value was found reached when finishing started, so a completion thread busy copying back the one before makes it a little longer. | `Host::complete` |
| `copying back … MB` | After the GPU: GPU copy destinations written back whole (G3), then shader-written ranges compared with their snapshot and written back where they differ. Time, and bytes written into title memory. Since G8 mostly on the completion thread (`xtp-gpu-finisher`), beside the title. | `Shadow::write_back`, `Shadow::copy_back` |
| `waiting for them` | Time the title's threads spent held by submissions they did not prepare: waiting for the host GPU, or for a submission to be finished (by the completion thread or themselves: copying back, bottom-of-pipe writes). A step that touches memory an earlier submission's GPU work writes waits for it; so do the present (it drains the frame), a list submitted again while it runs, page mappings and more than 16 submissions in flight. | `Host::waiting` |
| `overlapped` | `on the GPU` plus `copying back` minus `waiting`, never below 0: the GPU's work and the copying back that ran while the title's threads did something else. About 0 when every submission is waited for (`XTP_SYNC_SUBMIT=1`). | `overlapped` |
| `outside submissions` | `ms/frame` minus `preparing` and `waiting`, never below 0. It is the time the title's threads spend outside submissions: the title's own CPU work, the recording of lists (including pipeline waits, up to 500 ms per title frame), and presentation. | `outside` |
| `submissions/frame of … lists` | Title submissions (one per `ExecuteCommandLists`) per frame, and lists per submission on average. The present copy, ours, is not counted. | `Host::submit` |
| `the longest … ms (… on the GPU, … lists)` | The title submission that took longest in all (preparing, GPU, copying back) in the window, with its GPU part and its lists. Windows resets the device at about 2 s of GPU time in one submission. Counted when it finishes. | `Submissions` |
| `buffer copies … on the GPU … MB` | `CopyBufferRegion`, buffer `CopyResource` and `CopyMemoryToMemoryX` recorded as host GPU copies, in list order (G3). They are counted per submission they run in. | `shadow::copy` |
| `… on the CPU … MB` | The buffer copies left on the CPU, made at submission because neither side had a host copy. | `Step::Copy`, `copy_memory` |
| `in-list writes on the GPU` | Bottom-of-pipe writes (`Write32/64BitValueBottomOfPipeX`, query results) also written into their host copy on the GPU at their place in the list (G4). The CPU still writes title memory once the list has run. | `shadow::write` |
| `draws/frame`, `dropped …`, `indirect` | As before: draws recorded, draws without a host pipeline (pipelines still being made in brackets), indirect executions not run. | `draw.rs`, `list.rs` |

`preparing`, `on the GPU`, `copying back` and `waiting` include our present
copy (one small submission per frame).

Two sums hold: `preparing` + `waiting` + `outside` = `ms/frame` (what the
title's threads do), and `on the GPU` + `copying back` = `waiting` +
`overlapped` (where the GPU's work and the copying back went). With
`XTP_SYNC_SUBMIT=1` every submission is waited for, as before G8: `waiting`
is then `on the GPU` plus `copying back` and `overlapped` about 0, and the
line reads as it did. A run with and one without it compares the two
models.

## 2. Reading it: which cause of gpu.md §6.2 dominates

1. **Pipeline waits** show up in `outside submissions`, together with the
   title's own work. If `outside` is large while `being made` is not 0, the
   waits are likely. A second run with a warm pipeline library separates them.
   The line has no column of its own (section 7).
2. **Serial submission.** Since G8 the GPU and the copying back run beside
   the title's threads; what is still serial is `waiting`. A large `waiting`
   against a small `overlapped` means steps wait for the submissions before
   them (diagnostics builds name none yet), or the present drains a frame
   whose GPU work is long.
3. **CPU work around each submission.** `preparing` plus `copying back`. Where
   `copies` dominates `preparing`, the diagnostics copy line (section 4) says
   whether refreshes, watches or small copies take it.
4. **GPU time.** `on the GPU`, and `longest` for the danger of a device reset.
   `XTP_GPU_TIMING` (diagnostics builds) names the slowest commands.

`preparing`, `waiting` and `outside` add up to `ms/frame`. The largest one is
the cause to fix first.

## 3. What G3 and G4 change

- A buffer copy with a host copy on either side now runs on the GPU between
  host copies, in list order. Before, every buffer copy ran on the CPU before
  the submission's GPU work, so a copy of what a dispatch had just written took
  the old bytes. The destination goes back to title memory after the GPU,
  merged per host copy, so `copying back` grows a little and `copies` shrinks.
- A buffer copy with no host copy on either side stays on the CPU (the
  `on the CPU` count). Its mapping check now runs only when the kernel's copy
  fails (section 5).
- A memory write into a host copy is also made on the GPU at its place in the
  list, so later commands of the same list read it.
- Before this change there was no counter of copies or writes. Earlier runs
  show none of these fields.

### What G8 changes (asynchronous submission, VIctory #595)

- A submission no longer waits for the GPU: it signals the host fence and
  returns. The completion thread `xtp-gpu-finisher` finishes submissions in
  order once the GPU has run them: write-back, then bottom-of-pipe writes,
  then the title fence signals queued behind them. So `on the GPU` and
  `copying back` no longer add to the frame; `waiting` and `overlapped` say
  how much of them the title still waits for.
- Lines before G8 have no `waiting` or `overlapped`. There, `on the GPU` was
  the time waited for the GPU and `outside` was `ms/frame` minus `preparing`,
  `on the GPU` and `copying back`. Since G8 `preparing` leaves out the waits
  inside it, which VIctory's own numbers for #595 (241 ms preparing per world
  frame) did not.
- The present still drains the frame, so the GPU's work overlaps the title's
  CPU work within a frame only.
- `XTP_SYNC_SUBMIT=1` brings back the old model (every submission waited for
  and finished in `execute`, no completion thread), to bisect a failure the
  asynchronous one may cause (gpu.md, risk 2).

## 4. The diagnostics copy line

Diagnostics builds add `xtp_gpu::copystat: copies per frame: …` after the
frame-rate line, then the ten ranges copied most. These are the fields that
matter here:

| Field | Meaning |
|---|---|
| `title copies … MB` | CPU copies and upload fills (`Step::Copy`, `Step::Fill`). |
| `refreshes … MB` | Bytes the submission steps asked to refresh, before merging. |
| `watches … ms` | Write-watch and guard queries, and their time. |
| `… MB checked, … MB unchanged` | Bytes of write-watched or guarded ranges checked, and of them found unchanged (the rest was copied). |
| `moves … ms` | Time in the kernel's copy (`ReadProcessMemory`) on its first attempt. |
| `… MB unmapped`, `… fallbacks` | Bytes read as zeros because a page was not mapped, and copies that had to go page by page for that reason. |
| `copied from A+L (memory T): n/frame, … MB/frame, … ms/frame` | A range refreshed `n` times per frame (memory type `T`: `0x40000` is a view, `0x20000` private memory), with the merged bytes and the time. |

In run1 (`E:/xtp-data/run1.clean.log`), the 12 copy lines of world frames
(20 submissions or more) show, per frame:

- 21 to 37 submissions and 2 800 to 7 800 title copies of 500 to 900 bytes
  each.
- 750 to 1 500 watches taking 20 to 46 ms.
- 4.6 to 9.9 GB checked, for 75 to 270 MB copied.
- 55 to 240 MB unmapped, in 260 to 1 100 fallbacks.
- `moves` of 4 to 18 ms.

In one sampled frame, the 174 MB heap (`0x26020000+0xae00000`, a view) was
refreshed 20 times in 12 ms, and the ten largest ranges took 17 ms in all.
Most of the 140 to 560 ms of `copies` was therefore not the copying itself,
nor the watch queries. It was the small CPU copies: each asked `VirtualQuery`
about its destination (section 5).

## 5. Measured on this PC (micro-benchmarks)

The benchmarks are ignored tests in `crates/xtp-gpu/src/watch.rs`:

```
cargo test --release -p xtp-gpu -- --ignored --nocapture watch_and_copy_costs guarded_copy_costs
```

They ran on the person's PC (Ryzen 9 7950X3D) at below-normal priority,
through `cargo-slot`, on 2026-11-24. The case is 30 submissions of 200 copies
of 600 bytes into a guarded 174 MB view, with the guards reset before each
submission as a refresh resets them:

| How the copies go to title memory | Copies in a ring | Scattered copies |
|---|---|---|
| On the CPU, mapping asked first (before this change) | 69 ms | 115 ms |
| On the CPU, mapping asked only when the copy fails (now) | 6.4 ms | 17 ms |
| Written back after the GPU, range by range | 9.5 ms | 25 ms |
| Written back after the GPU, merged per host copy (now) | 4.0 ms | 21 ms |

| Other cost | Measured |
|---|---|
| Write-watch query over 174 MB | 0.24 ms with a few pages written, 3.4 ms with all written |
| Copy through the kernel (`ReadProcessMemory`) | 5.1 GB/s in 64 KiB runs, 10 GB/s in 4 MiB runs |
| Plain copy | 14 to 22 GB/s |

Expected effect on a world frame: roughly 50 to 100 ms less `copies` if run1's
2 800 to 7 800 copies behave like these, and more if the title's regions are
larger than this view (`VirtualQuery` walks the region to its end). The
run-analysis worker confirms or corrects that in section 6.

## 6. Runs (filled in by the run-analysis worker)

| Run | Build | fps | ms/frame | preparing (copies, textures) | GPU | copying back | outside | submissions/frame, lists each | longest (GPU) | GPU copies, MB | CPU copies, MB | in-list writes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| run1, world (from its log: 10 lines of 1 000 draws or more) | main before G1–G4 | 1.0–1.9 | 515–1 042 | 213–687 (142–562, 9–54) | 90–404 | 25–157 | not logged | 21–37 (copy line), not logged | not logged | none (all on the CPU) | 2 800–7 800, 2.4–4.2 (with fills) | none |
| run-world first numbers | main 2d0257e | | 750–830 | 365–460 (280–345) | 200–290 | 90–120 | 30–100 | 26–37 | | | | |
| first world run with G1–G4 | | | | | | | | | | | | |

The conclusion for §6.2 (which cause dominates, with its share) goes here
once measured.

## 7. Not measured here, and next steps

- **Pipeline-wait milliseconds** are not in the line. The wait budget in
  `pipeline/make.rs` keeps its time in a static local to `wait()`, reset at
  each title frame, which a one-line accessor cannot reach. The pipeline task
  can add a cumulative counter: a static `AtomicU64` added to in `wait()` and a
  `take_waited()` accessor. The line would then show it next to
  `outside submissions`.
- **Watch queries per submission (not done).** A world submission asks the
  kernel and the guards about 35 to 40 times (750 to 1 500 per frame), once per merged
  refresh batch: `Batch::flush` runs before every shader-written range and
  every CPU copy step, and every list refreshes its ranges again. Within one
  submission, a range already brought up to date needs no new query: title
  writes made after the title called `ExecuteCommandLists` race with the GPU
  on the console too. `Batch` could keep the ranges refreshed so far in the
  submission per host copy, cut new requests by them, and forget them only at
  `title_written` (a CPU step that writes title memory). This would save part
  of the 20 to 46 ms of `watches` per frame. It needs a test that a title
  write before the submission is still seen, and a CPU copy step between two
  lists still triggers a refresh.
- **Unmapped pages.** 55 to 240 MB per frame are read as zeros through the
  page-by-page fallback. It looks like tiled memory with unmapped tiles being
  asked again at every refresh, because a guard cannot be set on an unmapped
  block, so that block is reported as written again. The cost is not in the
  ten largest ranges (they total 17 ms), but the bytes inflate `copies … MB`.
- **`copying back`** costs 25 to 157 ms for under 10 MB written. The time goes
  to comparing the snapshots of the large shader-written ranges, not to
  writing (gpu.md §6.2 item 3; G5 onwards).

## 8. GPU tests (G1)

GPU tests in `xtp-gpu` take the host device from `testgpu::device`:

- default: the hardware adapter;
- `XTP_TEST_ADAPTER=warp`: the software adapter;
- `XTP_TEST_ADAPTER=none`: no device.

Without a device, a test passes and prints `SKIPPED <test>: <reason>` on
standard error. That output is not captured, so a run lists its skips. With
`XTP_REQUIRE_GPU=1`, such a skip fails the test.

```
XTP_TEST_ADAPTER=warp XTP_REQUIRE_GPU=1 cargo test -p xtp-gpu --locked
```

The ordering tests of G3 and G4 (`list::tests::gpu`) record small compute
kernels into title lists, standing in for the title's passes:

- dispatch, copy, dispatch in one submission;
- whole-buffer copies and memory copies, overlapping ones within one buffer;
- a copy without host copies, which stays on the CPU;
- memory writes read by a later dispatch.

They fail on the code before G3 and G4.
