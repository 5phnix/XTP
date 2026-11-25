# Title CPU crashes in world sessions that survive the GPU

Task ecc6ca0c, 2026-11-25. BTP 6 1.0.2.1 (build blake3
`b99f876b74d4a77f…`), cloud L4 sessions. Title code is named by RVA in the
title executable (image base `0x140000000`); `crashes.log` names the same
offsets as `xtp-image-b99f876b74d4a77f.dll+<rva>`. Behaviour only: no title
bytes or listings (docs/asset-policy.md). The reads behind this page (the
static queries, the register decoding and the run-time samples) stay
outside the repository, under `E:\xtp-data\re\crashes\`.

Once the DXR input validation (4a0cdb3) kept the L4 world sessions alive
past their first GPU loss, three sessions ended on title code instead:

| Session | Run folder (`E:\xtp-data\cloud-runs\`) | Switches | Stop |
|---|---|---|---|
| `cxstrideon2-both` | `20261125-131558-game` | `XTP_RUN_TIME_VIEWS=1`, `XTP_STRIDE_TABLE=1`, world capture | `0xC0000005`, write to 0 at `+0x23cf683`, 13:29:32 |
| `debug-gbv-main` | `20261125-152638-game` | `XTP_D3D12_DEBUG=1`, `XTP_D3D12_GBV=1` | the same, 14:31:57, 31 s into the world |
| `dred-markers-l4` | `20261125-153431-game` | `XTP_D3D12_DRED=1` | `0x80000003` at `+0x63966f`, 14:58:56, 20.5 min into the world |

None of them set `XTP_WATCHDOG_HOOK`. All three were Series S,
`XTP_SKIP_INTRO=1`, autopilot from the golden save, `-AudioLockStopgap`.

## 1. Summary

- **The two stacks share nothing but the thread start.** `0x2215eca` is the
  return address after the call into a thread's own function in the title's
  common thread start (`0x2215ea0`). It sits at the bottom of every title
  thread's stack (`crates/xtp-game/src/watchdog.rs` says so too).
- **The write to address 0 at `0x23cf683` is the title's progress watchdog
  ending the process on purpose** (§2). No pointer was null: the
  instruction writes a constant marker to the absolute address 0. Both
  sessions' registers show the countdown path: no frame finished for 30
  one-second ticks. Their switches made one world frame last 30 s or more:
  GPU-based validation (frame 2036, 31 s) and the run-time views with the
  world capture (frames of 6 to 14 s copying 10 to 17 GB each, then one of
  30 s). Every world session of the analyses sets `XTP_WATCHDOG_HOOK=1` for
  this reason; these three did not. `cargo xtask play --measure` now sets
  it by default (§2.4).
- **The `int3` at `0x63966f` is the title's check that a fixed pool still
  has a free slot** (§3). The pool has 25,000 slots of `0x100` bytes. At the
  stop every slot was in use and two job threads asked for one at the same
  time. Each slot is a placement of a streamed map object (a world position
  and its owner). A read-only sampler on a new L4 session (`poolwatch1`)
  shows that no owner fills its slots twice and that unloads give the slots
  back, so the pool does not leak. Its use follows streaming: for 15
  minutes thousands of slots were taken and given back each minute, with
  900 to 11,000 in use. Then the frees almost stopped while loads went on,
  and the use reached 20,034 after 21 minutes. The title's own crash
  report (§3.3) shows that it was paused at the `int3`, as in VIctory
  #698, and that its reported pools (the instance lists included) were not
  full at any of the three stops. Why the frees stop is not proven yet
  (§3.6).

## 2. The write to address 0: the title's progress watchdog

### 2.1 What the title does

- **The watchdog thread** runs `0x2215a60`. Each tick sleeps one second
  between two clock reads. A tick much longer than that (the process was
  suspended, or held by a debugger) resets it. It keeps a countdown of 30
  ticks, reloaded whenever the progress stamp at `0x71b6338` changes.
  - The progress stamp is a clock value that only `0x22159f0` writes. Its
    callers are a per-frame function, `0x120790` (reached through two
    vtables; it stamps the progress, measures the frame's time and sets the
    render context's state for the next frame, then stamps again through
    `0xc9e330`), and a blocking wait loop, `0x1c82b20`, which stamps between
    its polls while loading. So in the world, "progress" means "a title
    frame went by". XTP's frame-rate lines time its own submissions and
    presents, not that function: `dred-markers-l4` logged a 31.3 s frame
    without a firing. The margin is a second or two either way.
  - When the flag at `0x71b6334` is set, the loop instead walks a table of
    per-entry heartbeats (64 groups at `0x6844a20`) and fires on a stale one.
    That path reloads the countdown at every tick, so it fires with a
    non-zero count.
  - A firing calls the handler whose pointer is at `0x71b6348`
    (`0x23cf620`). If there is none, the loop writes to address 0 itself
    (`0x2215c35`, `0x2215da9`).
- **The handler** (`0x23cf620`) records a short reason tag with the title's
  crash reporter (`0x2215950`), then writes a constant marker to the
  absolute address 0 (`0x23cf683`). The title's crash reporter then writes
  its minidump and the process ends with `0xC0000005`.
- **When the handler is armed.** Its one reference is in `0x23cf6a0`, the
  main thread's frame sync, called once a frame by the game loop's update
  (`0x812620`) and by a second loop that runs the same frame steps
  (`0x1b7530`). `0x23cf6a0` sets the
  handler through `0x22161f0`, which returns the previous one. It then
  polls a condition every millisecond (`0x23cb280`) until it holds, and
  puts the previous handler back. So `0x23cf683` is reached only when the
  watchdog fires while the main thread waits in its frame sync, in the
  world or in a loading screen. Nothing else, an allocation failure
  included, leads there.

### 2.2 Which path fired

At the fault, the non-volatile registers still hold the loop's values, as
the handler does not touch them:

- `rbx` = 0 and `rbp` = 0 in both sessions. The countdown path leaves both
  at 0; the heartbeat path leaves the count it had, at least 1.
- `rsi` is the loop's stack slot of the first copy of the loop.
- `rdi` (the tick's first clock read) minus `r15` (the last progress stamp)
  is 29.9 s (`debug-gbv-main`) and 29.7 s (`cxstrideon2-both`), at the
  10 MHz clock.

So in both sessions the title saw no frame finish for 30 s.

### 2.3 Why no frame finished for 30 s

- **`debug-gbv-main`** (`XTP_D3D12_GBV=1`): the autopilot handed control
  back at frame 2028 (14:31:25.9). Frame 2036 began at 14:31:26 and was
  still being processed when the watchdog fired at 14:31:57. During those
  31 s the log shows only first binds of frame 2036, about a hundred, and
  its heavy compute pipelines made in 0.7 to 4.4 s each under the debug
  layer. Earlier frames already took up to 27.7 s (the frame-rate line of
  14:31:12).
- **`cxstrideon2-both`** (`XTP_RUN_TIME_VIEWS=1`, `XTP_CAPTURE_WORLD`): from
  13:26:19 on, every world frame took 4.6 to 13.9 s. After the first one,
  2.4 to 5.1 s of each frame went to copies of 6 to 17 GB. The last
  frame-rate line is at 13:29:02. The watchdog fired at 13:29:32.

### 2.4 Cause and fix

The cause is ours: in both cases a diagnostic switch made one world frame
last longer than the title's 30 s, which no console frame takes. It is no
HLE return value and no null pointer. Without these switches the same
binary at 0.05 to 0.1 fps stayed just under the countdown:
`dred-markers-l4` ran 20 minutes at 5 to 22 s a frame (frame-rate lines),
the longest 31.3 s.

`XTP_WATCHDOG_HOOK=1` (`crates/xtp-game/src/watchdog.rs`) exists for exactly
this, and every world session of the analyses sets it by hand. As long as
our world frames can pass 30 s, it is a legitimate session setting.
`cargo xtask play --measure` now sets it by default, as it sets
`XTP_SKIP_INTRO` (`xtask/src/play.rs`, `hooks_watchdog`). A value the
caller set stays, and `XTP_PLAY_WATCHDOG=1` keeps the title's watchdog
armed. Cloud sessions (`tools/cloud/run-remote.ps1 -Game`) still take it
through `-Env` (§4).

### 2.5 The same write on an RTX 3070 (victory-fork #980)

On victory-fork #980 (branch `rt-772-vram-residents`, head `91562709fb`),
an RTX 3070 8 GB in Series S stopped at this same write about 110 s into
loading, in 2 launches of 2, with ray tracing and in-place residents on
together. Each one alone loaded, at 914 and 407 ms a frame. The tester
suspected a failed allocation.

- By §2.1, that write is the watchdog's handler and is armed only during
  the main thread's frame sync. So it is the same stop: no frame went by
  for about 30 s during loading. A failed allocation cannot reach it.
  Memory pressure can still be why that frame was so long: with 8 GB, ray
  tracing and residents together may page.
- Their registers would tell the two firing paths apart (§2.2): `rbx` and
  `rbp` at 0 for the countdown, or a count for a stale heartbeat. The
  report gives none.
- VIctory #1118's RTX 3070 Ti control stopped at the same write with the
  title's instance lists at 13,293 of 45,000: no pool was full there
  either.

There is no second cause at `0x23cf683` in either report. With the hook,
those runs would have gone on, and their frame times would then show what
made one frame take 30 s.

## 3. The `int3` at `0x63966f`: a fixed pool runs out of slots

### 3.1 The check

- `0x6395d0(record, id)` takes a slot from the pool whose object is behind
  the title's pointer at `0x98d79a8`. The pool object holds its storage
  (`+0x00`), the slot size (`+0x08`, `0x100`), the slot count (`+0x0c`,
  25,000), the slots in use (`+0x10`), a per-64 KiB page count of the
  storage, committed as slots are used (`+0x18`), the free list's head
  (`+0x20`) and a per-slot word (`+0x28`). A free slot's word links it to
  the next free one; a slot in use has all 17 low bits set.
- The head packs the first free slot (low half) with a tag that each free
  increments (high half), so a lock-free exchange can take or give a slot.
- When the head names no slot (the list is empty), or the storage is
  missing, the function reaches the `int3`. Nothing in the title catches
  it: its crash reporter runs and the process ends. The `int3` is the
  title's check that the pool has a free slot. It is not a failed HLE call
  or a null from a host allocation: the storage was there and in use.

### 3.2 The state at the stop (`dred-markers-l4`)

- Two threads stopped on the same `int3` within the same second. Both
  were job workers (`0x221c1f0`), in a job (`0x2570310` runs it) whose
  function is `0x3e54a70` (§3.4). They were filling two owners, `0x50048da`
  and `0x50083bf`.
- The head read "empty", with a tag of `0x4c98`: 19,608 frees since the
  pool was made. The last slots the two threads had taken were 24,998
  and 24,999, the end of the initial free list. A freed slot goes back at
  the head and is taken again first, so the end of the list is only reached
  when every slot is in use at once. So 25,000 slots were in use, after
  44,608 taken in all.

### 3.3 Pool use at each stop, from the title's own crash report

Before it ends, the title writes a report of its own (`temp\*.crash.log`
in the run's data folder, on the VM; copies in
`E:\xtp-data\re\crashes\title-crash-logs\`, not quoted here). It counts
the use of its main pools. VIctory #698 read the same report. At each of
the three stops:

| Session | Stop | Title time, frame | Paused | Instance lists | Buildings | Objects | Streaming requests |
|---|---|---|---|---:|---:|---:|---:|
| `debug-gbv-main` | watchdog | 2 min 32 s, 575 | no | 21,030 / 45,000 | 8,404 / 22,500 | 6,028 / 27,500 | 531 |
| `cxstrideon2-both` | watchdog | 5 min 1 s, 557 | no | 27,837 / 45,000 | 9,636 / 22,500 | 7,078 / 27,500 | 1,701 |
| `dred-markers-l4` | `int3` | 22 min 11 s, 734 | **yes** | 36,019 / 45,000 | 14,870 / 22,500 | 11,830 / 27,500 | 2,895 |

- **None of the reported pools was full at either kind of stop.** The
  pool behind `0x63966f` (25,000 slots, §3.1) is not in the report's list.
  It was full at the `int3` while the instance lists were at 80 %.
- VIctory #698's second run stopped at this same `int3`, with the same
  stack, with its instance lists at 45,000 of 45,000. Its first run
  stopped at `+0x5149aec`, also with the instance lists full. On their
  stacks both pools were full. On ours only the 25,000-slot one was, so
  the `int3` at `0x63966f` guards that pool and not the instance lists.
  Whether `+0x5149aec` guards the instance lists was not checked here.
- **The title was paused at the `int3`**, as in both of #698's runs, but
  not at the two watchdog stops. In all of them the title's scripted
  streaming list was still prestreaming (the report's "SRL" state).

### 3.4 What takes the slots, and what gives them back

- **The owner.** A 0x130-byte object (vtable `0x655ce30`, made by
  `0x7b6260`, which a factory table points to).
  - Its slot 1, `0x9da020(this, _, definition, id, scheduler)`, queues the
    job `0x3e54a70` when the definition has records: an array of 0x50-byte
    records at `definition+0x1c0`, counted at `+0x1c8`.
  - The job copies those records into an array of the owner's own (`+0x120`)
    and takes one pool slot per record, skipping records with a flag set.
    It keeps the slots' handles in a growing array (`+0xc8`, count `+0xc4`).
  - Its slot 11, `0x7cb840(this, id)`, is the owner's unload. Among other
    things, it gives back every slot whose handle it still holds
    (`0x3e53b40`, which checks each handle's generation first).
  - Its destructor (`0x107e50`) frees the handle array, not the slots.
- **The slot.** `0x5351090` fills a slot from its record: a position at
  `+0x20` (world coordinates, from `record+0x30`), the owner's id at `+0x34`
  (`0x5000000` + an index), the record's address at `+0x40`.
- **Other ways back.** A per-frame function (`0x54e680`) frees the handles a
  deferred list holds, and `0x3e57d80` frees every slot in use (a flush).
- So the pool holds the placements of the map objects that streaming has
  loaded, one slot per record, for as long as their owner stays loaded.

### 3.5 Measured: session `poolwatch1`

A new L4 session, `poolwatch1`: run folder
`D:\xtp-data\runs\20261125-152139-poolwatch1` on the VM, its logs brought
back to `E:\xtp-data\cloud-runs\` when it ends. It ran `main` at 3500052
with the `dred-markers-l4` switches plus `XTP_WATCHDOG_HOOK=1` and
`XTP_XMEM_LEDGER=1`, 1,500 s, autopilot. Beside it, a sampler read the pool
from outside the process (`ReadProcessMemory`, no debugger, nothing
written): its header every 5 s, and every live slot (owner, position,
record) every 60 s. Its output is under
`E:\xtp-data\re\crashes\runs\poolwatch-poolwatch1\` (partial copy in
`E:\xtp-data\re\crashes\partial\`).

Its course (UTC; owners from the snapshot of that minute):

| Time | In use | Frees so far | Owners | Note |
|---|---:|---:|---:|---|
| 15:23:41 | 3,970 | 0 | | loading |
| 15:23:51 | 10,463 | 0 | 237 | loading; positions spread over 10,400 × 8,300 world units |
| 15:24:51 | 10,578 | 0 | 239 | in the world (the autopilot's flags) |
| 15:25:51 | 2,627 | 7,951 | 31 | 208 owners gone |
| 15:26:51 | 6,384 | 7,951 | 111 | 82 new owners |
| 15:27:51 | 976 | 15,002 | 16 | |
| 15:29:51 | 7,702 | 15,095 | | |
| 15:35:52 | 10,459 | 27,389 | 210 | |
| 15:38:52 | 10,865 | 31,615 | 306 | |
| 15:40:52 | 17,541 | 32,247 | 452 | positions now 10,300 units deep in y |
| 15:43:53 | 17,963 | 32,247 | 462 | no free from 15:40:12 to 15:45:08 |
| 15:45:48 | 20,034 | 33,831 | | the session's end (1,500 s) at 15:46:40 |

The world ran at 14 to 21 s a frame throughout (0.05 to 0.1 fps).

- **The use climbs as in `dred-markers-l4`.** For the first 15 minutes
  loads and frees took turns, with 1,000 to 11,000 slots in use. From
  15:38 on, frees almost stopped (2,216 in 8 minutes) while loads went on,
  and the use reached 20,034 after 21 minutes in the world. At that rate
  the pool would have been full a few minutes after the session's end.
- No record is held twice: 0 in every one of 25 snapshots, of up to 20,034
  slots. A
  second fill of an owner would repeat every one of its records. Two
  owners have two records at one position (three positions), the same in
  both large snapshots: that is their definition's own data. So the job
  does not run twice for one owner, and the owners' unloads give their
  slots back. **The pool does not leak.**
- Its use follows streaming. Owners come and go by the hundred and their
  slots by the thousand, with nobody at the controls. Some owners hold 200
  to 300 slots each.
- The count moves in steps 20 to 60 s apart, loads and unloads alike
  (15:24:21, 15:25:21, 15:26:06, 15:26:31, 15:26:46, 15:27:06, …), as the
  title's world frames do on the L4.

### 3.6 What is left to prove

- **Why the frees stop.** `poolwatch1` gave back slots for 15 minutes,
  then almost none while it kept loading (§3.5). `dred-markers-l4`
  (`3706ba2`, dred's branch) had freed only 19,608 in 22 minutes. Unloads
  (slot 11) may run from the title's frames, at 14 to 21 s a frame here,
  while loads come at I/O speed. Or the streaming stops unloading in some
  state.
- **The pause.** The title was paused at the `int3` (§3.3), as in #698's
  two runs, and its scripted streaming list was prestreaming. A paused
  title that keeps prestreaming while it no longer unloads would fill the
  pool. Check whether owners' unloads (slot 11) stop while it is paused,
  and why it pauses on the L4 (a window that lost focus, a system event
  XTP sends) when nobody touches it.
- **What to read next in `poolwatch1`'s data** (snapshots in
  `E:\xtp-data\re\crashes\runs\poolwatch-poolwatch1\`, logs in
  `E:\xtp-data\cloud-runs\20261125-161913-game\`):
  1. the owners that stay after 15:40 against the player's position:
     owners that should have left (far away) or new ones loading;
  2. the title's pause state over time (a flag to find, then
     `XTP_WATCH`), beside the frees;
  3. a longer session, or one with the pause flag watched, to see the
     pool reach 25,000 and the title's own report at that stop.
- **The fix depends on that answer.** XTP must not raise the title's pool
  size or skip its check. If unloads wait for frames, the cure is the
  frame time of the L4 world. If the streaming loads more than a console
  would (a memory budget we report, a streaming request answered
  differently), the fix is in the HLE call that tells it.

## 4. Follow-up

- **Cloud sessions and the watchdog** (owner of `tools/cloud/`):
  `run-remote.ps1 -Game` could set `XTP_WATCHDOG_HOOK=1` unless `-Env`
  gives it, as `cargo xtask play --measure` now does. Until then, give it in
  `-Env` for every world session, and above all with `XTP_D3D12_GBV`,
  `XTP_RUN_TIME_VIEWS` or a world capture.
- **The pool**: §3.6, in a follow-up task.

## 5. Method

- Static reads of the executable with the `.pdata`, disassembly and
  reference scans of `E:\xtp-data\re\scripts\exe_scan.py` (capstone,
  pefile), queries and outputs in `E:\xtp-data\re\crashes\`.
- Registers and stacks from each session's `crashes.log`, times from its
  `run.log`.
- The sampler (`E:\xtp-data\re\crashes\scripts\poolwatch.ps1`, run on the
  VM beside `run-remote.ps1 -Game -Name poolwatch1` by
  `poolwatch-run.ps1`), and `analyze_snaps.py` for the snapshots.
