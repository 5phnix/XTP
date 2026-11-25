# Dynamic descriptor allocation in the Series S world on the L4

The fixed allocator recorded **zero slot failures across 350 post-gate
presents** in the detailed L4 replay. On the baseline, nine of 309
post-gate frames lost 3,473 operations to descriptor table allocation,
including 88 direct indexed draws, 54 indirect indexed draws and 34
indirect mesh dispatches. The fixed run still dropped 7,772 indirect mesh
dispatches in its fully counted windows. Shader translation failed for
7,759 and the host pipeline was still being built for 13.

## Original runs

Both diagnostics builds ran on the NVIDIA L4 on 2026-11-25 with the golden
save, `--console series-s`, `autopilot/gameplay.txt`, the audio lock stopgap,
`XTP_SKIP_INTRO=1` and `XTP_WATCHDOG_HOOK=1`. A shader survey shared the
machine during the experiment. Both game processes ran at normal priority
from within two seconds of launch. The fixed build ran first.

| Observation | Baseline | Fixed |
| --- | ---: | ---: |
| Commit | `4db9cd2` | `f39c1cd` |
| Local run directory | `20261125-105307-game` | `20261125-105524-game` |
| World gate, UTC | 10:47:05.629 | 10:43:26.173 |
| Present count at the gate | 2,515 | 3,907 |
| Seconds from launch to gate / from gate to stop | 103.3 / 137.0 | 181.6 / 58.8 |
| Complete frame-origin intervals between post-gate allocator reports | 360 (2,520–2,880) | 120 (3,960–4,080) |
| Terminal allocation failures, world / whole run | 0 / 0 | 0 / 0 |
| Highest logged usage threshold, out of 262,144 slots | 196,700 | 163,845 |
| World cause reports used | 23 | 8 |
| Indexed draws dropped per frame, direct and indirect | 0 | 0 |
| Indirect mesh drops per frame, shader translation failed | 17.05 | 15.91 |
| Indirect mesh drops per frame, pipeline being built (cheap pool) | 0.04 | 0 |
| Indirect mesh drops per frame, descriptor slots | 0 | 0 |

Line 22 of the autopilot confirmed all three world predicates in each
`run.log`. The original local launchers closed while the VM jobs remained
active. Their recovered `summary.txt` files record the checked-out commit
under each job's build lock and the SHA-256 of the executable copied under
that lock. The default frame log samples short windows roughly five seconds
apart. The drop rates above average those windows equally, starting with the
second report after the gate to exclude the transition. The frame-origin
intervals give a directly counted part of the world session. The queue also
recorded present 4,096 on the fixed build, 189 presents after its gate.

Every frame with a terminal allocation failure emits its count, and every
60th frame emits a count even when it is zero. The first failure and each
power of two thereafter also log the total free slots (`free`), the largest
contiguous free span (`largest_free_run`) and the allocation size
(`requested`). Neither original run emitted a failure sample. Those three
fields have no observed values at failure here. Usage thresholds are
reported in eighths of capacity and only bound the allocator's peak.

The earlier RTX 4080 SUPER run `20261125-094629` did have a failure. At
09:47:52.003519 UTC it requested 254 slots with 169,766 of 196,608 in
use, leaving 26,842 free. That build recorded only total headroom.
Both original L4 runs had a larger dynamic region.

## Replays with consecutive frame reports

Each replay used the executable and autopilot copied from its original run.
Both used the same golden save, console mode and 240-second limit. The
additional settings were `XTP_FRAME_LINE=1`, `XTP_LOG=info,xtp_gpu::frame=debug`,
`XTP_DISPATCH_COUNTS=1`, `XTP_MESH_STATS=1` and `XTP_CAPTURE_WORLD=900:8:2:30`.
The measurements include the cost of those diagnostics.

The cause counts cover complete meter windows after the world gate. For
each window, count the individual host `present` lines and recover the
integer number of operations from the reported rate (one decimal place).
Each listed rate had a unique integer numerator for that window.
Only `by cause` rows enter the totals. The subsequent per-shader rows
describe the same drops. The allocation counter covers the whole run.

