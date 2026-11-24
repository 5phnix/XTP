# FidelityFX on Series S: answered in HLE, and the methods FSR4 called

For the task "Series S: HLE of the FidelityFX calls (port VIctory #1284)
and ClearUnorderedAccessView Uint/Float (port VIctory #948), plus the
Series S method gaps", 2026-11-24, build 1.0.2.1 of BTP 6. RVAs are
relative to `game_scarlett_master.exe`. The title's code was read for
interoperability (cdb on the image, no title bytes or listings quoted);
AMD's DLLs were not disassembled: their licence forbids it, and nothing
here needs it.

**Answer.** In Series S the title runs AMD's FidelityFX upscaler (FSR4,
`amd_fidelityfx_upscaler_xbox.dll`, loaded by
`amd_fidelityfx_loader_xbox.dll`). XTP cannot translate its 20 console
compute programs, and without the DLLs the title divides by zero at
title+0x28f03e8. XTP now answers the title's five FidelityFX calls itself
(`crates/xtp-hle/src/modules/ffx.rs`): the queries as AMD's open FSR 2/3
code answers them, and each dispatch with a bilinear upscale of XTP's own
on the host GPU (`xtp_gpu::list::upscale`). No AMD code runs.
`XTP_FSR4=1` gives the title AMD's DLLs back. `ClearUnorderedAccessView
Uint`/`Float` (list 0x1a8/0x1b0), which FSR4 called, are implemented with
the console handles translated into host ones.

## 1. What the title asks of FidelityFX

The title imports `ffxCreateContext`, `ffxDestroyContext`, `ffxQuery` and
`ffxDispatch` from the loader (import thunks title+0x5f3ff90..+0x5f3ffc0);
the loader loads the upscaler, and both export the same five calls
(`ffxConfigure` too). Six call sites:

| Site | Call | Description (PC `ffx_api` layouts) | What the title does with it |
|---|---|---|---|
| +0x28ef2cc | create | 0x10000 upscale (flags, largest render 1280×720 and output 1920×1080), chained to 0x1000002 (console backend, the device at +0x10) and 0x1000b (version 0x1001001) | result not read |
| +0x28ef30f | query 0x10004 | jitter phase count: render width, display width, out | out is the object's +0x10c, the divisor at title+0x28f03e8; a title setting at +0x22c (-1: none) may replace it |
| +0x28ef625 | query 0x10005 | jitter offset: index, phase count, out x, out y | out are stack locals the title does not initialise; it moves the camera by them and keeps them for the dispatch |
| +0x28f03db | dispatch 0x10001 | `ffxDispatchDescUpscale`, 432 bytes | result not read; then the frame index advances modulo the phase count |
| +0x28f0651 | query 0x10008 | upscaler GPU memory: out | out is an uninitialised local; the title adds its first word to a memory total |
| +0x28ef512 | destroy | the context | |

Each description has `next` = 0 (title+0xdfe240 clears it) except the
create chain. The dispatch description, as title+0x28f016e..+0x28f03cc
fills it: the command list at +0x10 (the title's own list, ours), seven
48-byte resources from +0x18 (colour, depth, motion vectors with state
0xc, exposure, reactive and transparency masks, the output at +0x138
with state 2, unordered access), the jitter at +0x168 (the query's
offsets, negated, as the FidelityFX samples pass them), motion vector
scale, render size (the colour texture's size) at +0x178, output size
at +0x180, sharpening when the title's sharpness is above 0, frame time,
pre-exposure 1.0, reset, the camera's near, far and field of view.

So the title reads three things back from FidelityFX: the phase count,
the jitter offsets, and the memory figure. Without answers it divides by
zero, jitters its camera by stack garbage and adds garbage to its memory
total.

## 2. The stand-in

- **Phase count**: 8 × (display ÷ render)², cut to an integer
  (`ffxFsr3UpscalerGetJitterPhaseCount`): 18 for 1280 → 1920. #730's log
  with AMD's PC FSR 3.1.6 gave 32 for 1920 → 3840, the same formula.
- **Jitter offset**: Halton (2, 3) of `index % phases + 1`, minus 0.5
  (`ffxFsr3UpscalerGetJitterOffset`).
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

FSR4 calls `ClearUnorderedAccessViewUint` from its +0x72f4 (session C, the
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

## 4. Series S method gaps (watch item 85)

xcl #1337 lists the methods xcl stubs on Series S just before the
title's assert: list #75, #101, #126, #131, #132, #142, queue #31 (newer
layout), allocator #9. In XTP's newer tables:

| xcl | Slot | XTP |
|---|---|---|
| list #75 | 0x258 | old slot 73, "pixel shader depth order": ignored (the title's state flush passes a byte and reads nothing back, title+0x3281a) |
| list #101 | 0x328 | old slot 99, `DriverHintX`: returns 0 |
| list #126, #131, #132, #142 | 0x3f0, 0x418, 0x420, 0x470 | shading rate, both syncs, 0x470: present before this task |
| queue #31 | 0xf8 | old slot 29, `CopyPageMappingsBatchX` |
| allocator #9 | 0x48 | allocation callback: S_OK |

Nothing was missing there. XTP's own Series S logs name four other
methods not implemented: list 0x1a8 (done here), pipeline state 0x58
(FSR4's, from its +0x5870; no longer called), and device 0x218 and 0x1c8,
which the title calls itself (title+0x24800db and +0x247757e) and which
fail with `E_NOTIMPL` as before; the title goes on after both.

## 5. Page scatters in runs (watch item 86, VIctory #1264)

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
1,000,000 descriptors, not one more (measured with a D3D12 probe,
2026-11-24). XTP capped the host heap there and kept 65,536 slots
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

(Filled in below.)

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
  temporal accumulation the cycle's length only sets how often the jitter
  pattern repeats; the offsets come from the same count, so the title's
  sequence stays consistent.
- *Should a bilinear upscale jitter at all?* FSR replaces the title's
  TAA, so a jittered camera with no accumulation only moves the image by
  up to half a render pixel each frame. The upscale takes the jitter back
  (sampling at `+ jitter`, FidelityFX's convention, checked on WARP with
  a quarter-texel jitter); edges may still crawl a little, as their
  aliasing changes with the sample position. Zero offsets would give a
  steadier image; the title then renders without jitter. Kept as AMD
  answers until captures say otherwise.
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

- Picture quality: FSR4's own network (route B, watch item 81) or an
  edge-aware spatial upscale, if the bilinear output is too soft.
- Device 0x218 and 0x1c8, which the title calls itself.
- VIctory #1264 for `xmem.rs` (section 5).
