# Host pipelines refused with E_INVALIDARG: why, and what is left

Why the driver refused hundreds of XTP's translated pipelines with
`E_INVALIDARG` (0x80070057) in the session logs of 2026-11-24, what each
refusal was, what fixed it, and what the RTX 4080 accepts now. Measured on
the person's PC (RTX 4080 SUPER, 32-lane waves; Windows build 26300) on
2026-11-24. The title is BTP 6. Times are UTC. Logs and probe outputs are
under `E:\xtp-data\`, none in the repository (§8).

## 1. Summary

- **476 refusals, one cause.** The logs hold 476 "host pipeline refused"
  lines, all `E_INVALIDARG`, all compute pipelines: 116 shaders, every one a
  console wave64 program. 455 of them are in four Series X sessions between
  13:11 and 13:28. There are none in the sessions from 14:43 on.
- **The cause: `WaveSize(64)` on a host that has 32-lane waves only.** The
  morning's builds translated for any host. A console wave64 compute program
  that needs its whole wave then declares the console's wave size, and all
  116 translations declare `WaveSize(64)` (`PSV0` lanes 64 to 64). dxil.dll
  accepts all 116: the DXIL is valid. The RTX 4080 runs 32-lane waves only,
  and the runtime refuses such a pipeline before the driver compiles
  anything: the 116 are refused in 0.2 s in all, against 223 ms (median)
  for one compute pipeline the 4080 accepts. It is a capability the host
  lacks, not an emitter bug and not a driver quirk: there is nothing to work
  around in the emitter.
- **Fixed on `main` before this task, in two steps.**
  - `99d7ff4` (sessions from 14:43): a pipeline whose `PSV0` asks for lanes
    the host lacks is refused before the driver ("compute shader needs
    64-lane waves; the host has 32 to 32").
  - VIctory #223's wave64 stack (sessions from 17:05): shaders are
    translated for the host's lanes.
  - Of the 116: **95 now translate as `WaveSize(32)`** (two 32-lane halves
    per console wave), pass the validator and get pipelines in the world: 94
    in the sessions since 18:50, and the 95th was not met. **21 are refused
    by the translator** (§4).
- **The 4080 now.** In their morning form the 116 are refused, 116 of 116,
  with `E_INVALIDARG`. The pack's 23,590 VS/PS/CS/MS translations for 32
  lanes declare no wave size the 4080 lacks. A sample of 1,229 of them, all
  four stages, gets 1,229 pipelines. The sessions since 14:43 made every
  pipeline they asked the driver for: 1,175 in session `20261124-190642`,
  with 0 refusals.
- **The 21 cost little.** Each one is dropped at most once or twice a frame,
  against about 3,500 drops a frame in all (§4.2). Of the 21:
  - 14, groups of 128 or 256 threads with no LDS and no barrier, are what
    the testers' `shader/wave-per-group` form runs (watch pass 6, item 126).
  - 6 share LDS across the console waves of their group and need #1433's
    form.
  - 1 is a whole-wave section of a 32-thread group (L1).
- **No debug-layer message.** The D3D12 debug layer does not load on this PC
  (§6): the Graphics Tools feature is not installed, and on this Insider
  build the Agility SDK's layers are refused. What stands in for the
  message: the `PSV0` lane range, the validator's verdict, the 4080's
  refusal with no compile time, and the same programs accepted as soon as
  they declare 32 lanes.
- **Found on the way: 158 translations the validator refuses** (§7):
  - 53 mesh programs whose outputs exceed 32 KiB;
  - 51 whose container signature parts differ from the module's;
  - 54 whose signature rows pack elements with different interpolation
    modes.

  These are emitter bugs, but the 4080 accepts all of them, so they are not
  among today's refusals. Where each class is made is in §7; none is fixed
  here.
- **No emitter change in this task.** The one cause of the refusals was
  fixed on `main` before it, and the 21 left need a new form of the
  translator (§4.2), not the emitter files.

## 2. The refusals in the logs

All "host pipeline refused" lines of `E:\xtp-data\runs\*\run.log` and
`E:\xtp-data\cache\play\*\run.log`: 476. pipe-cache's review counted 484.
The logs' other `E_INVALIDARG` lines are not pipelines: the view heap's
cap (once a session) and host textures (28 lines in the morning's sessions).

| Session (`runs\`) | Start | Console | Refused | Compute pipelines made |
|---|---|---|---:|---:|
| `20261124-1410-A-fresh` | 13:10 | Series X | 115 | 407 |
| `20261124-1414-B-save` | 13:14 | Series X | 116 | 410 |
| `play-20261124-142006-test1` | 13:20 | Series X | 113 | 409 |
| `play-20261124-142538-C-timing` | 13:25 | Series X | 111 | 409 |
| the ten others with refusals, `142930-D` to `153856-N` | 13:29–14:38 | Series X | 21 (1 to 4 each) | 406 each |
| every session from 14:43 (`cache\play\20261124-144330` on) | | | **0** | |
| **all** | | | **476** | |

- **Stages.** Compute only, 116 shaders: 107 refused four times, two 13
  and 14 times (`88994c3496045f51`, `13dfdd3b1136e407`, the two refused in
  every morning session), the rest 1 to 5 times.
- **Every one is a console wave64 program** (the native program's wave
  size).
- **Why the ten later morning sessions show 1 to 4.** They did not reach
  the scenes that bind the other 114: those shaders appear nowhere in their
  logs.
- **No graphics or mesh pipeline and no root signature** was refused by the
  driver in any log.

What a refused compute pipeline cost: in the morning's world reports (the
first 40 lines of each), the 95 that translate today were dropped up to 350
times a frame, median 90 where shown; `c9da71da40af2e28` alone up to 94.

## 3. The cause

| Translation of the 116 | Wave size it declares (`PSV0`) | dxil.dll | RTX 4080 (32 to 32 lanes) |
|---|---|---|---|
| For any host (the morning's builds; `--target any`) | 64 to 64, all 116 | 116 accepted | **116 refused, `E_INVALIDARG`**, 0.2 s for all |
| For 32-lane hosts (`main`; `--target 32-32`) | 32 to 32, 95 | 95 accepted | 94 made in sessions since 18:50 |
| | the other 21 do not translate | | |

- **Why the morning's builds translated for any host.** `emit` declared the
  console's wave size for a wave64 compute program that depends on the lane
  layout (cross-lane operations, `v_readlane`). It was "so the host device
  must support it", and nothing checked that the host did.
- **Where the refusal comes from.** D3D12 checks the shader's required wave
  size (`PSV0`: `MinimumExpectedWaveLaneCount`,
  `MaximumExpectedWaveLaneCount`; the module's `WaveSize` entry property)
  against `WaveLaneCountMin` and `WaveLaneCountMax`. The 4080 reports 32
  and 32.
  - That the check is the runtime's, before the driver: the 116 take 2 ms
    each to refuse, while one accepted compute pipeline takes 223 ms
    (median) and up to 13.6 s to compile.
  - That it is the wave size and nothing else: the same programs translated
    for 32 lanes, with the same bindings, root signature and container
    writer, get pipelines.
- **WARP is no control here.** It reports 4 to 4 lanes on this PC and
  refuses the 116 in the same way.
- **What fixed it.** From 14:43 the GPU layer read `PSV0` before calling the
  driver (`pipeline/host.rs`, `Needs::refusal`; `99d7ff4`). From 17:05 the
  translator targets the host's lanes (`device::host_target`, VIctory
  #223), and the shader cache keys translations by target.
  - The translator has a 32-lane form for a console wave64 program in
    most cases: two 32-lane halves that exchange masks and lane values. It
    declares `WaveSize(32)`.
  - Where it has no form, it refuses the program itself (`lower/mod.rs`).

## 4. The 116 today

### 4.1 The 95 that translate

All 95 declare `WaveSize(32)` and pass dxil.dll. 94 of them got host
pipelines in the sessions since 18:50; the 95th, `b2b1ea29bc3987db`, was not
met. They were the costly ones (up to 350 dispatches a frame, §2).

### 4.2 The 21 the translator refuses

`main`'s translator for 32 lanes, from the programs themselves (group size,
the LDS field of `COMPUTE_PGM_RSRC2`, `s_barrier` and `ds_*` counts):

| Shader | Group | LDS | Barriers | Refusal | What could run it |
|---|---|---|---:|---|---|
| `191ecaed66eea3b1` | 16×16 | none | 0 | group of more than 64 threads (L2) | wave-per-group |
| `1d44a723bdebbf27` | 16×16 | none | 0 | L2 | wave-per-group |
| `240cb516c1af0bc3` | 16×16 | none | 0 | L2 | wave-per-group |
| `31d703207b51549a` | 16×16 | none | 0 | L2 | wave-per-group |
| `466ec41d3f9d24c0` | 16×16 | none | 0 | L2 | wave-per-group |
| `65de113e23fa4fdd` | 16×16 | none | 0 | L2 | wave-per-group |
| `ab5455fc6abb804f` | 16×16 | none | 0 | L2 | wave-per-group |
| `abd7a22106898c66` | 16×16 | none | 0 | L2 | wave-per-group |
| `b4b0427232707598` | 16×16 | none | 0 | L2 | wave-per-group |
| `d7db57731ada919b` | 16×16 | none | 0 | L2 | wave-per-group |
| `56f3bf2d9388e5a7` | 8×16 | none | 0 | L2 | wave-per-group |
| `804b342cb4b9d1e9` | 8×16 | none | 0 | L2 | wave-per-group |
| `922e1d0ee1e19a85` | 8×16 | none | 0 | L2 | wave-per-group |
| `be6ce5d849d51127` | 8×16 | none | 0 | L2 | wave-per-group |
| `02384a60fda6f2d0` | 128 | 33 | 5 | L2 | #1433 (LDS across waves) |
| `0c028454b58543f0` | 16×16 | 1 | 1 | L2 | #1433 |
| `5c6224a6bc3a4066` | 256 | 1 | 1 | L2 | #1433 |
| `a1bf269d5b19f025` | 256 | 5 | 10 | L2 | #1433 |
| `b8f3f7979d6bbc50` | 1024 | 7 | 3 | L2 | #1433 |
| `eae3498e6553afa8` | 1024 | 7 | 2 | L2 | #1433 |
| `e18931c72af42e46` | 32 | none | 0 | a whole-wave section of a wave64 (L1) | L1 |

- **L2's exact text:** "console wave64 lanes in a group of more than 64
  threads on a host without 64-lane waves".
- **L1's exact text:** "a whole-wave section of a wave64 on a host without
  64-lane waves".
- **Over the pack these are 43 programs (L2) and 4 (L1)**: `xtp
  shader-survey --host-lanes 32-32`.
- **The 14 without LDS or barriers** read lanes of their own console wave
  only (7 to 31 `v_readlane` / `v_writelane` / `v_readfirstlane` each).
  They are the case the testers' `shader/wave-per-group` (r4tedx/VIctory,
  update 10, watch pass 6 item 126) runs:
  - each console wave of the group becomes a host group of 64 threads;
  - a direct dispatch of (x, y, z) groups becomes (x × waves, y, z);
  - its 20 of the pack's 43 that validate are "most of them cascade-shadow
    programs";
  - those with LDS or a barrier across the waves stay refused;
  - its indirect form is r4tedx/VIctory#11 (item 115).
- **The 6 with LDS and barriers** share memory across the console waves of
  their group: #1433's form (mesh64's base), not the wave-per-group one.

What they cost now, in the 246 frame reports with drops of the 15 Series S
sessions since 18:50 (`cache\play\20261124-18*`, `-19*`):

- **Where they show.** All are direct dispatches. They show in only 3
  reports: `a1bf269d5b19f025` at 2 a frame, `02384a60fda6f2d0` at 1.
- **In the other reports they are below the 40th line**, whose count is 1
  a frame (median of 271 full reports): each of the 21 is then dropped at
  most once a frame.
- **Against all drops:** about 3,500 a frame (mean of the same reports).
- **In the morning's world reports** none of the 21 reached the first 40
  lines either.

So the 21 are under 1 % of the drops, and taking wave-per-group for the 14
is worth it for correctness (shadow cascades) more than for counts.

## 5. What the RTX 4080 accepts, before and after

The probe (§8) translates as `xtp-gpu` does (`emit::translate_for` per
container), makes the root signature exactly as `pipeline/host.rs` does
(user data, root CBVs, one-view SRV tables, static samplers), and creates
the pipeline on the RTX 4080 with the system's D3D12 runtime, as the title
does:

- compute alone;
- mesh with a pixel shader that writes target 0;
- vertex with that pixel shader;
- pixel with a vertex shader that feeds its inputs.

Each window held `measure.lock` and was announced on the task.

| Translations | Pipelines tried on the 4080 | Accepted | Refused |
|---|---:|---:|---:|
| Before: the 116 refused this morning, in their morning form (`--target any`) | 116 | 0 | **116** (`E_INVALIDARG`) |
| Before: every compute translation of the pack for any host that declares `WaveSize(64)` | 353 of 2,247 | 0 | **353** (`E_INVALIDARG`) |
| After: the same 116 for 32 lanes | — (95 translate; the game made 94, §4.1) | 94 in sessions | 0 |
| After: the pack for 32 lanes, a sample in hash order | 1,229 of 23,590 | **1,229** | **0** |
| of which compute | 116 | 116 | 0 |
| mesh | 399 | 399 | 0 |
| pixel | 603 | 603 | 0 |
| vertex | 111 | 111 | 0 |

- **Before, on the whole pack.** For any host, 2,247 of the pack's
  compute programs translate.
  - 353 of them declare `WaveSize(64)`: every wave64 program that needs its
    whole wave, the morning's 116 among them. The 4080 refuses all 353 (0.2 s
    in all).
  - The other 1,894 declare no wave size (1,201) or `WaveSize(32)` (693
    wave32 programs), and run on the 4080.
- **After, on the whole pack.** For 32 lanes, 2,203 compute programs
  translate.
  - None declares `WaveSize(64)`.
  - 309 of the 353 now run as `WaveSize(32)` halves.
  - The other 44 are refused by the translator: 40 for groups of more than
    64 threads (L2) and 4 for whole-wave sections (L1). These are distinct
    containers; the survey counts 43 and 4 programs.
  - So 309 more compute pipelines on the 4080 (1,894 → 2,203).
- **After, all four stages.** For 32 lanes, no translation declares a wave
  size the 4080 lacks:
  - 1,002 compute translations declare `WaveSize(32)`: 693 of wave32
    programs, and 309 wave64 ones in halves;
  - the other 22,588 declare none.
- **The whole pack was not re-created.** It would take about 10 minutes of
  the 4080: a compute pipeline takes 223 ms (median) to 9.5 s (90th
  percentile) to compile, and 16 threads made 1,229 in one 30-s window. The
  sample was judged enough (orchestrator), with the sessions' 0 refusals
  since 14:43.
  - The sample takes containers in hash order, which is unrelated to stage
    or content.
  - No refusal in 1,229 bounds the refusal rate below 0.25 % (95 %).
- **Mesh pipelines are made in the world only with `XTP_WAVE64_MESH=allow`.**
  By default the GPU layer refuses every wave64 mesh program before its
  translation (VIctory #578, `drops.md` §5.2). The 399 here are their
  translations, which the 4080 accepts. Whether they run correctly as
  32-lane waves is task mesh64's.

## 6. The debug layer

The task asked for the debug layer's message per cause; this PC gives none.

- **`D3D12GetDebugInterface` fails** with
  `DXGI_ERROR_SDK_COMPONENT_MISSING` (0x887A002D).
- **Graphics Tools is not installed.** The Windows optional feature that
  provides `d3d12SDKLayers.dll` in System32 is absent.
- **The Agility SDK's own layers are refused.** The probe was given the
  SDK's `D3D12Core.dll` and `d3d12SDKLayers.dll` in `D3D12\` beside it
  (`D3D12SDKVersion` and `D3D12SDKPath` exported; Microsoft-signed DLLs
  from the NuGet packages, nothing installed on the system):
  - with 1.620.2 and 1.619.6 the runtime loads the SDK's `D3D12Core.dll`;
  - with 1.616.1, equal to the system's version (616), it keeps the
    system's;
  - in all three cases it refuses the layers with the same error, even
    preloaded from that folder;
  - `ID3D12SDKConfiguration1::CreateDeviceFactory` gives the same.
- **What is left untried.** The preview SDKs need Developer Mode, which is
  off, and installing Graphics Tools is a system change: both are the
  person's to decide.

## 7. Found on the way: translations the validator refuses

`xtp shader-survey E:\uwu\shaders.rpf --validate --host-lanes 32-32` (`main`
`796e848`): 24,199 of the pack's 26,698 programs translate, and dxil.dll
accepts 24,041 of those translations. It refuses 158:

| dxil.dll's error | Translations |
|---|---:|
| mesh: "vertex plus primitive output size is greater than 32768" | 53 |
| "Container part 'Program Input Signature' does not match expected for module" | 27 |
| "Container part 'Program Output Signature' does not match expected for module" | 24 |
| "signature element … has interpolation mode that differs from another element packed into the same row" (TEXCOORD, NORMAL, COLOR, SV_IsFrontFace) | 54 |
| **all** | **158** |

The 158 programs are 156 distinct containers: 62 mesh, 63 pixel and 31
vertex.

- **The 4080 accepts all 156**, as the pipelines of §5 (2.7 s). D3D12 does
  not run the validator, and the driver compiles the module without
  checking the container's signature parts against it. So these are not
  refusals today.
- **They are emitter bugs all the same.** What a driver does with a module
  that breaks the rules it assumes is not specified, and another driver or
  a later runtime may refuse them.
- **None of them is among the shaders of the sessions' refusals.**

Where each class is made, for whoever takes them up:

| Class | Where | Owner now |
|---|---|---|
| Mesh outputs over 32 KiB | the mesh state and outputs (`emit/lower/mesh.rs`): refuse or split instead of declaring them | mesh64 (0a31f0cb) |
| Signature parts that differ from the module | `ISG1` / `OSG1` (`emit/container.rs` `signature`) against the module's signature metadata (`emit/shader.rs` `signature_md`), for the elements input and export lowering give them | free (this task's files) |
| Elements of different interpolation packed in one row | the rows chosen for inputs and outputs (`emit/lower/input.rs`, `emit/mod.rs`) | images-fix (84a6a313) for `input.rs` |

None of the three is fixed here. They are not this task's refusals: the
driver accepts them, and the orchestrator asked for the analysis to be
delivered.

## 8. Method

- **Logs.** `E:\xtp-data\runs\*\run.log` and
  `E:\xtp-data\cache\play\*\run.log`, read on 2026-11-24 up to 19:31.
  - Refusals by session and shader, and the shaders' fate in the latest
    sessions: one grep each.
  - Drops by shader: the frame reports' detail lines ("dropped N/frame
    without a host pipeline: dispatch: does not translate: compute
    <hash>"). Morning drops: the old report ("refused [compute <hash>]").
- **The probe**, `pso_probe`: a local program, not committed. It was built
  as an example of `xtp-gpu` on the build farm from this branch, with
  `--features xtp-shader/host`. It:
  - reads the pack's SGD5 records and stored containers once each, hashed
    as `xtp-gpu` hashes shaders (the first 16 hex digits of the
    container's BLAKE3);
  - translates them for a target (`--target any` or `32-32`), with dxil.dll
    if asked;
  - with `--gpu`, waits for `measure.lock` to be free, holds it, and creates
    the pipelines as §5 says for at most the time given;
  - binary and its Agility SDK folders: `E:\xtp-data\tmp\pso-probe\`;
  - outputs, one line per shader (hash, stage, native wave size, `PSV0`
    lanes, translation, validator, pipeline, milliseconds), in the
    session's scratch folder, copied to `E:\xtp-data\pso-refusals\`.
- **Windows on the 4080**, each under `measure.lock` and announced on the
  task:
  - the morning's 116, 0.2 s;
  - the pack sample, 36.5 s: the deadline was 30 s, and the compiles in
    flight finished after it;
  - the validator's 156 containers, 2.7 s;
  - the pack's 353 compute translations for any host that declare
    `WaveSize(64)`, 0.2 s;
  - debug-layer attempts that stopped before creating a pipeline.
- **The survey.** `xtp shader-survey` from `main` `796e848` (release build
  from the farm), `--validate --host-lanes 32-32 --jobs 16`, 121 s. Its
  JSON: `E:\xtp-data\tmp\pso-probe\survey-main-32.json`.
