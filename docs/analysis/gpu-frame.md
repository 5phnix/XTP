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

§1 to §9 were measured before the 3D ran (about 125 draws a frame). §10
measures the 3D world with the lit scene (task N6, the 30 fps plan's step
7): the GPU frame by kind and by program in two sessions, the A/B of
`XTP_DXIL_OPT`, and the top costs with their causes and owners.

## 1. Summary

- **With the 3D and the lit scene (§10)** the GPU works 220–225 ms of a
  370–390 ms frame (two thirds of it), against a budget of 25 ms. XTP's
  indirect argument clamp is the largest single cost (54–59 ms a frame,
  3,000 clamps of 18 µs), then one mesh program whose pixel stage runs a
  light loop bounded by data (1 ms per execution for a few thousand
  pixels), then a family of vertex-transform compute programs (about
  24 ms). `XTP_DXIL_OPT=1` gives no measurable GPU gain on this GPU.
  These historical timings predate the structured-buffer fix. For the
  light loop with structured reads enabled, see the trip counts and
  per-command timings in §10.11.

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

## 10. The 3D world's GPU frame, by pass and program (N6)

Task N6 of `30fps.md` (§8 step 7, §9): the GPU timestamps again, now that
the world draws in 3D (wave64 mesh programs, mesh64) and its scene is lit
(worldlight), with each command's pipeline statistics, in two Series S
world sessions on `main`. The second turns on `XTP_DXIL_OPT=1` (dxilopt's
DXIL clean-up passes), so the pair is also dxilopt's A/B (§10.6).

### 10.1 Summary

- **The GPU works 220–225 ms of a 374–388 ms frame** (timestamps, means of
  the world windows), two thirds of it, against a budget of 25 ms. Before
  the 3D it was 76–91 ms of a 213–252 ms frame (§4). The frame is now
  closer to GPU-bound: the GPU is idle or running untimed work 97–110 ms a
  frame.
- **XTP's own argument clamp is the largest single cost**: 54–59 ms a
  frame, 2,900–3,300 clamps of 18–20 µs. Before the 3D each took 2.5 µs.
  Each runs one group of 64 threads between two global UAV barriers, with a
  switch to compute and back. About a quarter of them precede executions
  that are then dropped (§10.7, N6a).
- **One mesh program is the costliest program**: `[mesh ba8d52f4ea1a1bac,
  pixel b37565055f2f7d3b]`, 13–29 ms a frame, 1.0–1.1 ms per execution.
  The pipeline statistics show the work is tiny: 270 to 750 mesh
  invocations and 460 to 2,000 pixel invocations per execution. Its pixel
  stage is a
  light loop that leaves only when an index reaches a bound read from data
  (§10.7, N6b).
- **A family of vertex-transform compute programs** (32-thread, wave32,
  heavy on legacy multiplies and 16-byte loads) takes about 24 ms:
  `24f989b98dcb03f0`, `3a79195f06d3ed85`, `ebe4e53f031f5249`,
  `12eae112b559091c`, `512416d975ace48b` (N6c).
- **Many small mesh executions**: 1,400–1,700 indirect mesh dispatches a
  frame of 16–21 µs each, plus a clamp before each (N6a).
- **The pipeline statistics show no runaway draw** like VIctory #1723's
  (51 million pixels for 184 primitives). The frame shades 27 million
  pixels; the draws with the most pixels are full-screen passes of 0.9 to
  3.8 million pixels a primitive.
- **`XTP_DXIL_OPT=1` gives no measurable GPU gain here.** Per execution the
  translated programs got 3–10 % slower in B. XTP's own passes, which the
  switch does not touch, got 10 % slower too. Measured against those, the
  translated programs are about 2.5 % faster, well within what two
  sessions differ by (§10.6).
