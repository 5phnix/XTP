# Series S mode: the crash a few seconds into the world, and the default

Measured on 2026-11-24 on the person's PC (RTX 4080 SUPER 16 GB, Windows 11
26300) with build 1.0.2.1 (BLAKE3 `b99f876b…e93`), from the golden save,
for the task "Series S mode: fix the title crash ~7 s into the world, then
make Series S the default". RVAs are relative to the base of
`game_scarlett_master.exe` (or of `amd_fidelityfx_upscaler_xbox.dll`, said
so). No title bytes or listings are quoted. Times are UTC. Logs are under
`E:\xtp-data\cache\play\<stamp>` and `E:\xtp-data\runs\stall-<name>`; none
is in the repository.

**Answer: XTP's DirectStorage wrote streamed textures into pages the title
had given to models.** A read whose destination is a list of physical
pages names them in an array the title writes **after** it enqueues the
request and before it submits it. At enqueue the array still holds what
its previous use left there: 0, or the page numbers of another operation.
XTP copied it at enqueue (only the 0 entries were read again, when the
read ran), so a read could take a page number that was no longer its own:
once, the page of a model the world was drawing. The texture went there,
the title followed a pointer of that model into texture bytes and
faulted. The page numbers are now read when the request runs
(`dstorage.rs`, `Read::pages`), as the console's DMA would read them; the
session with the fix stayed 150 s in the world, until the autopilot ended
it (section 4).

Series S is now the default console of `xtp run`, `cargo xtask play`,
`E:\xtp-data\play.ps1` and `E:\tools\stall-probe\session.ps1`;
`--console series-x`, `XTP_CONSOLE=series-x` or `-Console series-x` gives
Series X back (section 5).

## 1. Why Series S

In Series X mode the world takes 160 to 212 s to load: the host copies of
the title's textures push the RTX 4080's resident video memory past about
9.5 GiB, where Windows' video memory manager admits one new allocation
every 0.5 to 2 s (docs/analysis/stall.md). With Series S, the title picks
its smaller budgets (it goes by the device type): frame-prof's session C
(task 7112c207) had the world after about 9.6 s of loading, resident video
memory at 8.19 GiB at most, and a real world frame (79 draws, 2.6 fps).
It crashed about 8 s later. Series X sessions never ran long enough in the
world to meet the bug: their world frames took 5 s (0.2 fps), so they
streamed little in it.

## 2. What the crashes look like

| Session | Run | XTP | In the world | Crash |
|---|---|---|---|---|
| C (frame-prof) | `20261124-163332` | main | 16:34:10.6 | ~8 s later, title+0x29268aa |
| 1 | `20261124-170543` | FSR4 refused | 17:06:05.1 | 1.7 s later, title+0x28f03e8 (division by zero, section 6) |
| 2 | `20261124-171450` | main + mapping log | 17:15:55.8 | 27 s later, title+0x32648e1 |
| 3 | `20261124-172444` | 0 entries read at submission | 17:25:53.4 | 29 s later, title+0x1113c4 |
| 4 | `20261124-173419` | 0 entries never written | 17:34:43.1 | ~1 s later, title+0x4da63f8 |
| 5 | `20261124-173913` | array read when the read runs | 17:40:26.3 | none; ended by the autopilot at 17:42:56.3 |

Sessions C, 2 and 3 have the same shape, read from the title's own crash
dump (session C: its Breakpad minidump in `data\7952987A\temp`) and from
the live process under cdb (sessions 2 and 3):

- The faulting function walks objects of the title's heap (in session C an
  array at object+0x4a8 counted at +0x4a4, 0x20-byte entries, in
  title+0x29266f0; in session 2 object+0x220, then +0x90). Those objects
  are in the title's private heap (`MEM_PRIVATE`, from 0x9_0000_0000) and
  look sane.
- The pointer they hold leads into a **64 KiB view of one physical page**
  (`MEM_MAPPED`, exactly one page: 0x2_fa37_0000 in session C,
  0x3_20c7_0000 in session 2, 0x3_2c0e_0000 in session 3), which holds
  nothing but compressed texture blocks where the title expects a model's
  structure. The next pointer read from there is garbage, and the read
  after it faults.
- Session C's crash context (written by the title) shows a cutscene
  prestreaming (`fbk_stry000_FBK_STRY000_INTRO_min`) with 96 % of its
  physical budget in use: streaming is at its busiest.

## 3. The page's history

Sessions 2 to 5 ran a local diagnostic build that logged every mapping of
physical pages (the title's `XMemMapPhysicalPages`, the GPU's page-mapping
copies through `map_alias`, unmaps) and every DirectStorage write into
physical pages. The page under session 2's crash, 0x3_20c7_0000:

- 17:15:56.192: the title maps physical page 1276064 there (page 2986 of
  its 6.1 GiB pool; it maps the page 16 GiB higher too, and session 3
  shows the whole pool mapped linearly from 0x1_0000_0000 as well). The
  model is loaded into it.
