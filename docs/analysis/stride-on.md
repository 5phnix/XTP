# Stride table and run-time buffer views

**Keep both switches opt-in.** On the L4 after round 4b, table only lost
the D3D12 device 35 s into gameplay. The off control also lost the device.
With both enabled, a median frame took 8.03 s, including 4.15 s copying
12.12 GB of buffers. The title then crashed. Earlier PC captures showed
that the two switches together can recover geometry.

## Round 4b on the L4

Three separate diagnostics processes ran main `5018dd4`, which includes
`41d6cbc`. Each used Series S, the same golden save, the gameplay
autopilot, the L4 audio stopgap and a 600 s process limit. All set
`XTP_SKIP_INTRO=1`, `XTP_RAW_STRUCTURED=1` and `XTP_TEXTURE_POOL=1`.
Both feature switches were set explicitly in every process.

| Gameplay measurement | Both off | Table only | Both on |
|---|---:|---:|---:|
| Frame-rate reports after handoff | 6 | 4 | 21 |
| Median frame rate (fps) | 0.35 | 0.40 | 0.10 |
| Median frame time (ms) | 2,776.4 | 2,518.8 | 8,034.6 |
| Median refresh steps/frame | 3,596 | 2,419 | 266,205 |
| Median host buffer copies (GB/frame) | 0.051 | 0.030 | 12.120 |
| Median time in those copies (ms/frame) | 23.6 | 14.5 | 4,146.8 |
| Peak raw-view slots in a heap (capacity 65,536) | Table disabled | <8,192 | 32,768–40,959 |
| First failure after handoff | Device hung, 69 s | Device hung, 35 s | Access violation, ~206 s |

Each median uses the host's periodic per-frame reports after the
autopilot released control, including startup work still in progress.
FPS is logged to one decimal before taking the median. The sessions
ended at different points in the opening. Even in the first 30 s after
handoff, median refresh steps/frame were 3,818, 2,419 and 226,949
respectively (3, 4 and 2 reports). The both-on run peaked at 314,597
steps/frame and 17.004 GB/frame. Bytes are decimal and count host
buffer copies during preparation.

