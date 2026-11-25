# Stride table and run-time buffer views

**Keep both switches opt-in.** With both on, the PC captured substantially
more geometry, but median frame time rose from 255 ms to 1,721 ms. The
available table-only runs also lost frame rate and exhausted the raw view
pool. No table-only world capture on the revised implementation establishes
that it preserves the geometry gain at an acceptable frame time.

## PC measurements

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

## Table only and the default decision

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

The requested same-revision L4 comparison produced no game frames. The
off and table-only launches of `244cf78` waited for the shared VM lock
and were cancelled at handoff. The both-on launch was waiting for the
off run to start. The initial PC control (`stride-5d4d8f-off1`) reached
its 120 s limit before its first frame. Neither attempt provides a
post-fix table-only measurement to support changing the default. The available captures also leave instance-pool
occupancy and survival beyond seven minutes unverified.
