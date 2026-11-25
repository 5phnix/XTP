# Frame record and replay

Task cfb7ac93, 2026-11-25. Record a few world frames of the title's GPU work
once, then replay them without the title, through our translator and host,
in seconds, as often as needed. Each lighting or mesh fix is then tested on
the same frame.

Every image experiment used to cost a full session: boot, the save, the
world, a queue for the PC, about 5 minutes. Each run also gave a different
frame, so before/after comparisons were noisy. PIX captures our *host*
frame, and its replay needs Developer Mode; RenderDoc's replays hung the GPU
(pix-world-frame.md). Neither replays the *title's* frame through the code
we change.

Code: `crates/xtp-gpu/src/record.rs` and `record/`, `apps/xtp/src/replay.rs`
(`xtp replay`), `xtask/src/replay.rs` (`cargo xtask replay`). Diagnostics
builds only.

## 1. Use

Record, in a session (`queue-session.ps1` as usual, or `cargo xtask play`):

```
XTP_RECORD=E:\xtp-data\replay\<name>   XTP_RECORD_FRAMES=capture:3
XTP_CAPTURE=<folder>   XTP_CAPTURE_WORLD=1000:22:3:1
```

or `xtp run … --record <dir> --record-frames <first>[:<count>]`.

- `XTP_RECORD_FRAMES=<first>[:<count>]` records frames `first` to
  `first + count - 1` (3 by default).
- `capture[:<count>]`, or nothing, starts at the first frame
  `XTP_CAPTURE_WORLD` or `XTP_CAPTURE_FRAMES` lists. The world starts at a
  different frame in every run, so this is the form to use. With
  `XTP_CAPTURE_WORLD=<draws>:<after>:<count>:1` (every frame), the session's
  own captures are of the recorded frames.
- The folder must be outside the repository: a recording is game data.
  It is refused when its volume has less than `XTP_RECORD_MIN_FREE_GB`
  (16 by default) free.

Replay:

```
xtp replay <dir> [--frames A..B] [--loop K] [--out <dir>]
           [--poke <address>[+<offset>]=<f32|u32|u8|hex>:<values>[@<frame>]]...
           [--pokes-file <file>] [--stat <name>=<what>]... [--session-gpu-pages]
cargo xtask replay <dir> [--no-build] [-- <xtp replay options>]
```

- One JSON line per frame on standard output, with the session's frame
  number: calls, draws and dispatches recorded and run on the host,
  `not_run`, the CPU pages written, how the pages the session wrote back
  compare (`gpu_pages_same`, `_written`, `_different`), objects missing,
  the time, the presented image (`frame-<n>.png`) and its hash.
- With `XTP_PASS_DUMP` set, each `--stat` adds a number from the frame's
  pass dump: `<name>=ds` the largest depth target's coverage,
  `<name>=<program hash>` the first output of the first pass running it,
  `<name>=<W>x<H>:f<format>` the last output of that size and format. The
  values are the dump's "lit" share (texels with a channel other than 0).
  `cargo xtask replay` adds `depth=ds`, `albedo=1280x720:f27` and
  `light=1cd9bc634417a981` (the deferred lighting) when none is given.
- `cargo xtask replay` prints each number beside the previous replay's
  (`<dir>/replay/last.jsonl`). It has no watch mode yet.
- The replay sets itself up as the session was (`info.txt`): the console
  model, and the session's `XTP_*` switches unless the replay's own
  environment sets them (so a switch can be A/B'd on one recording).
  Recording and capture switches (`XTP_RECORD*`, `XTP_CAPTURE*`,
  `XTP_FRAME_PROBE*`, `XTP_PASS_*`, `XTP_PRESENT_TARGET`,
  `XTP_DUMP_TARGETS`) are the replay's own. `XTP_CAPTURE` defaults to
  `<dir>/replay`; `XTP_CAPTURE_FRAMES` defaults to the replayed frames.
- The replay brings the frame count to the session's before the recorded
  frames (presents of no plane), so `XTP_CAPTURE_FRAMES`, `XTP_PASS_DUMP`,
  `XTP_FRAME_PROBE` and `XTP_PRESENT_TARGET` name the same frames as in the
  session. With `--loop K`, the later loops' frames continue the count.
- Shader translations and pipelines come from the session's caches
  (`cache_dir/shaders`, `cache_dir/pipelines`): only what a translator
  change touches is translated again.

### Pokes

A poke writes values into title memory wherever the replay writes memory:
after the snapshot is restored and after each set of captured CPU pages,
for every frame or for the one after `@`. Memory is only written where it
differs, so an unchanged poke does not make the host copies refresh.

The first use (task 42c20033): the directional light's colour is 0 in the
340-byte record of the constant ring that the lighting reads as `s1+0`,
words +300 to +312. The record's address is in the frame's pass dump
(`XTP_PASS_INPUTS=1cd9bc634417a981`: `Srv s1+0: placed … at 0x…`) and stays
the same in every replay of a recording:

```
xtp replay E:\xtp-data\replay\night --poke 0x14626b80+300=f32:1,1.2,2 --frames 618
```

The term also multiplies by `s0+232` word 21, which a compute pass writes:
a poke there is overwritten by the GPU. See lighting-terms.md.

## 2. How it works

### The boundary

The title calls our objects through their tables, with no packet room, so
every command is a call. While recording, the title gets a copy of our
device whose table records and forwards the methods kept; each object it
makes gets a table of its kind made the same way, at once (the word before
slot 0 points at the original table). Objects keep their memory and
layout, so the title's inline code and our methods see no difference. The
narrowest boundary is therefore xtp-gpu's own table entry points: no HLE
object forwards D3D12.X calls.

Two lines in `device.rs` hand out the recording device; two in
`resource.rs` let `is_resource` know a resource by its recording table
(multi-plane presents, `XTP_PRESENT_TARGET=0x…`).

### What is kept

- **The journal**, from the device's creation: objects (queues, lists,
  allocators, heaps, resources, descriptor heaps, root and command
  signatures, query heaps, pipelines, fences), views and descriptor copies,
  page pools, and the title's last release of each object.
