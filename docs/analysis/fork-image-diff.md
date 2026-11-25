# XTP's image path against the VIctory forks that light rooms on NVIDIA

Task forkdiff (f5446618), 2026-11-25. Read only: the forks were cloned
with `git clone --filter=blob:none` into `E:\xtp-data\refs\` and never
built or run; their trackers were read with `gh`. Programs are named by
hash. XTP's side is `main` at `9829ed2` and its last world session,
`E:\xtp-data\cache\play\20261125-070757` (imgsrc-2 of
[pix-world-frame.md](pix-world-frame.md) §11, RTX 4080 SUPER, Series S).

| Tree | Head read | What it is |
|---|---|---|
| [fxceless/victory-fork](https://github.com/fxceless/victory-fork) `main` | `9376fd6` (2026-11-25 07:30 UTC push) | VIctory `ffe310e` + the fork's `578d459` and 10 later commits |
| [ZSlayerHQ/victory-world-runtime](https://github.com/ZSlayerHQ/victory-world-runtime) `next` | `b9ce641` | merge `99f8834` of the fork at `310c6eb` and myokex's victory-amd line (`f25e735`), plus CI commits |
| [Marvisak/VIctory](https://github.com/Marvisak/VIctory) `86a5ed6` | one commit, read through the API | the upstream source of the fork's `a26859f` |

## 1. Verdict

- **The forks' lit rooms are direct light only.** Every report agrees: the
  fork's README (RTX 4070: "anything the sun does not hit directly is
  black"), fork #512 (RTX 3090, two runs of 50 and 60 minutes; RTX 4080 at
  `a803c87`, 1,500 s), VIctory #841 (RTX 3060 Ti on the fork at `d279e69`:
  "everything in shadow is black"). Ray-traced light runs on none of them:
  the fork keeps its builder off by default, and with it on (RTX 5070,
  fork #512, 240 s at `a26859f`) "the room is as dark as with it off".
  victory-world-runtime's one NVIDIA report (#2, RTX 5070 at `99f8834`)
  judges no image.
- **XTP is darker than that:** its deferred lighting writes 0 where direct
  light falls too (§11.3 of pix-world-frame). So the difference is not the
  ray tracing the forks also lack. It is upstream of the lighting pass: the
  light the chain `8cb27ccf2d50c606` / `17e736204a05a105` →
  `2dc364df5d3278b8` → `34fb51b8a0312e82` computes, the tile light lists,
  and the textures and buffers shaders pick by index.
- **Three differences found in code, none measured on XTP yet** (section
  2): the title's descriptor records copied into its heap as bytes, which
  XTP never mirrors for slots shaders pick by index (VIctory `86a5ed6`, the
  fork's `a26859f` and `9376fd6`); texture staging buffers XTP holds for a
  texture's whole life, where the fork lets idle ones go (`578d459`); and
  buffer views off their copy's elements, which the forks give their own
  bytes (`86a5ed6`, `a26859f`) and XTP makes raw through the stride table
  or null.
- **Lighting: the fork lights the room partly by accident.** On 32-lane
  NVIDIA cards the fork refuses the reflection tiler `2cc4c197ee29e291`;
  with it running, the ray pair `0873f80dd84a3c5c` / `6d7fe7745ab9c6d8`
  traces against no structure and the room goes much darker (fork #512,
  RTX 5080). XTP translates and binds that tiler (section 4). One session
  with `XTP_SKIP_PIPELINES` settles whether it matters here.
- **Output (VIctory #841) is not XTP's problem.** There the 3840×2160
  output was 0 because the HDR scene was 0 after the fog apply multiplied
  by an unwritten transmittance (VIctory #2045); the composite ran every
  frame. XTP's screen carries the sky, fog and coronas, so its output path
  works; the Series X composite `4f4ec7b16673362f` is not used in Series S
  (absent from `-070757`), and Series S's `6318f669e4f17d65` translates.
- Most of what the fork's base commit `578d459` brought is in XTP already
  (section 6). The ports left are the fork's commits from 24 to 25 November
  and upstream VIctory's `86a5ed6`, which XTP does not have.

## 2. Black albedo: textures and buffers picked by index

XTP's lead (§11.2): the albedo pixel `09eb0a73df63b87d` reads its textures
`T` and `B` through handles taken from raw-buffer words, and 14,000 to
19,000 such slots wait for a first upload behind a full 1 GiB
`PASS_STAGING` from the world's start (`-070757`: "staging memory full,
the rest wait mib=1022" at 07:08:19, as the world loads).

| What the forks do | Fork commit and site | XTP | Evidence (GPU) | Action |
|---|---|---|---|---|
| **Records copied into the heap as bytes are mirrored.** The title fills its shader-visible heap with plain byte copies, which no `Create*View` or `CopyDescriptors` call shows. The placed heap's pages are write-watched; before each submission the changed pages are read through the kernel and each slot whose record differs from the last one seen is mirrored. | VIctory `86a5ed6` (Marvis, 2026-11-24 21:36 UTC: `descriptor.rs` +81, `host.rs` +4); fork `a26859f` (`descriptor.rs` `sync_heaps`, `changed_slots`, `HostHeap::mirror_copied`; after fork PR #305 by Flyander; `VICTORY_HEAP_SYNC=0` turns it off) and `9376fd6` (a multiply-and-fold fingerprint instead of BLAKE3, 35–42 ms a frame over 250,000–300,000 records; slots draws read through tables lately left to the draws: 70–220 ms → about 3 ms a frame) | **No.** `descriptor.rs` `mirror` runs on view creation and descriptor copies only, and a draw re-reads a slot only through its tables (`descriptor/host.rs`, header). A texture slot is queued for its upload only from `mirror` (`mirror_once`, `!for_draw` → `queue_texture`). A slot the title fills by byte copy and shaders pick by index is never made: null for a placed texture, the previous view otherwise. | VIctory's commit: "1,300 to 14,400 such slots a frame" in the world, "without it those host slots stayed null, or went on showing what they showed before" (machine not named). Fork `a26859f`: about 40,000 records a frame, RTX 4070, title "the first room renders right", one five-minute run, its test suites not run. | **Port** (rank 1): a counter first |
| **Staging buffers of idle textures are let go.** A staging buffer no list holds and no list used for 4 frames and a time that shrinks with free RAM is dropped; the next list that samples the texture stages and uploads it again. | fork `578d459` `texture.rs` `let_go_idle` (unchanged to `9376fd6`): "2,246 of them, 2.1 GB, at the end of run 24" kept before | **No.** `texture.rs` `STAGING` drops an entry only when its texture goes (`forget`); `descriptor/host.rs` `PASS_STAGING` (1 GiB) then caps first uploads by the staging alive, "held for as long as the texture lives". | the fork's run 24 (RTX 4070); XTP's own `-065811` and `-070757`: cap reached at world start, 14–19 k slots waiting | **Port** (rank 2) |
| Textures picked by index are looked at again after their upload (4, 12, 40, 120, 400 frames, a 48 KiB fingerprint) | fork `a803c87` (`draw.rs`, `texture.rs`, `descriptor/host.rs`; `VICTORY_HEAP_RECHECK`) | **Yes, own design:** served textures are checked in turns by the write watch or the guards (`texture::memory_change`, `PASS_CHECKING`, `PASS_HASHING`), which see writes the fork's sampled fingerprint misses | fork: skin and hair without colour "in about three launches of four" before (RTX 4070) | none |
| Textures picked by index uploaded while the world loads and before the title's decoration bake, which samples them once | fork `c96b4f2` `draw.rs` (`VICTORY_EARLY_TEXTURES_MS`, `VICTORY_BAKE_TEXTURES_MS`) | **No** (no bake handling in `xtp-gpu`) | fork commit (RTX 4070); watch item 326 (RTX 5060 Ti, RX 7800 XT, combined changes) | rank 6, after ranks 1–2 |
| A newer texture view makes an older one stale only when it starts in its first 4 KiB | fork `310c6eb` `descriptor/host.rs` (`VICTORY_STALE_TEXTURES`) | **Not applicable as far as read:** XTP's `TextureQueue` keys waiting slots by slot and write, with no span table that drops neighbours | fork: "tens of thousands of slots a run" dropped before (RTX 4070); watch item 358 (RTX 3090) | none; recheck if XTP adds span-based staling |
| A texture already on the card is uploaded again when the card is full | fork `310c6eb` (`VICTORY_REUPLOAD_WHEN_FULL`) | not checked against `PASS_VIDEO_SHARE` (85 % of the budget, per new texture) | fork commit | read with rank 2 |
| Guards against hashing on every check (skin as coloured noise with guards, faces back without) | fork's defaults; watch item 326 (the brief's "watch 333") | **Switchable:** `XTP_TEXTURE_GUARDS=0` hashes every check, `=verify` (diagnostics) hashes what the guards call clean and counts misses | one run per condition on an RTX 5060 Ti, different routes | an A/B session, no port |

**COLOR2 is settled** (§11.2): its buffer reads what the title wrote. The
forks add nothing there.

## 3. Missing meshes: the instance culling

XTP's lead: the culling programs `55679a0547649d0b`, `17af15c4015cb4b6`,
`e30970f1727063fd`, `1c4b3c0547e3e6fa` are refused for "a structured
buffer of unknown stride" (73 such lines in `-070757`); the stride table
that makes them translate is being switched on (task 5d4d8f6f).

| What the forks do | Fork commit and site | XTP | Evidence | Action |
|---|---|---|---|---|
| A buffer view's stride read from a host table by heap slot | fork `578d459` `victory-shader/src/emit/lower.rs` `slot_stride` (`SLOT_STRIDES`); the same refusal string stays for a stride under 4 or not a multiple of 4 | **Yes:** R3, `xtp-shader/src/emit/lower/buffer.rs`, the runtime stride table, being switched on | fork README: "Meshlet culling read its scene buffers as the wrong kind of view, so the world drew almost nothing; fixed" (RTX 4070) | task 5d4d8f6f |
| **A view off its copy's elements gets a copy of its own bytes.** "A structured view that does not start on an element of its host copy was refused and read zeros: the world's culling constants and the colour table bakes among them." | VIctory `86a5ed6` (`shadow.rs` +78, `draw.rs`); fork `a26859f` (`VICTORY_VIEW_OWN_COPY=0`: refused as before) | **Partly:** `descriptor/host.rs` `off_element_view` gives it a raw view where it starts on 16 bytes and `XTP_RAW_STRUCTURED` is on, or reads it through the stride table; else null. `-070757`: "not on an element of its host copy: null" up to count 16, and "a structured view off 16 bytes … stays structured" up to count 65,536 | upstream's commit message; the fork's run (RTX 4070) | **Port** (rank 3), after 5d4d8f6f lands and both counts are taken again |
| Gathers at a named mip level read that level (the culling's depth pyramid) | VIctory `gather_at_level`; fork README | **Yes:** `xtp-shader/src/emit/image_tests.rs`, `gathers_at_a_run_time_level_read_that_levels_footprint` | VIctory #841 (RTX 3060 Ti) | none |
| The four instance-culling programs fill a title pool to its assert when they run (45,000 of 45,000, `+0x5149aec`) | not fixed anywhere; skipping them ran 22 minutes (VIctory #841) | the programs are still refused | VIctory #841 (Asenje's runs) | watch the pool once 5d4d8f6f lands (watch items 342, 365) |
| Walls in 4 of 4 launches with every view of a page guarded | fork `8dd30ab` (code by myokex, after fork PR #115) | **Yes:** `xtp-gpu/src/watch.rs` guards every view of a page | fork #616 (sagouindupm: walls in 4 of 4 launches at `8dd30ab` against 7 of 14 before; watch item 374) | none |

## 4. Zero lighting

XTP's lead (§11.3): the diffuse light `s0+1008` is 0 from its writer
`34fb51b8a0312e82`, whose inputs from `2dc364df5d3278b8` and, before it,
`8cb27ccf2d50c606` / `17e736204a05a105` are 0 too; the tile light lists
`s1+80` hold no light. All four programs translate and bind in `-070757`.
No fork report reads these four programs' outputs: their trackers name
`2dc364df5d3278b8` and `34fb51b8a0312e82` only in VIctory #71, refused on
an older tree. The forks give no direct answer for this chain.

What the forks show about lighting:

| Finding | Source | XTP (`-070757`) | Action |
|---|---|---|---|
| With the reflection tiler `2cc4c197ee29e291` and the ray pair `0873f80dd84a3c5c` / `6d7fe7745ab9c6d8` all running, "much darker; characters' clothes black"; skipping the pair (2 launches) or the tiler: lit. The fork's default on 32-lane cards refuses the tiler, which leaves the pair nothing to trace (the issue's reading). | fork #512, Gargalindis, RTX 5080, `c96b4f2`, 300 s runs, one per row except one | tiler translated and bound; `6d7fe7745ab9c6d8` refused ("an address not from a root parameter"); `0873f80dd84a3c5c` not bound in that session | **A/B session** (rank 4): `XTP_SKIP_PIPELINES=2cc4c197ee29e291`, then `=0873f80dd84a3c5c,6d7fe7745ab9c6d8`; read `s0+1008` and P618's output as in §11.3 |
| The builder off by default; run whole or not at all | fork `578d459`, `pipeline.rs` `Builder`; VIctory #2118 | **Yes:** `pipeline/builder.rs` admits the builder's programs whole or holds them all (VIctory #2118) | none |
| The light-loop prescalarization `11eeb27c2d15980d` translates once a descriptor dword that only fills an M0-indexed VGPR array reads 0 | lines9686-rgb/VIctory `261b653` (in the world on an RTX 3060 Ti: runs every frame, writes a 320×180 R32_UINT); NuraadGit/VIctory `c1c3fbc` (operand reads return only real operands; offline, only this program changes) | **refused:** "a descriptor's address or format read as data" | **Port** (rank 5) as the second form, after checking its authors' licence terms; then see whether `s1+80` fills |
| The 16-byte record of `e18931c72af42e46` multiplies the lighting variants | VIctory #841 | **refused:** "a whole-wave section of a wave64 on a host without 64-lane waves" | check first whether `s0+168` (non-zero, [1.27, 0.79, 8, 0.125]) is that record; if so, not the zero |
| Light packed into R9G9B9E5 by compute, read through views of every level and layer | fork README, `578d459` `TARGETS_AT`, `alias_of` | **Yes**, as RGBA16F stand-ins decoded once a frame before the next submission (`resource.rs`); the decode runs before its writer (§11.4) | in-list decode (pix-world-frame §11.5), not a port |
| Depth bounds make the first room lit by its lamp | fork `c96b4f2` (fork #82 by Kaivyyy, RTX 4070) | **Yes:** `pipeline/host.rs:1394`, `list.rs` `depth_bounds` | none |
| Colour twins of depth planes the tiled lighting writes by compute | fork `578d459` `resource.rs` `Twin` | **Yes:** `host/twins.rs` | none |
| 14 cascade-shadow programs in their default wave form give wrong shadows (dark room); `VICTORY_WAVE_DEEP_SKIPS=0` changes that | fork #62 via #512 | XTP's wave64 lowering is its own; not compared | compare when task 71e45f7c reaches the shadow inputs |
| NaN in the exposure blacks out a frame (AMD) | watch item 288 | XTP's exposure is finite (0.79–0.85, §11.3) | none |

## 5. Other parts of their image path

| What | Source | XTP | Action |
|---|---|---|---|
| Typeless texture views mapped to a typed format of the family (black cutscenes and post passes before) | VIctory #876 (open) | **Partly:** a view without a description of a typeless host copy gets the title's format (`resource.rs` `whole_typed_view`); a view the title makes in a typeless format was not followed further here | a census line in `fits` before any port |
| One pipeline wait window shared by every thread, 12 s per kind, so no draw is skipped while its pipeline is made | victory-world-runtime `99f8834` | XTP's frames 805–806 drop thousands of draws whose pipelines are still being made (§6.3 of pix-world-frame) | measure on precooked caches; no port |
| Card room for the first world frame's buffer copies and targets | fork `a26859f` (`VICTORY_CARD_ROOM_FOR_BUFFERS`, `…_TARGETS_MB`) | XTP's card copies are its own (`shadow.rs`) | performance, notes only |
| Kernel reads into title memory read beside and copied in (guard over every view on) | victory-amd `10ea38e` (RX 7800 XT) | XTP's DirectStorage path not compared | out of scope |

## 6. Already in XTP

From the fork's `578d459` and later: colour twins of depth planes, queue
waits for fences, rectangles from the vertex shader's outputs, code-table
fog loads, R9G9B9E5 views, buffers and textures picked by index (XTP's own
pass), scalar loads from heap-picked buffers, structured views made raw,
the DXIL clean-up passes (`emit/bitcode/opt.rs`), sRGB view casts (fork
#578), depth bounds (fork #82), the builder whole or not at all (VIctory
#2118), the guard over every view of a page, dispatches over 65,535
groups, the 9-bit primitive index mask. The ports left are in section 7.

## 7. Ports, ranked

Credit as XTP does: "Ported from … (commit `<sha>`)" with the human
authors named (Marvis for `86a5ed6`, fxceless for the fork, Flyander for
fork PR #305); the fork's commits are GPL-3.0 and say they were written
with an AI assistant.

| # | Port | Source | XTP files | Expected effect | Size |
|---|---|---|---|---|---|
| 1 | Byte-copied heap records: first a counter (changed heap pages, slots on them, slots whose record differs from the host's), then the mirror before each submission, slots read through tables left to the draws | VIctory `86a5ed6`, fork `a26859f` + `9376fd6` | `xtp-gpu/src/descriptor.rs`, `descriptor/host.rs`, `host.rs` (the call), `watch.rs` (`writes` over the heap's range) | slots picked by index (the albedo's `T`, `B`, light data) made instead of null or stale | M: about 250 lines, 2 tests; the counter alone is S |
| 2 | Let idle staging buffers go | fork `578d459` `texture.rs` `let_go_idle` | `xtp-gpu/src/texture.rs`, `descriptor/host.rs` (`PASS_STAGING` counts what lives) | the 14–19 k waiting slots served within a few passes | S |
| 3 | Views off their copy's elements get their own bytes | VIctory `86a5ed6` `shadow.rs`, fork `a26859f` | `xtp-gpu/src/descriptor/host.rs` (`off_element_view`), `shadow.rs` | culling constants and colour-table bakes read their bytes | M, after 5d4d8f6f and a recount |
| 4 | A/B: the tiler and the ray pair skipped | fork #512 | none (one session, `XTP_SKIP_PIPELINES`) | tells whether XTP's zero comes from the tiled ray path | XS |
| 5 | `11eeb27c2d15980d` translates | NuraadGit/VIctory `c1c3fbc` (or lines9686-rgb `261b653`) | `xtp-shader` IR operand reads | the light-loop data written; maybe the tile lists | S |
| 6 | Textures picked by index uploaded before the decoration bake | fork `c96b4f2` `draw.rs` | `xtp-gpu/src/draw.rs` | characters' baked colour | M |

Each port is a new task, reviewed critically and measured on the PC
against the numbers in pix-world-frame §11 (the waiting slots, the
albedo's non-zero share, `s0+1008`).
