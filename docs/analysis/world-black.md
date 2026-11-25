# From the G-buffer to the screen: why the Series S world stays black

Task worldlight (c19d640b), 2026-11-24, on the person's PC (RTX 4080 SUPER,
Series S mode). The title is BTP 6, build 1.0.2.1; programs are named by
hash only. Runs, logs and dumps stay under `E:\xtp-data\`, none in the
repository. Times are local (UTC+1) unless a log line says otherwise.

## 1. Summary

- **Where it goes black.** Every pass that writes the 1280×720 RGBA16F
  scene colour (FidelityFX's input) runs and leaves it at 0: the first
  material pass (compute `f0c91b4d2578f5bf`), the sky (`348e1e8c414c7eca`,
  `577248728544d74b`), the deferred lighting (233 indirect rectangle draws,
  pixel `1cd9bc634417a981` among others), the forward draws and the
  temporal pass. Nothing after it can bring light back: the upscale, the
  tone mapping (which adds only its dither, 0 to 4 of 1023) and the planes
  are black because their input is (§4).
- **Not the exposure.** The 16-byte exposure buffer every one of those
  passes reads, written each frame by `3fe821a8916a4564` from a 5×1
  luminance texture, holds [1.57, 0.636, 8, 0.125], then [1.08, 0.93, 8,
  0.125]: exposure, its inverse and the bounds, sane (§4.3).
- **Not dropped work, not the upscale.** The passes above run on the host;
  the FidelityFX stand-in runs and upscales a black input. mesh64's session
  C ran AMD's own FSR4 (its build predated the stand-in on `main`), whose
  untranslated programs left their own output black too (§2).
- **Why, first cause found: light read through views in another format.**
  The sky and the lighting read light that compute programs pack as
  R9G9B9E5 into R32 textures, through R9G9B9E5 views placed at those
  textures' addresses (1280×720 at two addresses, 480×272, 192×108,
  2048×512). On the host the view is a second texture that nothing writes:
  black light. XTP's log says so ("a texture over a GPU-written one gets a
  copy of its own … placed view … f67 … another shape or format family").
  This is item 146 of the watch, which fxceless's VIctory fork fixed with
  `alias_of`.
- **The fix (§5).** Ported from fxceless's fork (commit 4e17478), reworked
  onto XTP's alias registry: such a view is brought up to date from the
  GPU-written texture when a list reads it, by a copy through a buffer;
  and, where the host keeps R9G9B9E5 as RGBA16F (the RTX 4080: XTP's
  stand-in, VIctory #492), by a decoding pass of ours on the GPU. Two WARP
  tests; the view reads 0 without the change.
- **Second fix: depth twins (§6.1).** The deferred lighting also tests
  each tile's rectangle against material classes a compute program writes
  into a depth texture through unordered access, which XTP's host depth
  textures did not allow ("view the host does not allow unordered=true
  format=53 flags=2", every session). Ported from fxceless's fork (commit
  578d459): such depth textures get colour twins. The refusal is gone.
- **Still black (§6.3).** With both fixes, and with smemraw's and
  mesh-fix's branches merged for the sessions, the scene colour still
  holds 0 after every pass that writes it (worldlight-11, -15, -16, -19):
  the lighting rectangles, the sky compute and the sky dome draws alike.
  With stride's run-time views switched on (`XTP_RUN_TIME_VIEWS=1`), the
  world's first frame never completes (§6.2, stride's task).
- **Suite (§8).** The open question, the leads (the sky's view LUTs; the
  exposure buffer read through a raw handle on a structured view on NVIDIA,
  watch item 190; a refused first writer, item 191), the scene colour's
  writers, what is set aside and the exact commands.
- **The refused first writer translates (§9, task present).**
  `2da4beb386509b77`, a colour pyramid whose full-resolution level is the
  1280×720 R11G11B10_FLOAT target of item 191, was dropped every frame:
  first for a sampler the console compiler set in an arm under the
  starting EXEC (ported from fxceless's fork), then for lane reads of the
  other 32-lane half in a group of four console waves (a form of XTP's
  own). It now translates, validates and gets a pipeline on the RTX 4080.
- **The 3D world on screen, unlit (§6.2, worldlight-18).**
  `XTP_PRESENT_TARGET=1280x720:f28@f0c91b4d2578f5bf` shows the material
  resolve's RGBA8 G-buffer target in the window: the game's street moving.
- **Tooling (§3).** A per-pass view of a frame (`XTP_PASS_DUMP`), and
  `XTP_PRESENT_TARGET` to put any target the frame writes on screen: the
  person watched the world's street move, as an R8 material target, in two
  sessions.

## 2. Session C's end-of-frame dumps (mesh64's run `20261124-195143`)

`XTP_DUMP_TARGETS` saves what each target holds **at the present**, not
after the pass that wrote it, and scales float formats by their largest
value: the PNGs show structure, the log's `target values` lines the range.
Frame 1009's chain, read from them:

| Target (session C address) | Size, host format, flags | At the present | Reading |
|---|---|---|---|
| `0x22a83109010` | 1280×720 f19 (D32S8), DS | 0 to 1; only poles, wires, a lamp | the main depth: the street's buildings and ground are not in it |
| `0x22d1a0e7a20`, `…6e40`, `…7980`, `…77a0`, `…7f20` | 1280×720 f28/f27, RT+UAV | the same few objects | the G-buffer's render targets |
| `0x22d2636b5a0`, `0x22a83114730` | 1280×720 f62 (R8_UINT), UAV | 143–255, 1–245: **the whole street** | written by compute programs (the tile classification) |
| `0x22d26eeab60`, `0x22a83115270` | 1280×720 f28, f87, UAV | the whole street, packed | written by compute programs |
| `0x22a83115130` | 1280×720 f54 (R16_FLOAT), UAV | 0.15 to 10,000: poles only | linear depth |
| `0x22a83108070`, `0x22d2636a060` | 1280×720 f10 (RGBA16F), RT+UAV, UAV | **0 everywhere** | the scene colour: FidelityFX's input is R16G16B16A16_FLOAT 1280×720 |
| `0x22a8310be50`, `0x22d223b44f0` | 1920×1080 f10, RT+UAV | 0 everywhere | FidelityFX's output (R9G9B9E5 on the console, RGBA16F stand-in) |
| `0x22d31d40970`, `0x22d31d3f250` | 1920×1080 f24 (RGB10A2), RT+UAV | 0 to 4 of 1023: a dither pattern | after the tone mapping |
| `0x22ac392f2f0` | 1920×1080 f28 | near black | plane 0 (the scene); plane 1 is the 3840×2160 interface |

Two findings follow from the dumps and the log alone:

1. **Session C ran AMD's FSR4, not XTP's stand-in.** The FidelityFX HLE
   (`ffx.rs`, bilinear upscale) reached `main` at 20:39; mesh64's branch
   took `main` at 20:56; session C ran at 20:51. Its log loads
   `amd_fidelityfx_upscaler_xbox.dll` ("loaded by the title") and has no
   `ffx:` line. FSR4's console programs do not translate, so its 1920×1080
   output stays 0 and everything after it is black whatever comes before.
2. **The colour FidelityFX reads is already black.** Both 1280×720
   RGBA16F targets hold 0 in frames 949, 979 and 1009. So the break is
   before the upscale, between the G-buffer and the lit scene.

End-of-frame dumps cannot place the break further: a target may be written
by several passes, cleared, or read before it is written. Hence the
per-pass view.

## 3. The per-pass view (`passdump.rs`)

Two diagnostics switches, in diagnostics builds (`crates/xtp-gpu/src/passdump.rs`):

- **`XTP_PASS_DUMP=<width>`**, with `XTP_CAPTURE=<folder>` and frames
  listed by `XTP_CAPTURE_FRAMES` or `XTP_CAPTURE_WORLD`, writes
  `passes-<n>.txt` at the present of each listed frame:
  - every op of every title list in submission order, with the text
    `HostList::note_op` gives it (pipeline, each stage's program hash, user
    data with the heap slot it names, root views, the views its tables
    read), and the draws and dispatches dropped, with their pipeline and
    reason;
  - ops grouped in **passes**: a run of ops on one list writing the same
    outputs (render targets and depth for draws and mesh dispatches,
    unordered-access textures for every draw and dispatch, the texture of
    a UAV clear or of the FidelityFX stand-in's upscale);
  - after each pass, each output **sampled on the GPU in list order**: 24
    rows across it copied into a read-back buffer (a depth texture whole,
    as D3D12 copies no part of one), decoded at the present: the share of
    texels with a channel other than 0 ("lit") and each channel's largest,
    mean and lowest finite value;
  - before each op, the buffers of at most 64 bytes it names (exposure,
    counters), as floats and words;
  - a timeline per output: what it holds after each pass that wrote it.
  Outputs narrower than `<width>` are not sampled; a frame's samples take
  at most 1 GiB of read-back memory.
- **`XTP_PRESENT_TARGET=<width>x<height>[:f<format>][:<n>][@<program>][*<scale>]`**
  (or `0x<address>`) shows, at every present, the `n`-th texture of that
  size and host format the frame writes (through an op of `<program>`, if
  given), in first-write order, instead of the title's planes, stretched
  over 1920×1080: integers divided by 255, floats as they are, one channel
  as grey, all times `scale`. The texture shown stays while frames write
  it. `1280x720:f62@2dc364df5d3278b8` is the lighting's tile
  classification: the world's street, steady, in grey.

Hooks (one line each, diagnostics only): `HostList::note` (the op),
`HostList::close` (the last pass's samples), `Host::submit` (submission
order) in `host.rs`; `State::set_targets`, `readable` (UAVs) and
`note_unready` (drops) in `draw.rs`; `queue::present`.

Shortcuts: a dropped op is listed in the pass of the list the thread last
recorded for (it is not told its list), its outputs unknown; copies and
resolves are listed but not sampled.

## 4. Where the image goes black

### 4.1 Sessions

Series S, `E:\tools\stall-probe\session.ps1` (autopilot from the golden
save, `XTP_SKIP_INTRO=1`), 150 s in the world, all ended by the autopilot
with exit code 0.

| Session | Build | Switches | What it gave |
|---|---|---|---|
| worldlight-1 (21:25) | `main` b3ae3b2 + passdump | `XTP_PRESENT_TARGET=1280x720:f62:1`, `XTP_PASS_DUMP=640` | the street on screen (changing texture from frame to frame); passes of frames 732 and 762, the budget spent in 762 |
| worldlight-2 (cloud L4) | eea9914 | `XTP_PASS_DUMP=0` | never left the front end in 900 s (9 draws a frame): no data |
| worldlight-3 (21:51) | eea9914 | `XTP_PRESENT_TARGET=1280x720:f62@2dc364df5d3278b8`, `XTP_PASS_DUMP=0` | the street steady (one texture chosen); full passes of frames 729–760 with the exposure buffer |
| worldlight-4 (22:06) | eff86c0 (alias copy) | `XTP_PASS_DUMP=0`, title image | still black: no alias copied (R9G9B9E5 is RGBA16F on this GPU, §5.2) |
| worldlight-5 (22:14) | 40f11cb (alias decode) | `XTP_PASS_DUMP=0`, title image | §6 |

### 4.2 The scene colour's passes (worldlight-3, frame 759)

FidelityFX's colour input is `0x2392855b770` (the stand-in's dispatch
names it). Its timeline:

| Pass | Programs | What it is | After it |
|---|---|---|---|
| P33 | compute `f0c91b4d2578f5bf` (one 160×90 dispatch) | material resolve: writes the packed G-buffer UAVs (f87, f28: the street, 15 % and 26 % lit) and the scene colour | colour: 0 % lit |
| P166 | 233 indirect rectangle draws, pixel `1cd9bc634417a981` and others | the deferred lighting, a rectangle per tile and material class, depth test "equal" | 0 % |
| P173, P174, P176 | 235, 171 and 29 draws | lighting of other classes, decals | 0 % |
| P190 | compute `577248728544d74b` | the sky | 0 % |
| P223 | compute `731a39171b568298` (indirect) | the temporal pass (writes the motion vectors too) | 0 % |

Every one of them ran (no drop in these passes). The light they add is
black. What they read, from their op texts:

- P166's pixel program reads the G-buffer (f27/f28 RT+UAV), the depth, the
  exposure buffer, and two **R9G9B9E5 views placed** at 1280×720
  (`0x97d8e0000`, layout 0, and `0x97b110000`, layout 0x102: the bounced
  light the lighting adds);
- P190's sky reads a 480×272 R9G9B9E5 placed view (`0x994380000`);
  worldlight-1's sky (`348e1e8c414c7eca`) reads two 192×108 ones.

These views lie over R32 textures that compute programs write (watch item
146: they cannot write R9G9B9E5). XTP's log has the refusal:
`a texture over a GPU-written one gets a copy of its own … how="placed
view" want="dim 3 1280x720x1 mips 1 f67 flags 0x0" why="another shape or
format family"` (worldlight-1 and -3; the log shows the first 64 aliases
and every power of two after).