- **The recorded frames**: every method kept (`record/methods.rs`): list
  commands, submissions, signals and waits, presents, page mappings, fence
  signals, allocator resets.
- **Lists open when the frames start**: each list's calls since its last
  reset are kept until then.
- **Arguments** are copied deeply: descriptions, arrays, shader code and
  root signature blobs, pipeline streams with what their subobjects point
  at. Object pointers, GPU addresses and descriptor handles inside them are
  marked, for the replay to translate.
- **Memory.** Title memory is what is committed below 2^40, neither code
  nor a stack (xmem keeps title memory there, decision 0001; the host's
  heaps lie above). When the frames start, it is snapshotted (`memory.bin`,
  zero pages left out). Around each submission and present, every page is
  hashed: the pages that changed before it are the CPU's writes; those that
  changed while it ran (after a drain) are what we wrote back. Memory our
  objects own outside title memory (descriptor heaps, heaps, committed
  resources from the host heap, list uploads) is kept the same way, to the
  byte.
- **Fences**: their values when the frames start.

Compression is deflate at its fastest level (`miniz_oxide`): no zstd
dependency yet (Samuel, 25 November). The format is behind `record/store.rs`.

### The replay

1. Title memory is reserved where the session had it, before anything else
   allocates. Memory taken by the replay process itself is reported
   (`unplaced pages`) and left out.
2. The journal is made again, without what the frames do not need: objects
   the title released before the frames, unless an object kept was made
   from them or the frames name them, and the views naming them.
3. When the frames start: empty presents up to the session's frame count,
   the snapshot restored, the fences set, the owned memory written.
4. The frames: calls made in order on one thread, with the CPU's pages
   written before each submission. After it, each page the session wrote
   back is compared with the replay's: the same; the replay left it as it
   was before, so it is written as recorded (a CPU write during the
   submission); or the replay wrote something else, which is kept (a
   different GPU result: what a fix changes). `--session-gpu-pages` writes
   them all as recorded.
5. Objects are named by their session address; memory our objects own is
   at other addresses in the replay, and addresses and handles inside it
   are translated. Committed resources get their session address back
   (the replay is the allocator).

## 3. Limits

- **CPU memory is frozen.** CPU or HLE fixes do not show in a replay; the
  title's CPU reactions to GPU results (exposure read-backs, occlusion) are
  as recorded.
- **GPU state from before the frames is not in the snapshot.** Our host
  textures (render targets, history buffers, GPU-written textures) are not
  title memory: the first recorded frame starts with them empty, and later
  frames see the replay's own earlier frames. Temporal effects are only as
  right as that; buffers we write back to title memory are in the snapshot.
  Recording one frame more than needed and looking at the later ones
  reduces it.
- **One thread.** The replay makes calls in the order they returned in the
  session. Queues keep their recorded order and fences; work the session
  ran on a queue's waiting thread runs where the replay's waits release it.
- **The ray-tracing builder's host mirror** (`rt::Builder`, called from
  `xtp-hle/bvhbuilder.rs`) is not recorded. The title's builder dispatches
  themselves go through the lists and are.
- **Page mappings** are copies in the replay: its memory has no physical
  pages to share. A later write through one view does not show through the
  other, except where the captured pages say so.
- **Not copied:** enhanced barrier groups (we only use their count), the
  console extensions of `CreateComputePipelineStateX` and the sync settings
  of `Signal2X` (we read neither).
- **The replay shows a window**, as a session does (the presenter's swap
  chain).
- **Objects the title gets other ways** (through DXGI) keep our own tables,
  so their calls are not recorded.

## 4. Proof: how to run it

Not run yet: the session and the replays below are the in-game proof
(task step 4). Everything runs on the PC, one GPU user at a time.

### 4.1 Build

In the worktree the session runs from:

