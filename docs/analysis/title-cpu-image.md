# What the title's CPU side decides for the image

Task rea (cea6e486), 2026-11-25. Static analysis of the title's executable
(build 1.0.2.1) with Ghidra 12.2 headless and REA 6.10.1 on the cloud L4 VM,
and light scans on the person's PC, read against the per-pass views of
present-1 and worldlight-15 (`E:\xtp-data\runs\`). Interoperability only:
nothing of the title is in the repository. Programs are named by hash (the
first 16 hex digits of the BLAKE3 of the container the title passes), code
by RVA (image base 0x140000000), and behaviour in our own words. The tables
and listings behind this page stay outside the repository, under
`E:\xtp-data\re\` (§5).

## 1. Summary

- **The zeroing pass `577248728544d74b` is the volumetric fog's combined
  apply** (§2). It runs every frame, at render resolution in 8×8 groups,
  from the scattering system's render phase node. It is not a first-frame
  clear, a history reset or a transmittance-only pass. Its output is
  `colour.rgb × T.rgb + S.rgb`, with T the fog's transmittance and S its
  in-scattered light. It writes 0 over a lit scene only where T and S both
  read 0. Its inputs are textures that other compute passes of the same
  system write in the same frame, plus CPU constants. The candidates for
  the zero are two 80×46 volume textures and two placed 1×1 textures,
  which the per-pass view does not sample (§2.4).
- **The 18 mesh programs refused "a scalar memory read as data" are not
  what keeps the street out of the depth** (§3). They are skinned
  characters and shadow-map programs. In present-1 frame 677, 240 of
  their 265 dropped dispatches draw 768×768 and 256×256 light shadow maps.
  The street's G-buffer draws are refused by "a descriptor read from the
  first lane of computed VGPRs" (86 dispatches), and characters by "a lane
  mask read as data" (52). The dwords read as numbers are buffer contents
  and buffer descriptors, which the engine's state flush writes at each
  draw.
- **Depth of field focuses by an autofocus that measures the depth on the
  GPU** (§4). The focus distance is no CPU parameter. Four compute passes
  scan the main depth per 16×16 tile, reduce it, and compute the lens
  parameters into a 48-byte buffer that the CoC pass reads. The CPU gives
  the lens and the scan's limits as constants. The depth reads "far" on
  about 85 % of the screen. So the autofocus settles 600 to 850 m away and
  every street pixel gets a far-field CoC: **the missing depth explains
  the blur by itself**, through both the focus and each pixel's CoC.

## 2. Question 1: the zeroing pass `577248728544d74b`

### 2.1 What it is

The title's shader pack names each program record (`effect:program`). The
hash → record table built for this task (`shader-names.tsv`, §5) places
`577248728544d74b` in the scattering/transmittance system's effect, as
its program n° 105 of 129. In that effect:

- the volume passes of the same system write its inputs in the same
  frame: tile classification, ray-marched gathers, the reconstruct passes,
  then the transmittance and scattering volumes;
- `731a39171b568298`, the last rewrite before FidelityFX, is the same
  system's diffusion in-scattering apply (indirect, heat-haze variant);
- a sibling program, `e9f044cc2135ac16`, is the same apply with a fog
  modifier added. The CPU picks one of the two each frame (§2.3).

### 2.2 How it computes (native code, `xtp shader-info --disasm`)

The program reads the scene colour through its UAV (1280×720 RGBA16F),
then writes back:

- RGB = colour × T + S;
- alpha = colour alpha × max(T) + the Rec.709 luminance of S.

T comes from the transmittance inputs and S from the in-scattering inputs.
Which input feeds them depends on per-pixel branches: far field or volume,
and one more input when a per-pixel class read from another view equals 4.

Its views in worldlight-15 frame 1349 (P419), and who fills them:

| Slot | View | Filled by |
|---|---|---|
| s1+8 | main depth, 1280×720 D32S8 | the G-buffer passes |
| s1+16 | 480×272 R9G9B9E5, placed at `0x994380000` | the reconstruct passes (in-scattering, packed into R32) |
| s1+24 | 480×272 RGB10A2 | the reconstruct passes (transmittance) |
| s1+32 | 480×272 R32F | the far-distance depth downsample |
| s0+304 | 80×46×N RGB10A2, a volume | the transmittance volume pass (present-1 P620) |
| s0+624 | 80×46×N R11G11B10, a volume | the scattering volume pass (P626) |
| s1+40, s0+184, s0+568 | placed 1×1 textures at `0x957000000`, `0x957010000` | title memory: defaults the CPU fills |
| s0+600 | placed 64×64 R16 | title memory |
| s0+808 | constant buffer, 3 424 bytes at title address `0x99276ed80` | the CPU, each frame |
| s1+56 | constant buffer, 480 bytes | the CPU, each frame |
| the 16-byte buffer | exposure | `3fe821a8916a4564`, on the GPU |

### 2.3 Who dispatches it, and when (CPU)

- **The node.** RVA `0x200f580` is a method of the scattering system,
  reached through its vtable at `.rdata` RVA `0x63de988`. It registers the
  system's per-frame callbacks with the renderer's phases:
  - if bit 0 of `[[this+0x768]+0x8a0]` is set: phase 0xb → `0x1258ad0` (a
    jump to `0x1258ae0`), phase 0xd → `0x11544b0`, phase 0xe → `0x1304dd0`;
  - else, only phase 0xa → `0x32b9e50`.
- **The phase node `0x1258ae0`** runs the system's steps in order:
  `0x3276190`, `0x3278690`, then `0x327b920`. The call to `0x327b920`
  comes right after a marker call (`0x369a50`, id 0x6e) and a call
  `0x4dbce00(ctx, 1, <scene colour>, rect, 0x105)` on the texture that
  `0x327b920` then binds as its UAV. The phases call `0x4dbce00` on many
  targets, with flags 0x103, 0x105 and 0x113: a generic step on a
  texture's state, not specific to this pass. No condition guards the
  call to `0x327b920`.
  - `0x3276190`: ray-tile argument reset, cloud presence tiles, precision
    volume;
  - `0x3278690`: tile sort, the 32 classified gathers, the transmittance
    volume, and the 32 classified local-light gathers when local lights are
    on.
- **The dispatch `0x327b920`:**
  1. It returns at once if the global int at RVA `0x7658348` equals 1.
     That int is copied from `0x7658340` by `0x229ae0`, read as a state
     machine (values 1 and 2) by `0x25e6580`, and cleared by `0x56fdda0`.
     It reads a mode other than 1 in the world, since the pass runs there
     every frame.
  2. It binds its inputs from the effect's variable slots (globals from RVA
     `0x849b350` on).
  3. It selects pass `42 | b` of the effect's technique 0: 42 is
     `577248728544d74b`, 43 the fog-modifier sibling. b is a byte at
     +0x36 of a 56-byte record at RVA `0x8528148`. The record's index is
     read from `0xa210d10` or `0xa210d14`, depending on the render
     thread's flags (a double-buffered per-frame setting).
  4. It dispatches through the engine's `Dispatch` (`0x56a20`) with
     `((W+7)>>3, (H+7)>>3, 1)`. W and H are the render resolution (RVA
     `0x99fecf8`/`0x99fecfc`): 160×90 at 1280×720, as the logs show.
- **How the pass ↔ program link was established.** The engine binds a
  compute program as `programs[passes[first(technique) + k].byte5]`:
  effect +0xc60, +0xc70 and +0xc68, with 24-byte pass entries. Techniques
  1 and 2 are the two 32-pass classified families, dispatched indirectly
  in loops. In technique 0, pass k is program n° k+63 of the effect's
  records. Every pass number found in the system's functions gives, under
  that rule, a program that runs in present-1 in the same order. For
  instance:
  - `0x3276190` → passes 54, 6, 7, 8 → P598, P600–P602;
  - `0x32754e0` → 45, 46, 44, 47 → P586–P589;
  - `0x3277110` → 28, 29, 61 → P625, P626.

  Passes 0 and 66 have no record in the pack: their programs are
  elsewhere.
- **The renderer's own command stream.** `0x56a20` first flushes the dirty
  binding state: a callback at ctx+0x122c0 when the bits 0x30000000000
  are dirty, `0x248c2a0` otherwise. Then it writes the dispatch into the
  command buffer: a header 0x4002c400 or 0x4002e800 (+4), then x, y, z.
  The vtable entries the steps call on ctx are +0x60 (program), +0x160
  (dispatch), +0x168 (indirect dispatch), +0x90/+0xd0/+0x138 (views),
  +0x1a8/+0x1b0/+0x1f0 (states), +0x110 (inline data).

### 2.4 What this means for world-black

- It is a pass of every frame, so skipping it (`XTP_SKIP_PIPELINES`)
  drops the fog and the aerial perspective from the image. It is not a
  correct image without them.
- For C·T + S to give 0 on a lit scene, T and S must both read 0 on the
  path taken. In present-1 (frame 677, with the pass skipped) the
  480×272 transmittance holds 0.55 on average (83.8 % lit), and one of the
  two 480×272 R32 outputs of the reconstruct pass holds 0 everywhere.
  The inputs that no per-pass view has measured are:
  1. the two 80×46 volumes (`not sampled: not a 2D texture`);
  2. the placed 1×1 textures at `0x957000000`/`0x957010000`. These are
     title memory the CPU fills at load: XTP must upload them, not leave
     them 0;
  3. which of the reconstruct pass's two R32 textures lies under the
     R9G9B9E5 view placed at `0x994380000`.
  Next step (world-black's): sample those inputs in the per-pass view,
  then read the program's branch on them.
- The fog-modifier sibling `e9f044cc2135ac16` translates too: same
  inputs, plus the modifier's term.

## 3. Question 2: what the refused mesh programs read as numbers

### 3.1 Which programs, and which draws

present-1 sessions `20261124-234706` and `-234824`:

- **The refused programs.** 19 mesh programs are refused "a scalar memory
  read as data", 18 per session. 15 of them are skinned: 9 character
  pre-skin programs, 2 skinned linear-depth programs (`910b4be059bfb28f`,
  `321d411b20a35ef9`), 2 skinned G-buffer programs and 2 skinned
  cascade-shadow programs. Of the other 4, 2 are cascade-shadow programs
  and only 1 is a generic static-geometry G-buffer program
  (`9aff566ca0fa7c98`).
- **The dropped mesh dispatches, frame 677 (683 in brackets).** Each
  dropped pipeline is tied to its programs by the log's `first bind`
  lines. By refusal:

  | Refusal | Dispatches | Where |
  |---|---|---|
  | a scalar memory read as data | 265 (225) | 240 (211) skinned linear depth into 768×768 and 256×256 shadow maps (P440–P502); in the G-buffer: 13 (8) characters, 6 (3) static geometry, 6 (3) skinned |
  | a descriptor read from the first lane of computed VGPRs | 86 (120) | foliage 35 (74), static geometry `903776ba469c8bac` 27 (30), skinned foliage 24 (16): the street's G-buffer draws |
  | a lane mask read as data | 52 (26) | characters (`ef5409053e14aa82`, `093c64546c55554e`): all 70 drops of P293 are characters |
  | other (wave64 first-lane reads, a descriptor joined by control flow, pixel programs) | 47 (40) | level-of-detail, skinned and lit variants |

- **The main depth.** Its writers are P63 (a blit, 8.3 %), P69 (272 of
  272 draws ran, 11.1 %) and P293 (15.2 %). It does not change afterwards.
  So the 18 programs' translation alone would bring back about 12 G-buffer
  dispatches in frame 677, and the shadows of skinned objects. Task
  d687ecdb was told so on the board.

### 3.2 What the dwords are

The programs' layouts are in their root parameters (`xtp shader-info`).
`910b4be059bfb28f`'s user data, with the values the per-pass view prints
for the G-buffer mesh dispatches that ran:

| User data | Kind | What the CPU puts there |
|---|---|---|
| u8, u9, u10 | descriptor tables | byte offsets into the resource heap: u8 = 0xa01900 is slot 327 880 (32 bytes a slot) |
| u11–u12 | root descriptor | the GPU address of a buffer (`root3` in the view) |
| u13–u14 | root constants | numbers: 0x10000096, 0x3162380 (per-draw ids and offsets) |
| u15, u16 | heaps | the resource and sampler heap bases |

In `910b4be059bfb28f` (the user SGPRs start at s8, so s16 is u8, s17 u9,
s23 u15, s24 u16), two kinds of scalar reads go through buffer views
taken from the u8 and u9 tables, not through constant buffers:

- **Before the refusal at 0x1098** (0xfe0–0x1000), it loads the view at
  u8 table dword 224. Its own resource list gives it as t78, a structured
  buffer of 680-byte elements. It then reads three fields of it (bytes
  0x1e8, 0x20c, 0x21c) with `s_buffer_load` and computes a box test with
  them (|x| ≤ 0.5).
- **At its start** (0x80–0xc0), it reads a dword at +0x14 of the view at
  u9 table dword 16, computes it × 48 + 36, and uses that as the offset
  of another `s_buffer_load` through the view at u8 table dword 712.

`descriptor_data` (`emit/lower/smem.rs`) refuses a descriptor loaded from
a table whose use is a scalar read (`Fields::Smem`) when the code also
reads its dwords. What these programs read as numbers is therefore:

- **buffer contents**: per-draw and per-instance records, and an index
  into them, which the CPU writes each frame into memory the views point
  at;
- **the views' own words**: address, stride (680, 48) and record count.
  The title gives them to D3D12.X when it creates the views, and XTP's
  descriptor HLE sees them.

These dwords are neither heap slots nor user-data counts. Which dword of
each of the 18 programs the tracer stops on is for the tracer itself to
name (d687ecdb).

### 3.3 Where the CPU fills them

- **The binding calls.** The engine never writes user data or tables
  directly in its draw code. Each step binds views by slot through its
  context's vtable (+0x90, +0xd0, +0x138; §2.3), which marks dirty bits.
- **The writes.** The draw or dispatch wrapper flushes the dirty bits
  into tables in the command stream: `0x56a20` for dispatches, through
  `0x248c2a0` or the callback at ctx+0x122c0.
- **The descriptors' words.** They come from the views the title creates
  through D3D12.X, so XTP knows each view's address when the title
  creates it.
- **For the fork's two forms:**
  - `view_address` (the view's address as a user-data dword) has what it
    needs at run time;
  - `dead` applies only where the number side is never used.

  The per-program census of which dwords each of the 18 reads is
  d687ecdb's (the tracer).

## 4. Question 3: depth of field

### 4.1 The chain (present-1 frame 677)

| Pass | Role (program) | Reads | Writes |
|---|---|---|---|
| P334 | the tiled deferred lighting's depth processing | main depth | 80×45 RG32F per 16×16 tile: min/max linear depth; max 9 986 m, mean 7 394/9 076 m |
| P650 | autofocus scan, first reduction | the 80×45 tiles | 5×3 |
| P651 | autofocus scan, final reduction | 5×3 | 1×1: [7 496, 9 984, 1 024, 0] |
| P652 | lens parameters (`2bfdc19dc1f25b3a`, 1×1×1) | 1×1, CPU constants (224 bytes) | a 48-byte lens buffer, ping-ponged (`0x95e0ff960`, `0x9191fcf30`), and 16 bytes |
| P653 | temporal resolve aware of the DOF (`c89f7bd38af4f529`) | lens buffer, depth | 640×360 |
| P657–P660 | DOF tile classification | lens buffer, depth | 80×45, 27×15, indirect arguments |
| P673 | `30ee260ef303ac7b` + `f987c56098b9faff`: half-resolution colour and CoC | lens buffer, **main depth 1280×720**, colour | 640×360 RGBA16F, CoC in alpha (mean 2.66, max 2.91) |
| P674, P675 | prefilter, main blur | | 640×360 |
| P677 | composite (`5ec7617b0f7cb43c` + `813a7ead7f09ebd7`) | blurred layer, depth | 1920×1080 |

The lens buffer holds [0.854, 50.0, 4.167, 854.2, 0.769, 0.769, 769.1,
960.5, 1/24, 12]. The meanings are inferred from the values and their
consistency between frames, not from code:

- 50.0 is the focal length (mm), 4.167 the f-number, 12 the largest CoC;
- 854.2 is the focus distance (m), 0.854 the same in km;
- 769.1 and 960.5 are the near and far planes of the in-focus range;
- the frame before reads 600.9 m, with planes 558.6 and 650.2: the focus
  moves smoothly from frame to frame.

### 4.2 Where the parameters come from

- **Code sites.**
  - The DOF renderer finds its effect by name hash at RVA `0x57456c0`.
  - It looks up its techniques at `0x123bb10`: 13 hashes, among them the
    CoC downsample (0x3ffc6f68) and the main blur (0xfb6b2f09).
  - The autofocus finds its effect and two techniques at `0x57453f0`.
- **The focus distance is no CPU parameter.** The GPU passes above
  compute it from the depth.
- **What the CPU gives.**
  - The lens and the scan's limits: scan area, smallest and largest focus
    distance, nearest and farthest depth considered, aperture range, and
    the response (a damped spring). They come from the title's camera
    data files, in 64- and 160-byte records.
  - In present-1, these constants start [0.0125, 0.0222, 80, 45]:
    1/80, 1/45 and the tile grid.

### 4.3 Does the missing depth explain the blur alone?

Yes, through two paths:

- **The autofocus.** It scans a depth that reads ~10 km (the far clip) on
  most tiles, because the street is not in it (§3.1). It settles 600 to
  850 m away, bounded by its farthest-distance limits.
- **Each pixel's CoC.** P673 reads the main depth itself. The street's
  pixels read "far" there, so they get a far-field CoC near its largest
  value, whatever the focus.

Even a correct focus would leave the street blurred while its depth reads
far. present-2 (the CoC pass skipped) gave a black image. So the presented
colour goes through the DOF layer, and skipping it cannot show a sharp
image. Limits:

- the lens buffer's meanings come from its values (§4.1);
- the 224-byte constants were not read past their first 16 bytes (the
  per-pass view prints 16);
- no session has yet run with a full depth to confirm.

## 5. Method, tools and where the material is

- **Names.**
  - `E:\xtp-data\re\scripts\sgd5_names.py` decodes the pack's 26 503
    records (read in place, read-only), hashes each container as the
    runtime does, and writes `E:\xtp-data\re\out\shader-names.tsv`.
  - `passes_named.py` names the programs of a per-pass view.
  - Both are private, outside the repository.
- **Hashes in code.**
  - The title stores few names in clear; one setup decodes its string on
    the stack.
  - Effects and techniques are looked up by joaat hash: effect lookup
    `0xb5a4f0`, technique index `0x4f28cd0` (returns 0xff when absent),
    variable lookup `0x8e8930`.
  - `tech_hashes.py` hashes 10 067 names derived from the record names
    and finds 305 of them in the code (`tech-hashes.tsv`).
  - The scattering system's technique names are not its programs' names,
    hence the pass-number route of §2.3.
- **Static queries on the PC** (`exe_scan.py`, `imm_scan.py`, Python with
  pefile, capstone and numpy, in a venv under `E:\xtp-data\re\py`):
  strings, RIP-relative references, 8-byte pointers, direct calls,
  `.pdata` function bounds (299 257 entries), disassembly of one function.
  - Each scan takes 10–20 s on one core. They run at idle priority when
    `measure.lock` is absent and no `xtp.exe` runs (`when-idle.ps1`).
  - One 10 s scan ran while a lock was present: its check was chained
    wrongly. Fixed for every later scan.
- **Ghidra 12.2 and JDK 25 on the L4 VM**, under `D:\re`, as scheduled
  tasks at idle priority (they outlive the SSH session):
  - the full analysis (`analyze-job.ps1`): project `D:\re\proj\title`,
    4 threads, Java heap 12 GB, so that the heap plus a ~24 GB game session
    stay under the VM's 64 GB minus 4 GB (Samuel). It is bound by its
    database's I/O: one thread at about 30 %, 0.7 to 1 GB in use. After
    two hours it was still in its entry-point disassembly of the
    executable's ~300 000 functions. It was stopped before delivery, so
    that no process stays on the VM; `analyze-job.ps1` starts it again;
  - targeted queries (`q-job.ps1`, `ReaQuery.java`): a project imported
    **without analysis** (2 min). The script makes the function at each
    `.pdata` entry and decompiles it: 30 functions in 26 s. That gave
    every decompilation used here. The output stays in
    `E:\xtp-data\re\out\vm\`.
  - The executable was read in place on the VM's `D:\game`. No copy was
    made outside our Google Cloud project.
- **REA 6.10.1** (npm under `D:\re\npm`, Node 26.13.1):
  - `rea doctor --provider ghidra` reports its Ghidra 12.2 provider
    healthy on the VM (Windows P0: Job Object ownership, private ACLs,
    JDK 25);
  - on Windows, REA cannot open an existing Ghidra project and imports
    the target anew for each CLI call, or once per MCP session. With a
    120 MB target that costs the import (and, with REA's default
    analyzers, the analysis) every time;
  - no MCP server was registered in any Claude configuration. REA served
    as the provider check and the reference for the headless route;
    Ghidra's own headless scripts did the queries.

### Commands

```
# names (PC, idle, read-only pack)
E:\xtp-data\re\py\Scripts\python.exe -I E:\xtp-data\re\scripts\sgd5_names.py <pack> E:\xtp-data\re\out\shader-names.tsv
# per-pass view with names
E:\xtp-data\re\py\Scripts\python.exe -I E:\xtp-data\re\scripts\passes_named.py <passes-N.txt> E:\xtp-data\re\out\shader-names.tsv "<regex>" --outputs
# static queries (commands in a file: str, refs, func, dis, callers)
E:\xtp-data\re\py\Scripts\python.exe -I E:\xtp-data\re\scripts\exe_scan.py <exe> <commands> <out>
# VM: targeted decompilation (commands in D:\re\scripts\q-cmds.txt: decomp, refs, callers, str, bytes, ptrs);
# vm-start-q.ps1 registers the scheduled task rea-q-job and starts it, vm-stop-re.ps1 stops and removes it
tools\cloud\invoke-vm.ps1 -File E:\xtp-data\re\scripts\vm-start-q.ps1
E:\xtp-data\re\scripts\vm-xfer.ps1 -Get D:\re\out\q-out.txt -To E:\xtp-data\re\out\vm\q-out.txt
```

### Not done, and why

- **The pass ↔ program table** of the scattering effect comes from code
  order matched to run order, not from the effect file's own tables: they
  are not readable in the pack.
- **The early-out state at `0x7658348`** is described by its readers and
  writers only. Which game mode its value 1 stands for is not established.
- **Q2's per-program census** of which descriptor dwords are read as
  numbers belongs with the tracer (d687ecdb).
- **The full Ghidra analysis** was stopped unfinished (§5). It would add
  data references and switch tables, not the answers above.
