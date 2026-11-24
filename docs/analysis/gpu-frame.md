# The Series S world frame on the GPU: passes, texture uploads, waiting

Where the GPU time of a Series S world frame goes, why the frame line
showed 107–129 MB of "textures" per frame, how much of the waiting is the
present's `drain`, and what was fixed here. Measured on the person's PC
(RTX 4080 SUPER 16 GB, 32-lane waves; Ryzen 9 7950X3D, 32 logical
processors; Windows 11 26300) on 2026-11-24, in Series S mode
(`XTP_SKIP_INTRO=1`, `XTP_WATCHDOG_HOOK=1`, autopilot from the golden save,
`--measure 120`). The title is BTP 6; programs are named by their
translated shaders' hashes. Times are UTC. Logs are under
`E:\xtp-data\` (§9), none in the repository.

## 1. Summary

- **The GPU works about a third of the frame.** GPU timestamps
  (`XTP_GPU_TIMESTAMPS=1`, new, §3.1) put the world frame's GPU work at 91 ms
  of a 240 ms frame in session A (76–91 ms in A, A2 and B3), and the GPU idle
  for 123–143 ms between submissions. The frame line's "on the GPU"
  (94–111 ms) adds the completion thread's wake-ups to it.
- **Compute is two thirds of the GPU's work, and one program a sixth of
  it**: dispatches 38–48 ms a frame, of which `compute 3a79195f06d3ed85`
  13–16 ms (29–40 dispatches of 290–340 µs). Wave64 programs, run as two
  32-lane waves, take 13–17 ms in about 1 900 dispatches: they do not
  dominate. Draws cost under 1 ms: the 3D is still mostly dropped.
- **XTP's own passes**: the argument clamp before each indirect execution
  takes 7–9 ms a frame for about 3 600 clamps, three quarters of them
  before executions that are dropped right after; the rectangle passes
  1.1–1.4 ms.
- **The "107–129 MB of textures" were hashes, not uploads.** 160 to 200
  textures in physical-page views were hashed whole at every submission
  that sampled them (100–130 MB, 23 ms a frame), for 1 to 4 changes a frame.
  On the GPU, every list that sampled one copied its whole staging buffer
  into the host copy: about 300 copies and 290 MB a frame across PCIe,
  12.4–12.8 ms of GPU, all but 1 to 4 of them redundant.
- **Fixed here** (`texture.rs`): textures in views are guarded like buffer
  copies (sound since ds-guards, dac3601) and hashed only after a write;
  each list's copy runs under a predicate set at submission, so only the
  first list after a change copies. Same binary, before (A2) and after
  (B3): textures 23.6 → **0.8 ms** and 121 → **0.0 MB** hashed a frame,
  staging copies 309 → **2** run, their GPU time 12.8 → **1.8 ms**. In session V every check the guards found clean was hashed too: none
  of about 61 000 had changed.
  The frame itself does not get shorter in these runs (213 and 227 ms;
  sessions differ by ±40 ms): the time saved comes back as waiting, behind
  the chain below.
- **The present's drain is 0.1–0.3 % of the waiting.** What the title
  threads wait for (109–121 ms a frame) is, for 78–80 %, a submission's
  steps waiting for the submission before it to have run and been copied
  back, because both touch the same title memory: preparing (91–101 ms),
  GPU (76–91 ms) and copy-back (52–75 ms) run one after another, and that
  chain is the frame. The title's page-mapping copies, which drain every
  submission, are 12–17 %.
- **Top costs and owners** (§8): the submission chain (copyback, then one
  frame in flight), checking 7.8 GB of title memory a frame for 25 MB of
  refreshes (G16, copyback), copy-back (copyback), `compute
  3a79195f06d3ed85` (translator), the textures (done here), the clamp before
  dropped executions (drops-fix), the page-mapping drains (`queue.rs`).
- **Two device removals** (B, fix on; A2, fix off; same binary) match the
  DEVICE_HUNG under investigation elsewhere; the fix does not cause them
  (§2).

## 2. Sessions

All on this task's branch after main 32886d8 was merged into it (so with
ds-guards, dac3601), diagnostics builds, through
`E:\tools\stall-probe\session.ps1` under `measure.lock`, one at a time.
The fix is off with `XTP_TEXTURE_PREDICATION=0` and `XTP_TEXTURE_GUARDS=0`.

| Session | Play start | Binary (sha256) | Fix | Also | World | End |
|---|---|---|---|---|---|---|
| A | 19:14:11 | 8e4b0d2 + waits patch, first version (`25d31979…`) | off | timestamps | +34 s, 120 s | exit 0 |
| B | 19:22:55 | 3ab561a + waits patch (`831d68de…`) | on | timestamps | +24 s | **GPU removed** 19:23:44, after about 25 s (0x887A0005, DEVICE_HUNG) |
| B2 | 19:27:05 | as B | on | `XTP_D3D12_DRED=1` (0.6 fps) | +36 s, 120 s | exit 0 |
| A2 | 19:31:36 | as B | off | timestamps | +23 s | **GPU removed** 19:33:40, after about 100 s (same) |
| B3 | 19:39:55 | as B | on | timestamps | +22 s, 120 s | exit 0 |
| V | 19:48:41 | 5fefd17 + waits patch (`74cc9b94…`) | on, `XTP_TEXTURE_GUARDS=verify` | no timestamps | about 2 min | stopped by me at 19:51:21 to leave the PC to another task (no removal) |

- The two device removals have the message of the removals under
  investigation in other tasks' sessions of the same evening
  (stall-samuel-main-79fa391, stall-ds-guards-after, stall-copyback-B-fix:
  "removed after a submission of 0 lists (29–32 lists in flight); no
  breadcrumbs; DEVICE_HUNG"): one with the fix (B) and one without (A2), on
  the same binary, while B2 and B3 (fix on) and A (off) ran their 120 s.
  The fix does not cause them. In B the last submission read ended at
  19:23:39.56 and the next one ran for more than 4.8 s, just after new pixel
  pipelines were made.
- B3's `session.ps1` waited behind other sessions and hit its 230 s hard
  stop before its play started (the play then ran alone, 19:39:55–19:42:18):
  its sampler counters are missing, its log is complete. That hard stop
  killed another task's run (`Get-Process -Name xtp | Stop-Process`):
  reported on that task; V ran with a 600 s stop.
- The reference is Samuel's session on main 79fa391
  (`E:\xtp-data\cache\play\20261124-181140`): 275–333 ms a frame, textures
  23–29 ms for 107–129 MB, "on the GPU" 125–181 ms.

## 3. What was added to measure

### 3.1 GPU timestamps (`XTP_GPU_TIMESTAMPS=1`)

`crates/xtp-gpu/src/timestamps.rs`, off by default. A host list's recording
gets a timestamp when it opens (a reset; a list made and not reset yet
starts at its first mark), one before and one after each marked command,
and one when it closes, which resolves them into a read-back buffer at the
list's end. Marked commands (lines added in `draw.rs` and `texture.rs`):
dispatches, mesh dispatches, draws, indexed draws, indirect executions by
argument kind, XTP's argument clamp before each indirect execution, the
rectangle expansion passes and their indirect draw, and texture staging
copies. The time from a timestamp to the next goes to what the first one
starts; what a list records outside marked commands (buffer copies,
barriers, clears) is "between commands". Once a submission has finished
(`Host::after`, after its copy-back), its timestamps are read and summed per
kind, per program (the host pipeline's name, its translated shaders'
hashes, wave64 programs flagged) and per submission. A submission's first
and last timestamps are put on the CPU's clock (`GetClockCalibration`,
again every second): when the GPU started and ended it after
`ExecuteCommandLists` returned, how long the GPU sat idle between two
submissions, and when the submission was read after it ended. Every
`XTP_FRAME_LINE` seconds (5) the sums are logged per frame; with
`xtp_gpu::timestamps=debug`, one line per submission.

Timestamps come from one query heap and one read-back buffer of 512 Ki
entries, in chunks of 256 that a recording takes as it goes and gives back
once the list was reset and every submission of the old recording was read.
Host hooks, lines added only: `Host::run_as` after the submission joins the
timeline, `HostList::close` before `Close`, `HostList::reset` after
`Reset`.

WARP tests (`timestamps::tests`): a title list with two marked dispatches
is timed end to end through the title's queue (spans labelled in order,
adding up to the list, the kernel's writes really there); a list reset
after its submission is timed again from the reset; off, nothing is
recorded; the chunk pool and the line's ranking without a GPU.

What timestamps cost: not measurable against the differences between sessions. A world
frame takes about 14 000 timestamps (two per marked command, 7 000 spans
"between commands") and one resolve per 256 of them at each list's end. The
frame line's "on the GPU" was 103 ms in B3 (timestamps on) and 121 ms in V
(off, same fix, a slower session); the timestamps' own spans add up to the
lists' work, which they cannot time without themselves.

### 3.2 Texture counters (diagnostics)

Every 5 s, `texture.rs` logs per frame the textures checked, how often
(and how often again in a submission that had checked them), how many its
guards found clean, the MB hashed, the changes found, and the staging copies
recorded, run and skipped (MB), then the twelve textures that took most
hashing.

### 3.3 Waiting by cause (local patch, not delivered)

`E:\xtp-data\runs\gpu-prof\waits.patch` splits `Host::waiting` (the frame
line's "waiting for them") by caller: the present's `drain`; at submission,
finishing what has run, a list still running and the in-flight limit; a
step's wait for an earlier submission that writes or reads the same title
memory (`Batch::settle*`: copies, refreshes, GPU writes, textures); a fence
wait with a null event; the title's page-mapping copies
(`copy_page_mappings`, which drains); list drops. In session A its sums
covered 5 s but were divided by the last second's frames: A's shares are
right, its milliseconds are not (§6). Fixed for the other sessions.

## 4. GPU time per pass and per program

Session A (before the fix; timestamps on), 22 windows of 5 s in the world,
13 to 32 frames each, medians per frame (lowest to highest window in
parentheses):

| | ms/frame |
|---|---:|
| frame (the 5 s windows) | 240.5 (156–385) |
| frame line "on the GPU" (its 1 s windows) | 111 (43–178) |
| GPU timestamps: the lists' work | **91.2** (39–152) |
| the title's submissions, first to last timestamp | 92.2, in 33.7 submissions |
| GPU idle, or running what is not timed, between submissions | **143.2** (107–259) |
| a title submission: from `ExecuteCommandLists` returning to its first timestamp / to its last / read after its last | 1.5 / 4.4 (at most 51) / 3.1 |

So **the GPU works for about a third of the frame** (35 %; A2 39 %, B3
36 %) and waits for the CPU the rest of it. The frame line's "on the GPU"
counts each submission from its submission (or the end of the one before)
until the completion thread sees it done, so it adds that thread's wake-up
after each of the 34 submissions of a frame to the GPU's work. A
submission is read 2–6 ms after its last timestamp: its copy-back runs
first (`Host::after` signals follow it). Of the title's submissions, half
run in under 0.2 ms on the GPU; 6.5 % run 10 ms or more and take 57 % of
the GPU time (at most 101 ms).

### 4.1 By kind

| Kind (session A) | ms/frame | per frame | µs each |
|---|---:|---:|---:|
| dispatch | **47.3** | 1 289 | 37 |
| texture upload (staging copies) | **12.4** | 278 | 45 |
| between commands (copies, barriers, clears) | 10.1 | 7 086 spans | |
| indirect argument clamp (XTP's) | **8.7** | 3 655 | 2.4 |
| indirect dispatch | 7.5 | 958 | 8 |
| rectangle expansion (XTP's) | 0.9 | 249 | 3.6 |
| draw | 0.5 | 125 | 4 |
| indirect rectangles | 0.2 | 249 | 0.8 |
| indirect indexed draw, indexed draw, indirect draw | 0.2 | 80 | |
| of the programs, wave64 ones (two 32-lane waves) | 15.4 | 1 881 | 8 |

Draws cost next to nothing: the world draws a few hundred, and most of the
3D is still dropped (wave64 mesh programs, drops.md). Compute is two thirds
of the GPU's work.

### 4.2 By program

Mean ms per frame over the 22 windows (programs by their translated
shaders' hashes):

| Program | kind | ms/frame | per frame | µs each |
|---|---|---:|---:|---:|
| `compute 3a79195f06d3ed85` | dispatch | **16.2** | 39.6 | 290 |
| `compute 512416d975ace48b` | dispatch | 4.2 | 17.0 | 189 |
| `compute ebe4e53f031f5249` | dispatch | 2.9 | 10.8 | 310 |
| `compute 12eae112b559091c` | dispatch | 2.3 | 31.3 | 44 |
| `compute 53af89c0e1dccd2a` | dispatch | 1.7 | 43.3 | 94 |
| `compute 7c90a988167650a1` (wave64) | dispatch | 1.7 | 1.0 | 1 658 |
| `compute a1d865bdfb3559eb` | dispatch | 1.5 | 7.6 | 169 |
| `compute 42683eebec081520` (wave64) | dispatch | 1.5 | 1.0 | 1 607 |
| `compute aaf7da7a94dc72a9` (wave64) | dispatch | 1.4 | 8.4 | 188 |
| `compute 8bbcbda6008eaa37` | indirect dispatch | 1.3 | 1.0 | 1 242 |
| `compute 776d835da5ffa646` | indirect dispatch | 1.2 | 1.0 | 1 518 |
| `compute 660814153af43185` (wave64) | indirect dispatch | 1.1 | 87.4 | 12 |
| `compute f8f50ab85e7b10bb` (wave64) | indirect dispatch | 1.0 | 87.4 | 11 |

- **One program dominates**: `compute 3a79195f06d3ed85` takes a third of
  the dispatch time and a sixth of the GPU's work. It is not a wave64
  program. Its 290 µs per dispatch (807 µs in session B, a different spot
  of the world) is the translator's question: what it loops over, and
  whether a loop limit or a spin (`emit/lower/spin.rs`) runs long.
- **Wave64 compute does not dominate**: the programs written for 64-lane
  waves, run as two 32-lane waves with the console flow and LDS exchange,
  take 15.4 ms a frame in 1 881 dispatches. A few are slow per dispatch
  (`7c90a988167650a1` and `42683eebec081520`, 1.6 ms each, once a frame).
- **XTP's own clamp costs 8.7 ms**: one compute dispatch between two UAV
  barriers before every indirect execution (`indirect::clamp_uav`), 3 655 a
  frame. The executions that run are about 1 000 a frame (958 indirect
  dispatches and about 40 draws; the rectangle lists clamp inside their own
  expansion). **About 2 650 clamps a frame, 73 %, are for executions that
  are dropped right after**: `State::execute_indirect` (`draw.rs`) clamps
  before `prepare`, which then finds no host pipeline (wave64 mesh programs
  refused, pipelines being made). Each clamp's leading UAV barrier also
  waits for all the work before it.

## 5. The texture "uploads"

### 5.1 What the 107–129 MB are

The frame line's "textures N ms M MB" is the time and the **bytes of title
memory read** to bring the staging buffers of sampled textures up to date
(`Staging::refresh`), not bytes sent to the GPU. Session A's copystat and
texture lines split them:

| Per frame (session A, 22 windows) | median (range) |
|---|---:|
| frame line "textures" | 22.9 ms (19.5–30.5), 107.5 MB (90.4–131.0) |
| textures checked | 162 distinct (142–182), 219 checks (183–276) |
| of the checks, again in a submission that had checked the texture | 0 |
| MB hashed (blake3 of the whole texture memory) | 107 (92–121) |
| MB read from write-watched memory | 0.1–0.4 |
| changes found (detilings) | 1.5 (1.0–3.9) |
| **staging copies run on the GPU** | **278 (219–329), 280 MB (200–291)** |
| their GPU time (timestamps) | **12.4 ms** (9.7–13.4) |

- Nearly every texture the world samples lies in **physical-page views**
  (xmem's sections: the title's texture pool), which cannot be
  write-watched. Until now each was **hashed whole at every submission that
  sampled it**, `watch::writes(.., false)` never guarding them. A texture
  is sampled in 1 to 2 of the frame's 33 submissions on average (5.5 times
  for the most hashed one, a 1536x1748 16-bit texture of 5.4 MB: 30 MB of
  hashing a frame). **About 1 % of what is hashed changed**: 1 to 4
  detilings a frame. The rest, 105 MB and 22 ms a frame of the preparing
  thread, is redundant.
- On the GPU, **every list that sampled such a texture recorded the copy of
  its whole staging buffer into the host copy**, changed or not, from an
  upload heap (system memory) across PCIe: 278 copies and 280 MB a frame,
  45 µs each (about 22 GB/s), 12.4 ms of GPU time and the copy-destination
  round trip of every such texture in every list. **All but 1 to 4 of them
  copy what the host copy already holds.**
- Not the causes: GPU-written targets (host-written textures keep their
  host contents, `texture::host_written`), streaming (1 to 4 changes a
  frame) or false dirtiness from page-granular watches (the textures in
  views were not watched at all; the watched ones read 0.1–0.4 MB a frame).
- The textures hashed most: 1536x1748 16-bit (5.4 MB, 5.5 checks a frame),
  three BC 2048x2048 or 1024x2048 textures of 2.8 MB and a 128x128x128 BC
  volume of 2.8 MB (2 to 3 checks each), a 1280x720 32-bit texture of
  3.7 MB (7.4 MB staged: its host format is a wider stand-in), and about 150
  smaller ones.

### 5.2 Why every check was a hash

A batch checks a texture at most once per submission and again after a
step that writes title memory; the counters show no texture checked twice
in one submission in any session, though the title makes 4 000 CPU copies
of title memory a frame. So the 107–129 MB
are one hash per texture per submission that samples it: the cost came from
hashing at all, not from checking too often. Hashing was the only way to
see a change in a view while DirectStorage wrote pages through the
runtime's own view, which no guard saw; since dac3601 (ds-guards) xmem
opens the guards of every view of the pages it writes, so the guards that
buffer copies already relied on can answer for textures too (§7).

## 6. Waiting, and the present's drain

"Waiting for them" is the time title threads spend in `Host::waiting`,
summed over threads. Split by caller (§3.3), session A, shares of each
window's waiting, median of 21 windows (lowest to highest):

| Caller | share of the waiting |
|---|---:|
| a step waits for an earlier submission that writes or reads the same title memory (`Batch::settle*`) | **77.9 %** (69.7–87.9) |
| the title's page-mapping copies drain every submission (`queue::copy_page_mappings`) | **17.4 %** (5.1–25.7) |
| at submission, finishing what has run (`finish_reached`: copy-back of the submissions the GPU finished) | 3.2 % (0.4–14.1) |
| **the present's `drain`** | **0.3 %** (0.0–1.7) |
| a list still running, the in-flight limit, fence waits with a null event, list drops | 0 |

In milliseconds, session A2 (same binary as B and B3, the patch's windows
corrected, the fix off), medians of 17 world windows: waiting 109.4 ms a
frame (40.8–154.4), of which title memory conflicts **87.3** (35.5–129.8),
page-mapping drains **13.7** (2.0–32.6), finishing at submission 1.3, and
**the present's drain 0.1** (0.0–1.5).

- **The present's drain costs almost nothing**: by the time the title
  presents, its submissions have run (the GPU is idle 60 % of the frame,
  §4). The "shortcut" in `Host::present` is not what holds the frame.
- **What holds it is the order between submissions**: a submission's steps
  (title memory copies, refreshes of host copies, GPU write snapshots,
  texture checks) wait until the earlier submissions whose GPU work writes
  or reads the same title memory have run *and* been copied back
  (`Timeline::conflict` and its kin). In the world nearly every submission
  touches what the one before wrote (the title's big heaps), so the three
  stages run one after another: the CPU prepares submission k+1 only once
  the GPU has run k and the completion thread has copied its writes back.
  Per frame (A): preparing 101 ms + GPU 91 ms + copying back 68 ms ≈ the
  240–252 ms frame. The overlap the frame line shows (37 ms) is what little
  runs beside.
- The page-mapping copies (`copy_page_mappings`, the title's tiled-heap
  updates) drain every submission in flight before they map: 17 % of the
  waiting.

## 7. What was fixed: texture checks and copies (`texture.rs`)

### 7.1 What changed

- **Checks** (`Staging::refresh_now`): texture memory in views is now
  guarded as the host copies of buffers are (`watch::writes` with
  `guarded`). A check whose blocks no one wrote since the last one is
  answered by the guards, without a hash; a written block (or one that
  cannot be guarded, which every asking reports written) still takes the
  hash, and a detiling if it changed. This is sound now that every writer of
  view memory opens the guards: the title's CPU faults into the handler,
  XTP's own writes and the kernel's reads call `watch::writing`, and since
  dac3601 (ds-guards) xmem opens them after DirectStorage's page writes,
  before the request completes. `XTP_TEXTURE_GUARDS=0` hashes at every
  check as before; `XTP_TEXTURE_GUARDS=verify` (diagnostics) also hashes
  what the guards find clean and counts a change they missed (§7.3).
- **Copies** (`texture::upload`): each list still records the copy of the
  staging buffer into the host copy, now between `SetPredication` on a
  predicate word of its own (64 Ki words of one upload buffer, held until
  the list's recording goes) and `SetPredication(None)`. At submission a
  step that follows the staging buffer's refresh decides: the copy runs
  only if the host copy has not got the staging buffer's current contents
  (`Staging::copy_due`: a count of detilings, and the count and the host
  copy, held weakly, the last copy gave). So the first list after a change
  copies and the others' copies are skipped by the GPU. A list named twice
  in one submission reads one word: a second decision there keeps the copy
  the first let run. Without a free word, or with
  `XTP_TEXTURE_PREDICATION=0`, the copy runs as before.
- Tests (WARP, `texture::tests`): a texture copied by the first of two
  lists and not the second, then not at all while its memory does not
  change — a staging buffer overwritten behind its back does not reach the
  host copy, so the GPU really skipped the copy (the test fails with the
  predicate always set) — and copied again after a change; a texture in a
  real section view not hashed nor copied without a write, then hashed,
  detiled and copied after a CPU write and after a write through another
  view that the writer reports; a copy into another host copy is due; a
  list named twice in one submission copies in both runs (fails without
  the rule above).

### 7.2 Before and after

Sessions A2 (fix off) and B3 (fix on), the same binary, medians of the
world windows (lowest to highest):

| Per frame | A2: before | B3: after |
|---|---:|---:|
| frame line "textures" (CPU, preparing) | 23.6 ms (20.4–27.1), 125 MB | **0.8 ms** (0.6–3.2), **0.1 MB** (0.1–6.6) |
| textures checked, checks | 183, 257 | 205, 263 |
| of the checks, found clean by their guards (no hash) | 0 | **218** (190–247) |
| MB hashed | 121 (100–130) | **0.0** (0.0–5.5) |
| changes found | 1.4 (1.0–4.6) | 1.5 (1.0–4.3) |
| staging copies run | 309, 291 MB | **2** (1–4), 0.3 MB (0.1–5.8) |
| staging copies skipped by the GPU | 0 | 322, 293 MB |
| GPU time of the texture uploads (timestamps) | 12.8 ms (11.3–15.1) | **1.8 ms** (1.4–2.5) |
| guard protection changes, guard faults, guard time | 1 129, 202, 5.2 ms | 1 450, 220, 5.4 ms |
| frame | 213 ms (111–250) | 227 ms (122–323) |
| waiting | 109 ms | 121 ms |

B2 (fix on, 120 s of world, DRED on) agrees: 0.1 MB hashed (0.0–4.8), 2
copies run (1–22) for 300 skipped.

- **The texture work is gone**: 23 ms of the preparing thread and 120 MB of
  hashing a frame, 11 ms of GPU and 290 MB of PCIe reads. Each change is
  copied once (copies run = changes found), and what is left is the
  per-list copy-destination round trip and the skipped copy, 1.8 ms of GPU
  for about 320 recordings a frame (about 5.6 µs each; see #1500, §7.4).
- **The frame does not get shorter in these runs.** Frames differ by
  ±40 ms from one session to the next (A 252, A2 213 ms, both before), more
  than the fix's share, and the frame is bound by the chain of §6: the
  checks ran while their submission was about to wait for the one before,
  so most of the time they took comes back as waiting (109 → 121 ms) until
  that chain is cut (§8 #1). The guards cost about 300 more protection
  changes and 20 more guard faults a frame.


### 7.3 Are the guards enough?

Session V ran the fix with `XTP_TEXTURE_GUARDS=verify`: every check the
guards found clean was hashed as well and compared with the hash of the
memory last detiled. Over 29 windows (19:48:52–19:51:19, about 2 minutes
of the world and the load before it), about 77 000 checks, of which about
61 000 were found clean by the guards: **no change was missed** (the
counter stayed at 0, no warning). Changes found otherwise: 1.0–1.7 a frame,
each copied once. The writers of view memory this covers are those of
BTP 6's world in these 2 minutes; a writer that opens no guard would be
missed (watch.rs's shortcuts: the title's own views of sections, its calls
of the memory API set that bypass xmem, system calls into guarded blocks).
`verify` keeps that checkable in any session.

### 7.4 VIctory #1500, compared

VIctory #1500 (A6thGraderFr, open, not run on Windows nor the title) does
the same two things another way:

| | #1500 | here |
|---|---|---|
| when a copy runs | once per change, in one list of its own run before the submission's lists (`Host::record_uploads`), with the textures moved to the copy destination there | once per change, by the first list after it: every list records its copy under a predicate, set at submission |
| what stays per list | nothing: lists record no copy and no copy-destination round trip | the copy-destination round trip, `SetPredication` twice and the skipped copy: 1.8 ms of GPU a frame for about 320 recordings (B3) |
| bookkeeping | detilings counted per staging buffer, the count the host copy last got | the same, plus which host copy got it |
| a list named twice | handled | handled, after reading #1500 (credited in the code) |
| textures in views | still hashed whole at every check, on up to 8 threads (one per 4 MiB) at the batch's refresh points | not hashed unless a guard reports a write (ds-guards made that sound in XTP) |
| files | `host.rs` (`Batch`, `run_as`, `Step::Texture`), `texture.rs` | `texture.rs` only |

#1500's layout saves the per-list round trip and the skipped copies, about
1.8 ms of GPU a frame here, but needs `run_as` and `Batch` reordered in
`host.rs`; worth taking with credit when `host.rs` is free (the
one-frame-in-flight task reworks `run_as`). Its threaded hashing is not
needed here: what is left to hash is 0–5 MB a frame.

## 8. Top costs, fixes and owners

Per Series S world frame, session A (before) unless said; "after" is the
fix of §7.

| # | Cost | Measured | Fix | Owner |
|---|---|---|---|---|
| 1 | **Preparing, GPU and copy-back run one after another**: a submission's steps wait for the earlier submission that wrote the same title memory to have run and been copied back | 78 % of the waiting (87 ms a frame in A2); the frame ≈ preparing 101 + GPU 91 + copy-back 68 ms, the GPU idle 60 % of the frame | Wait only for what conflicts, and only for its GPU completion: copy back the conflicting ranges on demand before the step that reads them (the rest on the completion thread), and skip refreshing a range whose host copy the earlier submission's GPU wrote and the title did not touch since (`settle_refresh` already does it for refreshes only). Then let the next submission's preparation overlap the GPU. One frame in flight alone gains little: the present's drain is 0.1–0.3 % of the waiting | **copyback 78d58a42** (`shadow.rs`, `host/timeline.rs`), then **one frame in flight f1d37c5d** (`host.rs`) |
| 2 | **Checking title memory for host-copy refreshes** | "copies" 81 ms a frame (30–176): of it, copying 25 MB takes 1.8 ms; the rest is checking 7.8 GB of ranges a frame (5 700 refreshes, 1 700 updates; watches 8.6 ms, guards 5.8 ms, 56 000–76 000 page faults) and 4 400 title copies of 3.2 MB | Cut the volume checked: small CPU-written data read in place from imported heaps (G16), refreshes merged per frame (W1, landed) and per page | **G16 ce8bcd0a** (`uma.rs`, `shadow.rs`), **copyback 78d58a42** |
| 3 | **Copy-back** of GPU-written ranges into title memory | 68 ms a frame (33–109) for 1–6 MB | The three-way merge of AnyPS5 pick 4 (write back only bytes the GPU changed and the title did not), already the copyback task's | **copyback 78d58a42** |
| 4 | **One compute program**, `compute 3a79195f06d3ed85` | 16.2 ms of the 91 ms of GPU work (40 dispatches of 290 µs; 807 µs in session B) | Find what it loops on in its translation (loop limits, spins, a lane mask taken as data) | **translator**: loop-limits 35e2aa06, or a new task |
| 5 | **Textures hashed and copied every time** | CPU 22.9 ms and 107 MB hashed a frame for 1–4 changes; GPU 12.4 ms, 278 copies of 280 MB | Guards instead of hashes for textures in views; each list's copy predicated, run only by the first list after a change | **done here** (§7): textures 23.6 → 0.8 ms of CPU, 12.8 → 1.8 ms of GPU |
| 6 | **XTP's indirect argument clamp** before executions that are then dropped | 8.7 ms of GPU a frame for 3 655 clamps, about 2 650 of them (73 %) before a drop; a UAV barrier before each | In `State::execute_indirect`, drop an execution whose pipeline is missing (refused, being made) before clamping: about 6 ms of GPU and 2 650 barriers a frame while the 3D is dropped | **drops-fix 06aada7c** (`draw.rs`) |
| 7 | **Page-mapping copies drain every submission** | 17 % of the waiting (14 ms a frame in A2) | Wait only for the submissions whose work touches the pages remapped (a timeline conflict on the range) | `queue.rs`: **ffx2 7ffe5da3** holds it now, or a follow-up |

## 9. Method

- Logs: `E:\xtp-data\cache\play\<folder>\run.log`, folders 20261124-191411,
  -192255, -192705, -193136, -193955 and -194841 for A, B, B2, A2, B3 and
  V, copied to `E:\xtp-data\runs\gpu-prof\{A,B,B2,A2,B3,V}-run.log`; samplers
  in `E:\xtp-data\runs\stall-gpu-prof-*`; binaries in
  `E:\xtp-data\runs\gpu-prof\bin\`.
- `E:\xtp-data\runs\gpu-prof\tools\gpuframe.py <run.log>` gives every number
  above: the frame lines with at least 100 draws ("world frames"), and the
  waiting, texture and timestamp lines logged in the same windows, as
  medians and ranges; with the per-submission debug lines
  (`XTP_LOG=info,xtp_gpu::timestamps=debug`), the submissions' spread.
- The waiting-by-cause patch (`E:\xtp-data\runs\gpu-prof\waits.patch`,
  §3.3) is the only change in the binaries outside the delivered code; it
  touches `host.rs`, `fence.rs` and `queue.rs` and is not delivered.
- VIctory #1500's diff: `E:\xtp-data\runs\gpu-prof\upstream\victory-1500.diff`
  (head 2340003).