```
powershell -NoProfile -ExecutionPolicy Bypass -File E:\xtp-data\tools\cargo-slot.ps1 build --profile diag -p xtp --features diagnostics --locked
```

It leaves `target\diag\xtp.exe`, which `queue-session.ps1` runs.

### 4.2 Record three world frames

Today's imagecheck setup (image-status.md §2), the intro cut
(`session.ps1` sets `XTP_SKIP_INTRO=1`), the world capture every frame so
that the session captures exactly the recorded frames:

```
powershell -NoProfile -ExecutionPolicy Bypass -File E:\tools\stall-probe\queue-session.ps1 `
  -Name replay-1 -Worktree <worktree> -Measure 60 -Limit 600 `
  -EnvPairs 'XTP_STRIDE_TABLE=1;XTP_RUN_TIME_VIEWS=1;XTP_TEXTURE_POOL=1;XTP_FRAME_PROBE=11eeb27c2d15980d,1cd9bc634417a981,db4835a364449a45;XTP_CAPTURE=E:\xtp-data\pix\replay-1;XTP_CAPTURE_WORLD=1000:22:3:1;XTP_PASS_DUMP=0;XTP_RECORD=E:\xtp-data\replay\night-1;XTP_RECORD_FRAMES=capture:3'
```

- `-Limit 600`: the captures around each submission add tens of seconds a
  frame (two passes over title memory each), on top of 0.5 to 0.7 fps.
- Wait for `E:\xtp-data\runs\stall-replay-1\done.txt`. Then:
  - the run log has `recording: the recorded frames start` and
    `recording: done`;
  - `E:\xtp-data\replay\night-1\summary.txt` ends with `error: none`;
  - `info.txt` there has `window=<n-1>` and `last=<n+2>`;
  - `E:\xtp-data\pix\replay-1\` has `frame-<n>.png`, `passes-<n>.txt` and
    `probe-<n>.txt` for the three recorded frames (`XTP_CAPTURE_WORLD`'s
    log line `world capture armed` gives the first).

### 4.3 Replay three times

Only while no session runs (no `E:\xtp-data\measure.lock`): hold the lock
as `run_probe2.ps1` does (the shell's pid as its first line), and remove it
after. Each replay shows a window.

```
$env:XTP_CONFIG = 'E:\xtp-data\xtp.toml'
$env:XTP_PASS_DUMP = '0'
$env:XTP_FRAME_PROBE = '11eeb27c2d15980d,1cd9bc634417a981,db4835a364449a45'
foreach ($k in 1..3) {
  & <worktree>\target\diag\xtp.exe replay E:\xtp-data\replay\night-1 `
      --out E:\xtp-data\replay\night-1\replay-$k `
      --stat depth=ds --stat albedo=1280x720:f27 --stat light=1cd9bc634417a981 `
      > E:\xtp-data\replay\night-1\replay-$k.jsonl 2> E:\xtp-data\replay\night-1\replay-$k.err.txt
}
```

`XTP_CONFIG` is the session's (its `cache_dir` holds the shader and
pipeline caches). The session's other switches come from `info.txt`.

### 4.4 Compare

- **Replay against replay**: in `replay-1.jsonl` to `replay-3.jsonl`, each
  frame's `image_hash` and every number except `ms` the same.
- **Replay against the session**: for each frame `n`:
  - the BLAKE3 of `E:\xtp-data\pix\replay-1\frame-<n>.png` against the
    replay's `image_hash` (the first 16 hex digits);
  - the session's `passes-<n>.txt` against the replay's: the header's ops
    and dropped, the largest `-> ds` line's lit, the last
    `1280x720 f27` output's lit, and the first output of the pass running
    `1cd9bc634417a981` (what `--stat` reads).

  State every difference measured, and its likely cause from §3 (the first
  frame's missing GPU history first).
- **Health of each replay**: `gpu_pages_different` and `missing_objects` 0;
  the `.err.txt` summary's "snapshot pages not placed" 0 (else title memory
  was taken in the replay process).
- **Time**: one replay's total from its `.err.txt` (and its setup part)
  against the session's `times.txt` (`game started` to the recorded
  frames, and the whole session). The first replay may translate shaders
  the cache lacks: time the second.

### 4.5 The poke

The light colour of task 42c20033, on a recorded frame `n`: the record's
address is the session's `probe-<n>.txt` or `passes-<n>.txt` line
`Srv s1+0: placed … 340x1 … at 0x<address>` of `1cd9bc634417a981`.

```
& <worktree>\target\diag\xtp.exe replay E:\xtp-data\replay\night-1 --frames <n> `
    --out E:\xtp-data\replay\night-1\poke --stat light=1cd9bc634417a981 `
    --poke 0x<address>+300=f32:1,1.2,2@<n>
```

Then compare `light` and the image with an unpoked replay of the same
frame. `s0+232` word 21 also multiplies the term and is written by the GPU
(§1, Pokes): a zero there keeps the light at zero whatever the poke.
