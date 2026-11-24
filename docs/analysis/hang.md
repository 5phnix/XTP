# The Series S world hang: compute program d3e44e754b3eb6aa

Why the Series S world lost the host GPU (`DXGI_ERROR_DEVICE_HUNG`,
0x887A0006) 40 s to 2.5 min after its first frames, on the person's PC
(RTX 4080 SUPER 16 GB). The first device removal with DRED on (task
78d58a42, session `stall-copyback-B3-dred`) stopped in compute program
d3e44e754b3eb6aa. Programs are named by hash only: the first
16 hex digits of the BLAKE3 of the container the title passes, as the
runtime logs them. Times are UTC. Logs are under `E:\xtp-data\runs\`; none
is in the repository, and no title bytes or listings are quoted here.

## 1. Summary

- **Not a loop.** The program has three loops, one after the other, none
  nested. On the console they run at most 4, 255 and 254 rounds, and their
  bounds are byte-sized fields of a buffer the host has. With XTP's limits
  the worst case is still a few hundred rounds per lane: well under a
  millisecond for the 17 dispatches that ran together, against a watchdog of
  about 2 s. The two programs DRED lists after it (5b6eed17c131f881 and
  269b802f878c24e1) are bounded the same way (§4).
- **Not the null slots of the report.** The DRED report writes every
  user-data value as a heap slot. u5 to u8 are the program's four root
  constants, not heap indices: u8 is an element number, u6 a count, u5 a
  byte offset (§3).
- **The defect: one heap slot read as two kinds of view.** Each of the three
  programs loads one descriptor from the resource heap at an index it reads
  from a buffer. It reads that descriptor both with scalar buffer loads and
  with vector buffer loads. The translator makes the scalar loads read a
  constant buffer handle (CBV) and the vector loads a raw buffer handle (SRV)
  over the same slot. A D3D12 slot holds one view, and reading it through a
  handle of the other kind is undefined on the GPU (§5).
- **What the host holds there.** A slot reached only through an index loaded
  at run time is never made for a draw, so `HostHeap::mirror` leaves a null
  view of the title's kind there. The slot is a real view when another draw
  read it through a table. The console reads the title's buffer (§5.3).
- **Not FidelityFX.** The three programs come from the title's own shader
  pack, not from the FSR4 library. Sessions with the FidelityFX emulation
  lost the GPU too (§6).
- **The fix:** scalar buffer loads read a slot through the raw view the
  slot holds: the one its table entry says (SRV or UAV), or, with no entry,
  the one the program's vector accesses of the same descriptor declare.
  Every handle of the slot then has one kind (§7).
- **Evidence.** On WARP, the same shape loses the device without the change
  ("The GPU device instance has been suspended") and runs right with it.
  Two Series S sessions of 5 min with the change, with DRED and without,
  had no device removal (§7). The same rules wait upstream in VIctory
  #1015 and #1055 (§8).

## 2. The report

Session `stall-copyback-B3-dred`, build 502e91e (main 796e848 with
copyback's commits), Series S, `XTP_D3D12_DRED=1`, `XTP_SKIP_INTRO=1`,
`XTP_WATCHDOG_HOOK=1`. The world's first frames at 19:04:13; the GPU stops
answering at 19:06:16 and the device is removed at 19:06:21. The report is
the line "host GPU device removed after a submission of 0 lists (29 lists in
flight)" of `play.err.txt`.

- Page fault address 0 and no allocation listed: a hang, or a fault the
  driver did not place.
- Of the 29 lists in flight, only list 0 started. It stopped at command 105
  of 243, between the barriers at commands 96 and 130.
- Commands 97 to 129 are about 17 direct dispatches of d3e44e754b3eb6aa,
  from 136 down to 7 groups, with no barrier between them: they run
  together. Command 105 (31 groups) is the first one not finished.
- After barrier 130 come dispatches of 5b6eed17c131f881, then of
  269b802f878c24e1. DRED's counts may lag behind the GPU on NVIDIA, so the
  command really running may be a little later.

Every removal of the Series S sessions since 18:13 has the same signature,
"a submission of 0 lists (8 to 32 lists in flight)": `copyback-B-fix`,
`copyback-B2-main`, `copyback-B3-dred`, `ds-guards-after`, `ffx2-fsr4`,
`ffx2-hle2`, `gpu-prof-B`, `samuel-main-79fa391`. In each of them the three
programs are translated about 15 s before the world and run every frame
from its start; the removal comes 40 s (`gpu-prof-B`) to 2.5 min after the
program first appears in the log.

## 3. The program and what it reads

d3e44e754b3eb6aa is the record at offset 0xd4ff153 of the pack (a 5196-byte
container; a compute program of 64-lane waves, 6144 bytes of native code).
`xtp shader-info --disasm` and `xtp translate --validate` read it; a short
script found the record by hashing every unpacked SGD5 record.

Its root parameters: a descriptor table (u4), four root constants (u5 to
u8, register b500) and the resource heap (u9).

- **u4**, the table: a structured buffer of 336-byte elements (table entry
  168, 8736 bytes: 26 elements), a large buffer of 80-byte records (entry
  176), a structured buffer of 48-byte elements (entry 8), the UAV it writes
  (entry 288, 47 MB), and a raw buffer (entry 56).
- **u8** is multiplied by 336: it is the element of the 336-byte buffer the
  dispatch works on. The values in the report (0 to 21) are all under 26.
- **u6** is a count of points: the program multiplies by it and compares it
  with 2.
- **u5** is a byte offset into the UAV; the program compares it with −1.
  The values of the report (0x2c49210 to 0x2ce9f80) lie inside its 47 MB.
- **u7** is overwritten before any read, so the translation does not list it.

The report's "[heap 30: null]" for u6=0x1e and "[heap 17: null]" for
u8=0x11 come from the diagnostic text of a dispatch (`op_text`), which
describes every user-data value as a heap slot whatever its parameter.

From the element u8 of the 336-byte buffer the program reads two heap
indices (dwords 0x120 and 0xE0 of the element) and loads those descriptors
from the resource heap: a buffer it reads at constant and computed offsets
(§5), and a typed buffer.

## 4. Its loops, and those of the two programs after it

| Program | Loop | Bound on the console | XTP's limit |
| --- | --- | --- | --- |
| d3e44e754b3eb6aa | L1 | 4 rounds (a counter from 0 to 4) | 2^20 (counted) |
| | L2, the main loop | counts down from the top byte of dword 0xA8 of the element: at most 255 | 2^14 |
| | L3, an empty loop | the same byte: at most 254 | 2^20 (counted) |
| 5b6eed17c131f881 | 5 loops in sequence | 4 or 5 rounds each | 2^20 for the first, 2^14 for the others |
| 269b802f878c24e1 | 1 loop | `ceil(log2(b))` for a byte b: at most 8 | 2^20 (counted) |

- No loop is inside another, in the native code or in the translation (the
  only back edges of the DXIL are these loops'). Nested limits do not
  multiply here, and a budget shared by a lane's loops (loop-limits, task
  35e2aa06) changes nothing.
- The translation keeps the console's bounds: the counter, its start and its
  test are the console's. The bound of L2 is the top byte of a dword of the
  336-byte buffer, read through the table with a raw handle: whatever the
  data, at most 255.
- L2 gets the general limit (2^14) rather than the counted one: its exit test
  is in the block before its latch, and `counted_loops` (lower/flow.rs) only
  looks at the header and the only latch. With a real bound of at most 255,
  this does not matter.
- Worst case per lane at the console's bounds: 4 + 255 + 254 rounds, a few
  tens of microseconds per wave. The 17 dispatches together are under a
  millisecond, against the host's watchdog of about 2 s. Even at XTP's
  limits with a garbage bound, L2 would be 2^14 rounds of about a
  microsecond: tens of milliseconds, not a hang.

## 5. The heap slot read as two kinds of view

### 5.1 What the console program does

The descriptor the program loads from the resource heap, at the index in
dword 0x120 of the element, is read in two ways:

- by scalar buffer loads at constant offsets: four section offsets
  (0x10 to 0x1c), counts and flags (0x44, 0x50, 0x58, 0x5c, 0x74). The count
  at 0x58 bounds the lanes that run at all;
- by vector buffer loads of 8 bytes at computed byte addresses: each section
  offset read above plus 8 bytes per lane.

The layout (a header holding its own section offsets, then 8-byte records
addressed in bytes) is a raw buffer, which the console compiler reads with
scalar loads where the address is uniform. The title made a raw buffer view
(SRV) in that slot.

### 5.2 What the translation does

`Lower::smem` (lower/smem.rs) translates a scalar buffer load by the kind of
the descriptor's used-resource entry: an SRV entry gives a raw buffer
handle, anything else a constant buffer handle. A descriptor loaded from the
heap at an index read at run time has no entry, so the scalar loads become
`cbufferLoadLegacy` through a handle annotated as a constant buffer, while
the vector loads of the same descriptor become `rawBufferLoad` through a
handle annotated as a raw buffer. Both handles come from
`createHandleFromHeap` with the same index.

In each of the three programs exactly one heap index is annotated both ways
(the descriptor at dword 0x120 of the element in all three); every other
handle has one kind. The DXIL validator accepts it: the kinds of heap
handles are not checked statically.

D3D12 leaves the access undefined when a handle's kind is not the kind of
the view in the slot. The GPU reads the slot's 32 bytes as a constant buffer
view (address and size) although they hold a raw buffer view, or a null one.

### 5.3 What the host holds in that slot

The host heap keeps the title's order: slot i is made from what the title
wrote at slot i (`descriptor/host.rs`). `HostHeap::mirror` makes buffer
views only for draws, which also copy their data. A title write of a buffer
view (`for_draw` false) leaves the slot as a null view of the same kind
(`later`, then `null_for`). A slot reached only by an index the program
loads at run time is never read through a table, so it stays that null
view: producers' "mirror zeros" (task 0b01ce59, #704), which r4tedx's branch
`gpu/run-time-indexed-views` addresses. It holds a real view when another
draw read the same slot through a table since the title last wrote it.

So on the host:

- the raw reads of the slot read zeros, which a null view gives: the lane
  count is then 0 and the program does nothing, harmlessly;
- the constant buffer reads of the slot are undefined, and what they read
  depends on whether the slot holds a null or a real raw view at that
  moment. That fits a hang that comes after minutes, not on the first frame.

The console reads the title's buffer at that slot, and the program updates
its points.

## 6. What was ruled out

- **FidelityFX.** The three programs are records of the title's shader pack,
  not of the FSR4 library. Sessions without the library lost the GPU too:
  `ffx2-hle2`, and ffx2's third session (task 7ffe5da3). ffx2 found that
  neither the upscale nor the unordered-access clears are needed.
- **Loop limits.** §4: no loop of these programs can run long enough.
- **Heap indices past the host heap.** The host view heap is capped at
  1,000,000 slots (the RTX 4080 SUPER's limit): it mirrors 737,856 of the
  title's 1,245,184 and keeps 262,144 dynamic slots after them. A title slot
  from 737,856 up would read a dynamic slot, one from 1,000,000 up past the
  heap, and translated shaders do not bound indices they compute from data
  (VIctory #483 is not ported). But no session ever logged "title slot past
  the host heap", which `mirror` writes when the title writes a view there:
  the title made no view that high. It stays a risk, not this cause.
  Proposed task: port VIctory #483 (bound every heap index a translated
  shader computes from data, with a null guard slot past the mirrored
  slots), and log the first title slot at or past the mirrored count that a
  descriptor copy reaches, since copies of descriptor bytes do not go
  through `mirror`.
- **Memory faults through the other views.** The other handles of the three
  programs have the kind of their table entries; out-of-range reads and
  writes through them return zero or are dropped.

## 7. The fix

`Lower::smem` (lower/smem.rs) picks the handle of a scalar buffer load from
the view the slot holds:

- the table entry says SRV: a raw buffer handle, as before;
- the table entry says UAV: a raw unordered-access handle (it was a
  constant-buffer handle over the UAV, the same defect through a table);
- no entry says (a descriptor loaded from the heap at an index the program
  read, or a table offset past the entries), and every vector buffer access
  of the same descriptor, or of a twin load of its dwords, reads it raw: the
  raw handle those accesses take (`Lower::raw_kind_of_vector_accesses`),
  unordered-access where one of them writes;
- otherwise (a constant-buffer entry, a root descriptor, no vector access,
  or one that is typed, by index or into LDS): a constant-buffer handle, as
  before.

In the three programs the scalar loads of the slot now read it through the
same raw handle as their vector loads.

Tests (lower/smem.rs, on WARP):

- `a_heap_slot_read_by_scalar_and_vector_loads_is_read_raw_both_ways`: a
  program of the three programs' shape loads a descriptor from the heap at
  an index it reads from its table, reads it with a scalar load and with
  vector loads, and stores their sums; the slot holds a raw view of known
  data;
- `a_uav_entry_read_by_a_scalar_load_is_read_raw`: every lane stores 7 into
  a UAV of the table, reads it back with a scalar load and stores it plus
  its lane number.

Without the change, **WARP removes its device** on both dispatches ("The
GPU device instance has been suspended"): the constant-buffer handle over a
raw view is enough to lose a device, on the software adapter too. With it,
both store what the console would.

A trade-off, marked as a shortcut in the code: with no entry, a slot that
held a constant buffer view and that the program also read per lane (a
dynamically indexed array) is now read raw by its scalar loads too. Its
vector loads already read it raw (`buffer.rs`), so the slot had two kinds
before and has one now; the raw view is what such descriptors are taken to
be everywhere else in the translator.

What it does not change:

- the slot still holds a null view on the host (§5.3), so the three
  programs read zeros and do nothing; the title's data there needs views
  for slots reached at run time (r4tedx's `gpu/run-time-indexed-views`);
- `buffer.rs` gives the loads of a descriptor with no entry a raw SRV
  handle and its stores a raw UAV handle: a program that both reads and
  writes such a descriptor by vector accesses still names the slot with two
  kinds (proposed for the owner of `buffer.rs`);
- the dispatch text of the DRED report (`op_text`, draw.rs) still labels
  root constants as heap slots; proposed for the owner of draw.rs: label
  only the user data of a table or heap parameter.

Sessions (Series S, build cf8ac38, `XTP_SKIP_INTRO=1`,
`XTP_WATCHDOG_HOOK=1`, autopilot and golden data, `session.ps1 -Measure 310
-Limit 420`, one at a time under `measure.lock`):

| Session | DRED | World | Ended | Time in the world | Device removed | Median frame |
| --- | --- | --- | --- | --- | --- | --- |
| `stall-hang-fix-dred` | on | 19:59:53 | 20:05:02, the autopilot's end | 5 min 9 s | no | 989 ms (1.0 fps) |
| `stall-hang-fix-nodred` | off | 20:08:04 | 20:13:14, the autopilot's end | 5 min 10 s | no | 189 ms (5.3 fps) |

Neither session logged a wait for the host GPU. The three programs were
translated by this build (a new cache key) and their pipelines made before
the world.

How much the sessions prove, read with the others:

- Before: eight Series S sessions from 18:13 to 19:23 lost the device 40 s
  to 2.5 min after the program first appeared in the log (§2).
- The four sessions after that, on builds without this change
  (`drops-fix-before`, `drops-fix-after`, `gpu-prof-B2-dred`, `mesh64-c`),
  ran 2 to 2.5 min in the world without a removal. Main changed meanwhile:
  for instance 9ab3e98, where DirectStorage writes open the guards of every
  view of a page.
- So two clean sessions of 5 min agree with the cause found, without proving
  it alone. The direct evidence is the DRED report (§2), the defect in the
  three programs (§5) and WARP losing its device on that shape (§7). A
  control session of main without the change, run for 5 min or more, would
  tell the rest.

## 8. Upstream

- **VIctory #1015** (justrealyme, open, `b8cfe8ce7`): the same rule as §7
  for a descriptor with no entry, reached independently. Its survey of the
  pack: 38,829 scalar loads in about 1,900 programs change kind, and the
  emitted and validated counts stay the same. On an RTX 5070 Ti it did not
  fix that tree's world hang (#578, another program), and that tree ran
  18.7 min without it too: how often the mismatch removes an NVIDIA device
  depends on what the slot holds.
- **VIctory #1055** (adamdotpy, open, `21d1b4682`): a table's UAV read by a
  scalar load goes through a raw UAV handle, as in §7; its WARP test also
  removed the device before the change.
- **VIctory #1409** (icdmkg2, open, `de109f1d3`): reads every descriptor
  loaded from the resource heap raw in scalar loads, whatever its uses. Not
  taken: XTP's host heap makes a real CBV for a constant buffer view of the
  title (`make`), so a heap constant buffer that only scalar loads read
  would get the wrong kind instead.
- VIctory #1322 (the "downsample" that looped 4 billion times on 0 sizes)
  does not apply: these programs' bounds come from data the host has.
- VIctory #1483 (the loop limit's conditional back edges) does not apply:
  no loop of these programs reaches a limit (§4).
- VIctory #483 bounds heap indices computed from data (§6).
