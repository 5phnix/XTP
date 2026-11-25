# Cross-queue waits: `Wait32BitValueX` and `Wait64BitValueX`

Task d2b62778, upstream watch item 532 (VIctory `9cc37c5f5b`). Series S
mode.

## 1. Summary

A console list can stop until a word of memory compares true with a
reference (`Wait32BitValueX`, `Wait64BitValueX`: list slots 71 and 72,
0x238 and 0x240, two slots later in newer lists). Another queue's list
writes that word at its end (`Write32/64BitValueBottomOfPipeX`), or the
CPU writes it. BTP 6 orders its queues this way:

- its four compute queues start each list by waiting for the frame's value
  of a word that a list on another queue writes;
- its graphics lists wait for words the compute lists write.

XTP took these waits for no-ops. Its reasoning: "lists run one after
another and to completion". Lists did run in the order the title
submitted them. That order meets every wait only if the title submits the
writer first, and nothing made it do so. VIctory measured the result in
its trace: the characters' depth-processing pass ran before the G-buffer
it reads. Hair came out as grey blocks, faces dark or patched, and
building bodies were erased.

Now (`crates/xtp-gpu/src/queue.rs`):

- A list notes the values it waits for in its host list
  (`queue::wait32`, `queue::wait64`, `HostList::defer_value_wait`).
