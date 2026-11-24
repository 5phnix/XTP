# Upstream watch: what XTP should take or reimplement

**Pass 1, 2026-11-24.** GitHub read between 12:10 and 13:40 UTC, read-only
(`gh pr list/view/diff`, `gh api`). Nothing was posted, starred or opened.
The next pass starts from the marks in [README.md](README.md).

XTP is VIctory imported at `ffe310e` (GPL-3.0-or-later), with the crates and
variables renamed (`victory-*` → `xtp-*`, `VICTORY_*` → `XTP_*`). The goal
here is the world at 30 fps. The first XTP run showed it at 1–2 fps on an
RTX 4080; the README adds a dark scene and GPU hangs.

## 1. Summary

- **Nobody has the world at 30 fps.** The fastest measured world frame is
  267 ms median: VIctory #578 + #595 on an RTX 5090, with the wave64
  programs dropped, so with less work than the title asks for. Every other
  report is 0.3–2 fps. xcl has not reached gameplay.
- **Upstream main branches have not moved since XTP's import.** VIctory's
  `main` is still `ffe310e`, xcl's still `47efa2e`. Everything new is in
  open pull requests: 443 in VIctory and 552 in xcl. VIctory's maintainer
  has not reviewed or merged any of them since `ffe310e`. Upstream approvals
  come from other contributors.
- **VIctory has taken no pull request since 07:12 UTC.** Contributors now
  post fork branches in issues (section 4.5). The next pass must read
  issues too.