The off and table-only processes exited with `DXGI_ERROR_DEVICE_REMOVED`
and reason `DXGI_ERROR_DEVICE_HUNG`. The both-on crash log records a
write to address zero at title RVA `0x23cf683`. None logged a raw-view
refusal or the 45,000-instance pool assertion. Instance occupancy was
not measured. The three local log folders under
`E:\xtp-data\cloud-runs\` are `20261125-131529-game` (off),
`20261125-131544-game` (table) and `20261125-131558-game` (both).

The imagecheck trigger was `XTP_CAPTURE_WORLD=1000:22:3:3`, with
`XTP_PASS_DUMP=0`, `XTP_PASS_INPUTS=1cd9bc634417a981` and the same
three frame-probe hashes as [imagecheck](image-status.md#2-sessions).
Only the control captured both lighting inputs. At frame 2807 (+25
from the trigger), the 24 sampled rows of main depth (`s0+432`) and
albedo (`s0+584`) at 1280 × 720 were both 0.0% nonzero.
Table only never armed. Both on armed at frame 2362 and exited before
the first requested capture (2384). These runs therefore provide no
matched three-arm coverage comparison or seven-minute gameplay result.

## Round 4b on the PC

The both-on run `stall-stride4b-1` had one frame-rate report after the
autopilot handed back control. It reported 953.6 ms/frame (1.0 fps),
226,575 refresh steps/frame and 2.270 GB of host buffer copies/frame in
346.4 ms. The step counter counts entries visited during submission.
The copy counter measures bytes after overlapping refresh ranges are
merged and also includes ordinary host buffer copies. The raw-view pool
crossed 24,576 of 65,536 slots. It logs each eighth of capacity, so the
last threshold gives a lower bound. The log ends 10 s after handoff.

## Earlier PC measurements

These are Series S runs on the RTX 4080 SUPER. The baseline is `imgsrc-1`
with both switches off. `imagecheck-4` used both switches on at `c44a716`.
Both used `XTP_RAW_STRUCTURED=1` and `XTP_TEXTURE_POOL=1`. The later build
also changed the light loop and texture staging, so the differences include
those changes. See [Image status, sections 2–5](image-status.md#2-sessions).

| Gameplay measurement | Both off | Both on |
|---|---:|---:|
| Distinct shaders refused, all causes (whole run) | 158 | 73 |
| Compute shaders refused for unknown stride (whole run) | 73 | 0 |
| Median frame rate (fps) | 3.9 | 0.6 |
| Median frame time (ms) | 254.7 | 1,720.8 |
| Median frame preparation (ms) | 73.9 | 1,256.6 |
| Median GPU time per frame (ms) | 166.2 | 242.4 |
| Median dedicated process VRAM (MiB) | 12,565 | 13,117 |

The refusal counts use distinct stage/hash pairs from the shader translation
warnings in each `run.log`. Frame times are the medians of the 21 and 9
gameplay frame-rate reports in `play.out.txt`. VRAM uses `xtp`'s
`dedicated_mb` samples in `gpu.csv` after the autopilot handed back control
through the last log timestamp (38 and 16 samples).

| Run and world frame | Main depth covered | G-buffer albedo nonzero | Albedo mean (R, G, B) |
|---|---:|---:|---|
| Both off, 675 | 20.1% | 12.0% | 0.004, 0.003, 0.002 |
| Both on, 618 | 57.1% | 39.5% | 0.071, 0.047, 0.041 |
| Both on, 621 | 84.2% | 59.9% | 0.049, 0.045, 0.041 |
| Both on, 624 | 64.4% | 46.6% | 0.054, 0.042, 0.031 |

These are the dumps' 24 sampled rows at 1280 × 720 before lighting, with
albedo at `s0+584`. Nonzero counts a texel with any nonzero channel,
including alpha. RGB means include all sampled texels.

### Which dispatches were dropped

For each direct dispatch, the pass dump gives the compute hash on success
and the pipeline address on a drop. Joining the latter to the same run's
`first bind` records gives these counts. Each cell is **dropped / total**.

| Compute program | Off, 675 | On, 618 | On, 621 | On, 624 |
|---|---:|---:|---:|---:|
| `48426bae580a080c` | 0/1 | 0/1 | 0/1 | 0/1 |
| `969c1cef8b783b39` | 0/5 | 0/5 | 0/5 | 0/5 |
| `e30970f1727063fd` | 76/76 | 0/83 | 0/85 | 0/146 |

The earlier 921/922 and 138/139 counts in [PIX world frame, section 6.3](pix-world-frame.md#63-what-is-dropped)
describe whole passes. A pass can include several programs. For example,
in baseline frame 675, P223 ran one dispatch of `969c1cef8b783b39` and
dropped 76 of `e30970f1727063fd`. In frame 621, the 162 drops in P245
belong to ray-tracing structure builder programs. Its one dispatch of
`48426bae580a080c` ran.

## Earlier table-only runs

The earlier PC runs `stride-2` and `stride-2b` explicitly set table=1 and
views=0 on `600e7f7`. They reported 2.1 and 2.5 fps (478 and 400 ms/frame),
against 4.3 fps in the off run `stride-3a`. Both table-only runs logged
zero unknown-stride compute refusals. Both also ran out of raw view slots
about four to six seconds into gameplay, causing subsequent buffer reads through
unavailable views to return zero. Neither run supplies comparable depth
and albedo pass measurements. Later revisions changed slot reuse and
retirement. See [the stride session history](refusals.md#8-is-xtp_stride_table1-safe-to-turn-on-in-the-image-sessions-now).

With both switches on, imagecheck saw 110,000–220,000 buffer refreshes
and 8–17 GB copied per frame. All 65,536 raw view slots were in use about
30 seconds into gameplay. Task `472490fc` tracks the run-time view refresh
cost. The header, view-budget, remapped-table and retirement fixes from
`6cc97dd7` landed on main in `a090f22`; these PC measurements predate them.

The earlier same-revision L4 comparison produced no game frames. The
off and table-only launches of `244cf78` waited for the shared VM lock
and were cancelled at handoff. The both-on launch was waiting for the
off run to start. The initial PC control (`stride-5d4d8f-off1`) reached
its 120 s limit before its first frame. Neither attempt provides a
post-fix table-only measurement to support changing the default.
