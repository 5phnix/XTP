# One frame in flight

Task f1d37c5d. Before this change a present finished every submission
before it returned (`host.rs`, `present`, `self.drain()`): the title
prepared frame N+1 only once the GPU had run frame N, written back and
all. In Series S the world frame waited 159–242 ms for its submissions, of
which only 25–75 ms overlapped CPU work (session
`E:\xtp-data\cache\play\20261124-181140`, frame lines of 18:13). Now one
frame may run on: the title prepares frame N+1 while the GPU runs frame N,
and is never more than one frame ahead. Behind `XTP_FRAMES_IN_FLIGHT`.

Times are UTC. Logs are under `E:\xtp-data\` (not in the repository).
Title code is named by RVA; no title bytes or listings are quoted.

## 1. What changed

| Where | Before | Now |
|---|---|---|
| `Host::present` | records the copy into the back buffer, presents, then finishes every submission | first finishes the frame presented last (`Host::flip`), records and presents, then finishes every submission only with frames in flight off, or for a captured frame |
| `frame::wait`, flip event, no time-out | a clock tick | the frame presented last finished on the GPU, then the next tick (a console flips a frame once its GPU is done with it) |
| `HostList::reset` of a list of ours (last run by `Host::execute`) while it runs | hands its memory to the running submission | waits for that submission first |
| Submissions in flight at most | 16 | 16 off, 128 on (a world frame makes 20 to 40) |
| Black-frame probe (diagnostics) | read at its own present | with frames in flight, read at the next present, once its copy has run: reported one present late |
| Frame-rate line | waiting, overlapped | also: of the waiting, the part at flips and presents; whether frames run in flight; the longest frame of the second |

`XTP_FRAMES_IN_FLIGHT`:

- `0`: as before.
- `1`: one frame in flight.
- `ab[:<phase s>[:<settle s>]]`: off and on in turn within one run, phases
  of 20 s by default, the first 3 s of each not measured, a line per phase
  (`frames in flight A/B: phase …`). For measurements on the same scene.
- Unset: off, until the session of §5 shows it safe (then on).

`XTP_SYNC_SUBMIT=1` still waits for every submission; frames in flight
change nothing there.

### 1.1 What was taken from AnyPS5

Ported from simnJS/AnyPS5, the person's own commits on its AnyKyty layer
(docs/analysis/anyps5.md §7.1):

- `7b85afc1aa` (`DeferFlipCompletion`): a flip completes once its command
  buffer is submitted and the previous flip's work is done. Here: a present
  returns once its copy is submitted and the frame before has finished; a
  flip wait ends once the frame presented last has finished.
- `4328d9d796` (suspend points): up to eight in flight, but the title never
  more than one frame ahead, after eight frames ahead let a title rewrite
  vertex tables the GPU had not read. Here: one frame, by the flip wait and
  by the present, whichever comes first.
- `2eadda9b42` (`KYTY_AB`): settings alternated at flips within one
  session, with a settling time and per-phase counts. Here: one setting,
  switched at the frame-rate line's seconds (each second belongs to one
  phase), the phase lines with fps, the longest frame, GPU time, waiting
  and overlap per frame.

Not taken: AnyPS5's flip thread (its flips complete on a thread of their
own; here the title's present or flip wait does it), its eight-deep suspend
points (XTP has no such points: the title's own submissions run as they
come), and its in-place memory mode that forces one frame in flight (G16
here, measured slower there).

## 2. How BTP 6 waits for GPU work

The brief asks whether the title waits on its fences before it reads GPU
results, or takes its present call for the GPU being done.

**On the console a present never says the GPU is done.** `PresentX` queues
a flip behind the frame's GPU work and returns; the GPU runs the frame
while the title goes on. A title that read a frame's GPU results right
after its present, with no other wait, would read them unfinished on the
console. What can tell a console title that GPU work is done:

| Console signal | In XTP | Ordered after the copy-back? |
|---|---|---|
| A fence the queue signals (`Signal`, read as `GetCompletedValue`, `SetEventOnCompletion`, or straight from the fence's GPU address) | `queue::signal` → `Host::after`: set once every submission before it has finished | yes: `complete` writes back, then the bottom-of-pipe writes, then the signals queued behind (test `the_title_waits_for_its_fence_and_finds_what_the_frame_in_flight_wrote`) |
| A bottom-of-pipe write (label, timestamp) the title polls | `Pending::writes`, done in `complete` | yes, after the copy-back |
| The flip event (`WaitFrameEventX`, kind 2) | before: a tick; now, without a time-out, also the frame presented last finished | yes (test `a_flip_wait_ends_once_the_frame_presented_last_has_finished`) |
| A blocking `SetEventOnCompletion` (null event) | drains | yes |
| `PresentX` returning | before: everything finished; now: the frame before finished | not a console signal |

**BTP 6's frame loop** (debug log of run `stall-frame-prof-B`, 16:25–16:29,
`xtp_gpu::frame=debug`): 960 origin waits with time-out 0 and "token on
time-out" (it starts each frame at once) and 950 flip waits with no
time-out, one per frame, before its present. So each frame of BTP 6 passes
a blocking flip wait, which now ends only once the frame presented before
it has finished: after a flip, what the GPU wrote for that frame is in title
memory, as on the console.

**Where the title's threads wait** (run-world.md §8, two stack samples in
the world, before this change): the main thread waits in the title's wait
wrapper (`0x56a0d`, over `WaitForSingleObject`) called from `0x23cf735`
under the frame loop (`0x8126a7` ← `0x53ec040`): it waits for the render
frame before it starts the next one. The submission thread waited in XTP
(the synchronous submission of then). Neither sample showed a title wait on
a GPU value XTP never writes.

**What this does not establish.** Whether BTP 6 reads some GPU result
(occlusion, auto-exposure, a readback) at a fixed frame latency without a
fence (gpu.md question 7) is not settled by reverse engineering here: the
calls go through vtables, and a stack sample cannot show a read. One frame
in flight keeps XTP inside what a console title must already handle: a
result of frame N is ready once frame N's fence is signalled, once the flip
after frame N's present ended, or by the end of frame N+1's present. A read
at a latency of two frames or more without any wait is safe too; a read of
frame N's result during frame N+1 with no wait at all would be a race on
the console as well.

## 3. What stays correct

### 3.1 Copy-back before the title reads

Unchanged path: a submission's GPU copies and shader writes go back into
title memory in `Host::complete`, before its bottom-of-pipe writes and the
title fence signals queued behind it, on the completion thread or on
whichever thread first needs it. What changes is only that a present no
longer waits for it. Test:
`the_title_waits_for_its_fence_and_finds_what_the_frame_in_flight_wrote`
(the present returns with the frame on the GPU, title memory and fence
untouched; the title's event is set only once the shaders' output is back).

### 3.2 Host copies not overwritten in flight

The checks that keep a refresh from overwriting what GPU work in flight
writes (`Shadow::update` skips the busy ranges; `Timeline::refresh_conflict`
waits for the submission when the title wrote the page since the copy took
it, Marvisak/VIctory#595 and W1) look at submissions, not frames: they hold
across a present as they did within a frame. Test:
`the_next_frame_leaves_what_the_frame_in_flight_writes` (the next frame's
submission waits for the frame in flight, reads its output, and keeps the
title's write beside it).

What GPU work in flight *reads* from a host copy is not guarded against a
later refresh: a refresh writes only the ranges its own submission reads,
from title memory, and the title rewrites memory the GPU still reads only
if it did not wait for its fence, which would be a race on the console too.
Steps that write title memory (CPU copies, `Run` steps) still wait for
submissions in flight that read it (heaps over title memory, G15b).

### 3.3 Our own lists

Two lists of XTP's are reset every frame and would overlap with their own
running submission:

- the present's copy list: reset after `Host::flip`, so its last submission
  (the frame before's present) has finished;
- the plane compositor's list (`dxgi/planes.rs`; BTP 6 presents two planes,
  1920×1080 and 3840×2160, composed every frame): `compose` rewrites the
  shader-visible views its last submission reads, then records. A list of
  ours now waits for its submission when reset (test
  `a_list_of_ours_waits_for_its_submission_when_reset`); a title's list
  does not (`a_list_reset_while_it_runs_records_into_fresh_memory`). With
  BTP 6 the flip wait has already finished the frame before, so this waits
  for nothing.

The tests of `host/submissions.rs` that reset a running list stand for the
title's lists and now submit them as the title does (`Host::submit`).

### 3.4 Readbacks and probes

- **The title's readbacks**: GPU copies into title memory are written back
  before the fence signals behind them (§3.1).
- **Captures** (`XTP_CAPTURE`, diagnostics): a captured frame is finished
  at its present, as before, to be read back.
- **Target dumps** (`XTP_DUMP_TARGETS`): `capture::dump_one` drains itself.
- **The black-frame probe** (diagnostics, every frame): its readback buffer
  is reused, so it is read at the next present, after `Host::flip` and
  before the next probe records into it. With frames in flight a black frame
  is reported one present late, with the next frame's draws; set
  `XTP_FRAMES_IN_FLIGHT=0` for exact black-frame reports.
- **Readbacks of XTP's own** (`resource.rs`, `pool.rs`, tests) drain after
  their submission, as before.

### 3.5 Frame pacing and counters

`frame.rs` keeps its clock, ticks and tokens; the flip wait without a
time-out now first finishes the frame presented last (shortcut: a flip wait
with a time-out does not wait for the GPU; BTP 6's has none). Frame tokens
count origin waits, unchanged. The frame-rate line still counts presents;
its GPU time, copy-back and waiting are counted when they happen, so one
second's line may hold part of the frame before's GPU time.

## 4. Tests (WARP)

`cargo-slot.ps1 test -p xtp-gpu --locked` (farm, WARP):

| Test | What it shows |
|---|---|
| `a_frame_runs_on_past_its_present_until_the_next_flip` | a present returns with its frame on the GPU; the next flip waits for it |
| `without_frames_in_flight_a_present_finishes_every_submission` | the switch off: as before |
| `a_flip_wait_ends_once_the_frame_presented_last_has_finished` | `frame::wait` (flip, no time-out) ends once the frame finished |
| `the_title_waits_for_its_fence_and_finds_what_the_frame_in_flight_wrote` | copy-back before the title's fence, across a present |
| `the_next_frame_leaves_what_the_frame_in_flight_writes` | the in-flight checks of host copies across a present |
| `a_list_of_ours_waits_for_its_submission_when_reset` | the compositor's case |
| `frames::tests::*` | the switch's values; the A/B's phases, settling and lines |

## 5. Series S sessions

(filled in from the sessions below)

## 6. Critical review

Of AnyPS5's mechanism, of the brief, and of this change.

- **AnyPS5's commits were read at its head, not as diffs.** The local copy
  (`E:\xtp-refs\AnyPS5`) is a shallow clone of one commit (`7de61e3`); the
  three commits Samuel named are not in it. What was ported is the
  behaviour their code shows at that head (`commandScheduler.cpp`,
  `DeferFlipCompletion` and `FlipCompletionThread`; `graphicsRun.cpp`,
  `SuspendPoint` and `m_frame_ready`; `runtimeSwitches.cpp`, `AtFlip`), as
  docs/analysis/anyps5.md §7.1 describes them. No line of it was copied:
  the model differs (Vulkan command buffers and a flip thread there, the
  title's own D3D12.X submissions here).
- **AnyPS5 allows two frames in flight at its flip; this allows one.**
  Its own later commit (`4328d9d796`) bounds the title to one frame ahead
  after eight let a title rewrite tables the GPU had not read; the brief
  asks for one. A second frame would hide more GPU time but doubles what a
  title-side race can reach; not tried.
- **The present's drain was not only the title's safety.** The brief's
  shortcut comment suggested it stood for "titles not known to wait for
  their fences". The search found two of XTP's own uses that depended on
  it: the plane compositor rewriting the views its last submission reads
  (BTP 6 composes two planes every frame), and the diagnostics black-frame
  probe reusing one readback buffer. Both are handled (§3.3, §3.4); a
  reviewer should look for other per-frame state of ours that a running
  submission reads. None was found among the lists of ours that are
  reset: the others are made per use and drained.
- **The flip wait now waits for the GPU.** That is a change in a path the
  brief did not name, and it is what makes one frame in flight faithful for
  a title paced by flips: without it, the flip after frame N's present
  would end on a tick with frame N maybe unfinished, where a console's flip
  would not. Its cost is nil when the present already finished everything
  (frames in flight off). Risk: a title that waits for the flip on the
  thread the completion depends on would hang; none does (completion runs
  on XTP's own thread, and the GPU never waits for the CPU here).
- **Lists of ours wait at reset.** Chosen over editing `dxgi/planes.rs`
  (outside this task's files): it keeps every list of ours from
  overlapping with itself, the compositor's and any later one. It is
  keyed on how the list was last submitted (`execute` against `submit`),
  so a test standing for a title's list must submit it as the title does;
  three tests were changed so.
- **The cap on submissions in flight moves with the switch** (16 off, 128
  on): with one frame in flight, 16 would let the title start at most half
  of its next frame (20 to 40 submissions a frame) before waiting for the
  oldest. The A/B therefore compares the two caps too; the cap alone was
  not measured.
- **Black frames are reported one present late with frames in flight.**
  A second readback buffer per frame in flight would keep them exact; it
  would mean changing `capture.rs` (not this task's) for a diagnostics
  log. Documented instead, and `XTP_FRAMES_IN_FLIGHT=0` gives exact reports.
- **gpu.md question 7 stays open.** The safety argument is the console's
  semantics plus BTP 6's flip waits, not a proof that BTP 6 reads no GPU
  result without a wait. The sessions of §5 are the evidence for its world.

## 7. Open questions

- Does BTP 6 read a GPU result at a fixed frame latency without any wait
  (gpu.md question 7)? A stack sample cannot tell; a watch of the fence
  methods' callers (`GetCompletedValue`, `SetEventOnCompletion`, at the
  fence's slots 0x40 and 0x48) would show which title code waits on which
  fence per frame.
- Two frames in flight, as AnyPS5's flip thread allows, once one is known
  safe.
- A flip wait with a time-out does not wait for the GPU (frame.rs
  shortcut).