| Replay observation | Baseline | Fixed |
| --- | ---: | ---: |
| Local run directory | `cxdrops-2aa98b-before` | `cxdrops-2aa98b-after` |
| World gate, UTC / present number | 11:02:56.705 / 2,509 | 11:07:05.583 / 2,266 |
| Host presentations after the gate | 309 | 350 |
| Presentations in complete cause windows | 300 (2,518–2,817) | 341 (2,276–2,616) |
| Complete cause windows | 102 | 119 |
| Allocation failures / affected frames | 3,473 / 9 | 0 / 0 |
| Total operations dropped in complete windows, all causes | 293,059 | 321,443 |

The fixed heap reached at least 229,382 of 262,144 slots used and produced
no failure warning. Each cell below gives the **baseline / fixed** number
of dropped operations. The two columns for pipelines under construction
use the runtime's cheap and heavy compilation pools. For every window,
the cause counts reconcile with its rounded direct and indirect headlines.

| Recorded operation | Table allocation failed | Translation failed | Building, cheap | Building, heavy | BVH builder held |
| --- | ---: | ---: | ---: | ---: | ---: |
| Indexed draw | 88 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| Indirect indexed draw (kind 1) | 54 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| Indirect mesh dispatch (kind 10) | 34 / 0 | 4,815 / 7,759 | 3 / 13 | 0 / 0 | 0 / 0 |
| Draw | 0 / 0 | 0 / 0 | 0 / 1 | 0 / 0 | 0 / 0 |
| Indirect draw (kind 0) | 1,430 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| Mesh dispatch | 104 / 0 | 26 / 63 | 0 / 0 | 0 / 0 | 0 / 0 |
| Dispatch | 80 / 0 | 51,731 / 54,664 | 1 / 2 | 300 / 341 | 139,446 / 155,329 |
| Indirect dispatch (kind 2) | 1,683 / 0 | 41,691 / 52,313 | 0 / 1 | 51,573 / 50,957 | 0 / 0 |

**Submitted work with empty arguments.** Over the same windows the GPU
counted 578,446 / 647,989 submitted mesh records with at least one zero
group dimension. The completion probe separately classified 679,188 /
763,799 indirect executions as having no arguments, inspecting up to 64
records per execution. These are different units. Both counters describe
work that reached the host and are excluded from the drop totals. Neither
counter reads the meshlet count inside a shader.

### Baseline failure samples

All 3,473 baseline failures fell on frame-origin tokens 2,751 through 2,759.
In order, the nine frames lost 118, 291, 497, 284, 629, 396, 485, 357 and
416 allocations. All 12 warnings sampled at failure numbers 1 through 2,048
(powers of two) had `largest_free_run < requested <= free`. The request
could fit in the total free space, but no contiguous span could hold it.

| Failure number | Requested slots | Total free | Largest free run |
| ---: | ---: | ---: | ---: |
| 1 | 179 | 64,242 | 144 |
| 2 | 130 | 63,382 | 126 |
| 4 | 130 | 63,268 | 126 |
| 8 | 258 | 63,454 | 126 |
| 16 | 258 | 62,306 | 130 |
| 32 | 185 | 61,327 | 130 |
| 64 | 185 | 59,284 | 130 |
| 128 | 179 | 61,093 | 136 |
| 256 | 179 | 61,093 | 136 |
| 512 | 130 | 48,627 | 103 |
| 1,024 | 179 | 54,661 | 126 |
| 2,048 | 258 | 50,768 | 125 |

## Evidence

The four directories named above are under `E:\xtp-data\cloud-runs\`.
Each has the run log, summary and a `capture` directory. The replay
directories also contain `world-causes.csv` with the timestamp, denominator
and count for each cause in each complete window.

The baseline replay captured frames 2,525 and 2,555 and the fixed replay
captured 2,283 and 2,313 after their gates. The second capture in each pair
shows an intro card. Frame 4,096 from the original fixed run shows loading
artwork. The world predicates and the rendering counters establish world
work. The captures do not establish a visible scene improvement.