- Nothing maps, unmaps or aliases that range afterwards.
- 17:16:22.599: a DirectStorage read with a page destination writes 1 MiB
  into pages 2982 to 2997 of the pool, page 2986 included. The crash
  follows within the same second.

Session 3 shows it again: a texture read of 4 pages, the first three
consecutive (27679 to 27681) and the fourth page 3293, which the title had
mapped for a model 7 s before; the crash came from that model (title
+0x1113c4).

XTP never gives a freed page number out again, so either the title named
the page or XTP read the wrong one. The diagnostic build compared, for
every page read of session 5, the array at enqueue, at the submission and
when the read ran (the first 150 s of the world included):

| | Reads |
|---|---|
| 0 entries at enqueue, page numbers at the submission | 16,673 |
| non-zero entries at enqueue replaced by other page numbers by the submission | 5,288 |
| both | 34 |
| array changed between the submission and the read | 0 |
| 0 entries left when the read ran | 0 |

So the title enqueues a read with the array as its previous use left it,
writes this read's pages there, then submits. XTP's copy at enqueue
(introduced with the writes through one view per allocation, 60a978e)
kept the stale non-zero numbers; only the 0 entries were read again. The
5,288 reads with stale numbers wrote their data into pages of whatever the
array had named before, models among them. Two attempts confirm it the
other way round: reading the 0 entries at the submission instead (session
3) kept the stale numbers and crashed; skipping the 0 entries (session 4)
lost 16,673 reads' data on top and crashed within a second.

VIctory (Marvisak/VIctory, `crates/victory-hle/src/modules/dstorage.rs`,
`into_destination`) reads the array when the read is written too; its
Series S mode stays in the world for minutes according to its pull
request #904 (not checked here). No code is taken from it.

The page-mapping copies of the GPU (`CopyPageMappingsBatchX`, which map
pool pages into the title's tiled heap through `map_alias`) never touched
the ranges that crashed.

## 4. The fix and its check

`dstorage.rs`: a read takes its page numbers from the title's array when
a worker runs it, and keeps no copy from enqueue. Tests:
`page_arrays_are_read_when_the_request_runs` (an array rewritten between
enqueue and submission: the read lands in the new pages, the old ones stay
untouched) and `a_page_filled_in_after_enqueue_is_read_again`.

Session 5 (`series-s-fix3`, `20261124-173913`, diagnostic build with the
same behaviour, cdb attached): world at 17:40:26.3, the title ended by the
autopilot 150 s later, no crash. 26 frame-rate lines in the world: 0.4 to
2.6 fps (median 1.75), 40 to 695 draws per frame (median 230).

The captures asked for that session were not written: `XTP_CAPTURE` names
a folder XTP does not create, and it did not exist (`frame not saved`, os
error 3).

## 5. Series S by default

- `xtp run`: `--console series-s|series-x`, else `XTP_CONSOLE`, else
  Series S (`apps/xtp/src/main.rs`, `Console::pick`; tests
  `series_s_unless_told`, `the_flag_then_the_variable_bring_series_x_back`).
- `cargo xtask play`: passes `--console` from `XTP_CONSOLE`, `series-s`
  when unset, and prints it (test `play_runs_series_s_unless_told`).
- `E:\xtp-data\play.ps1` (outside the repository): `-Console series-s` by
  default, passed as `--console` (older checkouts accept the flag too);
  `-Console series-x` for Series X.
- `E:\tools\stall-probe\session.ps1` (outside the repository): sets
  `XTP_CONSOLE` from `-Console` (`series-s` by default). Worktrees made
  before this change ignore the variable and run Series X.

## 6. What Series S also showed, left as it is

FSR4 (`amd_fidelityfx_upscaler_xbox.dll`) is loaded on Series S only. Its
calls into XTP that are not implemented all fail in a way it handles:

- device slot 0x218 (`DeserializeComputePipelineStateX`, also called by the
  title at title+0x24800db): `E_NOTIMPL`; FSR4 logs it and compiles its 37
  pipelines from their portable form instead ("falling back to NewBE").
- pipeline state slot 0x58, `(0, 0, ID3DBlob**)`, read at upscaler+0x5870:
  on failure FSR4 leaves the pipeline's scratch size 0 and skips its GPU
  scratch buffer (upscaler+0x64c0), which translated shaders do not use.
- command list slot 0x1a8, `ClearUnorderedAccessViewUint` (upscaler
  +0x72ee; 0x1b0 is the float one): not done; FSR4's resources keep their
  contents.

FSR4 cannot simply be held back: a session with its DLL refused
(session 1) reached the world in 18.6 s, then divided by zero at
title+0x28f03e8 within 2 s (a modulo by a count the upscaler provides, 0
without it). The title has no Series S path without FSR4.
