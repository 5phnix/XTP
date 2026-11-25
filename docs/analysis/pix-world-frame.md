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
- **Walk:** `xtp-pix walk` replays the capture on the same GPU through
  the PIX API (`IPixGpuCaptureAnalysis`), under the same slot, and reads
  each pass's outputs after its last event (`GetTextureBytesFromResourceView`).
  A pass is a run of draws or dispatches with the same outputs (render
  targets, depth, UAVs). With `--accessed` PIX first records which
  resources each event really touched: XTP's shaders index the descriptor
  heap directly (shader model 6.6), so without it only render targets and
  depth are known. `--inputs` measures what an event reads, before it;
  `--depth-diff` counts the depth texels a draw changed. Luminance is
  0.2126 R + 0.7152 G + 0.0722 B of the decoded texels (linear for float
  formats, sRGB views decoded to linear).
