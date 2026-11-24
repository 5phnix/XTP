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
- **Not enough alone (§6).** The deferred lighting also tests each tile's
  rectangle against material classes a compute program writes into a depth
  texture through unordered access, which XTP's host depth textures do not
  allow ("view the host does not allow unordered=true format=53 flags=2",
  every session). fxceless's fork gives such depth textures colour twins
  (commit 578d459); that port is a task of its own.
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

## 6. What still keeps the lighting black

(worldlight-5's result goes here.)

- **Depth twins** (watch, fork study pick 1; fxceless's fork, commit
  578d459): the tiled lighting writes each pixel's material class into a
  depth texture through unordered access, then draws each class's
  rectangles with the depth test "equal". XTP's host depth textures have no
  unordered access (`resource::host_flags`) and the view is refused
  (`descriptor/host.rs` `fits`): "view the host does not allow
  unordered=true format=53 flags=2" in every session. No rectangle passes
  the test. The fork gives such depth textures a colour twin that shaders
  write, copies one side into the other when used, and draws a depth plane
  of a depth-stencil format from its twin with a pass of ours (`SV_Depth`,
  `class × asfloat(0x37800080)`). It touches `resource.rs`, `host.rs`
  (prelude), `draw.rs`, `list.rs` and `descriptor/host.rs`.
- **The world's geometry in the G-buffer**: the main mesh draws that do
  not translate (§4.3) and the meshlet culling read as constant buffers
  (watch item 147).
- `11eeb27c2d15980d` (the light loop's preparation) does not translate in
  any session ("a descriptor's address or format read as data").

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