- **Owners**: N6a (`indirect.rs`, `draw.rs`), N6b (the translator's loop
  lowering, `emit/lower/flow.rs`, then the pass that writes the bound), N6c
  (the translator's VALU and buffer lowering). Each is a ready task in
  §10.8.

### 10.2 Sessions

| Session | Play (UTC) | World | Binary | Switches | End |
|---|---|---|---|---|---|
| **A** | 22:43:46–22:45:24 | from +32 s, 60 s | diag of `main` 99b6376 (worldlight and dxilopt in) with this task's pipeline statistics, the tree of 54a1b76; sha256 `369987b8…` | `XTP_DXIL_OPT=0` | exit 0, no device removal |
| **B** | 22:46:00–22:47:46 | from +39 s, 60 s | the same | `XTP_DXIL_OPT=1` (all passes, `passes=31` on the log) | exit 0, no device removal |

- Both: Series S, `XTP_SKIP_INTRO=1`, `XTP_WATCHDOG_HOOK=1`, autopilot
  from the golden save, `--measure 60` with a 150 s hard stop (the
  person's rule for short sessions), `XTP_GPU_TIMESTAMPS=1`,
  `XTP_GPU_PIPESTATS=1` (§10.3), `XTP_FRAME_LINE=5`,
  `XTP_LOG=info,xtp_gpu::timestamps=debug`. Run through
  `E:\tools\stall-probe\session.ps1` under `measure.lock`, one after the
  other in one slot of the PC, sound muted.
- World windows: the frame lines with at least 1,000 draws a frame, 9 per
  session, and the 10 timestamp windows logged after them (10 to 25 frames
  each).
- Pipelines being made: 208–261 (A) and 215–573 (B) throughout. Most are
  heavy compute pipelines whose indirect dispatches are dropped as "being
  made (heavy)": 392 a frame in A, 412 in B. Each binary translates every
  shader again (the shader cache keys on our executable and on
  `XTP_DXIL_OPT`), and no world window of either session has no pipeline
  being made: the A/B compares time per execution (§10.6).
- Not measured: the cloud L4 (its world load landed after these sessions),
  and a session without the statistics queries. The queries add one
  query per marked command: the frame line's "on the GPU" (294–318 ms
  here) cannot separate their cost from the lit scene's.

### 10.3 What was added to measure: pipeline statistics per command

`crates/xtp-gpu/src/pipestats.rs` (new, off by default):
`XTP_GPU_PIPESTATS=1` with `XTP_GPU_TIMESTAMPS=1`. Every command the
timestamps mark (§3.1) also gets a pipeline statistics query,
`PIPELINE_STATISTICS1` where the device counts mesh work (the RTX 4080
SUPER does), else `PIPELINE_STATISTICS`. Each query counts:

- vertex, mesh, amplification, pixel and compute invocations;
- mesh primitives;
- primitives into and out of the clipper.

A list's queries are resolved into a read-back buffer when it closes and
read once its submission has finished. Every 5 s the sums are logged per
frame: the totals, per kind of command, and per program for the union of
the 15 programs that run the most of each counter, with pixel invocations
per primitive.

- Milliseconds say how long a command took; these counts say what it did.
  The idea comes from VIctory #1723 (a per-command statistics patch); the
  code is XTP's own, written for `timestamps.rs`.
- `timestamps.rs` calls it at the five places where marks are made (five
  lines added), and `lib.rs` gets the module line.
- Queries of one type cannot nest: a command marked inside another one's
  query is counted with the outer one, until the outer one ends. The line
  counts these as "nested"; there were none in these sessions.
- WARP tests (`pipestats::tests`):
  - a marked dispatch of the test kernel counts its 64 threads;
  - an outer query around an inner mark counts all three dispatches (192);
  - nothing is asked when off;
  - a result reads by counter with and without mesh counters;
  - the line ranks a draw of 184 primitives and 51 million pixels first.

### 10.4 The frame and the GPU by kind

Frame lines, medians of the 9 world lines (lowest to highest):

| Per frame | A (`=0`) | B (`=1`) |
|---|---:|---:|
| frame | 374 ms (226–572) | 388 ms (275–505) |
| preparing | 74 | 80 |
| frame line "on the GPU" | 318 | 294 |
| copy-back | 50 | 57 |
| waiting | 389 | 361 |
| draws | 2,539 | 2,346 |
| dropped without a host pipeline (of them being made); indirect dropped | 474 (210); 812 | 477 (216); 841 |

GPU timestamps, 10 windows each: the lists' work 233 ms (A, median;
mean 225) and 219 ms (B; mean 220) a frame, in 29–30 title submissions.
The GPU is idle, or runs untimed work, 97 ms (A) and 110 ms (B) a frame
between submissions. Of the 4,230 title submissions of A's whole session,
23 % ran 10 ms or more and took 86 % of the GPU time (at most 155 ms).

By kind, mean ms a frame over the windows (A's executions a frame and µs
each):