- At submission, the queue tests each wait ([`met`](#3-how-a-wait-is-met)).
  If one is not met, the queue holds that list and everything it is given
  after it. The submission is cut before each list that waits, so the
  lists before it run at once.
- A held list runs once its values are met. Lists that may run go to the
  host GPU together, as one submission.
- A held list never waits for the GPU or makes the GPU wait. The host has
  one queue, so a list submitted after the writer's submission runs after
  it.
- A queue that has stood still for 4 s runs on, with a warning line. The
  console would hang there.

## 2. The title's waits (VIctory's trace, behaviour only)

In one world frame of VIctory's trace, every comparison is
`GREATER_EQUAL` against the frame counter F:

| Queue | Lists | Waits |
|---|---|---|
| compute (kind 2), 4 queues | 22 submissions of one list | at the start, `[base + 16k] >= F`; the list writes `[base + 16k + 8] = F` at its end |
| compute, the depth-processing list | one | waits at the start, then again after a dispatch and a bottom-of-pipe write of its own |
| graphics (kind 0) | submissions of 32 to 33 lists | list 1 after 7 to 8 commands, lists 3, 5, 7, … at their start, for words the compute lists write |

The words lie in one 0x270-byte block of title memory. Section 5 gives
XTP's own counts.

## 3. How a wait is met

`met(wait, before)` checks these in order:

1. **Unknown comparison.** A comparison outside D3D12's 1 (never) to
   8 (always) is taken as met, and counted (`unknown`).
2. **Title memory.** The word, read through the kernel
   (`host::read_into`), meets the comparison. The word is 4 or 8 bytes,
   and a 32-bit wait masks it first. This covers values the CPU wrote and
   GPU writes that have already landed. A word that cannot be read
   counts as not met, and is counted (`unreadable`).
3. **An earlier list in the same submission.** The last bottom-of-pipe
   write that a list before it in the same submission makes to the word
   meets the comparison. That list runs first, on the same queue.
4. **A write in flight.** A bottom-of-pipe write in a title submission
   made so far, and not yet finished, meets the comparison (`WRITES`).
   That submission is on the one host queue, so a list submitted now runs
   after it. Each submission's writes are noted once it has been handed
   to the host, and they go once it has finished (`Host::after`), when
   they are in title memory. A write covers a wait if the bytes the wait
   reads lie inside it; a 64-bit write also answers a 32-bit wait on
   either half. Timestamps are left out: their value is not known ahead.

A list that waits stays held until `met` holds for all its waits. Its
queue is then tested again:

- after another queue's submission or signal;
- every millisecond by the queues' thread (`watch`), while anything is
  held, for words the CPU writes;
- by a fence call.

## 4. VIctory's port, reviewed against XTP

| VIctory `9cc37c5f5b` | XTP | Why |
|---|---|---|
| A submission runs to completion when the title makes it. Its writes are in memory at once, so the pump checks memory only. | Submissions run on while the title goes on (backlog G8). Writes land when their submission finishes. | Testing memory alone would hold each list until the writer's GPU work had finished and been written back: a CPU wait for the GPU before each held list. Instead, writes in flight count (section 3, step 4), and the one host queue keeps the order. |
| A list is cut where it waits after commands. `List.host` becomes several host lists, and `State::cut` replays the state. | The whole list waits. Submissions are cut at list boundaries only. | Cutting restructures `list.rs` and `draw.rs`. The brief allowed holding "a list (or the part after the wait)", and the orchestrator chose the whole list for this step. A whole-list hold can block in one case: the list writes a word before a later wait, and another queue waits for that word. That case is counted (`after_write`). |
| A list the title resets while it is held keeps its own host list, and the title list gets a fresh one. | The held list runs first, with its waits unmet: its commands are about to be replaced. The run is counted (`forced_reset`). | Same reason. VIctory counted 0 to 21 such resets a world frame. Section 5 says how many XTP sees. |
| `PresentX` does not wait behind held work (a shortcut). | `PresentX` runs what its queue holds first, as XTP did for fence waits. The run is counted (`forced_present`). | Unchanged. A present shows what was submitted before it. |
| `CopyPageMappingsBatchX` runs ahead of held work (a shortcut). | What the queue holds runs first, as before. The run is counted (`forced_mapping`). | Unchanged. |
| The valve: a head held 10 s runs anyway. | A queue that stood still 4 s runs on, as for fence waits. | Waits share one rule. The title's own watchdog is 30 s. |
| A 2 ms timer pumps. | The queues' thread looks every millisecond while anything is held. | It already existed for the title's CPU fence signals. |
| Unknown comparisons are taken as met. Memory that cannot be read is also taken as met. | Unknown comparisons are taken as met. Memory that cannot be read is taken as not met. Both are counted. | A word no one can read cannot meet a comparison on the console either. A write in flight still meets it. |
| Consecutive runnable segments form one host submission (a piece). | Consecutive runnable lists of a queue form one host submission. A signal ends it. | Same. |

Taken as is from VIctory: the comparisons (D3D12's), the 32-bit form's
mask in the fifth argument, the per-queue order, and the queue's signals
staying behind what it holds (XTP already held those).

## 5. Measurement

Every wait is counted at submission. The `list waits for memory values so
far` line repeats at each power-of-two frame and gives:

- **at submission:** `waits`, how each was met (`in_memory`,
  `written_here` for a write in flight from the same queue,
  `written_elsewhere` for one from another queue, `earlier_list`), or
  `unmet`; also `unknown` and `unreadable`;
- **where it was recorded:** `after_work`, meaning after commands, where
  the console stops mid-list; `after_write`, meaning after a
  bottom-of-pipe write of the list's own, where a whole-list hold can
  block;
- **held lists:** what released them (`released_by_memory`, `_by_here`,
  `_by_elsewhere`), the lists run before their waits were met
  (`forced_reset`, `_present`, `_mapping`, `_stuck`), and `held_ms`.

`XTP_VALUE_WAIT_LOG=<n>` logs the first `n` of each event, one line
each: submission (queue, address, comparison, reference, mask, how it was
met), release, forced run. `XTP_VALUE_WAIT=0` turns holding off and
keeps the measurement.

Session: pending (section 7).

## 6. Tests

`powershell -NoProfile -ExecutionPolicy Bypass -File E:\xtp-data\tools\cargo-slot.ps1 test -p xtp-gpu --locked queue`
runs these tests, on the host GPU or WARP:

- `value_waits_compare_the_masked_value`: the eight comparisons on a
  32-bit wait's masked low half; unknown comparisons;
- `a_write_stores_what_a_wait_inside_it_reads`: which writes cover which
  waits;
- `lists_wait_for_memory_values_at_their_slots`: slots 71 and 72, or 73 and
  74 in newer lists;
- `a_list_waiting_for_a_value_another_queue_writes_runs_after_the_writer`:
  queue B, submitted first, is held, runs once A's writer list is
  submitted, and reads what A drew. With `XTP_VALUE_WAIT=0` it fails, so
  it tests the hold;
- `a_wait_for_a_value_already_there_holds_nothing`: a value in title
  memory, or one an earlier list of the same submission writes, does not
  hold the list;
- `a_wait_for_a_value_a_submission_in_flight_writes_holds_nothing`: with
  the writer kept on the GPU (`host::Gate`), the waiting list is not held
  although the word is still 0 in memory;
- `a_list_waiting_for_a_value_the_cpu_writes_runs_once_it_is_written`:
  released by the queues' thread with no further call;
- `a_submission_is_cut_before_a_list_that_waits`: graphics
  `[G0 writes A][G1 waits for B]` and compute `[waits for A, writes B]`.
  Held whole, each would wait for the other; cut, they run G0, compute,
  G1;
- `a_value_nothing_writes_is_reported_and_does_not_hold_the_queue_for_ever`:
  the queue runs on after its patience, counted as `forced_stuck`, with a
  warning line;
- `a_list_held_for_a_value_the_title_resets_runs_first_and_is_counted`.

## 7. Session

Pending.

## 8. What next

- If the session shows `after_write` waits that block (`forced_stuck`),
  or many `forced_reset`: cut lists at waits and let a held list keep its
  own host list, as VIctory does. That changes `list.rs` (several host
  lists per title list) and `draw.rs` (replaying the state into the next
  one).
