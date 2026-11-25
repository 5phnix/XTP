# One world frame in PIX: where the light goes and where meshes vanish

Task pixcap (eccb80f1), 2026-11-25, on the person's PC (RTX 4080 SUPER,
Series S mode). One world frame of `main` captured with PIX for Windows
2611.19 and walked pass by pass on the D3D12 side, against the person's
reference for the start of the game: night, but you see well; street
lamps light the street and the pavement, the facades take a teal and
green ambient light, characters are lit from the side, palm trees, signs
and cars are readable. Programs are named by hash only; captures, tables
and screenshots stay under `E:\xtp-data\pix\`, none is in the repository.

## 1. Summary

(Filled in from the capture: see the sections below.)

## 2. How the frame was captured

- **Build:** `main` at `10e7f2d` (be56d45 plus the fog fix, the
  ray-tracing drop log and its stand-in, off by default), diagnostics
  build (`cargo build --profile diag -p xtp --features diagnostics`).
- **Session:** `tools/pix/capture-world.ps1 -Name world-1 -WorldDraws 2000
  -WorldAfter 40 -Settle 20`. It waits for the PC's game slot as
  `session.ps1` does (`E:\xtp-data\measure.lock`, no game `xtp.exe`),
  copies the golden data, runs the precook, and starts `xtp.exe run …
  --autopilot autopilot/gameplay.txt --console series-s` through
  `xtp-pix capture`, which launches it for GPU capture with the PIX API.
  Environment: `XTP_SKIP_INTRO=1`, `XTP_RAW_STRUCTURED=1`, Series S, and
  `XTP_D3D12_DRED=1` so that XTP names its host textures and buffers and
  puts a marker before each draw and dispatch with its program hashes
  (`draw.rs`, `op_text`): that is how a PIX event is tied to a program.
  `XTP_CAPTURE_WORLD=2000:40:1:1` writes one PNG 40 frames after the first
  frame with 2,000 draws; once it appears, and 20 s more, `xtp-pix` takes
  a one-frame capture (present to present) and ends the title.
- **PIX's replay is not available on this PC.** Every PIX analysis
  (`xtp-pix walk`, and `pixtool open-capture … save-resource` alike) stops
  with "This feature requires developer mode to be enabled on the analysis
  device" (`E_PIX_FEATURE_REQUIRES_DEVELOPER_MODE`): Windows Developer Mode
  is off, and turning it on is the person's decision (an administrator
  setting). What PIX gives without a replay is the frame's structure:
  `xtp-pix events` (every call of every queue, with its arguments) and
  `xtp-pix walk --no-replay` (each draw and dispatch with its pipeline's
  depth, blend and cull state and the render targets, depth and views it
  binds).
- **The values come from RenderDoc** (1.47, `E:\tools\gpu\RenderDoc`),
  which replays without Developer Mode: `xtp-rdoc` (same tools folder)
  captures through RenderDoc's injection and target control, then replays
  through its replay API. A pass is a run of draws or dispatches with the
  same outputs: render targets, depth, and the textures the shaders write
  through unordered access, heap-indexed ones included (RenderDoc's
  descriptor access). Each pass's outputs are read after its last event;
  `--inputs` reads what an event reads, just before it; `--depth-diff`
  counts the depth texels a draw changed. Luminance is 0.2126 R + 0.7152 G
  + 0.0722 B of the decoded texels (linear for float formats, sRGB decoded
  to linear); one-channel targets give their channel.
- **Checked on a known frame:** `tools/pix/selftest.ps1` renders, on WARP
  only, a draw of a constant colour into an RGBA16F target with depth, and
  a dispatch that reads it and writes 4 × red into an R32F texture. Both
  tools capture it; `xtp-rdoc` reads back 0.294125 (the colour's
  luminance), depth 0.25 on all 32,768 texels, the dispatch's input at
  0.294125 and its output at 2.0, exactly; `xtp-pix` passes its capture
  and its walk without replay, and reports its replay as skipped.
