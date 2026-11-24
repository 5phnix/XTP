# From the G-buffer to the screen: why the Series S world stays black

Task worldlight (c19d640b), 2026-11-24, on the person's PC (RTX 4080 SUPER,
Series S mode). The title is BTP 6, build 1.0.2.1; programs are named by
hash only. Runs, logs and dumps stay under `E:\xtp-data\`, none in the
repository.

## 1. Summary

(in progress)

## 2. Session C's end-of-frame dumps (mesh64's run `20261124-195143`)

`XTP_DUMP_TARGETS` saves what each target holds **at the present**, not
after the pass that wrote it, and scales float formats by their largest
value: the PNGs show structure, the log's `target values` lines the range.
Frame 1009's chain, read from them:

| Target (session C address) | Size, host format, flags | At the present | Reading |
|---|---|---|---|
| `0x22a83109010` | 1280×720 f19 (D32S8), DS | 0 to 1; only poles, wires, a lamp | the main depth: the street's buildings and ground are not in it |
| `0x22d1a0e7a20`, `…6e40`, `…7980`, `…77a0`, `…7f20` | 1280×720 f28/f27, RT+UAV | the same few objects | the G-buffer's render targets |
| `0x22d2636b5a0`, `0x22a83114730` | 1280×720 f62 (R8_UINT), UAV | 143–255, 1–245: **the whole street** | written by compute programs |
| `0x22d26eeab60`, `0x22a83115270` | 1280×720 f28, f87, UAV | the whole street, packed | written by compute programs |
| `0x22a83115130` | 1280×720 f54 (R16_FLOAT), UAV | 0.15 to 10,000: poles only | linear depth |
| `0x22a83108070`, `0x22d2636a060` | 1280×720 f10 (RGBA16F), RT+UAV, UAV | **0 everywhere** | the HDR scene: FidelityFX's colour input is R16G16B16A16_FLOAT 1280×720 |
| `0x22a8310be50`, `0x22d223b44f0` | 1920×1080 f10, RT+UAV | 0 everywhere | FidelityFX's output (R9G9B9E5 on the console, RGBA16F stand-in) |
| `0x22d31d40970`, `0x22d31d3f250` | 1920×1080 f24 (RGB10A2), RT+UAV | 0 to 4 of 1023: a dither pattern | after the tone mapping |
| `0x22ac392f2f0` | 1920×1080 f28 | near black | plane 0 (the scene); plane 1 is the 3840×2160 interface |

Two findings follow from the dumps and the log alone:

1. **Session C ran AMD's FSR4, not XTP's stand-in.** The FidelityFX HLE
   (`ffx.rs`, bilinear upscale) reached `main` at 20:39 (+0100); mesh64's
   branch took `main` at 20:56; session C ran at 20:51. Its log loads
   `amd_fidelityfx_upscaler_xbox.dll` ("loaded by the title") and has no
   `ffx:` line. FSR4's console programs do not translate, so its 1920×1080
   output stays 0 and everything after it is black whatever comes before.
2. **The colour FidelityFX reads is already black.** Both 1280×720
   RGBA16F targets hold 0 in frames 949, 979 and 1009. So the break is
   before the upscale, between the G-buffer and the lit scene.

End-of-frame dumps cannot place the break further: a target may be written
by several passes, cleared, or read before it is written. Hence the
per-pass view (section 3).

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
  - a timeline per output: what it holds after each pass that wrote it.
  Outputs narrower than `<width>` are not sampled; a frame's samples take
  at most 256 MiB of read-back memory.
- **`XTP_PRESENT_TARGET=<width>x<height>[:f<format>][:<n>][*<scale>]`** (or
  `0x<address>`) shows, at every present, the `n`-th texture of that size
  and host format the frame writes, in first-write order, instead of the
  title's planes, stretched over 1920×1080: integers divided by 255,
  floats as they are, one channel as grey, all times `scale`. It lets the
  person see the world's geometry move while the lighting is black.

Hooks (one line each, diagnostics only): `HostList::note` (the op),
`HostList::close` (the last pass's samples), `Host::submit` (submission
order) in `host.rs`; `State::set_targets`, `readable` (UAVs) and
`note_unready` (drops) in `draw.rs`; `queue::present`.

Shortcuts: a dropped op is listed in the pass of the list the thread last
recorded for (it is not told its list), its outputs unknown; copies and
resolves are listed but not sampled.

## 4. Where the image goes black

(in progress)

## 5. The fix

(in progress)