| Kind | A | B | A per frame | A µs each | share of A |
|---|---:|---:|---:|---:|---:|
| dispatch | 63.4 | 62.3 | 1,226 | 51 | 26 % |
| **indirect argument clamp (XTP's)** | **57.7** | **59.3** | 3,302 | 18 | **23 %** |
| mesh dispatch | 55.5 | 33.9 | 452 | 125 | 22 % |
| indirect mesh dispatch | 31.5 | 31.6 | 1,701 | 19 | 13 % |
| indirect dispatch | 17.5 | 18.1 | 682 | 27 | 7 % |
| texture upload (XTP's predicated staging copies, §7) | 5.9 | 6.6 | 320 | 19 | 2 % |
| rectangle expansion (XTP's) | 4.1 | 4.7 | 254 | 16 | 2 % |
| indirect rectangles | 3.8 | 4.3 | 254 | 15 | 2 % |
| draw | 3.1 | 3.2 | 146 | 23 | 1 % |
| between commands (copies, barriers, clears) | 2.6 | 2.6 | 8,598 spans | 0.3 | 1 % |
| indexed, indirect indexed and indirect draws | 1.6 | 1.6 | 59 | | 1 % |
| *of the programs, those with a wave64 stage* | *135.1* | *116.6* | *3,956* | | |

By class:

- mesh: 87.0 ms (A), 65.4 (B);
- compute: 81 (A), 80 (B);
- XTP's own passes (clamp, rectangle expansion, texture copies): 68 (A),
  71 (B);
- draws, rectangles included: 8.5 (A), 9.1 (B);
- between commands: 2.6.

Mesh dispatch differs between A and B by where the autopilot walked, not by
the switch (§10.6): the costliest mesh program ran 27 times a frame in A,
12 in B.

Pipeline statistics, A, per frame:

- 8,205 commands queried;
- vertex invocations 1.3 M, mesh 0.95 M;
- 2.1 M primitives into the clipper and 0.6 M out of it;
- pixel invocations 27.4 M: draws 20.4 M, indirect mesh 3.8 M, mesh 1.5 M;
- compute invocations 48.9 M: dispatches 46.1 M, indirect dispatches
  2.6 M, the clamps 0.20 M (62 threads per clamp, one group each).

The draws with the most pixels shade 0.9 to 3.8 M pixels a primitive:
full-screen passes, one or a few large triangles each. The largest of
them, `[vertex 1d65098be346d062, pixel 5510a9fd27ceb4b1]`, shades 2.5 M a
frame. The program with the most pixels is a mesh one, `[mesh
f5257487f2945fdb, pixel 2edbd011d7110758]`, 3.4 M a frame at 70 a
primitive. The other mesh programs listed shade 2 to 115 pixels a
primitive. Nothing approaches the 51 M pixels for 184 primitives of
VIctory #1723.

### 10.5 By program: the top 20

Programs by A's ms a frame, summed over the 10 windows (each window's top
25 lines: a lower bound for a program that left a window's top). Their
pipeline names are their translated shaders' hashes.

| # | Program | Kind | Wave64 stage | A ms/frame | A per frame | A µs each | B ms/frame | B µs each | B/A per execution |
|---:|---|---|:-:|---:|---:|---:|---:|---:|---:|
| 1 | `mesh ba8d52f4ea1a1bac, pixel b37565055f2f7d3b` | mesh dispatch | yes | 28.60 | 27.4 | 1,042 | 13.24 | 1,099 | 1.05 |
| 2 | `compute 24f989b98dcb03f0` | dispatch | | 8.49 | 10.5 | 809 | 9.04 | 832 | 1.03 |
| 3 | `compute 3a79195f06d3ed85` | dispatch | | 8.18 | 45.8 | 179 | 6.23 | 186 | 1.04 |
| 4 | `compute ebe4e53f031f5249` | dispatch | | 4.09 | 5.7 | 720 | 4.83 | 784 | 1.09 |
| 5 | `mesh 7972997d15fd9e69, pixel 9f14b72117e2abec` | indirect mesh dispatch | yes | 3.79 | 230.5 | 16 | 3.54 | 18 | 1.07 |
| 6 | `mesh 32c5e0e62380b489, pixel 4c7bfa5c7bc65187` | mesh dispatch | yes | 2.14 | 61.4 | 35 | 2.66 | 38 | 1.10 |
| 7 | `compute 660814153af43185` | indirect dispatch | yes | 2.05 | 65.7 | 31 | 2.13 | 33 | 1.04 |
| 8 | `compute 12eae112b559091c` | dispatch | | 2.02 | 21.3 | 95 | 1.86 | 125 | 1.31 |
| 9 | `compute 6a04b371a8e26853` | dispatch | yes | 2.01 | 108.7 | 18 | 2.31 | 20 | 1.06 |
| 10 | `compute f8f50ab85e7b10bb` | indirect dispatch | yes | 1.96 | 68.8 | 29 | 1.92 | 30 | 1.06 |
| 11 | `mesh f0c7efd2aceb2aaa, pixel 7d85c9cab3c80721` | mesh dispatch | yes | 1.81 | 49.2 | 37 | 1.82 | 41 | 1.11 |
| 12 | `compute 7c90a988167650a1` | dispatch | yes | 1.79 | 1.0 | 1,795 | 1.96 | 1,945 | 1.08 |
| 13 | `compute 7de0f9db96ddd295` | dispatch | yes | 1.51 | 63.6 | 24 | 1.76 | 25 | 1.06 |
| 14 | `compute 42683eebec081520` | dispatch | yes | 1.49 | 0.8 | 1,754 | 1.26 | 1,680 | 0.96 |
| 15 | `compute 512416d975ace48b` | dispatch | | 1.41 | 24.4 | 58 | 1.54 | 71 | 1.23 |
| 16 | `compute c9da71da40af2e28` | indirect dispatch | yes | 1.29 | 55.5 | 23 | 1.48 | 25 | 1.06 |
| 17 | `mesh 37f363f0a5dcd806, pixel 4a45195a99ac14f7` | mesh dispatch | yes | 1.02 | 25.3 | 40 | 0.88 | 44 | 1.09 |
| 18 | `mesh c962201cddbdd7dd, pixel e5e9af2bbd2c006a` | mesh dispatch | yes | 0.91 | 19.8 | 46 | 0.68 | 49 | 1.08 |
| 19 | `mesh a26422c01dcf5612, pixel 6a3a8d3fe52b4eab` | mesh dispatch | yes | 0.80 | 1.4 | 590 | 0.95 | 729 | 1.23 |
| 20 | `mesh 8f690b0f5dbbce02, pixel 271a87c54ac648a5` | indirect mesh dispatch | yes | 0.75 | 40.4 | 18 | 0.96 | 19 | 1.03 |

- The top 20 programs take 76 ms of A's 225. Programs with a wave64 stage
  alone number 876 a window (median). Below the top 5, no program reaches
  2.2 ms. Most of the rest is many executions of 16 to 50 µs each.
- Of the previous top programs (§4.2), `3a79195f06d3ed85` (16.2 ms then)
  and `ebe4e53f031f5249` are still in the top 5. Per execution,
  `24f989b98dcb03f0` grew from 45 µs to 809 µs.

### 10.6 `XTP_DXIL_OPT`: the A/B for dxilopt

Per execution, B against A, over the 58 programs in both sessions' lines
(`n6ab.py`):

- **Translated programs**: weighted by A's time, 7.3 % slower in B (A's
  88.6 ms of these programs would take 95.0 ms at B's times). Of the top
  20, only `42683eebec081520` got faster (0.96).
- **XTP's own passes**, HLSL compiled by the system's compiler and not
  touched by the switch (the clamp, the rectangle expansion, the texture
  copies): 18.0 µs per execution in A, 19.8 µs in B, **10 % slower**.
- So B's GPU ran about 10 % slower for everything. Against that control,
  the translated programs are **about 2.5 % faster** with the passes.
- Two sessions of identical shaders differ by more than that: gpu-prof's
  A2 and B3 (same translations) differed by 0.89 to 1.24 per program among
  their 8 largest, and by 12 % over all.
- **dxilopt's gain is not measurable on this GPU.** The NVIDIA driver
  compiles DXIL with its own optimizer, and that optimizer already removes
  what the passes remove (dead code, folded constants, repeated values and
  loads). The fork's figure of about 20 ms a frame was not reproduced
  here. Another GPU or driver (AMD's, the L4) is untested.
- **No harm seen**: both sessions ran their 60 s without a device removal.
  The draw and drop counts are the same within the scene's spread. Keeping
  the switch off by default costs nothing measurable on NVIDIA.

### 10.7 The top costs: why, and who owns them

**0. XTP's indirect argument clamp: 54–59 ms a frame** (`indirect.rs`,
`State::execute_indirect` in `draw.rs`).

- **What each clamp records.** Every indirect execution is preceded by
  `indirect::bound`:
  1. a global UAV barrier;
  2. the clamp's root signature and pipeline (a compute pipeline, so a
     switch from the title's graphics or mesh state);
  3. one dispatch of one group (62 threads on average by the statistics);
  4. a second global UAV barrier;
  5. a transition of the arguments to the indirect-argument state.

  `execute_indirect` then sets `host_pipeline = 0` and `host_roots[1] = 0`:
  the title's pipeline and roots are bound again for every execution.
- **What changed with the 3D.** Before it each clamp took 2.5 µs between
  compute dispatches; now 18–20 µs, between mesh executions. The clamp's
  own work is one 64-thread group. What grew is the switch between
  graphics and compute, the barriers' drain of the mesh work before each
  clamp, and the rebind after it.
- **Redundant barriers.** The leading barrier waits for all earlier work,
  even when nothing since the last barrier wrote the arguments.
- **Before dropped executions.** A ran 2,413 indirect executions a frame
  that clamp: 1,701 mesh, 682 dispatches and about 30 draws (the 254
  rectangle draws bound their records in their own expansion). It ran
  3,302 clamps, so about 890 a frame (27 %) clamp the arguments of an
  execution that `prepare` then drops (no host pipeline, still being
  made). B: 2,998 clamps for 2,096 executions (30 %). §4.2 found the same
  before the 3D (73 % then).
- **Attribution caveat.** A timestamp is written once the GPU has finished
  the commands before it (§3.1), so the clamp's span starts after the
  previous execution. Its barriers still remove the overlap the next
  execution would have had. How much of the 57 ms comes back once the
  clamp is batched can only be measured (N6a's acceptance).

**1. `[mesh ba8d52f4ea1a1bac, pixel b37565055f2f7d3b]`: 13–29 ms a frame,
1.0–1.1 ms per execution** (translator; the pass that writes the loop's
bound).

- **Tiny work.** Per execution the statistics count 270 to 750 mesh
  invocations and primitives, and 460 to 2,000 pixel invocations (1.8 to
  8.6 a primitive). A millisecond for that is the cost of a loop, not of
  geometry or fill.
- **The mesh stage is not it.** It is wave64 with 556 instructions, no
  loop, 2 group barriers and 7 first-lane reads (the exchange through
  group-shared memory of `mesh-wave64.md` §3.2). That costs a few
  barriers per group, not a millisecond.
- **The pixel stage** is wave32, 5,405 instructions with 9 loops, and does
  tiled lighting:
  - an outer loop walks the words of a light mask. Its index (`s81`, one
    more each pass) leaves only when it **equals** a per-lane bound (`v56`:
    `v_cmp_eq_u32 vcc, s81, v56` then `s_cbranch_vccz` back);
  - each pass ORs the lanes' mask words across the wave (`dpp8`,
    `row_mirror`, `v_permlanex16`, `v_readlane`);
  - an inner loop of 1,916 instructions, 90 of them memory instructions
    (light data by `s_load_dwordx4`), runs over the wave's mask and leaves
    on `s83 == s84`.
- **The suspicion.** A loop bounded by equality with data runs until XTP's
  loop limit when the bound is not what the console's data says (commit
  fca2059: such loops are no counted loops). Many passes that
  could write the light grid do not run yet: 529 dispatches a frame "do
  not translate", 392 indirect dispatches are "being made (heavy)".
- **Not proven.** At this GPU's issue rate, one wave would run a few
  hundred to a thousand passes of the 1,916-instruction body in a
  millisecond: far more than a tile's lights. The trip count has to be
  counted (N6b).

**2–4. The vertex-transform compute family: about 24 ms a frame** (the
translator's VALU and buffer lowering).

| Program | ms a frame (A) | runs a frame × µs | Before the 3D (§4.2) |
|---|---:|---:|---|
| `24f989b98dcb03f0` | 8.5 | 10.5 × 809 | 18.5 × 45 µs |
| `3a79195f06d3ed85` | 8.2 | 45.8 × 179 | 39.6 × 290 µs |
| `ebe4e53f031f5249` | 4.1 | 5.7 × 720 | 10.8 × 310 µs |
| `12eae112b559091c` | 2.0 | 21.3 × 95 | 31.3 × 44 µs |
| `512416d975ace48b` | 1.4 | 24.4 × 58 | 17.0 × 189 µs |

- **What they are.** All are 32-thread, wave32 programs of 2,400 to 12,000
  instructions, with no group-shared memory, no barrier and no wave64.
  - Most of their loops are scalar (`s_cbranch_scc0` over constants read
    by `s_buffer_load_dwordx4`). `24f989b98dcb03f0` also has a vector
    loop that leaves on a float comparison.
  - In the three checked (`3a79195f06d3ed85`, `24f989b98dcb03f0`,
    `12eae112b559091c`):
    - legacy multiplies are about 15 % of their instructions
      (`v_mul_legacy_f32`: 0 times anything is 0);
    - each thread makes 102 to 138 16-byte buffer loads;
    - `3a79195f06d3ed85` writes vertex-sized results
      (`buffer_store_dwordx3`), and its translation has no back edge.
- **The translation of `3a79195f06d3ed85`**: 9,075 DXIL instructions for
  2,432 console ones (3.7 times):
  - 854 `fcmp` and 533 `select`, mostly the 358 legacy multiplies (both
    operands compared with 0, then a select);
  - 130 `createHandleFromHeap` and `annotateHandle` pairs, one per buffer
    load;
  - 2,800 bitcasts.

  dxilopt's passes did not change their time (1.03–1.09 per execution, §10.6).
- **Not known.** Their threads per execution: they are below the
  statistics line's top 15 for compute invocations, so under about 0.9 M
  a frame each. So it is not known whether the growth since the 3D
  (`24f989b98dcb03f0` 18 times per execution) is more work per dispatch or
  slower work.

**5. `[mesh 7972997d15fd9e69, pixel 9f14b72117e2abec]`, and every small
mesh execution: 3.8 ms for this one, 31.5 ms for all indirect mesh
dispatches** (`draw.rs`, `indirect.rs`: N6a).

- **Fixed cost per execution.** 230 indirect mesh dispatches a frame of
  16 µs each, for 100 to 200 mesh invocations and a few hundred pixels per
  execution. Over the 1,701 indirect mesh dispatches a frame (19 µs each),
  the per-execution cost dominates, not the work.
- **The clamp doubles it.** Each execution also carries a clamp of 18 µs
  and the rebind of its pipeline and roots after it (item 0). Removing the
  per-execution clamp and rebind is the lever; the title's execution
  count is its own.
- The rectangles path is not a top cost: its expansion and draws take
  7.9 ms for 254 draws a frame (31 µs per rectangle draw), part of it the
  same pattern of passes between barriers.

### 10.8 Ready tasks

**N6a. The indirect argument clamp: once per run of executions, no
barrier it does not need, nothing for a dropped execution.**

- *Goal.* 54–59 ms a frame of 3,000–3,300 clamps of 18–20 µs (§10.7 item
  0) to at most 5 ms, without unbounded arguments reaching the GPU.
- *What to change.*
  1. In `State::execute_indirect`, find the host pipeline (and drop the
     execution) before clamping: about 890 clamps a frame.
  2. Clamp the arguments of a run of executions in one dispatch, where
     nothing between them writes those arguments (one group per execution
     or per arguments range, records in order, the budget in order).
  3. A UAV barrier before a clamp only when a UAV write was recorded since
     the last one. Barriers on the arguments buffer rather than global
     ones.
  4. Keep the title's pipeline and roots bound across the clamp where the
     clamp's state did not replace them (compute roots only for a
     graphics or mesh execution).
- *Files.* `crates/xtp-gpu/src/indirect.rs`,
  `crates/xtp-gpu/src/draw.rs` (`State::execute_indirect`, the rectangle
  path that clamps inside its expansion).
- *Acceptance.*
  - In a Series S world session with `XTP_GPU_TIMESTAMPS=1`: the clamp at
    most 5 ms a frame, the indirect executions run a frame unchanged, and
    the frame's GPU work lower by at least 40 ms against this section's A.
  - The WARP tests of `indirect.rs` (budget in order, no wrap) green, plus
    one for two executions clamped in one dispatch and one for a dropped
    execution that clamps nothing.

**N6b. The light loop of `pixel b37565055f2f7d3b`: count its passes, then
find who writes its bound.**

- *Goal.* `[mesh ba8d52f4ea1a1bac, pixel b37565055f2f7d3b]` takes about a
  millisecond per execution for a few thousand pixels (13–29 ms a frame).
  Find whether its outer light loop (leaves on `s81 == v56`) or its inner
  loop runs far more passes than the console's data allow. If so, find
  the pass whose output is the bound, and why it differs: a dropped pass,
  a pipeline still being made, or a wrong translation.
- *What to change.*
  1. A diagnostics counter of the lanes that leave a loop by XTP's limit
     or budget, per program: a UAV the translated program increments
     where the limit's exit is taken, read with the timestamps, named by
     program.
  2. One short session with it, or with `XTP_SHADER_LOOP_LIMIT=256`
     against the default, comparing this program's µs per execution.
  3. Then the light grid's writer, from the dropped-execution lines.
- *Files.* `crates/xtp-shader/src/emit/lower/flow.rs` (the limit's exit),
  a diagnostics file in `xtp-gpu` for the counter (or `pipestats.rs`'s
  line).
- *Acceptance.*
  - The number of lanes and programs that leave by the limit, per frame,
    in a Series S world session.
  - For this program: its passes per pixel, and its time per execution
    under 100 µs once the bound is right, or the reason it cannot be.

**N6c. The vertex-transform compute family: what a thread costs, and the
legacy multiply and per-load handles.**

- *Goal.* `24f989b98dcb03f0`, `3a79195f06d3ed85`, `ebe4e53f031f5249`,
  `12eae112b559091c` and `512416d975ace48b` take about 24 ms a frame
  (§10.7 items 2–4). Find their threads per execution and their cost per
  thread, and lower what the translation adds.
- *What to change.*
  1. Their compute invocations from `XTP_GPU_PIPESTATS` (widen the
     line's per-program list, or list these programs by name).
  2. Lower `v_mul_legacy_f32` in fewer host instructions where an operand
     is known finite, or as one compare of the product's NaN and a
     select.
  3. Create a buffer's descriptor handle once per program and range, not
     once per load (130 in `3a79195f06d3ed85`).
  4. Compare with the driver's own code (the SASS) for one of them.
- *Files.* `crates/xtp-shader/src/emit/lower/valu.rs` (legacy multiply),
  `crates/xtp-shader/src/emit/lower/buffer.rs` and `descriptor.rs`
  (handles); measurement only in `xtp-gpu`.
- *Acceptance.*
  - Threads per execution and µs per thousand threads for each of the
    five, in a Series S world session.
  - The translated instruction count of `3a79195f06d3ed85` lower by at
    least 25 %, the survey's translations otherwise byte-identical where
    nothing changed, and the shader tests green (WARP and the L4).

### 10.9 Toward 25 ms

| Step | Today (A, ms a frame) | After (estimate) | Task |
|---|---:|---:|---|
| the clamp | 57.7 | ≤ 5 | N6a |
| the light loop of the top mesh program | 13–29 | ≤ 3 | N6b |
| the vertex-transform family | 24 | 10–15 | N6c |
| indirect mesh executions' fixed cost (beyond the clamp) | 31.5 | 15–20 | N6a (rebind), then per program |
| everything else (hundreds of programs, XTP's other passes) | about 83 | 60–83 | per program, from this section's lines |
| GPU work | 225 | **about 100–130** | |

- **These steps do not reach 25 ms.** The budget needs a factor of about
  9, not 2.
- **Rough scale.** The title is made to run this frame within 33 ms on
  the Series S's GPU, at a fraction of an RTX 4080 SUPER's peak rates. So on this GPU the frame's
  own work should take a few milliseconds. Most of the 225 ms is
  translation or emulation overhead per execution and per instruction,
  repeated over 8,000 commands a frame. This is an estimate from peak
  rates, not a measurement.
- **The dropped work will add to it.** 1,300 executions a frame are still
  dropped (474 without a host pipeline, 812 indirect). As they translate,
  the GPU work grows.
- **After N6a–N6c**, measure again with this section's sessions and rank
  the next programs. The per-execution fixed cost (16–50 µs for the
  smallest executions) is then the next item to measure.

### 10.10 Method

- Logs:
  - `E:\xtp-data\cache\play\20261124-224345\run.log` (A) and
    `-224559` (B), copied to `E:\xtp-data\runs\n6gpu\A-run.log` and
    `B-run.log`;
  - samplers in `E:\xtp-data\runs\stall-n6-A` and `stall-n6-B`;
  - the binary in `E:\xtp-data\runs\n6gpu\bin\xtp-dc4bb11.exe`;
  - the session plan in `E:\xtp-data\runs\n6gpu\sessions.md`.
- `E:\xtp-data\runs\n6gpu\tools\n6frame.py <run.log> --min-draws 1000
  --top 25` gives §10.4 and the statistics, joined to the program ranking
  (`A-report.txt`, `B-report.txt`). `n6ab.py A-run.log B-run.log` gives
  §10.5's and §10.6's per-execution comparison (`AB-report.txt`,
  `top20-AB.md`).
- Programs' code:
  1. `n6probe.exe` writes a program's container from the title's shader
     pack by its hash (a local probe, not in the repository);
  2. `xtp shader-info --disasm` lists the console code, and
     `tools\loops.py` finds its loops and its instruction mix;
  3. `xtp translate --list --host-lanes 32-32` with `XTP_EMIT_DUMP` writes
     the translation, which `dxc -dumpbin` disassembles;
  4. `pack-info.tsv` holds the native properties of all 26,084 programs of
     the pack: wave64, group size, barriers, LDS, lane reads, instructions.

  Nothing of the title is in the repository; programs are named by hash
  and code by address.
- Pipeline statistics: §10.3. Both sessions ran with them; their own
  cost is not separated (§10.2).

### 10.11 N6b. The bounds of the pixel light loop

With the ordinary guard, the target visits **0.548 mask words and
1.451 light bits per pixel invocation** in the HDR world session.
Every one of its 95 measured frames had **zero guard exits across all
programs**. The intact pair takes 58–200 µs per command on L4.
Globally lowering the limit to 256 cuts work in five other
programs and gives a slower median host frame. Keep the existing
guard and the CPU-uploaded light range.

In the Series S capture `stride-3e/passes-730.txt`, the draw with
`pixel b37565055f2f7d3b` reads the light-index ranges from the SRV at
dword 144 of user-data table 8 (t51). Its producer is the title CPU.
Pass 559 copies 4,098 bytes from upload memory into the 4,100-byte view.
The pixel shader indexes the first 1,024 elements by depth and reads the
last element separately for a depth test. Capture addresses are specific
to this frame.

The cell mask comes from a different SRV, at dword 624. Its size fits
40 × 23 tiles × 32 depth slices × 13 dwords per cell. P561 clears it
with `compute a7eeed0646be1ab7`. P563 draws the light volumes with
`pixel 5ad3a82b1c3cc63a` and all three draws run. P560 produces the
40 × 23 depth texture that selects a cell. The dropped dispatches in
that pass cannot account for the CPU upload that sets the loop bounds.

For each wave the translated shader reduces the first index by unsigned
minimum and the last by unsigned maximum over participating lanes. A
lane enters when its cell mask is nonzero and the first word is at or
below the last word. The outer loop visits
`(last >> 5) - (first >> 5) + 1` words, including both endpoints.
The inner loop visits the set bits in the wave's combined, range-masked
word, at most 32 visits per word. The upload size is consistent with
two 16-bit indices per element and a final 16-bit depth value. Unsigned
16-bit indices would allow up to 2,048 outer visits. The capture records
the view extent. The translator declares the typed load as float4 and
bitcasts its results, so the input size alone cannot establish the trip
count on the GPU. A separate WARP fixture bound an `R16G16_UINT` SRV
and ran the translated two-channel load with a four-byte stride. It
returned both integer endpoints exactly for `(0x8011, 0xfffe)`,
`(31, 32)` and `(0, 0xffff)`. That verifies
the load for a known view format. The pass dump does not record the
format bound in the captured draw.

Watch item 202(c) covers two independent guards. A forward edge into a
loop initializes its counter, so re-entering the inner loop grants a
fresh allowance. The separate budget starts at shader entry and charges
every taken back edge across nested loops. Both light loops have the
ordinary limit of 16,384 per entry, with a shared budget of 1,048,576
back edges per invocation on the reference build `471711d`.

The initial L4 run and the timings in §10.7 predate the structured-buffer
fix. The N6b reference and comparisons below use the same translator tree
(`84ef72b`) with `XTP_RAW_STRUCTURED=1` on every Series S run.

The private counter build splits the failed-limit edge of each translated
loop after lowering, including edges into the barrier drain. Only that
edge increments the shader's dword. Three separate dwords count
non-helper lanes at pixel entry, at the outer word visit and at the
inner bit visit for `b37565055f2f7d3b`. A fourth counts returns to the
next outer word. The host reads cumulative counters after the present
fence and subtracts the previous sample. This keeps shader exits through
the normal condition out of the limit count. A WARP fixture with both
conditional and unconditional back edges counted one when one lane hit
its limit, and zero when all lanes completed.

The 32-lane survey ran with `XTP_RAW_STRUCTURED=1` and validated every
translation, both with the counter disabled and with it enabled. Of
26,698 programs, the same 25,446 translated and the same 25,375 passed
DXIL validation. The probe changed 7,558 translated outputs. All 71
existing validation refusals were mesh shaders exceeding the output-size
limit. The WARP shader suite on the reference tree had 339 passes,
37 ignored and zero failures.

Measurements use Series S, golden data, and 60 seconds after the
autopilot confirms the world. GPU commands are timed with the counter
disabled. Only timestamp windows wholly inside that interval enter the
comparison. XTP prints the 25 most expensive programs in each window.
When the target is listed, multiply its reported ms/frame and
commands/frame by the number of queried frames, then divide total time
by total commands. A missing row has unknown cost.

On L4, the first counter session recorded **zero guard exits in each of
98 consecutive world frames** (2778–2875), across all instrumented
programs. The outer loop ran in 34 frames. Its lanes visited 1,640,520
words and returned for another word 570,848 times, giving 1,069,672
entrants. They visited 3,953,762 light bits. That is **1.534 words and
3.696 light bits per lane entering the loop**. Individual frame means
ranged from 1.00 to 2.34 words and 1.00 to 6.80 bits per entrant.
The first probe placed the pixel-entry counter at console offset zero,
a NOP discarded by the translator. The L4 capture therefore supports
these conditional means only. Its two world captures showed a dim
frame and a black one.

The corrected L4 probe increments the pixel denominator at generated
shader entry, before any title instruction. It also samples the
cell-header predicate and the unsigned word-range comparison. At the
default limit, frames 2751–2845 gave 2,560,426 pixel entries, 1,765,492
lanes reaching the header test and 942,168 entering the outer loop
(36.8% of all entries). They visited 1,403,070 words and 3,715,874
bits, or **1.489 words and 3.944 bits per outer entrant**. Frame 2829
alone recorded 108,264 pixel entries, 146,894 words, 387,340 bits and
zero guard exits. The counter timestamps are excluded from the timing
comparison because the probe adds GPU atomics and drains each present.

The 4080 reference used the ordinary loop limit and skipped downstream
fog compute `577248728544d74b`. The pair appeared in 4 of 11 full world
windows, covering 62 listed frames. At 209 mesh and 1,128 PS invocations
per command it took 46 µs. At about 945–968 mesh and 1,634–1,657 PS
invocations it took 135–147 µs. The other window took 95 µs. Across
the four windows the pair took 115.5 µs/command and 4.06 ms/frame.
Both ordinary composite captures were black, even with structured
reads enabled. These are GPU timestamps for draws prior to composition.
An attempted counter run on the PC reached its 150-second stop before
the title image was mapped. It contributed no frames. Both corrected
counter sessions ran on L4.

The L4 timing pair kept the same binary, skipped the same downstream
fog dispatch and displayed the 1280 × 720 RGBA16F target at ×1,000,
as in `stride-3e`. Capture 3264 of the reference shows a car beneath
overhead lines. Capture 3294 shows street lights and cables. The HDR
scale clips bright pixels but lets us verify that geometry reached the
target. At the default limit the pair took 112 µs for 271 mesh and
457 PS invocations per command, 193 µs for 946 mesh and 1,571 PS
invocations, and 58 µs for 191 mesh and 999 PS invocations.

| L4 setting | Listed / full windows | Listed frames | Commands/frame | µs/command | Pair ms/frame |
| --- | ---: | ---: | ---: | ---: | ---: |
| Default limit, HDR target | 5 / 10 | 41 | 47.38 | 122.28 | 5.794 |
| Limit 256, same HDR target | 3 / 11 | 23 | 34.48 | 64.25 | 2.216 |
| Cell header forced to zero at translation, ordinary present | 5 / 11 | 34 | 57.42 | 49.67 | 2.852 |

Each listed window in the 256 run was under 100 µs (60–67 µs). At
about 190 mesh invocations/command the two runs gave 58 and 60 µs.
At 899 mesh and 1,716 PS invocations/command the 256 run gave 67 µs.
The captures show different scene samples and the selected windows
have different dispatch counts. The median whole-GPU time across
all full windows was 215.8 ms/frame at default and 210.5 at 256.
The median host frame time went from **666.8 to 754.1 ms/frame** over
nine complete reporting intervals in each session. Median submitted
draw counts were 2,930 and 1,553 per frame respectively, with 668 and
992 drops for missing pipelines. These runs show no wall-frame gain.

The corrected entry probe at limit 256 counted 2,318,668 pixel entries
in 97 world frames (2768–2864). The target visited **0.582 words and
1.554 light bits per pixel entry**, or 1.530 and 4.086 per lane that
entered the loop. It never exited at an XTP guard. The global setting
stopped **70,166 lanes across five other shaders**.
Per frame the count ranged from zero to 65,536 (median 39). There were
38 frames with zero affected programs, 27 with one and 32 with two.

| Stage | Program | Guard exits in the 97 frames |
| --- | --- | ---: |
| Pixel | `a467d848ace5810d` | 65,536, all in frame 2777 |
| Compute | `776d835da5ffa646` | 2,147 |
| Compute | `87df99711607dd60` | 1,524 |
| Compute | `8bbcbda6008eaa37` | 513 |
| Compute | `915ed81ea580a276` | 446 |

The empty-cell control removes the initial mask load and lets the driver
eliminate the light path. Its 49.67 µs includes the mesh and the rest
of the pixel shader. It changes lighting and the work the compiler
generates, so subtracting it from the reference would not measure time
spent executing the loops alone.

The 100 µs threshold is reachable with the ordinary guard in the
lighter windows. The heavier windows include more mesh work, more
shaded pixels, and a data-dependent number of light bits. A visited
bit can enter the 1,916-instruction lighting body described in §10.7.
The GPU timestamps bracket both stages together. The counts establish
natural loop termination, but they do not apportion the remaining
time among mesh work, the lighting body and its memory accesses.

The run folders under `E:\xtp-data\runs` use the prefix
`cxlightloop-`. `L4-view-A` and `L4-view-B` hold the HDR timing pair,
`L4-lit-count` the first ordinary-limit counts, and `L4-lit-empty`
the empty-cell control. `L4-view-count-default` and
`L4-view-count256` hold the corrected HDR counter sessions and their
`world-counters.csv` files. The 4080 reference is in
`E:\xtp-data\cache\play\20261125-040546\run.log`.
`lit-survey` contains the per-program JSON and comparisons.
`lit-method` holds SHA-256 binary hashes, WARP logs, the run scripts,
`world-times.py` and `count-world.py`. The latter writes one CSV row per
world frame, including frames with zero hits and per-shader hit counts.
The Git bundles in that folder apply to `84ef72b2`. `counter.bundle`
reproduces the first probe, `entry-counter.bundle` the corrected probe,
`empty-cell.bundle` the compiler control and `range-warp.bundle` both
WARP fixtures.