### 4.3 Leads checked and set aside

- **Exposure read as 0** (the brief's first lead): the 16-byte buffer the
  passes read (`0x95d4cffe0`/`…fd0`, ping-ponged) is written once a frame
  by a 1×1 dispatch of `3fe821a8916a4564` from the previous one, a 5×1
  RGBA32F luminance texture and constants. Sampled before each op: [1.572,
  0.636, 8, 0.125] in frame 729, [1.077, 0.928, 8, 0.125] in frame 759
  (another tree's reference: [1, 1, 8, 0.125]). Not 0.
- **Dropped work**: none of the scene colour's writers is dropped. The
  frame drops 300 to 1,500 draws and dispatches (most "does not
  translate"), but in other passes: the G-buffer's main mesh draws
  (`910b4be059bfb28f`, `321d411b20a35ef9`: scalar memory read as data) are
  why the depth and G-buffer render targets hold only poles and wires
  (~10 % of the texels), while the street reaches the compute-written
  targets. That is mesh-fix's and the meshlet culling's (watch item 147).
- **FidelityFX**: the stand-in runs every frame (`ffx: dispatch … ran=true`)
  and upscales its black input; in session C, AMD's FSR4 ran instead.
- **Constant views** (`View::Constant(0, 0)`, padding): the passes' table
  constants show sane values in their op texts (sun direction, sizes);
  not the cause here.

## 5. The fix: views in another format read what the GPU wrote

### 5.1 The port (fxceless's `alias_of`)

`resource/alias.rs`:
- each GPU-written source now records its title format's texel width;
- `written_beside(address, want)`: for a texture the GPU does not write,
  the GPU-written texture at its address of the same shape (dimension,
  size, array or depth, levels, one sample) in another format family with
  texels as wide, not depth.

`resource.rs`, `alias_copy(list, placed)`, called by `draw.rs`'s
`readable` for every texture a shader reads (one added line):
- a texture over such a source gets its texels copied from it, through a
  buffer (the host copies between two formats in no other way), the first
  time a list reads it and again whenever the list used the source
  otherwise since (a change of its tracked state), as the fork does;
- its host copy is marked host-written, so title memory (zeros) is not
  uploaded over it again;
- a resource remembers the registry's change count when it was found to be
  no such alias, so the lookup runs again only after a change.

### 5.2 The RTX 4080: R9G9B9E5 is RGBA16F there

XTP keeps R9G9B9E5 as RGBA16F on GPUs that cannot render to it (the
stand-in of VIctory #492). The view then has 8 bytes a texel and the
source 4: no copy can move one into the other, which is why worldlight-4,
with the copy alone, stayed black (no "copied from it" line). So
`alias_copy` decodes instead (`decode_now`):
- the source's words are copied into a buffer; a compute pass of ours
  (HLSL, `cs_5_1`, root SRV and UAV, no descriptor heap) decodes each
  shared-exponent texel into half floats, alpha one, laid out as the
  stand-in's copy footprint; the result is copied into the view's texture;
- it runs in a list of ours submitted at once, before the list that reads
  the view: the title's list is in the middle of its own bindings, which a
  pass of ours would overwrite. It sees what the lists submitted so far
  wrote: light written by a list not yet submitted shows a frame later;
- once a frame per view.

### 5.3 Tests (WARP)

- `resource::tests::a_view_in_another_format_reads_what_the_gpu_wrote`: an
  R32 render target cleared to a packed R9G9B9E5 value, an R9G9B9E5 view
  placed at its address. The view reads 0 (title memory) before; after a
  list reads it, every texel holds the packed value; a second read in the
  list copies nothing; a view in the target's own format is no such alias.
- `resource::tests::packed_light_is_decoded_into_an_rgba16f_stand_in`: the
  decoding into an RGBA16F texture gives (0.5, 0.25, 0.125, 1) as half
  floats. WARP keeps R9G9B9E5 itself, so the stand-in is made directly.
- `passdump::tests` (diagnostics): selectors, formats, op kinds, texel
  decoding, a target shown scaled, a pass sampled after its ops with its
  small buffer.

## 6. Depth twins, and what still keeps the lighting black

### 6.1 The port (fxceless's fork, commit 578d459)

The tiled lighting writes each pixel's material class into a depth
texture through unordered access, then draws each class's rectangles with
the depth test "equal" (depth `class × asfloat(0x37800080)`). XTP's host
depth textures have no unordered access (`resource::host_flags`), so the
view was refused ("view the host does not allow unordered=true format=53
flags=2" in every session) and the classes went nowhere. Ported:

- `resource.rs`: `Twin`, a colour texture of the depth format's typeless
  family (R16 or R32) with unordered access, or for a depth-stencil format
  an R8 twin of the stencil plane (copied through a buffer) and an R16 or
  R32 twin of the depth plane; `twin_view` makes the title's unordered
  access views of the depth texture on its twin.
- `host/twins.rs`: per list, which side it uses and writes (`twin_use`,
  `twins_written`); a side written by one list and used by a later one is
  brought up to date in the later list's prelude at submission; a twin the
  list wrote last is copied back when the list closes, and a depth plane
  is drawn from its twin by a pass of ours (`SV_Depth`, test "always").
- Hooks (added lines): `draw.rs` (depth target, shader reads, unordered
  access, end of binding), `list.rs` (copies, resolves, depth clears, UAV
  clears), `descriptor/host.rs` (the view on the twin).
- Changed from the fork: a twin is made with the first unordered access
  view of its texture, not with the texture (e6d37c2). The title places
  its shadow maps anew every frame with unordered access asked and never
  viewed; a committed twin for each cost a video memory admission apiece.
- Tests (WARP): `host::twins::tests::a_depth_texture_reads_what_its_twin_was_written`,
  `a_depth_stencil_textures_twins_reach_its_planes` (classes 65 and 129
  found in the depth plane, the stencil plane copied).
- Effect in sessions: the refusal is gone (worldlight-10, -11).

### 6.2 Sessions after the fixes

| Session | Build | Result |
|---|---|---|
| worldlight-5, -6 | alias decode, before main's read-race fix | the title stopped (assertion in a file read; fixed on `main`, a2b07a9) |
| worldlight-7 | + mesh-fix | world in 25 s; 50 views decoded; light textures no longer 0 (two 384×384: max 0.16, 0.50); scene colour still 0 after its 7 writers |
| worldlight-8, -9 | + twins, + smemraw (8) | loading 90 s or more: shader surveys of other tasks ran on the PC at the same time (CPU 100 %); not the twins (§ worldlight-10) |
| worldlight-10 | + twins (made with the texture) | world in 30 s, 3.2 fps; no refusal; scene colour 0; a D32S8 depth uniform at 0.9994 after a mesh pass (not seen in -11) |
| worldlight-11 | + twins made at the first view | world in 31 s, 2.4 fps; depth normal (7.3 % geometry); scene colour 0 |
| worldlight-12 | everything (mine, smemraw, stride with its switches, mesh-fix) | world at 49 s, then no frame in 40 s: thousands of new translations, cold caches |
| worldlight-13, -14 | the same, a 150 s warm-up, then with `XTP_STACKS` | world at 64 and 25 s, then no frame at all, every pipeline made: not a cold cache; the title's main thread waits in the title, nothing of XTP's is blocked |
| worldlight-15 | the same, `XTP_STRIDE_TABLE=0`, `XTP_RUN_TIME_VIEWS=0` | frames again (world at 47 s, 1 fps); scene colour 0 (`frame-1349.png` black) |
| worldlight-16 | the same, `XTP_STRIDE_TABLE=1` alone | frames (0.2 fps, 3,306 draws a frame); the material resolve's RGBA8 target covers 100 % of the screen instead of ~20 %; scene colour 0 |
| worldlight-17 | the same, `XTP_RUN_TIME_VIEWS=1` alone | world at 25 s, then no frame: the run-time views alone keep the world's first frame from completing (stride's task) |
| worldlight-18 | the same as -15, `XTP_PRESENT_TARGET=1280x720:f28@f0c91b4d2578f5bf` | the window shows the material resolve's RGBA8 G-buffer target: the game's 3D street (poles, wires, scaffolding, awnings) moving, unlit |
| worldlight-19 | the same as -15, `XTP_SKIP_PIPELINES=731a39171b568298,577248728544d74b` | the two passes that rewrite the scene colour last, skipped: scene colour still 0 after its writers; presented image unchanged |

### 6.3 What still keeps the lighting black

- Every pass that writes the scene colour still leaves 0 with everything
  above (worldlight-15, frame 1349: seven passes). The light textures read
  through the R9G9B9E5 views now hold light (worldlight-7: max 0.16 and
  0.50 in two 384×384 targets), and the depth classes reach the depth
  textures, so the lighting's inputs are no longer all black; its output
  is.
- The sky (`348e1e8c414c7eca`, 11 dispatches) writes 0 with its inputs
  decoded. Its exposure is sane (8.39 then, the auto-exposure pushing up
  against a black scene); its other inputs are table constants read from
  title memory at submission (`draw.rs`: constant tables are filled from
  title memory, which misses constants the GPU writes in the same frame)
  and buffers.
- Watch item 183 (another tester): the combined deferred pass multiplies
  the lit scene by a 960×540 transmittance texture that nothing writes.
  On the Series S (1280×720 inside), the half-resolution textures are
  640×360: the last compute that rewrites the scene colour before
  FidelityFX, `731a39171b568298` (an indirect dispatch), reads three
  640×360 R9G9B9E5 views; `577248728544d74b` reads the 480×272 volume.
  Skipping both (`XTP_SKIP_PIPELINES`, which already drops a pipeline by
  program hash in diagnostics builds) changes nothing (worldlight-19): the
  scene colour is already 0 right after the lighting rectangles, the sky
  compute (`348e1e8c414c7eca`) and the sky dome draws (`3ae25b11290cadde`),
  before either of them runs. Here it is not the first black pass.
- The sky's view LUTs: `26b8bbb2e7ae4dc6` (one 6×54 dispatch) writes four
  unordered access views of 192×108 R32 textures, which the sky reads as
  R9G9B9E5 (decoded). The views land on two textures; one of them holds 0
  everywhere after the dispatch, in every session (-11, -15). The
  transmittance LUT (256×64) and the multiple-scattering LUT (32×32) are
  written. Not explained yet.
- Watch item 181: on NVIDIA the view heap is capped at 1,000,000 slots
  (1,245,184 asked, 737,856 mirrored, 262,144 dynamic): 41 % of the
  title's slots have no host slot and read zeros.
- Programs that still do not translate: 255 a session before smemraw
  (mesh `910b4be059bfb28f`, `321d411b20a35ef9`: scalar memory read as data,
  which smemraw fixes; `11eeb27c2d15980d`, the light loop's preparation: "a
  descriptor's address or format read as data").
- Shader surveys of other tasks ran on the PC during worldlight-8 and -9:
  their 90-second loads and five-times slower translations are those
  surveys', not the twins'.

## 7. Upstream, reviewed

**fxceless's fork, `alias_of` and `texture::alias` (commit 4e17478,
GPL-3.0).** Taken: the rule (same size, levels and layers, texels as wide,
another format), the copy through a buffer at read time, re-copying when
the list used the source otherwise. Changed:
- the lookup uses XTP's alias registry (sources registered with their
  host copies), not a second table of targets by address;
- the fork's rule needs the placed texture to be "placed" and title-filled;
  here any texture the GPU does not write qualifies, over a source in
  another family (same-family aliases already share the copy);
- the fork copies byte for byte and so needs R9G9B9E5 native: on XTP's
  RTX 4080 it is RGBA16F, hence the decoding pass (§5.2), which the fork
  does not have;
- the registry lookup is skipped until the registry changes: it runs for
  every texture every draw reads.
Not taken: the fork's switch `VICTORY_ALIAS_COPY` (XTP's `XTP_ALIAS_SHARING=0`
already turns aliasing off for A/B runs, diagnostics builds).

**fxceless's fork, depth twins (commit 578d459, GPL-3.0).** Taken: the
`Twin` (a colour texture per depth texture written through unordered
access; per plane for depth-stencil formats), the per-list record of which
side is used and written, the copies in the submission prelude, the copy
back when a list closes, the depth plane pass with `SV_Depth` and the class
step constant, the stencil plane copied through a buffer, the view
mapping. Changed:
- a twin is made with the first unordered access view of its texture: the
  fork makes it with the texture, and here every shadow map placed anew
  each frame with unordered access asked would cost a video memory
  admission;
- the twin's own textures are host textures of XTP's (resident, unpooled)
  and its lists' state tracking is XTP's (`HostList::transition` and the
  prelude);
- the fork's switches are `XTP_DEPTH_TWINS=0|stencil|nodepth`.
Kept as the fork has them, and said: whole textures are copied, not the
mips written; a depth plane's twin is not given the plane (draws that
write the plane after a shader wrote the twin are overwritten by the pass
when the list closes; in worldlight-10, with twins made eagerly, one
D32S8 depth read 0.9994 everywhere after a mesh pass, not seen with
lazy twins in worldlight-11); multisampled textures' views stay null.

## 8. Suite: where the next worker picks this up

What lands with this analysis: the R9G9B9E5 views (§5), the depth twins
(§6.1, on by default, `XTP_DEPTH_TWINS=0` turns them off), the per-pass
view and `XTP_PRESENT_TARGET` (§3). What does not: the lit image. The
world's presented frames are still black.

### 8.1 The open question

Every pass that writes the scene colour leaves it at exactly 0: the
lighting rectangles, the sky compute, the sky dome draws, the volume
apply and the last compute before FidelityFX alike. The sampled values
are 0, not NaN (the per-pass view counts NaN and infinities apart, §3).
Other RGBA16F targets the same frames write through unordered access do
hold values (the 960×540 one `51e19585b4753814` writes, the 384×384 light
textures), so typed writes to RGBA16F work in general. The common cause is
not known. Programs of very different kinds all write 0 to the same
texture, so look first for an input or a state they share, not at one
shader.

### 8.2 The scene colour's writers

worldlight-15, frame 1349 (`E:\xtp-data\runs\worldlight-15\passes-1349.txt`,
build 02cef68, stride's switches at 0): the scene colour is the 1280×720
RGBA16F (host format 10) texture that FidelityFX's stand-in upscales
(`grep "FidelityFX bilinear upscale"` names it as `color:`). Its seven
writers, in order:

| Pass | Programs | What it is |
|---|---|---|
| P247 | compute `f0c91b4d2578f5bf` (one 160×90 dispatch) | material resolve: writes the G-buffer UAVs (RGBA8 street, R8, the classes' depth through its twin) and the scene colour |
| P403 | compute `348e1e8c414c7eca` (11 dispatches) | the sky: reads the depth, the exposure buffer, the transmittance LUT and two 192×108 R9G9B9E5 views (`0xac4e00000`, `0xac4e20000`) |
| P404 | vertex `d91fdb5004fdadf1` + pixel `3ae25b11290cadde`; vertex `6e750a37b1da6949` + pixel `d7b97830ede61511` | the sky dome and clouds, drawn (3 draws) |
| P419 | compute `577248728544d74b` | the volume apply: reads the 480×272 R9G9B9E5 volume, writes the scene colour |
| P421, P431 | mesh `c5ffa556ab926155` + pixel `c4a269f4eeba3cac` (4 of 17 ran, the rest "NoPipeline") | forward geometry |
| P460 | compute `731a39171b568298` (indirect dispatch) | the last rewrite before FidelityFX: three 640×360 R9G9B9E5 views, the motion vectors (1280×720 f34) |

With the stride table on (worldlight-16, frame 638, `passes-638.txt`), the
deferred lighting rectangles run before the sky and write 0 as well:
P319 (232 indirect rectangle draws, vertex `25ebc92075b62822`, 183 pixel
programs, `1cd9bc634417a981` among them; depth test "equal" on the
classes' depth), P326 (235 draws, `1d2c970773ce2183`, `e070816c3b889df0`),
P327 (172 draws, 10 programs), P329 (11 draws, 6 programs). In
worldlight-15's frame 1349 they do not appear at all: with the stride table
off, the classified tiles are fewer (the G-buffer covers ~20 % of the
screen instead of 100 %).

### 8.3 The lead: the sky's view LUTs (192×108)

`26b8bbb2e7ae4dc6`, one 6×54×1 dispatch (worldlight-15 P14, list 3),
reads the multiple-scattering LUT (a 32×32 R9G9B9E5 view at `0xac4e40000`
over a 32×32 R32 texture `545138fdb1d94e1b` writes in P12) and the
transmittance LUT (256×64 RGB10A2, `53e3931a17a13ac5` in P11: mean 0.74,
0.64, 0.55), and writes four unordered access views of 192×108 R32
textures (`s1+16`, `+24`, `+32`, `+40`). The per-pass view finds two
textures behind them: one holds packed light (1.4e9 to 1.7e9 as integers,
the R9G9B9E5 bits), the other holds **0 in every texel**: in each of the
nine frames dumped where the pass runs (worldlight-10, -11, -15, -19). The sky (`348e1e8c414c7eca`) reads two
192×108 R9G9B9E5 views, 128 KiB apart, right before the 32×32 one; if one
of them is the zero texture, the sky's light is multiplied or replaced by
0. To do:
1. find which view of the four lands on which texture, and which of the
   sky's two views reads the zero one (the addresses of the R32 textures
   are not in the op text: `note_op` prints a committed texture's size,
   format and flags only; the alias registry knows them,
   `resource/alias.rs`);
2. read `26b8bbb2e7ae4dc6`'s native code (`xtp shader-info --disasm` on
   the shader pack, then its translation): why one pair of stores writes 0
   (a format, a store the translation drops, a mask);
3. the decoding of R9G9B9E5 views runs once a frame, before the title's
   next submission (`resource::run_decodes`): a view read in the same list
   as its writer gets the previous frame's texels. One frame late, not 0,
   but a transient texture the frame reuses may hold anything by then.

### 8.4 Two more leads (watch pass 12, items 190 and 191)

- **Item 190: a raw handle on a structured view, on NVIDIA.** There, a
  raw access through a structured view reads element `byte / 4 × stride`
  instead of the byte asked for. Read through such a view, byte 4 of the
  16-byte exposure buffer lands past its end and reads 0, and a scene
  colour multiplied by it is 0. This fits §8.1's "input they share": the
  sky (`348e1e8c414c7eca`, `Srv s0+16`), the lighting rectangles
  (`1cd9bc634417a981`, `Srv s0+168`), the volume apply
  (`577248728544d74b`, `Srv s0+48`) and the sky dome (`3ae25b11290cadde`,
  `Srv s0+16`) all read a 16-byte buffer, the exposure buffer
  (`buffer Some((<address>, 16))` in their op texts). §4.3 checked the
  buffer's bytes ([1.57, 0.636, 8, 0.125]), not what the shaders read from
  it. stride's task carries the fix (the fork's `raw_window`, VIctory
  #1762) and tests it in its second session. To check it here: a session
  with that fix, then the scene colour after P403 (sky) in the pass view.
- **Item 191: `2da4beb386509b77` is refused** ("a descriptor neither
  loaded nor built"). It is the first writer of a 1280×720
  R11G11B10_FLOAT target (host format 26; worldlight-15's frame 1349 has
  two such targets): a pass that should fill an input is dropped. Find
  it with `grep 2da4beb386509b77` in the run's log (the refusal) and in
  the pass view's `DROPPED` lines, then see which scene colour writer
  reads that target. **Translated since (§9).**

### 8.5 Already set aside

- Exposure, as bytes: the 16-byte exposure buffer is written every frame
  and its bytes are not 0 ([1.57, 0.636, 8, 0.125] in worldlight-3, [12.07,
  0.083, 8, 0.125] in -16 as the auto-exposure pushes up against a black
  scene) (§4.3). What the shaders read from it is not checked: item 190
  (§8.4) reopens it.
- Dropped scene colour writers: none of the sky, volume and last passes is
  dropped; the lighting rectangles all ran (232/232, 235/235, 172/172,
  11/11) (§4.3, §8.2).
- The R9G9B9E5 light read as R32 bits: fixed (§5), 50 views decoded a
  session; the light textures hold light; the image is still black.
- The material classes refused as depth UAVs: fixed (§6.1); still black.
- Watch item 183 (the combined deferred pass and its unwritten
  transmittance): skipping `731a39171b568298` and `577248728544d74b`
  changes nothing (worldlight-19); the scene colour is 0 before them.
- Cold shader caches: worldlight-13 made every pipeline ("host pipelines
  all made", 22:06:14) and still presented no world frame; the cause was
  `XTP_RUN_TIME_VIEWS=1` (§6.2).
- Stride's switches: `XTP_RUN_TIME_VIEWS=1` keeps the world's first frame
  from completing (worldlight-17; stride's task 84451453); with
  `XTP_STRIDE_TABLE=1` alone the frames come (0.2 fps) and the G-buffer
  covers the screen, still black (-16); both at 0, black (-15).
- Shader surveys on the PC during worldlight-8 and -9 explain their slow
  loads, not the twins (§6.3).
- smemraw's branch (de9f4f3): no GPU removal in seven sessions with
  `XTP_D3D12_DRED=1`.

### 8.6 How the sessions were run

The integration build: a local branch merging this branch, smemraw's
(de9f4f3), stride's (a8f280b) and mesh-fix's (44931a5) on `main` afc81fc
(`worldlight-big`, 02cef68; local only, never delivered). Built in the
worktree into `target\diag`:

```
powershell -NoProfile -ExecutionPolicy Bypass -File E:\xtp-data\tools\cargo-slot.ps1 build --profile diag -p xtp --features diagnostics --locked
```

A session (Series S, autopilot to the world, `--no-build`: it runs the
worktree's `target\diag\xtp.exe`; under `measure.lock`, the PC calm, the
capture folder made first):

```
New-Item -ItemType Directory -Force E:\xtp-data\runs\<name> | Out-Null
powershell -NoProfile -ExecutionPolicy Bypass -File E:\tools\stall-probe\session.ps1 -Name <name> -Worktree <worktree> -Limit 150 -Measure 40 -EnvPairs "XTP_WATCHDOG_HOOK=1;XTP_STRIDE_TABLE=0;XTP_RUN_TIME_VIEWS=0;XTP_CAPTURE=E:\xtp-data\runs\<name>;XTP_CAPTURE_WORLD=2000:5:2:10;XTP_PASS_DUMP=0"
```

- `XTP_CAPTURE_WORLD=2000:5:2:10`: two frames, ten apart, starting five
  frames after the first that runs 2,000 draws; each gives `frame-<n>.png`
  (the presented image) and, with `XTP_PASS_DUMP=0`, `passes-<n>.txt`
  (every output sampled; a width instead of 0 skips narrower outputs).
- Add `XTP_DUMP_TARGETS=1280` for every 1280-wide target at the end of
  those frames, `XTP_D3D12_DRED=1` for GPU removals, `XTP_STACKS=40,41,…`
  for every thread's stack at those seconds (into the session's
  `play.err.txt`), `XTP_SKIP_PIPELINES=<hash>,<hash>` to drop programs.
- The showcase (worldlight-18): `-Measure 60` and
  `XTP_PRESENT_TARGET=1280x720:f28@f0c91b4d2578f5bf` (the RGBA8 target the
  material resolve writes) instead of the pass view.
- Results: `E:\xtp-data\runs\stall-<name>\play.out.txt` (frame rate,
  `gameplay after`), the log `E:\xtp-data\cache\play\<time>\run.log`.

Reading a pass view:

```
f=E:/xtp-data/runs/<name>/passes-<n>.txt
# the scene colour (FidelityFX's input) and what it holds after each writer
c=$(grep -o "FidelityFX bilinear upscale Upscale { list: 0x[0-9a-f]*, color: 0x[0-9a-f]*" $f | head -1 | grep -o "color: 0x[0-9a-f]*" | cut -d' ' -f2)
sed -n '/== Outputs, pass by pass/,/== Ops/p' $f | awk -v c="$c" '$1==c{f=1;print;next} /^0x/{f=0} f'
# a pass's programs and outputs, then its ops' views
grep "^P403 L" $f
awk -v P=P403 '/^== Ops/{o=1} o && $1==P{p=1; next} o && p && /^P[0-9]+ /{exit} o && p' $f | grep -o '"[0-9a-f]* \(Srv\|Uav\|Cbv\) [^"]*"' | sort -u
```

## 9. The refused first writer: `2da4beb386509b77` (task present, 179cd4fc)

2026-11-25, offline first, then a session. Item 191 of the watch (§8.4):
on the person's PC and on the reporter's, the first writer of a 1280×720
R11G11B10_FLOAT target was refused ("a descriptor neither loaded nor
built", `GFX10 code at 0x4d0`), so its dispatches were dropped every frame.

### 9.1 Finding the program

- The runtime names a program by the first 16 hex digits of the BLAKE3 of
  the container the title creates (`xtp-gpu` `pipeline.rs`, `shader`).
  `shaders.rpf` holds the containers deflate-packed in SGD5 records
  (`xtp-game` `sgd5.rs`), so the record was found by unpacking every record
  of the size the log gives (`first bind … size=4920`: seven of them) and
  hashing each: record `@0xbdf7769`, kind 0x32. The script (BLAKE3's
  reference and a raw inflate) and the container stay outside the
  repository.
- `xtp shader-info --disasm <folder>`, then `xtp translate --list
  --validate --pipelines --host-lanes 32-32 <folder>` on that folder alone:
  one program, a few seconds, no survey.

### 9.2 What it does

A compute program, wave64, in groups of 16×16 threads (four console
waves; v0's even bits are x, its odd bits y). Each thread reads 2×2 texels
of a colour texture (three `image_gather4_lz`, one per channel) and a
depth-like texture at levels 0 to 4 (`image_sample_l`), weights texels by
their Rec. 709 luminance (0.2125, 0.7154, 0.0721) and by a depth test, and
writes six levels of a colour pyramid: 2×2 texels at full resolution (the
1280×720 R11G11B10_FLOAT target), then one texel per 2×2, 4×4 and 8×8
threads (quad DPP and `ds_swizzle` sums inside each 32-lane half), per
8×8 block (lanes 0 and 32 of a console wave added up: five
`v_readlane_b32 …, 32`) and per group (LDS between the group's four
console waves, after its one `s_barrier`).

### 9.3 First blocker: a sampler set in an arm under the starting EXEC

At 0x4d0, `image_sample_l v15, [v7,v8,v22], s[4:11], s[0:3]`: the
resource is loaded from the table, the sampler s[0:3] is four constants
(0x92, 0x00c00000, `s_brev_b32 40` = 0x14000000, 0). The console compiler
set them inside the arm 0x4a0–0x4cc, `s_and_saveexec_b64 s[20:21], s[40:41]`
and `s_cbranch_execz`, where s[40:41] is the copy of EXEC the program took
first. The edge that skips the arm joins in what s[0:3] held before: two
lane masks and a saved EXEC. Hence the refusal. No lane takes that edge:
each lane's bit of the copy of the starting EXEC is set, so the wave skips
the arm only when none of those lanes is left.

The fix (`resource.rs`, ported from fxceless's VIctory fork, commit
578d459): when an edge of a join holds no descriptor and the lanes' paths
(`Structured`) leave one edge the lanes that read the descriptor can have
taken, the descriptor is that edge's source, if lowering can name it in
any block (`only_edge`, `anywhere`). Changed from the fork: the edge's
conditions are known while its dwords are traced, and a load's base must
come from the entry through moves (an addition in the arm would not
dominate the access). Tests: `resource::tests::a_sampler_built_under_the_starting_exec_is_its_constants`
and `an_image_loaded_under_the_starting_exec_is_its_load`, each with its
refused control (a per-lane guard; a base moved in the arm).

Survey (`--host-lanes 32-32`, cloud L4, 578d62a before, 89a9aa9 after):
24,432 programs translated before and after, the same output hash
(`e5337437…`): no translation changes, none lost. The three programs
refused as "neither loaded nor built" move on to their next blocker;
`2da4beb386509b77`'s is the second blocker below.

### 9.4 Second blocker: lane reads in a group of four console waves

With its descriptors traced, the program stopped at "console wave64 lanes
in a group of more than 64 threads on a host without 64-lane waves": its
five `v_readlane_b32 …, 32` read the upper 32-lane half, and XTP ran such
programs only in the console flow, for one console wave per group. The
console flow branches each console wave by its own ballots, so in a group
of several waves the halves' exchanges would not meet at one barrier
(`docs/wave64-on-wave32.md`, "Groups of several console waves").

The form (`emit/lower/lanes.rs`, XTP's own): the program runs in the two
32-lane halves of the split form, each lane on its own path (DPP and
`ds_swizzle` stay inside a half there), and a `v_readlane_b32` at a lane
fixed in the code goes through group-shared memory: every thread writes
its value at its console thread's place, the group meets, each thread
reads its console wave's lane, the group meets again. Only for groups of
more than 64 threads (smaller ones keep their form), and only where
`group_reads_meet` holds:
- no loop;
- the blocks with barriers (`s_barrier`, lane reads) follow one another by
  dominance, so the barriers a lane passes are a prefix of one sequence,
  and a lane that skips one passes no barrier after it and ends (in this
  program, lanes out of bounds end at 0x0ec);
- every LDS write and later read, and read and later write, have an
  `s_barrier` between them on every path (the console runs a wave's 64
  lanes in step, so its compiler may leave out the barrier between lanes
  of one wave, which are two host waves here);
- no lane mask read as a number, no `v_readfirstlane`, `v_mbcnt`,
  `v_writelane` or permute, no whole-wave section.

Provenance: neither xcl's exchange forms nor the fork's (6a2d9f9:
`wave_base`, `exchange_pair`, exchanges only in blocks every lane runs,
which the early exit at 0x0ec rules out for this program).

Tests: `emit::lower::lanes::tests` (a loop, a lane read in an arm some
lanes skip, LDS read after a write without a barrier, a read then a write:
refused; two writes then a barrier then a read, and an early exit:
accepted); `emit::split_tests::lane_reads_cross_halves_in_groups_of_several_console_waves`
(hardware, cloud L4: 128 and 256 threads, a read of LDS another wave
wrote before an `s_barrier`, a console wave that ends first); its
translation and validation run everywhere in
`every_hardware_case_translates_and_validates_without_a_device`. Two
expectations changed from refused to translated, each keeping the refusal
for a lane the code computes.

`2da4beb386509b77` on the RTX 4080 (32-32): translated, validated by
dxil.dll, pipeline created. On the cloud L4 (32-lane waves) the 41
hardware tests of `split_tests` pass, the new one included, with no GPU
reset.

### 9.5 Survey, before and after

`xtp shader-survey --host-lanes 32-32 --json` on the cloud L4, main
54ab53d against bacfcc6 (main with §9.3 and §9.4): 24,432 then 24,433
programs translated of 26,698. The two JSON files differ in three program
lines and the totals:
- `2da4beb386509b77`'s record: "a descriptor neither loaded nor built",
  then translated;
- the two other programs refused as "neither loaded nor built" move on to
  their next blocker: "an address with an untracked high half" (the fork
  follows such a high half through `only_edge` too, its `live`; not
  ported) and "a lane mask read as data".
No translation that existed changes or is lost. The form of §9.4 takes
only programs that stopped at "console wave64 lanes in a group of more
than 64 threads" before; the 43 others of that bucket fail one of its
conditions (not looked at one by one).

### 9.6 Session present-1 (00:46, Series S)

Build bacfcc6 (main 54ab53d with §9.3 and §9.4), `-Limit 150 -Measure 40`,
`XTP_RAW_STRUCTURED=1` (stride's fix for item 190) and
`XTP_SKIP_PIPELINES=577248728544d74b`, with which the scene is lit since
stride-3e, stride's other switches at 0, `XTP_PASS_DUMP=0`,
`XTP_CAPTURE_WORLD=2000:60:3:10`. It ran twice (two launchers of mine
started; the same build and switches): frames 677, 687, 697 (run
`20261124-234706`) and 683, 693, 703 (`20261124-234824`), in
`E:\xtp-data\runs\present-1`. Gameplay after 27 s, 4.3 fps (median), 2,406
draws and 521 dropped a frame.

- **`2da4beb386509b77` runs**: translated at its first bind (6 ms), a host
  pipeline made; in frame 677 it is P638 (list 70, a 40×23 dispatch).
  The 1280×720 R11G11B10_FLOAT target it writes holds light after it (90.6 %
  of the texels, largest 15.9, mean 0.08), and so do its 640×360 levels;
  both were 0 while it was refused. It is the colour pyramid the
  transparent materials of P643 read (refraction), not the depth of field.
- **The scene colour is lit before the post-processing**: 90.6 % of the
  texels, largest 15.9, mean 0.080, after the forward passes (P643, P646).
- **The presented image** (the person, in the window): cars, poles, wires
  and the street are visible, better than stride's vitrine-2, but the whole
  image is very blurred, like a maximal depth of field, and dark.
- **Why it is blurred, from the pass view**: the depth of field. P673
  (`30ee260ef303ac7b` + `f987c56098b9faff`) makes its 640×360 RGBA16F layer
  with the circle of confusion in alpha: mean 2.66 for a largest 2.91
  (frame 677), 2.64 for 3.31 (693), over 91.7 % of the texels, so nearly the
  largest blur nearly everywhere. Then the blur (P674 `44fdeb707f882b7d`,
  P675 `03c01507703d7449`) and the composite onto the 1920×1080 image (P677,
  `5ec7617b0f7cb43c` + `813a7ead7f09ebd7`, which reads the depth again).
  The depth it reads, the main 1280×720 D32S8, holds only 11 to 19 % of the
  screen (poles, wires, some objects): the rest is 0, infinitely far in
  reverse Z, so the far field's largest circle of confusion covers the
  street and the buildings. The wires and poles, which are in the depth,
  are the sharpest things in the image.
- **Why the depth lacks the street**: the G-buffer's mesh dispatches that
  draw it are dropped for want of a pipeline (70 of 223 in frame 677's
  P293; 40 of 227 and 129 of 139 in frame 683; 70 of 149 in 703). Of the
  run's 28 mesh programs that do not translate, 18 stop at "a scalar
  memory read as data" (smemraw's class, watch item 147). The street
  reaches the image through the compute material resolve, never through
  the depth.
- Beside it: the temporal pass `731a39171b568298` (P671) writes motion
  vectors between −76 and −10,000 everywhere and leaves the scene colour's
  mean at 0.012 (0.080 before it): trails or darkening, not checked.
- The check, present-2 (00:56, the same build and switches,
  `XTP_SKIP_PIPELINES=577248728544d74b,f987c56098b9faff`, no pass view):
  without the depth-of-field layer the world's frames are black (657, 667,
  677; the loading screen, 512, is not). `f987c56098b9faff` serves P673
  alone, so the presented colour comes almost wholly through that layer:
  the composite takes the far field's blurred layer nearly everywhere, as
  a depth at infinity on most of the screen makes it. Leaving a pass out
  gives no sharp image; the depth has to be filled (the mesh programs
  above). The scene colour before the depth of field can be shown with
  `XTP_PRESENT_TARGET=1280x720:f10@731a39171b568298`.
