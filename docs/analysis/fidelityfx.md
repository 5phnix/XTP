# FidelityFX on Series S: answered in HLE, and the methods FSR4 called

For the task "Series S: HLE of the FidelityFX calls (port VIctory #1284)
and ClearUnorderedAccessView Uint/Float (port VIctory #948), plus the
Series S method gaps", build 1.0.2.1 of BTP 6. Offsets are kept in the
maintainers' private notes; no title bytes or listings are quoted. AMD's
DLLs were not disassembled: their licence forbids it, and nothing here
needs it.

**Answer.** In Series S the title runs AMD's FidelityFX upscaler (FSR4,
`amd_fidelityfx_upscaler_xbox.dll`, loaded by
`amd_fidelityfx_loader_xbox.dll`). XTP cannot translate its 20 console
compute programs, and without the DLLs the title divides by zero.
XTP now answers the title's five FidelityFX calls itself
(`crates/xtp-hle/src/modules/ffx.rs`): the queries as AMD's open FSR 2/3
code answers them, and each dispatch with a bilinear upscale of XTP's own
on the host GPU (`xtp_gpu::list::upscale`). No AMD code runs.
`XTP_FSR4=1` gives the title AMD's DLLs back. `ClearUnorderedAccessView
Uint`/`Float` (list 0x1a8/0x1b0), which FSR4 called, are implemented with
the console handles translated into host ones.

## 1. What the title asks of FidelityFX

The title imports `ffxCreateContext`, `ffxDestroyContext`, `ffxQuery` and
`ffxDispatch` from the loader; the loader loads the upscaler, and both
export the same five calls (`ffxConfigure` too). Six call sites:

| Call | Description (PC `ffx_api` layouts) | What the title does with it |
|---|---|---|
| create | 0x10000 upscale (flags, largest render 1280×720 and output 1920×1080), chained to 0x1000002 (console backend, the device at +0x10) and 0x1000b (version 0x1001001) | result not read |
| query 0x10004 | jitter phase count: render width, display width, out | out is a field of the title's object, the divisor of the division above; a title setting (-1: none) may replace it |
| query 0x10005 | jitter offset: index, phase count, out x, out y | out are stack locals the title does not initialise; it would move the camera by them and keep them for the dispatch. Never reached in XTP's sessions: the title jitters by a sequence of its own (section 7) |
| dispatch 0x10001 | `ffxDispatchDescUpscale`, 432 bytes | result not read; then the frame index advances modulo the phase count |
| query 0x10008 | upscaler GPU memory: out | out is an uninitialised local; the title adds its first word to a memory total |
| destroy | the context | |

Each description has `next` = 0 except the create chain. The dispatch
description, as the title fills it: the command list at +0x10 (the title's own list, ours), seven
48-byte resources from +0x18 (colour, depth, motion vectors with state
0xc, exposure, reactive and transparency masks, the output at +0x138
with state 2, unordered access), the jitter at +0x168 (the query's
offsets, negated, as the FidelityFX samples pass them), motion vector
scale, render size (the colour texture's size) at +0x178, output size
at +0x180, sharpening when the title's sharpness is above 0, frame time,
pre-exposure 1.0, reset, the camera's near, far and field of view.

So the title can read three things back from FidelityFX: the phase count,
the jitter offsets, and the memory figure. Without answers it divides by
zero, and would jitter its camera by stack garbage and add garbage to its
memory total. In XTP's Series S sessions only the phase count and the
memory query were asked: the dispatches carry the title's own jitter.

## 2. The stand-in

- **Phase count**: 8 × (display ÷ render)², cut to an integer
  (`ffxFsr3UpscalerGetJitterPhaseCount`): 18 for 1280 → 1920. #730's log
  with AMD's PC FSR 3.1.6 gave 32 for 1920 → 3840, the same formula.
- **Jitter offset**: Halton (2, 3) of `index % phases + 1`, minus 0.5
  (`ffxFsr3UpscalerGetJitterOffset`). Kept for a title or a setting that
  asks; BTP 6 did not in XTP's sessions.
- **GPU memory**: 0 total, 0 aliasable: the pass takes no title memory.
- **Dispatch**: a compute pass of XTP's (HLSL, `cs_5_1`, compiled once by
  the system's compiler) on the title's host list. Each output pixel
  inside the output size reads the colour input bilinearly at its place
  in the input, `(x + 0.5) × render ÷ output + jitter`, clamped to the
  rendered region. FidelityFX's upscalers place input samples that way:
  the texel at `i` holds the scene at `i + 0.5 − jitter`. Depth, motion
  vectors, exposure and the masks are not read; sharpening and reset are
  ignored. The colour input ends readable by compute shaders and the
  output in unordered access, the states the title declares to
  FidelityFX. The views sit in two dynamic slots of XTP's own view heap;
  afterwards the list's draw state is told to bind the title's pipeline,
  root signature and heaps again (`State::forget_host`).
- **Create/destroy**: a context remembering the largest sizes (a dispatch
  past them is logged once; AMD's upscaler would refuse it). Destroy frees
  only contexts it made, once.
- Unknown description types are logged once and refused
  (`FFX_API_RETURN_ERROR_UNKNOWN_DESCTYPE`).

The output, a 1920×1080 R9G9B9E5 render target with unordered access in
title memory (xcl #914), has a host texture in XTP: its stand-in format
R16G16B16A16_FLOAT (the logs' "2 of 67 as 10"), which takes typed
unordered writes. VIctory's lookup found none for it (#1284's "an upscale
resource without a host texture slot=6").

## 3. UAV clears (list 0x1a8, 0x1b0)

FSR4 calls `ClearUnorderedAccessViewUint` (session C, the
only caller in XTP's Series S logs; the title itself does not). Both
handles are console ones: slots of title memory holding XTP's view
records (the GPU one in the bound shader-visible heap, the CPU one in a
CPU-only heap). Handed to a host driver as they are, they are garbage: on
AMD the process ended in the driver at the world's start (xcl #1291).
XTP makes its own descriptors of the view the slots hold: one in a
dynamic slot of its own shader-visible heap, bound for the clear, one in
a CPU-only heap of its own (`descriptor::host::make_view`), then clears
the host copy: a texture (moved to unordered access), or a buffer range's
host copy (copied back to title memory after the submission, as shader
writes are). Float clears of raw and structured buffer views are refused,
as the host refuses them.

With the stand-in on, FSR4 does not run, so these clears matter for
`XTP_FSR4=1` and for any title that clears a UAV.

## 4. Series S method gaps

xcl #1337 lists the methods xcl stubs on Series S just before the
title's assert: list #75, #101, #126, #131, #132, #142, queue #31 (newer
layout), allocator #9. In XTP's newer tables:

| xcl | Slot | XTP |
|---|---|---|
| list #75 | 0x258 | old slot 73, "pixel shader depth order": ignored (the title's state flush passes a byte and reads nothing back) |
| list #101 | 0x328 | old slot 99, `DriverHintX`: returns 0 |
| list #126, #131, #132, #142 | 0x3f0, 0x418, 0x420, 0x470 | shading rate, both syncs, 0x470: present before this task |
| queue #31 | 0xf8 | old slot 29, `CopyPageMappingsBatchX` |
| allocator #9 | 0x48 | allocation callback: S_OK |

Nothing was missing there. XTP's own Series S logs name four other
methods not implemented: list 0x1a8 (done here), pipeline state 0x58
(FSR4's; no longer called), and device 0x218 and 0x1c8, which the title
calls itself and which
fail with `E_NOTIMPL` as before; the title goes on after both.

## 5. Page scatters in runs (VIctory #1264)

Session 5 of the series-s task (`20261124-173913`, diagnostic build)
logged every page mapping: 220,974 single-page maps, 127,695 of them
before the world, 82,736 of them unmaps (zero pages). Nearly all are
`XMemMapPhysicalPagesScatter` entries, which XTP maps one page per call.
They fall in 42,837 runs of consecutive addresses (5.2 pages on average,
at most 243), which #1264 would map in one call each. Each page map took
13 µs (median, p90 46 µs, logging included): about 3 s over the whole
session, spread over the title's threads, 1.7 s of it before the world.

The fix of the series-s task (DirectStorage reads its page list when the
read runs) is about which pages a read writes, not about mappings: the
two do not interact. #1264 would save at most a second or so of a load
that takes 10 to 18 s to the world. It is left for a task that owns
`xmem.rs` (the views1122 task holds it now), with this count as its
baseline.

## 6. Dynamic descriptor slots on capped heaps (VIctory #1349)

BTP 6's shader-visible view heap has 1,245,184 slots. The RTX 4080 SUPER
(resource binding tier 3) makes shader-visible view heaps of at most
1,000,000 descriptors, not one more (measured with a D3D12 probe).
XTP capped the host heap there and kept 65,536 slots
dynamic: the Series S world of 18:11 reached a peak of 57,364, and session
5 dropped draws for want of slots. As #1349 does for capped heaps, XTP now
keeps 2^18 (262,144) slots dynamic and mirrors 737,856 title slots.
None of XTP's Series S runs wrote a title slot past 934,464 (the log line
"title slot past the host heap" never came); past 737,856 that line now
tells.

Not taken from #1349: dropping the middle step (a heap the host takes
with 2^16 dynamic slots on top of every title slot keeps them all, as
before: #1349 would cut title slots there, which #747 does not ask), and
its draw.rs half (fresh tables trimmed to the entries used), which is the
drops task's file.

## 7. Sessions

Through `E:\tools\stall-probe\session.ps1` (Series S, golden data,
autopilot, `XTP_SKIP_INTRO=1`, `XTP_WATCHDOG_HOOK=1`), under
`measure.lock`, on the RTX 4080 SUPER. Times UTC.

**Session 1, HLE** (`20261124-182836`, build of the first commit,
3e65bac at `refs/ffx/wip`, + #1349; the autopilot's default 30 s in the
world):
- The world at 18:29:00 (frame 634), 24 s after the start; the
  autopilot ended the title 30 s later, exit code 0. No crash.
- `ffx`: a context 1280×720 → 1920×1080 (flags 0x29), phase count 18;
  the first dispatch recorded the upscale (`ran=true`). Its description:
  colour `[2, 4, 1280, 720, 1, 1, 0, 2]` (2D, FidelityFX's
  R16G16B16A16_FLOAT), output `[2, 29, 1920, 1080, 1, 1, 0, 2]`, states
  `[12, 12, 12, 0, 0, 0, 2]` (the reactive and transparency masks
  absent), jitter (-0, -0).
- FSR4's traces are gone: no "no DXIL part" refusal, no list 0x1a8, no
  pipeline 0x58. Device 0x218 and 0x1c8 still come, from the title.
- The capped heap: 737,856 title slots and 262,144 dynamic ones; peak
  32,774 in use; no title slot past 737,856.
- The world's frame line (32 s): 0.5 fps, 16 draws a frame, 483 dropped
  for want of a pipeline with 394 being made, the longest submission
  14.8 s. The shader cache is keyed by the executable's hash, so this
  new build translated its 1,208 shaders again: these are cold-cache
  numbers, not the upscale's cost.
- No world capture: no frame reached the 150 draws `XTP_CAPTURE_WORLD`
  asked for.

Sessions 2 to 4: one diagnostic build of this branch (its first three
commits on be92c57, which has #1349; the later ones add log lines and
text), `-Measure 120 -Limit 240`, at least 10 minutes apart. Session 2
ran on a cold shader cache (a new build), 3 and 4 on a warm one.

| | 2: HLE | 3: `XTP_FSR4=1` | 4: HLE |
|---|---|---|---|
| Run (`E:\xtp-data\cache\play\`) | `20261124-185341` | `20261124-190642` | `20261124-192036` |
| World after | 27 s | 24 s | 20 s |
| End | the autopilot's, after 120 s; exit 0 | **device removed** (`DEVICE_HUNG`) 110 s into the world | **device removed** (`DEVICE_HUNG`) 105 s into the world |
| fps (median, lowest, highest) | 3.8, 2.6, 7.5 | 4.8, 3.9, 9.5 | 5.0, 3.6, 7.7 |
| Draws, dropped per frame (median) | 481, 1,097 | 516, 1,045 | 490, 1,050 |
| FSR4 traces | none | 20 "no DXIL part", pipeline 0x58, its passes "falling back to NewBE" | none |
| Black frames after a lit one | 9 | 9 | 8 |
| Dynamic slots (peak of 262,144) | 65,543 | 65,543 | 65,573 |

- **The removals are not FidelityFX's.** Session 3 runs no upscale and
  session 4 no UAV clear (FSR4 is not loaded), and both end the same way:
  no list submitted, 31 or 32 in flight, no breadcrumbs. Sessions of other
  tasks on main that day, which have neither, ended the same way 68 s
  (copyback B) and 74 s (ds-guards after) into the world. Session 2, on a
  cold cache, went through the world's opening more slowly and may not
  have reached the point where it happens. The shader the hang task
  suspects, compute d3e44e754b3eb6aa, is translated and bound in every
  HLE session: it is the title's, not FSR4's.
- **The image is the same.** The world's captures of sessions 3 and 4
  are byte for byte identical (frames 800 and 1000; 568 to 571 of
  session 4 and 687 of session 3): the world's opening credits over
  black. The 3D scene reaches the screen in neither (about 1,050 draws
  dropped a frame on the RTX 4080, most of them wave64 mesh programs this
  host refuses: the mesh64 task). These captures cannot judge the
  upscale: a dump of its input and output (`XTP_DUMP_TARGETS`) is the
  next step.
- **The frame rate** is within the sessions' spread: FSR4's refused
  passes cost nothing, and the bilinear pass (one 1920×1080 dispatch) is
  not visible in the frame line.
- **The title's own jitter.** The jitter offset query (0x10005) was never
  asked: the dispatches carry the title's own offsets. After three
  dispatches at (−0, −0), the n-th is (−w(n·0.7548777), −w(n·0.5698403)),
  w wrapping to [−0.5, 0.5): (0.2451, 0.4302), (0.4902, −0.1397),
  (−0.2646, 0.2905), (−0.0195, −0.2794), (0.2256, 0.1508). That is the
  R2 sequence (the plastic number's), negated as FidelityFX's samples
  negate the camera's jitter. Whether it wraps at the phase count was not
  seen (8 dispatches logged).
- The black frames after a lit one come in both paths alike, every 70 to
  110 frames in the world.

## 8. Critical review

**#1284** (AwkGrep256, head f96ad82, open, not merged). It answers the
title's calls with AMD's PC loader for D3D12, swapping the console
backend, list and textures for the host's. Taken: the module standing in
for both DLLs, the five exports, the description layouts, logging unknown
types once. Checked against the title's own code rather than trusted:
the create chain (upscale, console backend, version), the 432-byte
dispatch description and its offsets, the resource states. Not taken:
loading AMD's DLLs (their licence; XTP ships none of AMD's code), the
upscaler picker and render-scale patches (Series X diagnostics for
1.0.2.1).

What #1284 leaves to AMD and an HLE must answer: the jitter offset query
(0x10005) and the memory query (0x10008), both written into the title's
uninitialised locals. #1284 forwards them; a stand-in that answered only
the phase count, as the brief first put it, would leave the camera
jittered by stack garbage and the title's memory total corrupted.

Doubts and answers:
- *Is 18 the count the title expects?* It is AMD's formula for the
  title's sizes, and AMD's PC upscaler gave the same formula's 32 for
  1920 → 3840 (#730). FSR4's own console DLL was not observed. With no
  temporal accumulation the cycle's length only sets how often the
  title's jitter pattern repeats; what must hold is that it is not 0.
- *Does the stand-in avoid the device removals?* No. Both paths lose the
  device about 105 to 110 s into the world (section 7), and nothing of
  FidelityFX's runs in the HLE one. What the stand-in removes is FSR4's
  20 refused programs, its missing pipeline method and its UAV clears;
  what it gives is an output the title's later passes can read, where
  FSR4's refused programs leave theirs unwritten.
- *Are the jitter offsets what the title uses?* No: it never asks for
  them (section 7). It jitters its camera by its own R2 sequence and
  passes that to the dispatch, so the Halton answer is dead code for
  BTP 6 1.0.2.1 and only the phase count matters. Zero offsets from XTP
  would not steady the image: the title would not see them.
- *Does the upscale take the jitter back the right way?* FSR replaces the
  title's TAA, so a jittered camera with no accumulation moves the image
  by up to half a render pixel each frame. The pass samples at
  `+ jitter`: FSR 2's own reconstruction puts the texel at `i` at
  `i + 0.5 − jitter` (`fSrcUnjitteredPos`), and the WARP test checks the
  pass against that with a quarter-texel jitter. The title's dispatch
  jitter is the negation of its camera's, as FidelityFX's samples pass
  it, so it follows that convention. Not checked on the title's frames:
  the world's scene does not reach the screen on this host yet (section
  7), so consecutive captures cannot show whether the image holds still.
  Edges may crawl a little even when it does, as their aliasing changes
  with the sample position.
- *Is the output resource and state right?* The output is the title's
  render target with unordered access (state 2 in the description); XTP's
  host copy of it is in its R16G16B16A16_FLOAT stand-in. The pass writes
  its top-left output-size pixels and leaves it in unordered access.
- *Does the pass break the title's later draws?* It binds its own heap,
  root signature and pipeline. `State::forget_host` makes the next draw or
  dispatch bind the title's again (the indirect clamp does the same in
  draw.rs). The title's root arguments, render targets and vertex
  buffers are kept.

**#948** (Sakura-Byte, head 35802af, open). Not applied: its helpers
(`prepare_clear_uav`, `uav_cpu`, `ensure_view_heap`) target VIctory's
`ffe310e` descriptor and draw code, which XTP has rewritten since. Taken
as behaviour and from its review: textures marked host-written, float
clears refused on raw and structured buffer views, a UAV barrier after
the clear. The handle translation is xcl #1291's idea (GPL-2.0-only, no
code taken), built on XTP's view records. Its `DiscardResource` (0x1b8)
is not taken: neither the title nor FSR4 called it in XTP's logs.

**#1349** (wbkepler, head eb5fea7): see section 6.

## 9. What is left

- The world's device removal, 105 to 110 s in, in both paths (section
  7): not FidelityFX's; the TDR tasks have the runs.
- The upscale on the title's own frames: consecutive captures of a
  world whose scene reaches the screen (after the mesh64 task), to see
  that the jitter is taken back and how soft the bilinear output is.
- Picture quality: FSR4's own network (route B) or an
  edge-aware spatial upscale, if the bilinear output is too soft.
- Device 0x218 and 0x1c8, which the title calls itself.
- VIctory #1264 for `xmem.rs` (section 5).
- VIctory #1444 (a view heap released and remade at the same address
  while a list records skips `SetDescriptorHeaps`): XTP has the same
  gap. `State::set_host_heaps` compares title heap addresses
  (`draw.rs`); comparing host heaps, as `host_pipeline` does for
  pipelines, would close it. It is `draw.rs`'s task's, not this one's.