- **XTP's base has a known regression.** Merged VIctory #612 stops 323
  programs that used to translate (VIctory issue #736). A second one blacks
  out menus on one AMD card, not yet bisected (#847).
- **The best lead on the dark world.** On an RTX 3090 and an RX 6750 XT
  the world's 2560×1440 targets hold the rendered scene, and the 3840×2160
  output stays zero (VIctory issue #841).
- **The VIctory PRs worth taking first** (GPL-3.0, take with credit):
  1. the wave64 stack (#223, #233, #320, #439) and the wave-size check of
     #578: WaveSize 64 on NVIDIA;
  2. #493 and #483: the NVIDIA world hang;
  3. #595: asynchronous submission (gpu G8);
  4. #546 and #275: pipeline waits and draws dropped while pipelines build
     (G12);
  5. #464 and #490: the CPU's preparation cost;
  6. #89 and #492: texture formats NVIDIA refuses;
  7. #180, the lane-mask branch and #539: +1,193 shader programs validate.
- **Four measurements change the plan:**
  - After #595, a world frame waits 1 ms for the GPU and spends 241 ms of
    267 preparing title memory on the CPU. The shadow model (gpu Phase 3,
    G15–G18) is what stands between XTP and 30 fps, not more work on the
    shadow path.
  - Phase 3 must put the title's GPU heaps in video memory. On an RTX 5080
    the same dispatch takes 431 ms reading host RAM and 0.45 ms reading
    VRAM (palmwave, item 41).
  - G15 must import xmem's sections by address, never with
    `OpenExistingHeapFromFileMapping`: those leak their commit past the
    process (OpenScarlett CE, item 42).
  - With the shader fixes in, GPU time per world frame went from 0.1–0.3 s
    to 1.34 s on an RX 9070 XT, which crosses the 2 s TDR in seconds.
    Translated programs must come with bounded submissions (G11) and GPU
    timing (G2).
- **xcl stays ideas only** (GPL-2.0-only). Three ideas are new since the
  analysis:
  - xcl #869 repacks root-changing `ExecuteIndirect` commands on the GPU
    (G13);
  - xcl #1182: GTA VI maps one physical page under render targets at two
    addresses (G19, the dark world);
  - xcl #758, #898 and #759 keep a journal of the pipelines a run asked for
    and build them offline (G12).
- **One correction to the analysis.** `docs/analysis/formats.md` says the
  BC7 dictionary fix (VIctory #658) was merged upstream. It is open, not
  merged, and still mergeable onto `ffe310e`.

## 2. How to read this

**Actions.**
- **Take with credit**: port the upstream code. Only for GPL-3.0(-or-later),
  MIT, Apache-2.0 and other GPL-3.0-compatible licences. How: section 6.
- **Reimplement**: write XTP's own version from the behaviour described.
  This covers GPL-2.0-only projects, unlicensed ones and PS5 projects.
  Nothing of their code is copied: a worker writes from this description
  and the ISA or Microsoft documentation, not with their source open.
- **AGPL-3.0** projects count as reimplement too. GPL-3.0 §13 allows
  combining with AGPL-3.0 code, but that part keeps the AGPL's network
  clause. Taking it is the person's decision, not a porting task's.
- **Ignore**: not worth it, superseded, or known to break something. The
  reason is given.

**State** is the pull request's state at the pass:
- `open`, `draft`, `merged` or `closed`;
- for VIctory, `clean` or `conflict` says whether it merges onto `ffe310e`,
  XTP's base before the rename. A clean one ports with a path rename; a
  conflicting one needs a rebase first.

**Symptoms** (the first XTP run and the README):

| Code | Symptom |
|---|---|
| C | CPU copies: title memory prepared and copied back around every submission |
| D | dropped draws and dispatches: untranslated shaders, pipelines not ready, refused signatures or resources |
| W | WaveSize 64 refused on NVIDIA (32-lane waves) |
| F | unsupported texture formats |
| H | GPU hang or device removal in the world |
| K | dark world: GPU results that do not reach their readers |

**Backlog items** are the ones in `docs/analysis/`:
- gpu: G1–G22;
- shader: S0–S6, M1–M2, R1–R4, I1, P1–P2, G1–G2, L1–L4;
- formats: F1–F17;
- hle: N1, R1–R12, K1–K3, I1–I3, A1–A3;
- loader: L-01–L-19;
- tooling: B0–B18.

"new" means the item has no backlog entry yet; the porting task creates
one.

## 3. Projects

| Project | Licence | What XTP may do | Approach | Activity since XTP's import | Head at the pass | GTA VI, by its own reports |
|---|---|---|---|---|---|---|
| [Marvisak/VIctory](https://github.com/Marvisak/VIctory) | GPL-3.0-or-later | take with credit | XTP's base: native x86-64, HLE, RDNA2 → DXIL | 612 PRs: 443 open, 89 merged, 80 closed; last created 07:12 UTC, #710. `main` has not moved. 8.6 k stars. | `ffe310e` | menus 30 fps; the world dark at 0.5–1.8 s per frame, a GPU hang on NVIDIA after 2–5 min (fixed in some PR sets, below) |
| [ivall/xcl](https://github.com/ivall/xcl) | GPL-2.0-only | ideas only | native x86-64, HLE, RDNA2 → HLSL → DXC; title memory GPU-visible | 910 PRs: 552 open, 200 merged, 158 closed; last #1196 at 13:05 UTC. `main` has not moved. | `47efa2e` | menus at a steady 30 fps; gameplay not reached (minimap mesh pipelines refused) |
| [simnJS/AnyPS5](https://github.com/simnJS/AnyPS5) | GPL-2.0 (its README: the original Kyty code by InoriRus is MIT) | process only | PS5 titles on Linux/Windows (AnyKyty, Vulkan) | 195 PRs: 15 open; last #292 at 06:15 UTC. No commit since 2026-11-22 23:46. | `7de61e3` (branch `AnyKyty`) | not a GTA VI project |

Other projects working on GTA VI or Xbox Series titles on PC are in
section 5.

## 4. Items, most useful first

### 4.1 Take with credit (VIctory, GPL-3.0)

Numbers are VIctory PRs (`https://github.com/Marvisak/VIctory/pull/N`).
The measurements are the PR authors' or testers' own, on their machines.
They are not XTP measurements.

**Before the list, for XTP's own check:**
[#689](https://github.com/Marvisak/VIctory/pull/689) (iurysf; open, clean,
head `e1c3557`, +6/−3, test only). `frame::tests::waits_end_on_ticks_and_count_frames`
fails under CPU load: 15 of 20 runs failed with 16 busy cores, against 20 of
20 passing with the change. It loosens two timing assertions:
- the second wait's wall-clock bound goes from 50 ms to 500 ms, still far
  below the 1 s time-out;
- flip ticks must rise strictly, instead of being exactly consecutive.

XTP's workspace check failed on this test on 2026-11-24 (ticks
`[7, 8, 10, 12]`), on a docs-only change. Alone, the test passed 10 of 10.
**Take with credit, first** (gpu · `frame.rs`, tooling · B1): every
delivery's check depends on it.

| # | Item | State | Author | What it does | XTP area · backlog | Sym. | Action and why |
|---|---|---|---|---|---|---|---|
| 1 | [#223](https://github.com/Marvisak/VIctory/pull/223) → [#233](https://github.com/Marvisak/VIctory/pull/233) → [#320](https://github.com/Marvisak/VIctory/pull/320) → [#439](https://github.com/Marvisak/VIctory/pull/439) | open, clean (stacked) | r4tedx | **The wave64 stack.** It translates for the host's wave size. A wave64 compute program becomes two 32-lane halves in the console's control flow, with lane reads and writes, ballots and LDS exchanged through group memory. It emits `WaveSize(32)`. Includes `docs/wave64-on-wave32.md`. | shader · L1, L2, M2 (and the GPU side of L1) | W D | **Take, first.** On a 32-lane host: +98 compute pipelines in a whole-title translate, 0 lost. Compute refusals 103 → 1 on an RTX 3080. On an RTX 5080, 6 min in the world without device removal; 958 draws dropped per frame against 1,470; the richest G-buffer target 67.9 % covered against 10.5 %. CI green; exact on hand-assembled programs on RTX 5070, 4070 SUPER, 5080 and 5090. The shader report planned this late, after xcl. This GPL-3.0 code removes that wait. Port the four in order. |
| 2 | [#578](https://github.com/Marvisak/VIctory/pull/578), [#579](https://github.com/Marvisak/VIctory/pull/579) | open, clean | wbkepler | #578 refuses a pipeline whose emitted DXIL wave range (`PSV0`) the host cannot run, and wave64 mesh programs on hosts without 64-lane waves. It logs one reason per refusal, WARP included. #579 adds a diagnostics switch that skips wave64 programs by stage. | gpu · pipeline/host.rs (guard for L1); new | W H | **Take, with the stack.** On an RTX 5090, dropping wave64 mesh and compute ended the world hangs: 6 min 48 s with no device removal. The `PSV0` check (head after `d2d26d1`) leaves alone what the stack translates. The first version refused it, so take the later head. |
| 3 | [#493](https://github.com/Marvisak/VIctory/pull/493), [#483](https://github.com/Marvisak/VIctory/pull/483) | #493 open, clean; #483 open, conflict | hophaver; Vaximous | #493 binds each root SRV as a one-view table bounded to its host copy, so reads past it return zero. #483 bounds every descriptor-heap index a shader computes from data: an index past the heap reads a null guard slot. | gpu · descriptor/host.rs, draw.rs; shader · resource; new (hang protection, gpu §6.3) | H D | **Take.** These are the NVIDIA world-hang fixes testers converge on. Submissions that took about 11 s (device removed) took 265–284 ms. 300 s with no removal on an RTX 4070, median frame 950 ms against 1,000–1,380. With #89, #469, #206, #454 (below), a wave64 form and a loop limit of 32, they are the set that ran 7–14 min in the world on an RTX 5080, 3080 and 3070. That set used #189 as its wave64 form. On an RTX 5080 the same set with the wave64 stack instead also stayed up, with 958 draws dropped per frame against 1,470, at 1,233 ms per frame against 850 (more work drawn). #493 builds on #336, which is in `ffe310e`. |
| 4 | [#595](https://github.com/Marvisak/VIctory/pull/595) | open, conflict | wbkepler | **Asynchronous submission.** Each submission signals a host fence and returns. A completion thread, in order, does copy-back, bottom-of-pipe writes and title fence signals. Lists reset while they run record into fresh memory, and their old memory retires with the submission. Later CPU steps wait only for the submission whose memory they touch. | gpu · G8, G9 (part) | C H | **Take.** It is the G8 the plan describes, in XTP's own code. On an RTX 5090 with #578: GPU wait 79 → 1 ms per frame, frame 283 → 267 ms median, no device removal in three 7-min runs. Present still drains the frame. Keep a synchronous switch, as gpu risk 2 asks. Its own numbers show preparation is the bottleneck after it. |
| 5 | [#546](https://github.com/Marvisak/VIctory/pull/546), [#275](https://github.com/Marvisak/VIctory/pull/275) | #546 draft, clean; #275 open, clean | melon0918; wbkepler | #546 builds pipelines a draw asks for before speculative ones. #275 makes the frame wait budget atomic and adaptive: 500 ms in the first 60 frames, then 8 ms, with an `…_PSO_WAIT_MS` override. Its library saves no longer block. | gpu · G12 | D | **Take #275, then #546 out of draft.** #546: draws dropped for a missing pipeline 2,040 → 1,359 per frame, draws recorded 10 → 1,721. #275 is in the testers' tree (section 4.6). Together they are most of G12; compute-before-graphics waits stay XTP's own work. |
| 6 | [#464](https://github.com/Marvisak/VIctory/pull/464) → [#490](https://github.com/Marvisak/VIctory/pull/490), [#497](https://github.com/Marvisak/VIctory/pull/497) | open, conflict (stacked on #464) | wbkepler | #464 remembers unmapped guarded blocks instead of copying them again each submission. #490 asks whether guarded memory is a view once per block. #497 keeps the shader-write snapshots by page, in reused buffers. | gpu · watch.rs, shadow.rs; interim before G15–G18 | C | **Take #464 and #490; #497 after G2 confirms it.** World frames on the author's RTX 5090: #464 cuts the fallback path from 383–1,021 to 7–67 ms; #490 cuts guard time from 102–237 to 24–35 ms. #497 cuts snapshot page faults from 142 k to 26 k, but its frame time did not improve in the author's run. Small, and they cut the cost #595 leaves. Phase 3 removes this code later, so do not invest further in it. |
| 7 | [#180](https://github.com/Marvisak/VIctory/pull/180), [#298](https://github.com/Marvisak/VIctory/pull/298), lane-mask branch, [#539](https://github.com/Marvisak/VIctory/pull/539) | open, clean; the branch has no PR | permadiego; permadiego; LAKSHYA-cmd-tech; SAitken91 | #180: descriptors read as data through a twin waterfall load, and quad gradients for compute samples. #298: lane masks spilled to VGPR lanes and read back. Lane-mask branch: `LAKSHYA-cmd-tech/VIctory` `fix/shader-lane-mask-ballots-and-data-reads` at `49d6d98`, lane masks read as data and ballots in every stage. #539: 64-bit image atomics on an R32G32 view. | shader · R2 (part), M1, M2, I1 | D K | **Take #180, the branch and #539, after S0.** An offline census of the whole pack on `ffe310e`: #180 +542, the 64-bit atomics +520 (#305, #539 and #705 clear the same 520). Together: 21,736 → 22,929 validated, compute 1,417 → 1,846, 0 newly failing. #298 +34, but it conflicts with the stack. **Caution:** with these, GPU time per world frame rose to 1.34 s and the device was removed in 10 s on an RX 9070 XT. Land them with G2's timing and G11's bounds. #539 rather than #305: the testers' tree notes #305 lacks the descriptor-heap 64-bit atomics flag. |
| 8 | [#89](https://github.com/Marvisak/VIctory/pull/89), [#492](https://github.com/Marvisak/VIctory/pull/492) | #89 open, conflict; #492 open, clean | HenriqueSBueno; Yaekai | #89: render-target and UAV textures in B5G5R5A1 (86) and B4G4R4A4 (115) become R8G8B8A8 where the GPU cannot render or write them; sampled ones keep their format. #492: R9G9B9E5_SHAREDEXP (67) render targets and UAVs become R16G16B16A16_FLOAT, clear values included, and views of 67 bind the host format. | gpu · resource.rs, descriptor/host.rs; new (G7 neighbour) | F D | **Take both.** On NVIDIA the title's 2560×1440 targets in 86 and 115 fail with `E_INVALIDARG`: no render target for B4G4R4A4, no typed UAV for B5G5R5A1. The HDR scene and lighting targets in 67 fail on every PC GPU. Those passes are lost. #89 is in the NVIDIA world set. xcl maps 67 to R11G11B10_FLOAT instead; RGBA16F keeps the precision. |
| 9 | [#469](https://github.com/Marvisak/VIctory/pull/469), [#206](https://github.com/Marvisak/VIctory/pull/206) | #469 open, clean; #206 draft, clean | AnoHunted | #469: a depth target that is also UAV gets a twin texture for its UAVs, copied both ways in each list that uses it. #206: a structured view that does not start on an element of its host copy becomes a raw view of the same bytes. | gpu · resource.rs, descriptor/host.rs; new | F H | **Take #469; take #206 when it leaves draft.** Both are in the set that kept NVIDIA cards in the world 7–14 min. The other way, D3D12 leaves #206's reads undefined, and #469's depth-UAV views are refused. |
| 10 | [#643](https://github.com/Marvisak/VIctory/pull/643) | open, clean | nathanbzrr | A resource made at the address of a live host texture shares it instead of getting a zero-filled one. Resource and view descriptions are made canonical (padding and unused union words zeroed), so views and placed textures deduplicate. | gpu · G19 (first part), §6.1 cause 4 | K | **Take.** It fixes aliases at one address reading zeros: one cause of the dark world. Tested on RTX 5070 Ti and 5060. Aliases at different addresses over one physical page are xcl #1182's case, reimplemented (4.2). |
| 11 | [#24](https://github.com/Marvisak/VIctory/pull/24), [#583](https://github.com/Marvisak/VIctory/pull/583), [#346](https://github.com/Marvisak/VIctory/pull/346) | open, clean | artor410; matheusfrta; BastienRodz | #24: indirect `DispatchMeshX` goes through indirect execution (clamp, budget) instead of being dropped. #583: the clamp honours the count buffer, the budget no longer wraps, and missing root views get a zeroed buffer. #346: indirect rectangle-list draws expanded on the GPU. | gpu · G13 | D | **Take.** On an RTX 3080 #346 makes the loading screen render; rectangle drops 256 → 12 per frame. Root-changing signatures remain: xcl #869's idea, 4.2. #346 must be adapted to #334's state tracker, per the testers' tree. |
| 12 | [#334](https://github.com/Marvisak/VIctory/pull/334) | open, clean | realhenners | Host copies of title buffers move between UAV and read states with barriers, instead of relying on promotion from `COMMON`. | gpu · shadow.rs, G3 neighbour | H | **Take.** It fixes the NVIDIA debug-layer error (vertex buffer in `UNORDERED_ACCESS`), then `DEVICE_HUNG`. In the testers' tree. Its in-game runs still hung, so it is a correction, not a fix of the hang. |
| 13 | [#453](https://github.com/Marvisak/VIctory/pull/453), [#592](https://github.com/Marvisak/VIctory/pull/592) → [#621](https://github.com/Marvisak/VIctory/pull/621), [#571](https://github.com/Marvisak/VIctory/pull/571), [#160](https://github.com/Marvisak/VIctory/pull/160), [#563](https://github.com/Marvisak/VIctory/pull/563) | open, clean | AntoninMichelq; SAitken91 → Halfect; SAitken91; r4tedx; SAitken91 | Pixel programs:<br>• #453: barycentrics read directly and computed interpolation, through `SV_Barycentrics`;<br>• #592 and #621: packed 16-bit inputs whose second row the metadata omits.<br>Compute: #571, LDS byte and half accesses.<br>Images:<br>• #160: texel offsets, comparison and gather forms of samples;<br>• #563: cube samples with gradients and `image_sample_d_cl`. | shader · P2, P1, S3, S1 | D | **Take, after S0, each measured by it.** #592: 172 programs. #571: 16. #160: +77. #563: +25. #453 is in the testers' tree. Choose #571 or #632 for LDS: #571 has green CI and #632's CI failed. |
| 14 | [#158](https://github.com/Marvisak/VIctory/pull/158), [#159](https://github.com/Marvisak/VIctory/pull/159), [#331](https://github.com/Marvisak/VIctory/pull/331) | open, clean | r4tedx; r4tedx; AnoHunted | #158: the depth bounds test reaches host pipelines and `OMSetDepthBounds`. #159: occlusion, statistics and timestamp queries on the host. #331: sRGB and linear views of one texture read with their own curve (castable formats). | gpu · G7, G14 | K | **Take.** These are the backlog's G7 and G14, with tests and green CI. #158 and #331 are in the testers' tree. #159 must be adapted to #334 first. |
| 15 | [#211](https://github.com/Marvisak/VIctory/pull/211) | open, clean | Yaekai | `PresentX` with several planes composites them: in Series S mode, a 1080p scene plus a 2160p UI plane. | gpu · G20 | K | **Take.** It answers half of gpu open question 3: GTA VI presents two planes in Series S mode (Series X not reported). Series S menus are black without it. |
| 16 | [#431](https://github.com/Marvisak/VIctory/pull/431) | open, clean | AntoninMichelq | Host textures and slot resources are freed only after the submissions that use them, and made on first use. | gpu · G9 | H | **Take after #595, keeping what #595 does not cover.** The longest run reported: 11 min 39 s in the intro cutscene. It halves host texture memory on an 8 GB card. #609 and #575 overlap; ignore them. |
| 17 | [#454](https://github.com/Marvisak/VIctory/pull/454) | open, clean | ruvenesh | An opt-in hook past the title's own watchdog for build 1.0.2.1. The title kills itself when its main thread has not advanced for about 30 s, which slow hosts trigger. | tooling · B15 (game layer); loader | H | **Take, into the game layer, off by default.** Testers need it to stay in a slow world. It is build-specific code addresses, which belongs in `xtp-game`. |
| 18 | [#452](https://github.com/Marvisak/VIctory/pull/452) | open, clean | AntoninMichelq | Spin-wait loops in translated programs stop after 4,096 iterations; the loop limit also applies in release builds. | shader · emit; gpu §6.3 | H | **Take.** Testers run with `…_SHADER_LOOP_LIMIT=32` to stay up. A bounded default in release is what G11 assumes. |
| 19 | [#509](https://github.com/Marvisak/VIctory/pull/509) or [#130](https://github.com/Marvisak/VIctory/pull/130); [#638](https://github.com/Marvisak/VIctory/pull/638) | open, clean | aycow; Sunkiller-qc; claude-GTA6 | #509: an opt-in GPU census by pipeline and shader hash (spans, groups, build time, refusals). #130: frame times, memory and caches to `frames.csv`, a Perfetto trace and an overlay. #638: diagnostics for what a hang waits for and for a black frame. | gpu · G2; tooling · B6, B9 | C D H | **Take #509 for G2.** It names the slow programs that h0useadv's TDR run points at. #130 is large (2,720 lines); take it only if G2 needs a trace. Take #638's switches when debugging the dark world. |
| 20 | [#658](https://github.com/Marvisak/VIctory/pull/658) | open, clean | coderanonymus | Reads the BC7 entries of the BCPack dictionary at their real offset. | formats · F1 | F K | **Take** (18 lines). formats.md calls it merged upstream; it is not. |
| 21 | [#234](https://github.com/Marvisak/VIctory/pull/234), [#703](https://github.com/Marvisak/VIctory/pull/703), [#603](https://github.com/Marvisak/VIctory/pull/603) | open; #234 and #703 conflict, #603 clean | wbkepler; wbkepler; AwsAli05 | #234: overlapped DirectStorage reads and persistent `xmem` section views (no per-chunk map/unmap). #703: BCPack without per-call allocations. #603: detiling dispatches 8/16/32-byte runs once per row. | formats · F3, F4; hle · xmem | C | **Take #234 as the start of F3**, the loading bottleneck formats §6 diagnoses. Take #703 and #603 after F2's timing shows decode or detiling matters. None was measured in the world. |
| 22 | [#644](https://github.com/Marvisak/VIctory/pull/644) | open, conflict | nathanbzrr | Host root signatures over 64 DWORDs spill per-stage user data to root CBVs, instead of the pipeline being refused. | gpu · root.rs, pipeline/host.rs; new | D | **Take after a rebase**, if G2's drop log shows root-signature refusals in the world. No in-game measurement. |
| 23 | [#463](https://github.com/Marvisak/VIctory/pull/463), [#208](https://github.com/Marvisak/VIctory/pull/208) | open, clean | Shaun-Fong; wbkepler | #463: an app-local Agility SDK runtime when present (SM 6.6 on Windows 10). #208: the high-performance adapter and a probe of host capabilities. | gpu · G22 | – | **Take** when G22 comes. |

### 4.2 Reimplement (behaviour only)

xcl is GPL-2.0-only and AnyPS5 is GPL-2.0: their code is never copied.
Numbers are xcl PRs (`https://github.com/ivall/xcl/pull/N`) unless the
project is named.

| # | Item | State | Author | What it does | XTP area · backlog | Sym. | Action and why |
|---|---|---|---|---|---|---|---|
| 24 | [xcl #869](https://github.com/ivall/xcl/pull/869) | open | utkarshxdev | A command signature that changes root arguments has each command repacked on the GPU, so a translated draw or dispatch sees the root values the record carries. | gpu · G13 | D | **Reimplement.** G13 calls the expansion new work, since xcl's `main` skips the case. This PR shows it can be done on the GPU. No VIctory PR does it. |
| 25 | [xcl #1182](https://github.com/ivall/xcl/pull/1182) | open | 3L1A5-1 | GTA VI maps one physical page under render targets at two addresses at once. In its first 40 s, 422 targets (2.1 GB) had their pages mapped under 276 others (1.85 GB), and the game used both. GPU-owned texture pages are now keyed by the physical page, not by the address. | gpu · G19; §6.1 cause 4; hle · xmem | K | **Reimplement in G19.** It explains part of the dark world: what one alias writes, the other never shows. It also costs 1.85 GB of VRAM. VIctory #643 covers only aliases at one address. |
| 26 | [xcl #758](https://github.com/ivall/xcl/pull/758) → [#898](https://github.com/ivall/xcl/pull/898) → [#759](https://github.com/ivall/xcl/pull/759), [#889](https://github.com/ivall/xcl/pull/889) | open; #758, #759 draft | oishigeso; JFyuhong; isaacwingate | A journal of every pipeline a run queues, the first bind of each, and `xcl pipelines`, which builds them outside the title on its own device. #889 builds compute pipelines before their first bind. | gpu · G12; tooling · new (offline pipeline build) | D | **Reimplement after G12.** XTP preloads last run's list. Building it offline, before the title starts, removes the first-run pipeline waits. On NVIDIA a big compute pipeline takes seconds. |
| 27 | [xcl #455](https://github.com/ivall/xcl/pull/455), [#562](https://github.com/ivall/xcl/pull/562), [#674](https://github.com/ivall/xcl/pull/674) | open | Cattn; gridoe; RandomGithub12309 | #455: title texture uploads go on the submitting queue with a GPU wait, not a CPU wait. #562: one completion worker per queue for bottom-of-pipe writes. #674: a triple-buffered presenter that does not wait for the previous present. | gpu · G6, G8, G10, G20 | C | **Reimplement inside those items.** Each removes a CPU wait from the frame. They are design notes for G8 and G20, not separate tasks. |
| 28 | [xcl #573](https://github.com/ivall/xcl/pull/573) | open | throny | The wave size comes from the FSSM header's wave32 flag, not from whether the code writes a 64-bit EXEC. A wave32 program that never writes EXEC was translated as wave64. | shader · native.rs; L1 | W | **Check in XTP, reimplement if it applies.** A wave32 program taken for wave64 is refused or split on NVIDIA for nothing. |
| 29 | [xcl #1163](https://github.com/ivall/xcl/pull/1163), [#1126](https://github.com/ivall/xcl/pull/1126), [#517](https://github.com/ivall/xcl/pull/517) | open | GamingChelsea; emirduran707 | #1163: `v_interp_mov_f32` P10/P20 through `GetAttributeAtVertex`. #1126: whole-wave reductions in wave64 VS/PS become `WaveActive*`, so lanes past a 32-lane wave are not read. #517: which wave64 VS/PS a 32-lane cut changes. | shader · P2, L3 | W K | **Reimplement if S0 shows the programs.** VIctory #453 covers most of P2. L3 waits for a measured need. |
| 30 | [xcl #1193](https://github.com/ivall/xcl/pull/1193), [#1111](https://github.com/ivall/xcl/pull/1111), [#1090](https://github.com/ivall/xcl/pull/1090) | open | zondy1i; TG0000 | Fewer cross-half exchanges in split wave64: wave-uniform `v_readfirstlane`, masks kept lane-local through 64-bit SALU, liveness per basic block. | shader · L1, L2 (speed) | W H | **Reimplement after the VIctory stack (item 1) is in and timed.** The split forms cost GPU time, and GPU time is the next wall. |
| 31 | [xcl #825](https://github.com/ivall/xcl/pull/825), [#640](https://github.com/ivall/xcl/pull/640) | open | Cattn; LeonardoS11 | Buffer mirrors unused for 10 s are freed. An optional page budget evicts CPU-filled title textures, least recently used first. | gpu · G19 | H | **Reimplement with G19.** VRAM is what fails first on 8–12 GB cards in the world. |
| 32 | AnyPS5 [`65ace00`](https://github.com/simnJS/AnyPS5/commit/65ace00), [#223](https://github.com/simnJS/AnyPS5/pull/223), [#235](https://github.com/simnJS/AnyPS5/pull/235), [#220](https://github.com/simnJS/AnyPS5/pull/220) | commit; closed; open; open | relativespace2K; vgpastor; Celegans12; mjnhbg3 | `65ace00`: a performance report form. #223: Markdown output for its A/B run reports. #235: at start, which `KYTY_*` variables this build does not read. #220: frame captures for offline replay. | tooling · B6, B9–B12 | – | **Reimplement the process.** A fixed report form and an A/B comparison are what VIctory's testers rebuild by hand in each comment. XTP's smoke-compare (B11) can produce them. |

### 4.3 Ignore

| Item | Author | Why |
|---|---|---|
| VIctory [#189](https://github.com/Marvisak/VIctory/pull/189) | alfycs | Another wave64-on-wave32 form. It changes the same lowering as item 1 and cannot coexist with it. Both kept NVIDIA cards up in tester runs, but the stack drew more of the frame (item 3) and the testers' tree carries the stack. |
| VIctory [#136](https://github.com/Marvisak/VIctory/pull/136) (closed) | mpDom | Bisected by two testers (RTX 5080, RTX 4080) as making every frame black, loading screens included. |
| VIctory [#193](https://github.com/Marvisak/VIctory/pull/193), [#704](https://github.com/Marvisak/VIctory/pull/704) | n-romanovich; r4tedx | Unknown-stride buffers through a raw handle. #193 reads 0 on NVIDIA and ended a run with a device removal. #704 (+39 programs, on #193) needs an NVIDIA world run before XTP takes it. Revisit next pass. |
| VIctory [#185](https://github.com/Marvisak/VIctory/pull/185), [#413](https://github.com/Marvisak/VIctory/pull/413) (for now) | aycow; wbkepler | Bindless coherence (G5, G6). #185 is a 5.6 k-line draft that changes every few hours. Against its own earlier revision: AMD 2.8 s → 0.85–1.6 s per frame, NVIDIA 2.0–3.3 s → 1.3–1.9 s. #413 reached the world at 0.6–0.9 fps. Both keep the shadow model the measurements say must go. Re-read them next pass for G5; take whichever is stable then. |
| VIctory [#693](https://github.com/Marvisak/VIctory/pull/693) | wbkepler | Shadows in VRAM with staging transfers. Unmeasured, no tests, and the opposite direction to G15–G18 (title memory imported, not copied). |
| VIctory [#609](https://github.com/Marvisak/VIctory/pull/609), [#575](https://github.com/Marvisak/VIctory/pull/575) | LAKSHYA-cmd-tech; Len1k | Lifetimes overlapped by #595 and #431. #609 did not end the freed-texture faults in a tester's run. |
| VIctory [#459](https://github.com/Marvisak/VIctory/pull/459) (closed) | AnoHunted | Mesh outputs within 32 KiB. Four corpus runs show it clears none of the 53 refused mesh programs. |
| VIctory [#305](https://github.com/Marvisak/VIctory/pull/305), [#705](https://github.com/Marvisak/VIctory/pull/705) | tarcisioads; Abhishek-Sood | The same 520 programs as #539, which XTP takes (item 7). |
| VIctory [#702](https://github.com/Marvisak/VIctory/pull/702) | wbkepler | A DXR backend, but the title's ray-tracing builder shaders do not translate yet (shader report). Too early. |
| VIctory [#526](https://github.com/Marvisak/VIctory/pull/526), [#542](https://github.com/Marvisak/VIctory/pull/542), [#178](https://github.com/Marvisak/VIctory/pull/178), [#190](https://github.com/Marvisak/VIctory/pull/190) | spidercraft; gingertonik; Ortomanic; LaunchDoesntCode | Linux, Wine and vkd3d-proton paths. XTP targets Windows. |
| VIctory [#231](https://github.com/Marvisak/VIctory/pull/231) | muhammed-aksoy | An integration of 138 PRs, 25 k lines, not a change. Use the testers' tree (4.6) as the reference instead. |
| VIctory [#458](https://github.com/Marvisak/VIctory/pull/458) | techselect | A mixed bag: mesh clamping, list slicing, a TDR watchdog and credits text. It conflicts, has no tests, and its parts are covered by items 2, 3 and 18. |
| xcl [#605](https://github.com/ivall/xcl/pull/605), [#651](https://github.com/ivall/xcl/pull/651), [#242](https://github.com/ivall/xcl/pull/242), [#615](https://github.com/ivall/xcl/pull/615), [#786](https://github.com/ivall/xcl/pull/786) | 0x6461726B; yyc160; mrp1no; Luxuryig | R9G9B9E5 targets, multi-plane PresentX and HDR colour space: VIctory has GPL-3.0 versions (items 8 and 15, and #212 for HDR). |
| xcl [#904](https://github.com/ivall/xcl/pull/904), [#903](https://github.com/ivall/xcl/pull/903), [#667](https://github.com/ivall/xcl/pull/667), [#1006](https://github.com/ivall/xcl/pull/1006) | mastrr9; RandomGithub12309; UnkwnDev | Lock-free hot paths and a pipeline library for xcl's own structures. XTP already has a library; its hot paths are measured by G2 first. |
| xcl [#680](https://github.com/ivall/xcl/pull/680), [#822](https://github.com/ivall/xcl/pull/822) | imnicho; broccolize | Memory windows from the descriptor heap: only meaningful with xcl's flat-memory shader ABI, which XTP does not adopt (shader §5). |
| xcl [#819](https://github.com/ivall/xcl/pull/819) | ZekkJJ | Parallel DirectStorage decode: XTP's pool is already ahead (formats §1). |

### 4.4 Checks to make in XTP (small, from xcl fixes)

xcl fixed these behaviours in its own code. XTP may have the same gap. Each
is a check first: a test, and a reimplementation only if the check fails.

| Item | What to check | Area |
|---|---|---|
| [xcl #990](https://github.com/ivall/xcl/pull/990) (merged), [#1165](https://github.com/ivall/xcl/pull/1165) | GFX10 image format 134 (1_5_5_5) is B5G5R5A1, not 135; formats 133–136 are 2 bytes per element. | formats · texture.rs; gpu · resource.rs |
| [xcl #1150](https://github.com/ivall/xcl/pull/1150) | `CreatePlacedResource` converts the clear value of a console format along with the resource format. | gpu · resource.rs |
| [xcl #935](https://github.com/ivall/xcl/pull/935) | A large committed BC texture gets its address range from the console layout, not a zero address. | gpu · resource.rs |
| [xcl #1195](https://github.com/ivall/xcl/pull/1195), [#1124](https://github.com/ivall/xcl/pull/1124) | `ExecuteIndirect` with an untranslated compute pipeline is skipped, as `Dispatch` is, not run with the graphics root signature. | gpu · indirect.rs |

### 4.5 VIctory after 07:12 UTC: branches offered in issues

VIctory has taken no new pull request since #710 (07:12 UTC). Contributors
write that pull-request creation is limited to collaborators. They now
post issues (#711–#884 by 13:25 UTC, 174 since 07:12), often titled
"[Ready to merge]" or "fix on a fork", each linking a branch of a VIctory
fork. Those branches are GPL-3.0 like VIctory, so the same rule applies:
take with credit, naming the fork, branch and commit.

| # | Item | State | Author | What it does | XTP area · backlog | Sym. | Action and why |
|---|---|---|---|---|---|---|---|
| 33 | [issue #847](https://github.com/Marvisak/VIctory/issues/847), [#736](https://github.com/Marvisak/VIctory/issues/736) | open | ItzMeGamer; iurysf | **Regressions in XTP's base.**<br>• #736: merged #612 (between `d2a4bae` and `ffe310e`) stops 323 programs that translated before, 42 of them vertex or pixel. #180 (item 7) brings 240 of them back correctly.<br>• #847: first-boot screens and menus black at `ffe310e` on an RX 6500 XT, fine at `d2a4bae`. Not #612: the bisect narrows it to the 19 commits between `9a92ccf` and `ffe310e`, still running. | shader · S0; all | D K | **Check first.** S0's census on XTP should show the 323. Take #180 for them, and follow #847's bisect next pass: XTP inherits whatever it finds. |
| 34 | [issue #841](https://github.com/Marvisak/VIctory/issues/841) | open | MD5Visual | In the world prologue, the title's 2560×1440 targets hold the rendered scene, and the 3840×2160 output stays zero. Seen on an RTX 3090 (testers' tree, 600–900 s without device removal) and confirmed on an RX 6750 XT. A diagnostics fallback that presents the 1440p target is on `mpDom/VIctory` `d6897fa`. | gpu · §6.1 (dark world), G20 | K | **Reimplement the diagnosis, not the fallback.** This is the most precise lead on the dark world so far: the scene renders, and the pass that writes the 4K output does not. G2's drop log for that pass names it. The 1440p fallback hides the bug and is diagnostics only. |
| 35 | [issue #876](https://github.com/Marvisak/VIctory/issues/876) (for [#850](https://github.com/Marvisak/VIctory/issues/850)) | open | Xvania7 | `Xvania7/VIctory` `fix/issue-850-typeless-view`. Views asked for in a typeless format (or `UNKNOWN`) on a typeless resource get the family's concrete format. Before, D3D12 refused them and left a null descriptor: cutscenes and post-processing read black. | gpu · G7 | K F | **Take with credit, with a fix.** A tester's A/B on an RTX 5090 and WARP shows the shader now reads the texel. It also shows the fallback picks DSV-only formats for `R24G8` and `R32G8X24`, which VIctory #651 (open) handles. Port both together. |
| 36 | [issue #877](https://github.com/Marvisak/VIctory/issues/877), [#878](https://github.com/Marvisak/VIctory/issues/878) | open | permadiego | Two `permadiego/VIctory` branches.<br>• `shader/geometry-shaders` (`6e8f760`, `828f825` on `ffe310e`): the legacy merged ES-GS translates, with both rings in LDS, the copy shader decoded, and pipelines with a geometry stage. That covers the 195 GS records, 13 distinct triples.<br>• `shader/mask-dword-joins` (`7ab94ab`): +47 mesh and vertex programs, 0 lost. | shader · G1, G2; M1 | D | **Take with credit, after S0.** The shader report planned G1–G2 as new work after xcl. This branch is GPL-3.0 and done on XTP's base. No GPU run of the changed programs yet. |
| 37 | [issue #776](https://github.com/Marvisak/VIctory/issues/776), [#783](https://github.com/Marvisak/VIctory/issues/783) | open | schmiticle (branch by lucaseddd); rocobalt1 (branch by DaviTostes) | #776: write-watch queries over nested buffers, one sweep per submission. On an RTX 4090: 50–80 ms less per world frame, 941–985 → 868 ms. #783: a copy that meets an unmapped page retried 64 KiB at a time, 2,300–2,500 kernel calls per world frame. Fix on `DaviTostes/VIctory` `gpu/copy-mapped-region-fallback`. | gpu · watch.rs, shadow.rs (interim, as item 6) | C | **Take with item 6**, same reasoning: cheap cuts in the CPU preparation until Phase 3. |
| 38 | [issue #407](https://github.com/Marvisak/VIctory/issues/407), [#880](https://github.com/Marvisak/VIctory/issues/880) | open | leonardocfirmino; anasssssssss1 | **Importing title memory, measured.**<br>• Feasibility: `OpenExistingHeapFromAddress` and `OpenExistingHeapFromFileMapping` (xmem's section views) both pass on AMD, an RTX 5070 Laptop and WARP. 64 MiB, every word checked, debug layer and GPU validation on. CPU and GPU addresses differ.<br>• What the copying bridge costs on an RX 9060 XT world frame (1,842 ms): copies into host buffers 927 ms (50 %), copy-back 330 ms (18 %), other preparation 214 ms, textures 82 ms. | gpu · G15, G2 | C | **Reimplement G15 with these as its first data.** The probes are standalone, not in a branch. They answer most of gpu open question 5 in the right direction. The cost table is the case for Phase 3. |
| 39 | [issue #853](https://github.com/Marvisak/VIctory/issues/853), [#840](https://github.com/Marvisak/VIctory/issues/840), [#785](https://github.com/Marvisak/VIctory/issues/785) | open | FabianDU; alessiodeia; fuzeofficlal | #853: per-lane size queries in the console's waterfall loops (`FabianDU/VIctory` `aabd9a7`, for #736). #840: 256-byte aligned CBV tables mirrored in place, without dynamic slots (`alessiodeia/VIctory` `1d21e06`). #785: a reproduction of a placed buffer-to-texture copy reading stale data after a same-list GPU write, with fix commits on two forks. | shader · R2; gpu · descriptors, G3 | D K | **Take #853 and #840 after review and S0 or G2 numbers.** Use #785's reproduction as G3's test. |
| 40 | [issue #798](https://github.com/Marvisak/VIctory/issues/798) | open | HrHd | A descriptor dword used as the address of another `s_load` stops translation: 879 programs on 1.0.2.1, the largest emit class left. A branch on `MartinIndra02/VIctory`, discussion still open. | shader · R1, R2 | D | **Watch; reimplement in R2 if no fix converges by the next pass.** It decides R2's design: the view's device address, not only its dimensions. |

### 4.6 Evidence worth reusing

- **The testers' integration tree.** `r4tedx/VIctory`, branch
  `integration/testers`, head `92bc27b`, 2026-11-24 08:43 UTC, base `d2a4bae`
  (older than `ffe310e`).
  - One merge commit per PR, so each can be found or left out.
  - `INTEGRATION.md` says why each excluded PR is out: #136, #189, #193,
    #305, #578 and others.
  - XTP can use that list as a first review of conflicts between PRs.
  - Its own runs with the title: no device removal on an RTX 2070 SUPER in
    8 min; world 0.5–0.7 fps; black playable section.
- **The shader census** in VIctory issue
  [#293](https://github.com/Marvisak/VIctory/issues/293).
  - It ranks untranslated shaders by draws lost per frame in the world.
    The top reasons have no PR: "an address in an SGPR that is no root
    parameter" (~600/frame), and "a descriptor neither loaded nor built"
    (~462, all mesh).
  - It has an offline table of each open shader PR against `ffe310e`
    (comment by h0useadv, 12:36 UTC).
  - The top reasons are shader R4 and R1/R2 in XTP's backlog. They are
    still XTP's own work.
- **NVIDIA world hang**: VIctory issues
  [#71](https://github.com/Marvisak/VIctory/issues/71) (110 comments) and
  [#468](https://github.com/Marvisak/VIctory/issues/468). Testers converge
  on the following:
  - wave64 programs run as 32-lane waves hang;
  - slow translated programs cross the TDR;
  - a loop limit of 32 and the PR set of item 3 keep the device up.

## 5. Other projects

Searched: GitHub repositories for GTA VI on PC, Xbox Series
compatibility layers and emulators, RDNA2 shader recompilers, and two port
trackers (`imyatesy-cmd/gta6-pc-tracker`, `NUXX-G/gta6-pc-tracker`), whose
project lists were used as leads only.

Stats are a snapshot from about 13:15–13:35 UTC. "Commits" counts
default-branch commits since 2026-11-23. Items marked "code" were read in
the commit or diff; the others are README claims.

### 5.1 Projects

| Project | Licence | What XTP may do | Approach | GTA VI, by its own reports | Activity | Head |
|---|---|---|---|---|---|---|
| [OpenCommunityEdition/OpenScarlett](https://github.com/OpenCommunityEdition/OpenScarlett) | AGPL-3.0-or-later (tools and tests MIT) | reimplement | XTP's design in C++, RDNA2 → HLSL → DXC; not a fork of californiaAlmonds' | game unnamed ("a newer retail game"). Start screen and intro at 30 fps on an RTX 5090; stops while the scene behind the menus streams. 25,601 of 25,889 programs translate. | 43 commits, 1 open PR | `7c85829` |
| [ThatRecompDev/five-star-recomp](https://github.com/ThatRecompDev/five-star-recomp) | GPL-3.0-or-later | take with credit (but C# and HLSL: in practice a design reference) | dynamic loader, HLE, RDNA2 → HLSL → DXC | reaches the story intro's title card, then stops on corrupted memory; "all 25,908 programs compile" | 361 commits, no PRs | `15219c4` |
| [fablinoo/palmwave](https://github.com/fablinoo/palmwave) | GPL-2.0-only (xcl fork) | ideas only | xcl plus patches, some of them byte patches of the title | first mission on an RTX 5080: about 7 k draws and 7 k dispatches a frame, 18 s to 2 min of GPU time per world frame, a uniform colour on screen | 533 commits, no PRs | `5a54236` |
| [LooperSalty/AnyXboxX](https://github.com/LooperSalty/AnyXboxX) | GPL-3.0-or-later | take with credit | its GTA VI runs are VIctory, branch `LooperSalty/victory` `anyxboxx/stable` (VIctory `main` plus 22 open VIctory PRs) | first cutscene, 5 min in the world without a GPU hang on an RTX 4070 SUPER; black scene, 0.3–0.5 fps | 84 commits, 96 merged and 10 open PRs, highest #139 | `b15d882` |
| [DesignWithTommy/Neonbridge](https://github.com/DesignWithTommy/Neonbridge) | GPL-3.0-or-later, **but** its README says xcl's GPL-2.0-only page pool was copied into `placement/pages.rs` | ideas only (licence conflict inside the project) | TuxBox fork (Apache-2.0) with credited parts of many projects | first mission, black, out of memory after 2 min | 406 commits, no PRs | `63daaa4` |
| [R0Y00/Xbox2Windows](https://github.com/R0Y00/Xbox2Windows) | MIT | take with credit | static recompilation research; shaders Python → HLSL → DXC | a modified diagnostic executable reaches loading. Its README's "26,500 of 26,503 shaders" is a scan; `docs/experiments.md` verifies 865. | 1,898 commits, 1,221 merged and 47 open PRs, highest #1503 (#1517 an hour later) | `309cfed` |
| [ubiquitarianism/leonida](https://github.com/ubiquitarianism/leonida) and forks ([LeonidaReforge](https://github.com/LeonidaReforge/Leonida), [ProjectLeonidaL](https://github.com/ProjectLeonidaL/leonida), [IAmNotSiob/leonida-ce](https://github.com/IAmNotSiob/leonida-ce)) | GPL-3.0-or-later | take with credit | static recompilation, offline RDNA2 → SPIR-V, Vulkan | ubiquitarianism does not boot to a frame. leonida-ce presents black frames, with a race crashing at about 2 min. All 26,503 shaders pass spirv-val. | ubiquitarianism: 2,852 commits, 560 open and 131 merged PRs, highest #739 | `2d73c24` |
| [Kajcsi/G6Recomp](https://github.com/Kajcsi/G6Recomp) | GPL-3.0-or-later | take with credit | early static recompilation and loader | no frame yet | 61 commits | `82ccaa5` |
| [uiliannn/VItoriaBr](https://github.com/uiliannn/VItoriaBr) | GPL-3.0-or-later | take with credit | VIctory at `ffe310e` plus 3 commits | world loads; `DispatchMesh` causes TDRs | 3 commits on VIctory | `dbff821` |
| [californiaAlmonds/OpenScarlett](https://github.com/californiaAlmonds/OpenScarlett) | MIT | take with credit | C++ translation layer | Minecraft at 60 fps; GTA VI early frames only | 89 commits, highest PR #50 | `46d8e2c` |
| [Batushn/KestrelX](https://github.com/Batushn/KestrelX) | MIT | – | C++ layer | no GTA VI | 38 commits, highest PR #41 | – |
| [jadeowaowa/scarlett-layer](https://github.com/jadeowaowa/scarlett-layer) | Apache-2.0 | – | C++ layer | Terraria menus | 14 commits | – |
| [sammwyy/xvalkyrie](https://github.com/sammwyy/xvalkyrie) | GPL-3.0 (no "or later" found) | – | Xbox One and Series layer | no GTA VI | 72 commits, highest PR #11 | – |
| [rpatty-dev/PercyScarlett](https://github.com/rpatty-dev/PercyScarlett) | GPL-2.0-only | – | Rust layer | first `PresentX` | highest PR #7 | – |
| [puffnfresh/rebox](https://github.com/puffnfresh/rebox) ([idvwork/rebox](https://github.com/idvwork/rebox) reposts it) | GPL-2.0-only | – | Linux only: WineGDK and vkd3d-proton, GPU address = CPU address through a Vulkan layer | – | 366 commits | – |
| [luizwidmer/palmetto](https://github.com/luizwidmer/palmetto), [OnlyMoisties/GTA6-BootTest](https://github.com/OnlyMoisties/GTA6-BootTest) (no licence), [vladbondarenko97/gta6-testbench](https://github.com/vladbondarenko97/gta6-testbench) (MIT) | – | – | runtime without a frame; stand-in DLLs to a first UI screen; a test-harness scaffold | – | – | – |

### 5.2 Items

Numbering continues from section 4.

| # | Item | State | Author | What it does | XTP area · backlog | Sym. | Action and why |
|---|---|---|---|---|---|---|---|
| 41 | [palmwave `80492db`](https://github.com/fablinoo/palmwave/commit/80492db9) | commit (code) | Stellarr Studio | Measures where the title's GPU heaps should live. GTA VI commits about 4.4 GB of GPU heaps, and xcl puts them in imported host memory that the GPU reads across PCIe. On an RTX 5080 the same dispatch takes **431 ms from host RAM against 0.45 ms from video memory**: descriptor loads cost 4–7 µs a workgroup, with the SMs at 100 % and PCIe busy. An opt-in switch moves heaps above a size to the VRAM windows. | gpu · G15–G17a, risk "Bandwidth" | C H | **Reimplement the rule in G16–G17a.** Imported host memory is only for small, CPU-read buffers (constants, readback, labels). The title's GPU heaps go to VRAM tiles that follow commits. This turns gpu.md's bandwidth risk into a measured fact, and a G16 that imported everything would be slower than the shadows. |
| 42 | [OCE OpenScarlett `a062bcf`](https://github.com/OpenCommunityEdition/OpenScarlett/commit/a062bcf5) | commit (code, with `tests/unit/section_heap_check.cpp`) | OpenScarlett | A section imported with `OpenExistingHeapFromFileMapping` stays committed after the process ends, even with buffer and heap released first. Measured on Windows 10 19045 and an RTX 5090: 512 MB of 512 left behind each time. Two days of runs left 200 GB committed until a reboot. A view of the section imported with `OpenExistingHeapFromAddress` is the same memory to the GPU and leaks nothing. | gpu · G15; hle · xmem, K1 | H | **Reimplement in G15: import xmem's physical-page sections by address (a view), never by file mapping.** G15 proposes trying `…FromFileMapping` for them, and VIctory issue #407's probe passed with it. A probe checks correctness, not what stays behind after exit. G15's test must check commit after the process ends. |
| 43 | [Neonbridge `1ddad1c`](https://github.com/DesignWithTommy/Neonbridge/commit/1ddad1c9), [OCE `7c85829`](https://github.com/OpenCommunityEdition/OpenScarlett/commit/7c85829e) and [`f8f5b79`](https://github.com/OpenCommunityEdition/OpenScarlett/commit/f8f5b796), [palmwave `9e78d0b`](https://github.com/fablinoo/palmwave/commit/9e78d0bd) | commits (code, no tests) | DesignWithTommy; OpenScarlett; theaustinayers | Pipeline policy, measured in the title:<br>• Neonbridge: VIctory's 500 ms per-frame wait took the intro from 14.6 to 1.6 fps; up to 11 binds a frame spent 300 ms waiting for pipelines the driver was compiling. Cut to 50 ms.<br>• OCE: compute pipelines built on first use, and large or uncompiled ones on a second, low-priority set of workers. In 50 s, 3 compute pipelines built instead of 2,600; 16.5 GB committed instead of 26; start screen at 12 s instead of 70.<br>• palmwave: preloads on low-priority workers; skipped dispatches in the first world frames 2,170 → 94–150. | gpu · G12 | D | **Reimplement in G12, on top of VIctory #275 and #546 (item 5).** They agree with #275's 8 ms steady budget and add two rules: build compute lazily, and keep heavy builds off the workers that serve binds. XTP inherited the 500 ms budget. |
| 44 | [five-star-recomp `docs/shader-translation.md`](https://github.com/ThatRecompDev/five-star-recomp/blob/main/docs/shader-translation.md), `Translation/Waves/` | `main` (code, with assembled wave64 fixtures) | ThatRecompDev | Wave-size emulation by program:<br>• a "floor", the narrowest wave that gives the console's results;<br>• a missing-lane analysis that lets VS/PS run on narrower waves;<br>• "supersteps", one group barrier per wave operation, for mesh and compute.<br>Also: packed f16 attributes split into halves, with VS/PS pairs linked from the PS inputs, and p10/p20 through `GetAttributeAtVertex`. | shader · L1–L3, P1, P2 | W D | **Reimplement as design input for L3 and for mesh.** The licence allows taking it, but it is C# emitting HLSL, so nothing ports as is. The floor and missing-lane analyses cover what the VIctory stack does not: VS/PS and mesh on 32-lane hosts. |
| 45 | [OCE OpenScarlett `0aa3f8f`](https://github.com/OpenCommunityEdition/OpenScarlett/commit/0aa3f8f9) | commit (code, hand-assembled ISA tests on an RTX 5090) | OpenScarlett | Wave64 on 32-lane GPUs with each PC thread running console lanes L and L+32, with exact 64-bit masks and no barriers. | shader · L1, L2 | W H | **Reimplement only if item 1's exchanges prove too slow** (item 30, GPU time). It trades group-memory exchanges for twice the registers per thread. |
| 46 | [G6Recomp `32ba134`](https://github.com/Kajcsi/G6Recomp/commit/32ba134a) | commit (code, tests) | Kajcsi | Decodes byte 5 of the FSSM pixel-input record: the register holding a packed f16 partner. 59,185 of 59,191 reads land on the right half. | shader · P1 (native.rs) | D | **Use it as the cross-check of VIctory #592 (item 13).** GPL-3.0-or-later C++: take with credit if XTP's decode differs, otherwise nothing to port. |
| 47 | Xbox2Windows [#327](https://github.com/R0Y00/Xbox2Windows/pull/327), [#1316](https://github.com/R0Y00/Xbox2Windows/pull/1316), [#1348](https://github.com/R0Y00/Xbox2Windows/pull/1348), [#1114](https://github.com/R0Y00/Xbox2Windows/pull/1114) | merged (code, tests on an RTX 3080 and WARP) | thegl0w; R0Y00; Jess-Doit; archielcqp-a11y | #327: xmem graphics allocations imported as existing heaps. #1316: vertex streams bound to live placed buffers, without staging. #1348: a console queue `Wait` done on the GPU. #1114: wave64 emulation in group memory, uniform control flow only. | gpu · G15, G16, G10; shader · L1 | C | **MIT references for G15, G16 and G10.** None is new against xcl. Python, so the behaviour helps more than the code. |
| 48 | AnyXboxX `LooperSalty/victory` `anyxboxx/stable` | branch | LooperSalty | VIctory `main` plus 22 open VIctory PRs: #189, #89, #638, #346, #393, #440, #464, #490, #497 and others. Ran 5 min in the world on an RTX 4070 SUPER. | – | – | **A second tested shortlist**, besides the testers' tree (4.6). It carries #189, not the wave64 stack. |

### 5.3 Ignore

| Item | Why |
|---|---|
| Neonbridge's code | It contains xcl's GPL-2.0-only page pool, by its own README. Its findings (item 43) are used; its files are not. |
| [VItoriaBr `279c90a`](https://github.com/uiliannn/VItoriaBr/commit/279c90a1) | Shadow-buffer states per list, no tests. VIctory #334 (item 12) does the same, with testers' runs. |
| [leonida #592](https://github.com/ubiquitarianism/leonida/pull/592) | R9G9B9E5 as RGBA16F, on Vulkan: VIctory #492 (item 8) does it for D3D12. |
| KestrelX "dual-wave" | Its code emits the native wave width; it does not run 64 lanes on NVIDIA. |
| californiaAlmonds OpenScarlett's rect-list fix | It draws half of each rectangle. |
| scarlett-layer, xvalkyrie, PercyScarlett, rebox, palmetto, GTA6-BootTest, gta6-testbench | No GTA VI progress past the first frames, Linux only, or no licence. Nothing XTP lacks. |

### 5.4 Lures

These names came up in searches, and some are tracked as candidates by
the port trackers. They look like the usual bait: a "GTA VI PC" or
"emulator" name, installers, near-identical copies. They were not opened
past their search entry. Do not download from them.
- `python001-dev`, `stancaedy10-glitch`, `mohamedhamdytaha`, `WLfear`:
  `GTA-6-Switch-Version--PC-Emulator-2026`.
- `Towerflareinvigorate/gta-vi-pc-version`.
- `bitovvnew/gta-vi-pc-emulator`.
- `WebConstableReckon/GTA-VI-Windows-Beta-PC`.
- `WerewolfRepairRedeem/GTA-6-Windows-PC-Beta`.
- `ViceroyModulator/gta6-ps5-pro-to-windows`.
- `wo12usernamewasntavailableig/GTA-VI-PC-LARP`.

## 6. How to take a VIctory PR (for the porting tasks)

1. **Fetch the head the testers measured**, not the latest push, when an
   item names one. Use `gh pr diff N -R Marvisak/VIctory --patch` or fetch
   `pull/N/head`.
2. **Apply onto XTP with the rename.**
   - Paths: `crates/victory-*` → `crates/xtp-*`, `apps/victory` →
     `apps/xtp`.
   - Identifiers: `victory_*` → `xtp_*`.
   - Variables: `VICTORY_*` → `XTP_*`.
   - Docs: `victory.toml` → `xtp.toml`.

   A clean upstream PR applies with `git am -3` after a `sed` over the
   patch. Keep the upstream commits: `git am` keeps their author.
3. **Credit.**
   - The commit message ends with `Ported from Marvisak/VIctory#N (head
     <sha>) by <login>`, plus a `Co-authored-by:` trailer taken from the
     upstream commit's author line when the porter squashes.
   - File headers that carry a notice keep it (NOTICE.md).
4. **Review it as new code.** No upstream maintainer has reviewed these
   PRs, and many say they are AI-assisted. The XTP reviewer reads the
   diff, runs its tests, and checks the measurement claim against XTP's
   own G2 numbers.
5. **One PR per porting task.** A stack is one task, ported in its
   order.
6. **Never take a conflicting pair.** Known conflicts:
   - #189 with the wave64 stack;
   - #578's first head with the stack;
   - #298 and #704 with the shader stack of item 7;
   - #346 and #159 need #334's state tracker;
   - #431 overlaps #595.

## 7. What changes in XTP's backlog

These are proposals for the porting tasks, not edits to `docs/analysis/`
(owned elsewhere).
- gpu: G8 and part of G9 become "port VIctory #595". G12 becomes "port
  #275 and #546, then add compute-first waits, lazy compute builds and a
  separate pool for heavy builds" (item 43). G15 imports sections by
  address only and checks commit after exit (item 42). G16 keeps imported
  host memory for small CPU-read buffers, and the title's heaps go to
  G17a's VRAM tiles (item 41). Add new items for #493 and
  #483, #89 and #492, #469, #643, and #644.
- shader: L1 and L2 become "port the wave64 stack" (item 1), early rather
  than late, because the dev machine is NVIDIA. I1 becomes "port #539".
  P1 becomes "port #592 and #621". P2 becomes "port #453". R2's first part
  becomes "port #180".
- formats: F1 is open upstream, not merged (#658). F3 starts from #234.
- tooling: B15 gets #454 (the title's watchdog). B6 and B11 take AnyPS5's
  report form and A/B output as process.
