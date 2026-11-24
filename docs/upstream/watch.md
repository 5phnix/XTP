# Upstream watch: what XTP should take or reimplement

**Pass 4, 2026-11-24.** GitHub read between 17:49 and 18:05 UTC, read-only
(`gh api`, `gh search`). Nothing was posted, starred or opened. xcl now
lives at `xcl-dev/xcl`: the older `ivall/xcl` links below redirect, but
the search API answers only to the new name.

- Section 0 is this pass: what appeared after pass 3's marks (17:18 UTC),
  wave64 mesh programs on 32-lane hosts first, then Series S, and what
  changed in the earlier items (0.8). Earlier items that answer the
  wave64 mesh question are read again here (items 95, 96).
- Section P3 is pass 3 (17:18–17:40 UTC), items 80–93.
- Section P2 is pass 2 (16:20–16:40 UTC), items 49–79.
- Sections 1 to 7 are pass 1's list (12:10–13:40 UTC), items 1–48.
- The next pass starts from the marks in [README.md](README.md).

## 0. Pass 4: since 17:18 UTC, wave64 mesh first

### 0.1 Summary

- **Volume.**
  - VIctory: 118 new PRs and 12 issues (#1310–#1440), none merged. `main`
    is still `ffe310e`.
  - xcl: 56 new PRs and 4 issues (#1530–#1589). `main` is still
    `9e3831e`.
  - AnyPS5: nothing new.
  - Triage by title, keyword searches of both trackers (wave64, mesh,
    readfirstlane, barriers, Series S, FSR, indirect), and the other
    projects' commits. About 35 items were read.
- **The question: how other projects run the title's wave64 mesh
  programs on 32-lane GPUs.** Four answers, the most portable first.
  1. **Prove that most of them need no whole wave.** five-star-recomp's
     window analysis (item 95) finds that 6,217 of the pack's 8,765 mesh
     programs give console results on any host wave of 4 lanes or more,
     as they are. Its rules cover XTP's three blockers:
     - a `v_readfirstlane` is right when the host wave holds an active
       lane of the console wave: after EXEC = −1, on the taken side of an
       execz branch, or for a value only the lanes active then wrote;
     - a mesh barrier after the output-count message is right when the
       host wave that skips it writes no output before its next barrier;
     - a divergent path that a host wave skips is right when it leaves
       nothing live behind.

     LDS shared between lanes still needs the whole wave.
  2. **For the rest, every thread passes every group barrier.**
     five-star's "supersteps" (2,511 of the remaining 2,548 compile) and
     VIctory #1433 (item 94) do this. #1433 pairs barriers by count and
     turns `s_barrier` into a stamped wait. It covers compute groups of
     64–1,024 threads: in a Series S world on an RTX 4050 Laptop, refused
     compute programs went from 112 to 1. Neither form makes one half
     spin on the other. #1433 sits on #439, which XTP already has.
  3. **xcl pairs the two halves through arrival counters** (a bounded
     spin) (item 96). XTP's design note dropped that form. xcl's NVIDIA
     runs show the spin does progress there, but #1433 makes it
     unnecessary.
  4. **Run mesh as compute plus a vertex-pulling draw** (VIctory #961,
     five-star's fallback, xcl #724; item 98). The compute forms of
     wave64 then apply. On the 4080, which has mesh shaders, this is only
     needed for programs whose outputs pass D3D12's 32 KiB.
- **NVIDIA device removals once mesh programs run.** The newest reports
  trace most NVIDIA world hangs to mesh draws that read null heap slots:
  the capped heap's 65,536 dynamic slots run out. Reserving 2^18 stops
  them on an RTX 5070, a 5060 Ti and a 5070 Ti (item 97). XTP keeps 2^16
  on a capped heap. Its own Series S run of 17:39 UTC already dropped
  draws for "no dynamic slots", while every wave64 mesh program was still
  refused.
- **Series S.**
  - A Series S world on an RTX 3080 held 5–7 min with wave64 mesh
    programs run as plain 32-lane waves (item 101). Fences and poles
    were drawn. Ground and buildings were missing: about 730 indirect
    mesh dispatches a frame were dropped.
  - five-star fixed its game-memory corruption: a translator bug in
    wave64 mask pairs (item 100). It also added a check for GPU buffers
    that keep their old physical frames after a remap, which is the
    class of XTP's crash object.
- **FidelityFX HLE (#1284):** nothing new since 17:07. A seventh UAV
  clear implementation came in (item 105).
- **Indirect producers (#1225, #1231):** no new commit, and #1276
  recovers the same two programs. #1407 lowers "a lane mask holding
  lanes elsewhere read as data", the second reason AnyXboxX named for
  the refused producer passes (item 103).

### 0.2 How to read the mapping

The tags are P3.2's. SS is today's state:

| Tag | Finding |
|---|---|
| SS | Series S mode, the person's direction (roadmap #15). The world holds 150 s at 0.9–1.7 fps, and every UI frame is drawn. There is no 3D: all 9,282 mesh programs are wave64 in 128-lane groups, and on the 4080 XTP refuses them (VIctory #578). `XTP_WAVE64_MESH=allow` lets none of them through, because each has a `v_readfirstlane`, an `s_barrier` and divergent flow. Task mesh64 (0a31f0cb) works on this. |
| W64 | NVIDIA and wave64 (32-lane host, the RTX 4080 SUPER). |
| DRAWS-P | Series S world: draws dropped for want of a pipeline (1,196 a frame in session C). |
| DRAWS-I | Series S world: indirect commands dropped (3,515 a frame). |
| STALL, VRAM | Series X: parked. |

### 0.3 Wave64 mesh programs on 32-lane hosts

| # | Item | Licence · state | What it does (measured) | Maps to | Action and why |
|---|---|---|---|---|---|
| 94 | VIctory [#1433](https://github.com/Marvisak/VIctory/pull/1433) (anasssssssss1, `anasssssssss1/VIctory` `fix-console-flow-multi-wave` `b3efa1c`, +3,544/−118 in 16 files, opened 17:58) | GPL-3.0 · open, stacked on #439 | **The console flow for groups of several console waves.**<br>• Before: a wave64 program that needs its whole wave ran in the console flow only in groups of one console wave. The waves of a bigger group branch and loop apart, so group barriers paired by place would hang or pair the wrong halves.<br>• Group barriers are now paired by count. Each thread counts the barriers it has passed (one groupshared dword per thread). Both halves of a console wave reach each exchange at the same count.<br>• The program's `s_barrier` becomes a wait. The half stamps its arrival (the count mod 4, and its phase), then keeps passing group barriers until every half has arrived or ended.<br>• A thread that ends, or reaches the loop limit, stamps its half as ended and keeps passing barriers. No exchange is ever left one thread short.<br>• Whatever must agree across threads is read after a barrier, from stamps written before it. Each check is one wave operation (at most 32 halves).<br>• Only groups of whole console waves (64 to 1,024 threads), and only where the LDS plus `threads + 3 × halves` dwords fit in 32 KiB.<br>• 4 GPU tests, exact on an RTX 4050 Laptop (split) and a Radeon 680M (native wave64): lane 40 read in groups of 128 and 256, console waves that branch and loop differently, LDS ordered between console waves by `s_barrier`.<br>• In the title, Series S, RTX 4050 Laptop, on the testers' update 9: refused compute programs in the world went from 112 to 1. The picture stayed black.<br>• Compute only. Mesh programs are not covered. | shader · lower (console flow); `docs/wave64-on-wave32.md`, "Groups of several console waves"; task mesh64 · W64, SS | **Take with credit, as mesh64's base.** It is the group-wide form XTP's design note leaves open, built on the #439 that XTP already has.<br>• Then extend it to the mesh stage: a 128-lane mesh group is two console waves. XTP calls `setMeshOutputCounts` at entry with the declared maxima, so that call stays in uniform flow.<br>• Review it as a port: one author, opened at the end of this pass, no second tester yet.<br>• Measure on the 4080 what a group barrier per exchange costs. |
| 95 | five-star-recomp `docs/shader-translation.md` ("Mesh windows", "Supersteps"), `Translation/Waves/WindowAnalysis`, `rdna_superstep.hlsli`; commits [`f652127`](https://github.com/ThatRecompDev/five-star-recomp/commit/f65212736) and [`910f023`](https://github.com/ThatRecompDev/five-star-recomp/commit/910f0239a) (2026-11-23) | GPL-3.0-or-later (C#, HLSL) · `main` | **Which mesh programs need their whole wave, and a form for those that do.**<br>• A host wave narrower than the console's (a "window") sees only its part of every mask. The analysis tracks, per variable, which bits are the console's. A program's "floor" is the narrowest host wave that gives console results.<br>• Masks used as masks are right in each lane; a ballot read as a number is not.<br>• `v_readfirstlane` is right in the bits all active lanes agree on, when the window holds an active lane: after EXEC = −1, on the taken side of an execz branch, or for a value only the lanes active then wrote.<br>• A path that a window skips is right when it keeps EXEC empty on the window's lanes, leaves nothing unguarded live where the paths meet, and holds no store, `s_endpgm` or barrier.<br>• The output-count message (GS_ALLOC_REQ) on the console's path is fine. So is a mesh barrier after it, when the skipping window writes no output before its next barrier. LDS shared between lanes keeps the whole wave.<br>• On the title's pack: of 8,765 mesh programs, 6,217 have floor 4 and 2,548 need the whole wave. First reasons: a first-lane read of a per-lane value in an output or in GS_ALLOC_REQ (1,338), an integer VCC branch (588), a VCC branch whose mask lost its relation to EXEC (304), an execz path that cannot be skipped (246).<br>• Supersteps, for the rest: blocks are cut at each wave operation. A thread stores its value and bit to groupshared and stops. Once every thread has stopped (at an operation, an `s_barrier` or the end), one group barrier passes and each thread reads its console wave's result. A thread at an `s_barrier` waits until no thread is at an operation. An LDS access is a step of its own.<br>• 2,511 of the 2,548 get a superstep form that compiles; the other 37 compile only through the compute fallback (item 98). WARP and an RTX 4090 accept them.<br>• Tests: assembled programs on WARP at 2, 4, 16 and 64 lanes, including a 128-thread group whose two console waves branch apart. No NVIDIA world measurement is given. | shader · lower (mesh), a new analysis; gpu · pipeline/host.rs (`Needs::mesh_reason`) · W64, SS | **Reimplement, the window analysis first.** It would replace XTP's blanket mesh refusals (`MESH_FIRST_LANE`, `MESH_LDS_IN_ONE_WAVE`) with a proof per program. On five-star's count, about 70 % of the mesh programs then run as they are, with no exchange.<br>• Count XTP's 9,282 under the same rules first, offline (`xtp shader-survey --host-lanes 32-32`).<br>• Then item 94, or supersteps, for the rest.<br>• It is C# emitting HLSL, so nothing ports as is; credit the design. |
| 96 | xcl `docs/wave64-on-wave32.md` and the merged [#39](https://github.com/xcl-dev/xcl/pull/39), [#67](https://github.com/xcl-dev/xcl/pull/67), [#94](https://github.com/xcl-dev/xcl/pull/94) (2026-11-22/23); [#1252](https://github.com/xcl-dev/xcl/pull/1252) (not-rex, draft, `77a8190`), [#1193](https://github.com/xcl-dev/xcl/pull/1193) (zondy1i, `7c16823`) | GPL-2.0-only · `main`; open | **How xcl runs wave64 mesh programs on NVIDIA**, in this document's words:<br>• A console wave runs as two 32-lane halves of one group. Both hold the whole 64-bit EXEC, VCC and SGPRs, so they take every branch alike. They meet in groupshared memory for masks read whole, `v_readlane` / `v_readfirstlane`, `ds_bpermute` and coherent scalar loads.<br>• With one console wave per group, each exchange is a group barrier. With several (a 128-lane mesh group), each pair meets through arrival counters with double-buffered slots. The wait is bounded at 2^20 polls; past that, the wave goes on with half-local values instead of hanging. The console's `s_barrier` stays a group barrier.<br>• Host waves take their console thread ids from a groupshared counter as they arrive: NVIDIA does not form waves from consecutive thread indices in 8×8×4 groups.<br>• A mask read only lane by lane is not exchanged (a backward liveness pass).<br>• #39, the first form (independent halves), took the title's mesh pipelines through xcl's hook from 0 of 7 to 7 of 7 on an RTX 5050.<br>• #1252: the halves meet before every LDS access, since a console wave orders LDS by instruction. Tested on an RTX 3080 and an RTX 4090 (a 20-run matrix). The title ran on an RTX 5080 into world loading with no device removal.<br>• #1193: no exchange for a `v_readfirstlane` of a value proven uniform. Green on an RTX 5080 and a 5060. An AMD run with the split forced shows failures that are `main`'s.<br>• Cost on an RTX 3090, in a worst-case loop: 1.0–1.8× with a group barrier, 1.1–4.3× with the counters. | shader · lower (console flow) · W64 | **Ideas only; do not take the counters.** XTP's design note dropped them because D3D12 promises no forward progress, and items 94 and 95 do without.<br>• One fact to keep: the counter form passed xcl's tests on five NVIDIA cards (3080, 3090, 4090, 5070 Ti, 5080), and one title run on the 5080, without a hang. In practice, the note's inference that the wait would never be met there does not hold on NVIDIA; D3D12 still promises nothing.<br>• #1193's uniform first-lane read is the proof `MESH_FIRST_LANE` needs beyond the group id (`v5`). |
| 97 | VIctory issue [#747](https://github.com/Marvisak/VIctory/issues/747) (Vaximous; urattems' and MrEjecutor's runs), [#1349](https://github.com/Marvisak/VIctory/pull/1349) (wbkepler, `eb5fea7`), issue [#264](https://github.com/Marvisak/VIctory/issues/264) (xFIR3x's run), [#1133](https://github.com/Marvisak/VIctory/pull/1133) (DeerSilver's run) | GPL-3.0 · open | **NVIDIA's world hang is often a mesh draw that reads null heap slots.**<br>• NVIDIA caps a shader-visible view heap at 1,000,000 slots, an RTX 5060 Laptop too (#1133). The title asks for 1,245,184. With 2^16 dynamic slots left, the world's tables run out of room.<br>• RTX 5070, Series X: `main` lost the device in 3 runs of 3. DRED stops in an indirect mesh draw whose `u8`–`u10` heap indices are null slots. With #578 and 2^18 dynamic slots: 11 min, 8 of them in the world, no device removal, and no drop for want of slots.<br>• RTX 5060 Ti, one change at a time, 240 s runs: `main` lost the device in 2 runs of 2; with #578 alone, 2 of 4; with 2^18 added, 0 of 2; with #322 too, 0 of 2.<br>• RTX 5070 Ti (#264): a `DispatchMesh` hung on null slots. The AnyXboxX tree with 262,144 dynamic slots ran 13 min.<br>• #1349 reserves 2^18 slots on a capped heap (737,856 for the title, 262,144 dynamic) and trims fresh tables to their first-to-last used entry. Unit and device tests only. | gpu · descriptor/host.rs (`MIN_DYNAMIC`), draw.rs · W64, SS, DRAWS-P, DRAWS-I | **Take #1349 with credit, before mesh64 lets mesh programs through.**<br>• XTP keeps 2^16 dynamic slots on a capped heap (`descriptor/host.rs:148`), which leaves the title 934,464.<br>• Its Series S run of 17:39 UTC (`E:\xtp-data\cache\play\20261124-173913\run.log`) already dropped draws for "no dynamic slots" (the count reached 128 at 17:42:38), while every wave64 mesh program was refused (537 host pipelines by 17:42:51).<br>• First, log which title slots above 737,856 a Series S frame uses: those would then read the null slot (#1133's clamp). |
| 98 | VIctory [#1385](https://github.com/Marvisak/VIctory/pull/1385) (Ctrl-Alt-Haze, for #201, `9e77b5c`), [#961](https://github.com/Marvisak/VIctory/pull/961) (EduBertozzi, `31568ce`; runs by zlwbbb-del and L1DRE); VItoriaBr [`bd33acc`](https://github.com/uiliannn/VItoriaBr/commit/bd33acc69); xcl [#724](https://github.com/xcl-dev/xcl/pull/724) (Cattn) | VIctory GPL-3.0 · open; VItoriaBr GPL-3.0 · commit; xcl GPL-2.0-only · open | **Mesh output counts, and mesh programs run as compute.**<br>• #1385: D3D12 refuses a mesh shader whose outputs pass 32,768 bytes, and the declared 256 vertices and 256 primitives can. It bounds the output-count message's `M0` through scalar code (moves, shifts, masks, min/max, selects, phis with widening). It narrows the declared maxima only when every site is proven, and passes the program's own counts, clamped, to `SetMeshOutputCounts`. 7 synthetic tests on an RTX 5070 Ti and WARP.<br>• #961: a mesh program becomes a compute shader that writes vertices, indices and per-primitive records, plus a vertex shader that reads them back. It is used on hosts without mesh shaders, by switch, or when the outputs pass 32 KiB. GTX 1070 Ti (no mesh shaders), on #1086's tree with #189's emulation: the world drawn at 900–1,970 draws a frame and 0.2–0.6 fps for about 2 min, after two fixes (root signature spaces, a scratch pool).<br>• VItoriaBr bounds mesh outputs and indices by the declared maxima.<br>• xcl #724 runs xcl's two-pass mesh form (compute, then mesh) on `DispatchMesh` and `ExecuteIndirect`. | shader · lower/mesh.rs; gpu · pipeline · W64, DRAWS-P, SS | **Check, then take #1385 with credit if it applies.** XTP passes the declared maxima and culls the extra primitives (`lower/mesh.rs`). It refuses only past 128 threads or 256 outputs: count the mesh programs whose outputs pass 32 KiB.<br>**Ignore #961 on the 4080**, which has mesh shaders, unless item 94's form does not fit in a mesh stage. Then #961 is the way to give mesh programs the compute forms. |
| 99 | VIctory issue [#1347](https://github.com/Marvisak/VIctory/issues/1347) (Ganfieof) with [#1392](https://github.com/Marvisak/VIctory/pull/1392) (kunalkhude99); issues [#1348](https://github.com/Marvisak/VIctory/issues/1348) (icdmkg2), [#1317](https://github.com/Marvisak/VIctory/issues/1317) (HenriqueSBueno), [#1323](https://github.com/Marvisak/VIctory/issues/1323) (darkjiros), [#71](https://github.com/Marvisak/VIctory/issues/71) (ThomasHFWright's runs); [#1400](https://github.com/Marvisak/VIctory/pull/1400) (piyonchu), [#1414](https://github.com/Marvisak/VIctory/pull/1414) (ESAT586) | GPL-3.0 · reports, open | **Device removals reported since the mark.**<br>• #1347 (RTX 4060): lost on a 32×1×1 compute dispatch, right after three dropped compute programs. #1392 emits a UAV barrier after any dropped compute dispatch: the next dispatch had read those UAVs with no barrier.<br>• #1348 (RX 6750 XT and WARP): lost at frame 1370 every time; with DRED's breadcrumbs, 240 s without a loss. A timing race, not a program.<br>• #1317 (RTX 3080): the hang pair is a mesh program with no loop and a pixel program whose light-list loop ends on an equality with a bound read from a buffer. With loop limits of 1,024 in the pixel, mesh and vertex stages: 17 min without a loss.<br>• #1323 (RTX 4090, Series X): a 5–6 s submission on entering the world; with a 60 s TDR, none. Then the title's watchdog stop at +0x23cf683, whose report names an unsignalled cross-queue barrier.<br>• #71 (RTX 5090, Linux): an MMU fault around one compute dispatch, twice. Skipping that program: 300 s clean.<br>• #1400, #1414: one host list twice in one submission resets its prelude and is submitted twice: `DEVICE_HUNG`. | gpu · list.rs, host.rs, queue.rs; shader · loop limits (G11) · W64, SS, STALL | **Check XTP, each small:**<br>• a dropped dispatch followed by a UAV barrier (#1392);<br>• one list twice in one submission (#1400);<br>• loop limits in mesh and pixel programs, not only compute (#1317, G11).<br>XTP's `queue.rs` `wait` returns `S_OK` without waiting, as VIctory's does. #1317 and #1323 suspect a consumer that runs before the producer it waits for. Log which queue waits the Series S world makes before changing it. |

### 0.4 Series S: the world and the crash

| # | Item | Licence · state | What it does (measured) | Maps to | Action and why |
|---|---|---|---|---|---|
| 100 | five-star-recomp [`2781f9c`](https://github.com/ThatRecompDev/five-star-recomp/commit/2781f9ca7), merge [`3160ea5`](https://github.com/ThatRecompDev/five-star-recomp/commit/3160ea569), status [`41b9e7e`](https://github.com/ThatRecompDev/five-star-recomp/commit/41b9e7e43); PR [#5](https://github.com/ThatRecompDev/five-star-recomp/pull/5) (marquezinii, merged, `fad0b7f`); Neonbridge [`e8f1f34`](https://github.com/DesignWithTommy/Neonbridge/commit/e8f1f343c), [`5958301`](https://github.com/DesignWithTommy/Neonbridge/commit/595830139) | GPL-3.0-or-later (C#) · `main`; Neonbridge: ideas only (5.1) | **five-star's game-memory corruption is fixed** (item 84's follow-up).<br>• The cause was its translator. `InstanceCulling~CS_Prop_BatchCull` sets s43 = s60 + 4, the base of its argument array. On another path, a `v_cmp` writes s[42:43]. Mask tracking went by emission order, so the translation read s43 as that mask's high half. The program's InstanceCount atomics then landed near the start of the transient pool.<br>• Now every mask write also sets a variable for its high dword (a ballot), and integer reads use that variable. A WARP case covers the path. Boot 18 ran past 200 s without corruption.<br>• The new blocker: about 200 s in, the GPU frame takes too long. Three compute programs run their loops to the 65,536-step guard, and some culling passes read constant buffers past their descriptor's end.<br>• PR #5: a buffer of device slot 133 keeps the physical frames that were mapped when it was made. When the game remaps a page under it, the CPU sees the new frame and the GPU the old one. Once that old frame is mapped elsewhere, GPU writes through the buffer change memory there. The check logs each such remap, and where the old frame turns up next.<br>• Neonbridge took both, with credit. | shader · IR (mask pairs); hle · xmem.rs; gpu · resource.rs (placed buffers) · SS, W64 | **For the series-s crash, reimplement both checks with credit** (C#):<br>• a test where a `v_cmp` writes s[n:n+1] on one path and s[n+1] is an integer on another. XTP's SSA should keep them apart; the test proves it.<br>• In XTP's xmem diagnostics (`xtp_diag_map`), a line when a page under a live host buffer or placed resource is remapped or unmapped, and where its old frame is mapped next. XTP's overwritten object lies in a 64 KiB view of one physical page that xmem placed: this is that class of fault. |
| 101 | VIctory issue [#71](https://github.com/Marvisak/VIctory/issues/71): DuarttBR's [RTX 3080 report](https://github.com/Marvisak/VIctory/issues/71#issuecomment-7139417481) (`DuarttBR/VIctory` `rgarage`) and Sunkiller-qc's [reading of it](https://github.com/Marvisak/VIctory/issues/71#issuecomment-7139798289); [#1322](https://github.com/Marvisak/VIctory/pull/1322) (KevinPratap, `7a03753`); issue [#1317](https://github.com/Marvisak/VIctory/issues/1317) | GPL-3.0 · reports, a branch, open | **A Series S world on a 32-lane card that holds, and why its colour is zero.**<br>• RTX 3080 10 GB, Series S, `ffe310e` plus 21 PRs (#189's emulation, but not #578): over several runs, 5 to 7 min in the world with no device removal, at 0.2–1.6 fps and about 18.5 GB of commit. Wave64 mesh programs run there as plain 32-lane waves.<br>• The depth target shows fences, poles and a bin. Ground and most buildings are missing: about 730 indirect mesh dispatches a frame are dropped as "prepare failed (compute false, mesh true)".<br>• The G-buffer pass writes (a forced constant covers 28 % of its target) and the first texture samples are right, but the material colour comes out 0.<br>• Textures read only by heap index were never uploaded; the branch uploads them at each bind, at most 192 MiB of title memory a frame. #458's cap of 384 placed textures was raised to 16,384.<br>• Sunkiller-qc's two leads:<br>  – #1322: title writes under a range that the submission's shaders write are never copied. The world's UAVs cover the whole 174 MB heap. On an RTX 4050 Laptop, a downsample read its sizes as 0, looped about 4 billion times and hung.<br>  – structured views read raw: NVIDIA addresses them in elements, AMD in bytes.<br>• #1317 (RTX 3080): Series S halves the GPU time (1,512 → 701 ms a frame). CPU coherence work dominates: 860 ms of preparation and 330 ms of copy-back. | SS, W64, DRAWS-I, DRAWS | **Evidence for mesh64.** Plain 32-lane mesh waves did not hang a Series S world on Ampere within minutes. A diagnostic switch that lets every wave64 mesh program through as plain 32-lane waves would show the 3D world's draws before the exact form lands. Run it once on the 4080, with item 97's slots.<br>**Check XTP** for #1322's case (its refresh under busy ranges) and for textures read only by heap index: either gives a black world on every vendor. Take #1322 with credit if XTP skips busy ranges. |
| 102 | xcl issues [#1452](https://github.com/xcl-dev/xcl/issues/1452) (dhanush2412; Ariorh1337's and 3L1A5-1's runs), [#1585](https://github.com/xcl-dev/xcl/issues/1585), [#1566](https://github.com/xcl-dev/xcl/issues/1566); VIctory [#1388](https://github.com/Marvisak/VIctory/pull/1388) (wbkepler, for #700) | xcl GPL-2.0-only (reports); #1388 GPL-3.0 · open | • xcl #1452, Series X: the device is lost 9–48 s after the title's first list call #101 (0x328), in runs on an RTX 3060, 2060 SUPER and 5090. Compute-queue `Wait64BitValueX` waits that no producer signals come before it. On an RTX 5090, with video memory and RAM far from full, the GPU sits at 100 % from that call to the loss: not oversubscription.<br>• #1585 (RTX 5060 8 GB): menus at 30 fps, then video memory runs out. #1566: the title's budget is reported but not enforced; 94 GB of commit.<br>• #1388: list slot 72's 64-bit memory waits. The part of a list after an unsatisfied wait is deferred to a scheduler, so submitting threads never block. | gpu · queue.rs, list.rs · STALL, VRAM | **Parked with Series X**, except one check: list #101 (0x328) is one of item 85's unchecked methods. Log whether the Series S world calls it, and list slot 72, before item 99's queue check. Take #1388 with credit only if XTP drops such waits. |

### 0.5 Dropped draws and indirect commands

| # | Item | Licence · state | What it does (measured) | Maps to | Action and why |
|---|---|---|---|---|---|
| 103 | VIctory [#1407](https://github.com/Marvisak/VIctory/pull/1407) (marquezinii, `82ad8ac`) and [#1420](https://github.com/Marvisak/VIctory/pull/1420) (stacked on it); [#1334](https://github.com/Marvisak/VIctory/pull/1334) (KainanP), [#1412](https://github.com/Marvisak/VIctory/pull/1412) (EatCodeSleep2); a count on [#1276](https://github.com/Marvisak/VIctory/pull/1276) (pablogonz12) | GPL-3.0 · open | • #1407 lowers "a lane mask holding lanes elsewhere read as data", the second reason AnyXboxX named for the refused passes that write mesh `ExecuteIndirect` arguments (item 87). When every lane with EXEC clear at the write is proven to have its bit set, the mask is the ballot of the running lanes plus every lane that does not run (`~WaveActiveBallot(true)`). It also takes joins of masks read by a VALU instruction, before the first loop. Compute only; 5 tests on an RTX 5070, wave32 only.<br>• #1420: a mask built in a loop that every lane runs alike. #1334: the SCC of a lane-mask operation read as data. #1412: scalar compares between lane masks stay uniform.<br>• #1225, #1231 and #1276 have no new commit. Offline over 768 programs on RADV, #1276 translates 2 more, the same two that #1225 and #1231 clear. | shader · lane masks, structure · DRAWS-I, W64 | **For the drops task:** once the producer passes are named (item 87), sort them by first reason.<br>• Take #1407 (and #1420) with credit for "lanes elsewhere", after a wave64 run on the 4080.<br>• Of #1225, #1231 and #1276, take one: they reach the same programs. |
| 104 | VIctory [#1402](https://github.com/Marvisak/VIctory/pull/1402) and the other bundles by MamaLeVrai ([#1319](https://github.com/Marvisak/VIctory/pull/1319), [#1418](https://github.com/Marvisak/VIctory/pull/1418), [#1426](https://github.com/Marvisak/VIctory/pull/1426), [#1439](https://github.com/Marvisak/VIctory/pull/1439)) | GPL-3.0 · open | Open PRs merged unchanged, with hand resolutions. #1402 carries the four shader PRs that gain most in #1247's offline benchmark on a retail dump, none losing a program: #1067 (+295 programs), #305 (+520), #306 (+153), #176 (+117). | shader · DRAWS-P | **Use #1402's list as a ranking, not the bundle:** take its PRs one by one, with their authors' credit. Read #1247's benchmark first. |

### 0.6 FidelityFX, UAV clears, CPU cost

| # | Item | Licence · state | What it does (measured) | Maps to | Action and why |
|---|---|---|---|---|---|
| 105 | VIctory [#1352](https://github.com/Marvisak/VIctory/pull/1352) (wbkepler, for #771); #1284, #1292 | GPL-3.0 · open | • #1284 (FidelityFX in HLE, item 80): no commit or comment since 17:07.<br>• #1292 (packed dot products, item 81): verified on an RX 9060 XT, 14 GPU tests.<br>• #1352: texture UAV clears, a seventh implementation for item 82. WARP tests only. | gpu · list.rs · SS | **Items 80–82 stand**: #948 first. |
| 106 | VIctory [#1405](https://github.com/Marvisak/VIctory/pull/1405) (SantannaCarlos), [#1382](https://github.com/Marvisak/VIctory/pull/1382) and [#1356](https://github.com/Marvisak/VIctory/pull/1356) (wbkepler), [#1424](https://github.com/Marvisak/VIctory/pull/1424) (BaggyALPHA) | GPL-3.0 · open | • #1405 (on #334 and #1019): when a view has its own copy of bytes the submission's shaders wrote, it reads them on the GPU right before the read. RTX 4070, 10 min: 1,938 such ranges (median 4.5 KB, about ten a frame).<br>• #1382: batched barriers, reused draw scratch, pooled list uploads. #1356: read-mostly tables under `RwLock`, per-list address and view caches.<br>• #1424 skips empty submissions, as #1268 does (item 90). | gpu · shadow.rs, host.rs · DRAWS (frame time) | **After the w1-copies task's frame report**, as item 90. #1405 only once #334 and #1019 are in. |

### 0.7 Ignore (new)

| Item | Why |
|---|---|
| VIctory: about 25 hardware tests of single instructions (LouisRiverstone, #1360–#1384) and ISA fixes from #1249's static audit (#1312, #1336, #1339–#1342, #1387, #1391, #1395, #1398, #1416, #1437) | Instruction coverage. One check from them: #1312 and #1336 both mask `v_readlane` / `v_writelane`'s lane index (& 31 or & 63) and let `v_writelane` ignore EXEC. Check XTP's per-lane form does the same. |
| VIctory #1310, issue #1410 | Linux. |
| VIctory: HLE, loader, input and CLI fixes (#1329, #1357, #1367, #1372, #1373, #1380, #1383, #1394, #1403, #1411, #1415, #1419, #1421, #1429–#1438) | Nothing on the world's frame. |
| VIctory #1330, #1345; issues #1316, #1318, #1321, #1343, #1375, #1413, #1440 | A fork's whole tree as one PR, docs, a community question, synthetic wave tests (#1318: an RTX 5070 refuses WaveSize 64, as known), byte-LDS PRs compared (XTP has #571), test reports. |
| xcl: about 50 PRs of tests, audits, HLE, scripts and DXC options (#1530–#1589), DXIL optimisation passes for xcl's own direct backend (#1544–#1547), 64-bit image atomics (#1533, #1541, #1551), BVH (#1542, #1554, #1559) | Nothing on the world's frame, or xcl's own backend. |
| xcl issues #1538, #1567 | Crash reports without a stack or a method. |
| New repositories: dankxylese/leotux (empty, no licence); from a port tracker's list, mewiof/Holy-Interoperability (MIT, nothing renders yet) and omonkeyz/xbox-emulator (GPL-3.0, progress notes and a runtime last pushed 2026-11-23, no BTP 6 world) | Nothing XTP lacks. |

### 0.8 Earlier items: what changed

- **Landed in XTP since pass 3:** VIctory #1124, item 58, with W1
  (`6a51f7b`, the merge `4ee60bd`); the copy-back now scans 8 bytes at a
  time (`2f0983b`).
- **Upstream:**
  - No tracked item was merged or closed. VIctory `main` is still
    `ffe310e`, xcl `main` still `9e3831e`.
  - VIctory issue #847 (item 33) has been closed since 13:32 UTC; passes
    2 and 3 missed it.
  - Item 84 is fixed upstream (item 100).
- **Wave64 (items 1, 45, 64, 65):**
  - alessiodeia's A/B in the title (#1132; RTX 2070 SUPER, 30 min each):
    #189 recovers 31 wave64 programs and loses 60 that translated before
    (57 of them as "v_readfirstlane in divergent control flow"). #439
    recovers 106 and loses none. XTP took #439's stack: confirmed.
  - On #439: 465d6a0eac's count over 26,148 programs on a 32-lane host,
    compute emitted 1,221 → 1,422 of 2,582. The 43 still stopped are in
    groups of more than 64 threads: item 94's case.
  - r4tedx's next branch, `shader/wave-per-group` (`05c7cbb`), runs a
    group of several console waves without LDS or `s_barrier` as one host
    group per console wave: the form XTP's design note plans. It also
    lowers whole-wave sections in groups of 32 threads or fewer.
  - OCE's two-lane form (item 45) now writes a third of the HLSL
    (`47db0c7`, 13:47 UTC): one 141 KB program went from 14.7 MB to
    5.1 MB. OCE loops that hold a barrier stop after 4,096 steps
    (`2c27467`). Neither commit mentions mesh programs.
- **The testers' tree** is still at `94e0ea5` (update 9).
- **Other projects since 17:18 UTC:**
  - OCE OpenScarlett: 3 commits (group-memory signed atomics, a root
    signature check, a worker for translations of 1 MB and more).
  - five-star-recomp: 13 (item 100).
  - Neonbridge: 26, among them five-star's two findings and lowerings
    from Xbox2Windows.
  - Xbox2Windows: about 40 merges (instruction lowerings, tests);
    leonida: 8 (GPU test results).
  - VItoriaBr: 5 (item 98, LDS bounds checks, chunked submissions).
  - AnyXboxX: only the 17:19 merge `38cc0fa`, the mark.
  - palmwave, G6Recomp and californiaAlmonds' OpenScarlett: none.

## P3. Pass 3: since 16:25 UTC, Series S first

### P3.1 Summary

- **Volume.**
  - VIctory: 109 new PRs (#1185–#1309) and 16 issues, none merged. `main`
    is still `ffe310e`. #1185–#1202 were opened at 16:22–16:24, after pass
    2's highest number (#1184): they are read here.
  - xcl: 59 new PRs (#1459–#1529). `main` moved 5 commits (`b7aa9f3` →
    `9e3831e`), #680 merged (item 92).
  - AnyPS5: nothing new.
  - Triage by title, then a keyword search of both trackers (any date) for
    Series S, FSR, FidelityFX, upscaler, lockhart, jitter and the three
    D3D12.X methods. About 40 items were read.
- **XTP's own measurement behind the FSR4 items.** The Series S session C
  log is `E:\xtp-data\cache\play\20261124-163332\run.log`. At 16:34:11
  UTC in it:
  - FSR4 calls pipeline slot 0x58 (from its +0x5870), then list slot 0x1a8
    (from its +0x72f4);
  - within 11 ms of the first call, 20 distinct compute programs are
    refused: "malformed shader: no DXIL part".

  XTP logs device slot 0x218 once, at 16:33:33. The title's own log names
  37 failures of it by FSR4.
- **The FSR4 upscaler and its D3D12.X methods.**
  - Device 0x218 (`DeserializeComputePipelineStateX`): xcl leaves it
    `E_NOTIMPL` too, and FSR4 falls back as it does in XTP.
  - The fallback creates FSR4's 29 passes with `CreateComputePipelineState`
    on blobs that hold no DXIL. Their RDNA2 code sits in an `FSPX`
    container, which XTP's `native.rs` does not read: those are the 20
    refusals above (item 81).
  - Pipeline 0x58: xcl stubs it, and FSR4 goes on.
  - List 0x1a8 is `ClearUnorderedAccessViewUint` (slot 53; 0x1b0 is the
    Float form). xcl translates its console heap handles, and without that
    the AMD driver faulted at world start in Series S. VIctory has six
    implementations, none on `main` (item 82).
  - The other way, taken by VIctory #1284 and once planned by xcl, answers
    the title's FidelityFX calls in HLE (item 80). `ffxQuery` gives the
    jitter phase count: the divisor that stays 0 at title+0x28f03e8 when
    XTP refuses the DLL.
- **The Series S world crash.**
  - No project reports a crash a few seconds into a Series S world.
  - Four VIctory-based trees reached a Series S world. Three report
    minutes there without a crash: #904 (8 min), #952 and #965 (about 3
    min each). AnyXboxX measured its frame rate there. Their FSR4 gaps
    are XTP's: no 0x218, no UAV clears (item 83).
  - So the gaps alone do not crash the title on VIctory. The crash likely
    comes from something XTP added, or from timing.
  - The closest finding: five-star-recomp traced a translated wave64
    program that wrote past its buffer into the game's own memory (item
    84).
- **What #952 and #904 implemented for Series S:** nothing specific.
  - #904 ran `c7a5f49` with #211 (planes) and #578 (wave64 programs
    refused). It added format stand-ins for render targets.
  - #952 only logs video memory.
  - Their 8-minute world was black, at 1–3 fps and 95–432 draws a frame.
    XTP already has #211 and #578.
- **Today's two drop counts.**
  - Draws without a pipeline (1,196 a frame): FSR4's passes are one
    group, about 20 dispatches. Split the rest by reason first (item 89).
  - Indirect commands (3,515 a frame): the passes that write their
    arguments are refused upstream too. Translator PRs and the testers'
    update 9 make them translate (item 87).

### P3.2 How to read the mapping

Each item's "Maps to" cell names the XTP area and backlog item, then the
findings it bears on:

| Tag | Finding |
|---|---|
| SS | Series S mode (`--console series-s`), the person's direction (roadmap #15). Session C (task 7112c207): the world in about 9.6 s, VidMm at most 8.19 GiB, a real frame at 2.6 fps with 79 draws. Then a crash about 7 s into the world: an access violation at title+0x29268aa, through a pointer into an overwritten object. Per the series-s task (36f1e12d), that object lies in a 64 KiB view of one physical page that xmem placed. Without FSR4, a division by zero at title+0x28f03e8. |
| DRAWS-P | Series S world: 1,196 draws a frame dropped for want of a pipeline. |
| DRAWS-I | Series S world: 3,515 indirect commands a frame dropped. |
| DRAWS | The world frame's draw count and frame time in general. |
| W64 | NVIDIA and wave64 (32-lane host). XTP's dev GPU is an RTX 4080 SUPER. |
| STALL, VRAM | The Series X world-load stall and texture memory (pass 2, P2.2). **Parked**: Series S stays under the PC's VidMm threshold, and X-only VRAM work waits. |

The symptom codes (C, D, W, F, H, K) are pass 1's (section 2).

### P3.3 Series S: FSR4 and the world crash

| # | Item | Licence · state | What it does (measured) | Maps to | Action and why |
|---|---|---|---|---|---|
| 80 | VIctory [#1284](https://github.com/Marvisak/VIctory/pull/1284) (AwkGrep256, `AwkGrep256/VIctory` `pc-fsr` `f96ad82`, +713, 3 commits), issue [#730](https://github.com/Marvisak/VIctory/issues/730) | GPL-3.0 · open, on `ffe310e`. AMD's signed DLLs it loads: AMD's binary licence | **The title's FidelityFX calls answered in HLE.**<br>• An HLE module stands in for `amd_fidelityfx_loader_xbox.dll` and `amd_fidelityfx_upscaler_xbox.dll`. It exports the five calls: `ffxCreateContext`, `ffxDestroyContext`, `ffxConfigure`, `ffxQuery`, `ffxDispatch`.<br>• The console API keeps the PC `ffx_api` layouts. The module swaps the console backend description (type 0x01000002, device at +0x10) for the PC DX12 one, the title's list for the host list, and each texture for its host copy. AMD's PC loader then does the work.<br>• `ffxQuery` answers the jitter phase count (type 0x10004): 32 for 1920 → 3840 in #730's log.<br>• Series S: a 1280×720 → 1920×1080 context on native Windows (RTX 5060, FSR 3.1.6, 30 fps at 45 s). On Linux (RX 9070 XT), an upscale ran in the world, and the run went on 2 min.<br>• Native Windows, Series X forced to FSR (RTX 4070 SUPER, bittertea0323): every dispatch stops. The output (slot 6, a title-memory RT/UAV texture in format 67) has no host texture in VIctory's lookup. The title's fallback then copies 569 MB a frame instead of 142. | hle (new `ffx` module); gpu · list.rs, resource.rs · SS, DRAWS-P | **Reimplement the module first, without AMD's DLLs (route A).**<br>• `ffxCreateContext` succeeds and keeps the sizes.<br>• `ffxQuery` answers the jitter phase count the way AMD's open FSR 2/3 code computes it, 8 × (display ÷ render)²: 18 for 720p → 1080p. It also answers every other query type the title makes; log them once first, as #1284 does.<br>• `ffxDispatch` records XTP's own bilinear upscale of the colour input into the output. In Series S that output is a 1920×1080 R9G9B9E5 RT/UAV texture in title memory (xcl #914). Check that XTP's lookup finds its host texture, where VIctory's did not.<br>• No FSR4 code runs then: its refused passes, its missing methods and its GPU writes all leave the frame. A Series S session with the module is the A/B for the crash about 7 s in. If the crash goes, FSR4's work was involved.<br>• #1284's code may be taken with credit. Loading AMD's DLLs is the person's decision: their licence allows binary redistribution only and forbids reverse engineering, so XTP never ships them.<br>• The picker and scale patches (title+0x7e93f0, +0x28ef680) are Series X diagnostics for 1.0.2.1. Series S does not need them. |
| 81 | xcl `main` [`fdeacf0`](https://github.com/ivall/xcl/commit/fdeacf0) (2026-11-23), rebuilt from PR [#33](https://github.com/ivall/xcl/pull/33) (ElHombreAlex, closed) and described in xcl's `HANDOFF.md`; VIctory [#1292](https://github.com/Marvisak/VIctory/pull/1292) (ESAT586) | xcl GPL-2.0-only · `main`; #1292 GPL-3.0 · open | **The console FSR4 run through (route B).** In xcl's description, in this document's words:<br>• FSR4 tries device 0x218 first. On `E_NOTIMPL` it creates its 29 passes with `CreateComputePipelineState` on a blob with no DXIL and no FSSM. The blob's `ACCC` part holds an `FSPX` container.<br>• `FSPX` layout: a header (magic, version 1, entry count, size), then 16-byte entries (type, offset, size, 0) from +0x10, with offsets from the container's start.<br>• Entry 0 (0x1C bytes): thread-group size as three u16 at +0x0C, `COMPUTE_PGM_RSRC1` at +0x14, `RSRC2` at +0x18.<br>• Entry 1: a hash. Entry 6: the RDNA2 code after a 0x24-byte header, up to the last `s_endpgm`. Entry 7: resource declarations, 32-byte records (types 0x41–0x43) whose dword offset is slot × 8.<br>• The model is fp8, set up for 720p → 1080p only. Its network passes hold thousands of `v_dot4`. With SM 6.4's packed dot-add, the largest pass went from 1.55 to 1.23 MB of DXIL, and the AMD driver stopped refusing it.<br>• A synthetic 1280×720 → 1920×1080 frame took about 11 ms on an RX 9070: 20 dispatches, 24 barriers, 2 UAV clears, 4 copies (list #85, `CopyTextureRegion`).<br>• Not run on NVIDIA. PR #33's earlier form broke at dispatch 14 on an RTX 4060 Ti, its author suspecting wave64 halves out of step.<br>• #1292 lowers the six packed dot products with 32-bit integer operations, saturation included. | shader · native.rs (new container), lower (dot products); gpu · pipeline · SS, DRAWS-P, W64 | **Reimplement only if route A is not enough**, for picture quality later. It needs `FSPX` in `native.rs`, `v_dot4` as DXIL's `Dot4AddI8Packed` / `Dot4AddU8Packed`, wave64 compute, item 82, and list #85's copy. On the 4080 it runs an ML network through wave64 emulation every frame. Compare #1292's emulation with the DXIL operations by size before taking it. |
| 82 | VIctory [#948](https://github.com/Marvisak/VIctory/pull/948) (Sakura-Byte, `35802af`) and [#1162](https://github.com/Marvisak/VIctory/pull/1162) (geiyiliyabumo, `12e9c6a`); issue [#771](https://github.com/Marvisak/VIctory/issues/771) with fork branches (fuzeofficlal `d359499`, MartinIndra02 `d31c482`, Gruzzberry `gpu/buffer-uav-clears`, MichaelSmithAI `9978134` + `1cac714`); xcl [#1291](https://github.com/ivall/xcl/pull/1291) (emirduran707) | VIctory GPL-3.0 · #948 open, +1,362/−48 in 6 files; #1162 open, +1,242/−21; branches. xcl GPL-2.0-only · open | **`ClearUnorderedAccessViewUint` / `Float`, list slots 53 / 54 (0x1a8 / 0x1b0).**<br>• XTP and VIctory `ffe310e` lack both, so FSR4's clear at +0x72f4 does not happen.<br>• xcl #1291: the two handles are console heap slots. The PC call needs the view in the bound shader-visible heap, and its copy in a CPU-only heap.<br>• madaraXor's run (RX 6700 XT, Series S, the retail title): without the translation, the process ended at world start in AMD's driver (read of 0xffffffffffffffff, FSR4's +0x72f4 on the stack), twice. With it, the upscaler's passes ran and no fault came there.<br>• FSR4 picks the Float clear for a target in format 0x12 (xcl HANDOFF).<br>• #948: texture and buffer UAVs, plus `DiscardResource`. After review: raw buffers clear as typed R32_UINT (raw views need a 16-byte first element), Float is refused on raw and structured views, and the texture's host-written mark waits for submission. Synthetic tests on WARP, RTX 5090 D, RX 6900 XT (RDNA2) and RX 7700 XT. Not run in the title.<br>• #1162 adds placed buffers and both list layouts: 9 GPU tests on an RX 9060 XT and WARP. | gpu · list.rs, descriptor/host.rs · SS | **Take #948 with credit.** Route B needs it, and so does any title clear. Test it through console heap slots, the case xcl #1291 describes. Take #1162's placed-buffer case if FSR4's targets are placed. #948 has the most reviews of the six, and an RDNA2 run. |
| 83 | VIctory issue [#904](https://github.com/Marvisak/VIctory/issues/904) (YaoweiXiao), [#952](https://github.com/Marvisak/VIctory/pull/952) (Ege-Mert), issue [#965](https://github.com/Marvisak/VIctory/issues/965) (zaidlutfi); AnyXboxX [`788f0fb`](https://github.com/LooperSalty/AnyXboxX/commit/788f0fb8c) | GPL-3.0 · reports; #952 open | **Series S worlds that held, and what they ran.**<br>• #904: VIctory `c7a5f49` (13 commits before `ffe310e`) with only #211 and #578. RTX 5060 Laptop 8 GB: menus and loading art right. The world stayed black but stable for 8 min, at 1–3 fps and 95–432 draws, with 7.2 of 8.0 GB of VRAM.<br>• #904 also mapped formats 85/86/115 to R8G8B8A8 and 67 to RGBA16F for every texture. Its follow-up found that only write uses are refused: format 67 with flags 4 or 5 fails, with 0 it is made. So the stand-in belongs on RT/UAV textures only.<br>• #952 adds only a video-memory log. Its Series S run (RTX 3060 Laptop 6 GB, `ffe310e` + #493, #206, #189, #454, #211, #492) held 420 s, in the world from 230 s, at 6.4–7.1 GB.<br>• #965 run 1 (testers' tree `92bc27b` with mpDom's 1440p fallback, wave64 skipped): about 3 min in the world at 0.7 fps, 181 draws, black.<br>• AnyXboxX (RTX 4070 SUPER): Series S world at 3.1–3.3 fps, against 2.0–2.4 in Series X. It renders at 1280×720 and presents at 1920×1080.<br>• None reports a crash a few seconds in, and none implements FSR4's methods. | SS; gpu · G2 (#952) | **Evidence: FSR4's gaps alone do not crash the title on VIctory.**<br>• For the series-s task: run XTP at its import (`ffe310e` renamed) plus #211 and #578, in Series S on this PC. If that holds past 7 s, the crash comes from something XTP added since.<br>• Or it comes from timing: XTP reaches the world in about 10 s, these runs after minutes.<br>• #952's periodic line: take with credit if XTP's budget log lacks it (item 52). |
| 84 | five-star-recomp [`a245f29`](https://github.com/ThatRecompDev/five-star-recomp/commit/a245f29af), merge [`7793597`](https://github.com/ThatRecompDev/five-star-recomp/commit/77935972b), roadmap [`68b6d06`](https://github.com/ThatRecompDev/five-star-recomp/commit/68b6d0654) | GPL-3.0-or-later (C#, HLSL) · `main` | **A translated program that wrote into the game's own memory.**<br>• On 64-lane waves, `s_mov_b32 exec_lo, 0xffff0000` had been taken as "every lane on". Any non-zero constant written to EXEC counted as all lanes. The integer copies of EXEC, which a later `exec_hi` write combines with, were stale.<br>• Now only 0 and all-ones are mask constants, and a constant written to one half keeps the other half. Found in `InstanceCulling~CS_Prop_BatchCull`'s prefix scan; a WARP test with assembled programs covers it.<br>• That pass wrote 20-byte `DRAW_INDEXED` argument records across the end of a transient-pool block, over the game allocator's node. Game memory was corrupted 130–150 s into the story load.<br>• The roadmap says the corruption is not fixed yet. The writer is named; its array base and index come from constant-buffer values still being traced.<br>• Earlier, the project applied the console's buffer range checks in its translator: stores out of range are dropped, loads out of range read 0. It saw no corruption, GPU hang or driver crash after that.<br>• `--trace-pool-stores` logs each translated program's stores into the game's memory. | shader · L1/L2 (wave64 EXEC), R1/R2 (buffer bounds); gpu · shadow.rs (copy-back) · SS, W64, DRAWS-I | **Check XTP now, for the series-s crash** (an entity array overwritten with texture-like bytes about 7 s into the world):<br>• a wave64 test where `s_mov_b32 exec_lo, 0xffff0000` leaves `exec_hi` as it was;<br>• raw and structured stores past the V#'s range are dropped, and loads there read 0;<br>• in the default shadow mode, GPU writes reach title memory only through copy-back. Log which submission's copy-back writes the overwritten page.<br>XTP's InstanceCulling programs probably do not translate yet (they need #704, item 87), so the check is for any wave64 program. Reimplement (C#); credit the finding. |
| 85 | xcl issue [#1337](https://github.com/ivall/xcl/issues/1337) (yangzj0844), "Failure C"; xcl issue [#914](https://github.com/ivall/xcl/issues/914) (oeirughrepuiohg) | xcl GPL-2.0-only (reports, no code) · open | • #1337, RTX 5060 Laptop 8 GB, xcl `930e8b4`, `lockhart`: 204 frames, then the title's assert at +0xcc2834. Just before, the title called methods xcl stubs: list #75, #101, #126, #131, #132, #142; queue #31 (newer layout); allocator #9.<br>• In Series X on the same card: texture-page failures at 70–80 s, then an assert or a device removal.<br>• #914: about 30 s in, Series S asks for a 1920×1080 R9G9B9E5 render target with UAV access (flags 0x300005). The host refuses it, and the title asserts. That is the shape of FSR4's output (item 80). | gpu · list.rs, queue.rs · SS | **Check XTP's tables against #1337's list.** XTP has #126 (0x3f0, shading rate), #131/#132 (0x418/0x420, syncs) and #142 (0x470, ignored). Left to check: list #75 (0x258) and #101 (0x328), queue #31, allocator #9. #914 is covered: XTP has #492 (item 8). |
| 86 | xcl [#1517](https://github.com/ivall/xcl/pull/1517) (oishigeso, draft); VIctory [#1264](https://github.com/Marvisak/VIctory/pull/1264) (pablogonz12), [#1224](https://github.com/Marvisak/VIctory/pull/1224) (matheusfrta) | xcl GPL-2.0-only · draft; VIctory GPL-3.0 · open | **Series S maps physical pages in small pieces.**<br>• xcl #1517: a title starting in Series S maps 4.3 GB of physical pages in small pieces. At one tile mapping and one signal per new tile (about 0.4 ms each), its loading thread was held about 25 s with nothing presented, and its stall watchdog ended it. New tiles are now mapped together before the next submit.<br>• #1264: `XMemMapPhysicalPagesScatter` mapped one page per call. Consecutive addresses now take one unmap, one view per section run, and one commit. Tested under Wine; not measured in the title.<br>• #1224: `map_alias` joins consecutive slices of one section into one view. It coalesces split placeholders first, so a remap over split pages no longer fails with `ERROR_INVALID_ADDRESS`. | hle · xmem.rs · SS | **Take #1264 with credit if XTP's Series S load scatters many pages**; its diagnostics line counts them.<br>**Read #1224 with the series-s lead in mind.** The overwritten object sits in a view that xmem placed, perhaps through `map_alias` for `CopyPageMappingsBatchX`. If those copies must not change what the CPU sees, #1224 is moot there. If they must, it is the faster form.<br>xcl #1517 is xcl's sparse design: nothing to take. |

### P3.4 Dropped draws and indirect commands

| # | Item | Licence · state | What it does (measured) | Maps to | Action and why |
|---|---|---|---|---|---|
| 87 | AnyXboxX [`788f0fb`](https://github.com/LooperSalty/AnyXboxX/commit/788f0fb8c) (measured); VIctory [#1225](https://github.com/Marvisak/VIctory/pull/1225) and [#1231](https://github.com/Marvisak/VIctory/pull/1231) (LooperSalty), [#1276](https://github.com/Marvisak/VIctory/pull/1276) (mercuryVM), [#1230](https://github.com/Marvisak/VIctory/pull/1230) (Ctrl-Alt-Haze, for #736); testers' tree update 9 (`94e0ea5`: #704 on #193, #489, #294, #346) with #965's runs | GPL-3.0 · open; branch | **The passes that write indirect arguments.**<br>• AnyXboxX (RTX 4070 SUPER): about 379 mesh `ExecuteIndirect` calls a frame start with zero arguments. The passes that write those arguments are refused: "an address from a phi", and "a lane mask holding lanes elsewhere read as data".<br>• #1225: a code-table address, read back with `v_readfirstlane` from a uniform VGPR sum, is traced; ColorDownsampleCS and InitCS translate.<br>• #1231 (stacked on #1225): the same, with loop counters (uniform phis) in the address. #1276: the same idiom, followed back through both first-lane reads.<br>• #1230: path-sensitive reachability across phi joins, for #612's regression (#736, item 33).<br>• Update 9: 205 more programs validate (24,933 of 25,889). The 30 InstanceCulling compute programs translate for the first time; "unknown stride" had stopped them.<br>• With update 9 on an RTX 4060 Laptop (#965): the opening scene's objects and people are recognisable, at about 2,500 draws and a frame every 7–10 s. The device was removed after 3 min, on a compute dispatch. | shader · R1, R2, S0; gpu · G13 · DRAWS-I, W64 | **For the drops task: first name the programs that write the dropped commands' arguments.**<br>• Then take #1225 and #1231 with credit (or #1276: they overlap).<br>• Take #704 on #193 as update 9 carries them. Pass 1 left them aside (4.3).<br>• Land the InstanceCulling programs with item 84's checks and G11's bounds: five-star found one of them writing past its buffer. |
| 88 | VIctory [#1279](https://github.com/Marvisak/VIctory/pull/1279) (Mumogg); VItoriaBr [`1ca533d`](https://github.com/uiliannn/VItoriaBr/commit/1ca533dfe); xcl #1428 (item 76) | GPL-3.0 · open; commit | • #1279 is for Wine only: its HLSL compiler refuses the argument clamp. Its count is a reference. With every indirect command dropped, one world frame lost on average 1,449 mesh dispatches, 776 dispatches, 285 draws and 79 indexed draws.<br>• VItoriaBr zeroes indirect `DispatchMesh` arguments that are out of range, and caps valid ones at 16,384 groups. | gpu · indirect.rs, G13 · DRAWS-I | **Ignore #1279**: Windows compiles the clamp. Read XTP's 3,515 by kind, as its breakdown does. **Check** what XTP's clamp does with out-of-range mesh arguments, against VItoriaBr's rule. |
| 89 | #965's update 9 run 1 (zaidlutfi); VIctory issues [#1286](https://github.com/Marvisak/VIctory/issues/1286) (aef200), [#1305](https://github.com/Marvisak/VIctory/issues/1305) (KIFIR4KK); [#1281](https://github.com/Marvisak/VIctory/pull/1281) (kss306); xcl [#1474](https://github.com/ivall/xcl/pull/1474), [#1472](https://github.com/ivall/xcl/pull/1472) | GPL-3.0 · reports, open; xcl GPL-2.0-only · open | **Draws without a pipeline.**<br>• #965: the first run of a new tree stayed black for 12 min. The world's draws were skipped while their pipelines waited: about 1,750 were still queued at the end. The second run showed the world.<br>• #1286 (RTX 4060 Ti 8 GB): #189 alone took refused compute pipelines from 92 to 0. The stop at +0x23cf683 stayed.<br>• #1305 (RTX 4060 8 GB, with #175, #189 and #352): stalls of about 2 s on one compute pipeline, then `DEVICE_HUNG`.<br>• #1281 (RTX 4080 SUPER): declares WaveSize 64 on wave64 programs without cross-lane operations.<br>• xcl #1474 and #1472 cache refusals and compiler failures by capability. | gpu · G12, pipeline · DRAWS-P, W64 | **For the drops task: split the 1,196 by reason before porting anything**: not translated (FSR4's 20 are one group, item 81), refused, or still building. Say whether the pipeline cache was warm. #189 and #1281 replace XTP's wave64 stack: ignore them. xcl #1474: XTP has #1094 (item 62). |

### P3.5 Other new items

| # | Item | Licence · state | What it does (measured) | Maps to | Action and why |
|---|---|---|---|---|---|
| 90 | VIctory [#1215](https://github.com/Marvisak/VIctory/pull/1215) (LooperSalty), [#1267](https://github.com/Marvisak/VIctory/pull/1267) and [#1277](https://github.com/Marvisak/VIctory/pull/1277) (TG0000), [#1268](https://github.com/Marvisak/VIctory/pull/1268) (EatCodeSleep2) | GPL-3.0 · open | **The CPU cost around submissions.**<br>• #1215: copy-back calls `NtWriteVirtualMemory` directly. `WriteProcessMemory` queries the region first, about 2 ms a call in the title's big pools. Copies into title memory run `VirtualQuery` only after a failure. RTX 4070 SUPER: copy-back from 1.5 s to 0.13 s.<br>• #1267: the snapshot before a submission keeps each declared byte once, not the whole span from the first range to the last.<br>• #1277: view memory is remembered until it is remapped, instead of about 900 `VirtualQuery` calls a world frame. Removing it alone added 98 ms to 551 ms of submissions a frame (RX 6800).<br>• #1268 skips empty submissions. | gpu · host.rs, shadow.rs, watch.rs (interim, before G15–G18) · C, DRAWS (frame time) | **For the w1-copies task: take #1215 with credit first**: one call, and the largest measured gain. Take #1277 and #1267 after XTP's frame report shows the same costs in Series S. |
| 91 | VIctory [#1228](https://github.com/Marvisak/VIctory/pull/1228) (Ctrl-Alt-Haze, for #732), [#1298](https://github.com/Marvisak/VIctory/pull/1298) (EatCodeSleep2) | GPL-3.0 · open | • #1228: the console list's pipeline syncs (slots 129/130; 131/132 and the barrier groups in the newer layout) now emit a UAV barrier. They were empty, which left shader writes and later reads in one host list unordered.<br>• #1298: a null CBV write replaces the stale descriptor in its slot. | gpu · list.rs, descriptor · K, H, DRAWS | **Check XTP.** Its `sync` (`list.rs:930`) is the same empty function, on the grounds that host lists order their own work. D3D12 does not order UAV writes between dispatches without a barrier. Take #1228 with credit if no other barrier covers it: it is small. Take #1298 after a test shows the stale slot. |
| 92 | xcl `main` `b7aa9f3` → `9e3831e`: [#680](https://github.com/ivall/xcl/pull/680) merged, `cad94a2`, `927adcc`, `deb4b92`; xcl [#1514](https://github.com/ivall/xcl/pull/1514), [#1513](https://github.com/ivall/xcl/pull/1513), [#1515](https://github.com/ivall/xcl/pull/1515) (oishigeso, drafts), [#1478](https://github.com/ivall/xcl/pull/1478) (jessenic) | GPL-2.0-only · `main`; open | • #680: memory windows from the descriptor heap. Pass 1 left it aside (4.3): it serves xcl's flat-memory shader ABI.<br>• `cad94a2`: `SetDescriptorHeaps(0, NULL)` clears the list's record of bound heaps.<br>• #1514: placed textures over reserved-only graphics memory get video memory only where the title has pages. 22,808 textures had held 6 GB, 4.3 GB of it over addresses with no title memory.<br>• #1513 detiles on the GPU. #1515 warns within a headroom of the budget.<br>• #1478: a resource's last `Release` no longer waits for the sparse lock. The frame loop had blocked there, and the title's stall check fired at +0x23cf683. | gpu · descriptor, tex-pool, G17 · VRAM, STALL | **Check XTP for `cad94a2`'s case**: a list's bound heaps after `SetDescriptorHeaps(0, NULL)`. The rest is Series X VRAM work, parked. #1514's numbers go to tex-pool's notes. #1478 is one more road to the watchdog stop (item 74). |
| 93 | VIctory [#1296](https://github.com/Marvisak/VIctory/pull/1296) (antikohza), [#1272](https://github.com/Marvisak/VIctory/pull/1272) (ESAT586), issue [#1302](https://github.com/Marvisak/VIctory/issues/1302) (Xvania7, closed); issue [#1226](https://github.com/Marvisak/VIctory/issues/1226) (Logan-Gaillard) | GPL-3.0 · open | • Mip drop (#737, item 53) as PRs. #1296 turns it on at level 1 by default on cards with at most 8 GB. #1272 turns it on after `E_OUTOFMEMORY`.<br>• #1226: on an 8 GB laptop, a host texture that cannot be made removes the device. VRAM in use was 501 MiB; the process had 15.6 GB private on 15.4 GB of RAM. The limit was commit, not the card. | gpu · resource.rs · VRAM | **Parked**: Series S stays under this PC's threshold. Keep #1226's rule for later: a host texture that cannot be made is dropped, and the device stays. |

### P3.6 Ignore (new)

| Item | Why |
|---|---|
| VIctory: about 30 PRs by geiyiliyabumo (#1186–#1209, #1301–#1309) | Synthetic tests and small HLE or loader fixes, opened minutes apart. None touches the world's frame. |
| VIctory [#1287](https://github.com/Marvisak/VIctory/pull/1287), issue [#944](https://github.com/Marvisak/VIctory/issues/944) (AwkGrep256) | AMD frame generation. Not a goal; FSR comes first (item 80). |
| VIctory #1218, #1241, #1243, #1266 | Linux and Wine. |
| VIctory #1235, #1252, #1278, #1299; issues #1219, #1242, #1249, #1297, #1234 | Docs, static audits, proposals, and a request for game files. |
| VIctory #1198; xcl #1519, #1521 | Ray tracing (BVH): too early. |
| VIctory #1262 | Double-precision arithmetic: no measured need. |
| xcl: about 40 PRs of tests, scripts, audio and HLE (#1459–#1529) | Nothing on the world's frame. |
| xcl [#1526](https://github.com/ivall/xcl/pull/1526), [#1529](https://github.com/ivall/xcl/pull/1529) | DXC options. XTP emits DXIL without DXC. |
| xcl [#1110](https://github.com/ivall/xcl/pull/1110) | A Series S cache file for xcl's coherent window: xcl's own design. |
| New repositories: dare-devil1/XCL, dare-devil1/VICTORY, Defintelythijsss/Stellar-Project, and a port tracker's e-ink plugin (named in the private notes, README) | No licence, copies, or a tracker plugin. Not opened past their search entry. |

### P3.7 Earlier items: what changed

- **Landed in XTP since pass 2:**
  - #985, item 61 (`265c209`, `5e41b92`, in the merge `702c621`);
  - #1094 and #1129, items 62 and 63 (`039c490`);
  - #539, #453, #592/#621, #571, #160 and #563, items 7 and 13 (`f93bf8e`);
  - #180 and the lane-mask branch, item 7 (`fdded4c`);
  - XTP's own copy refresh, the base of item 58 (`37a8922`);
  - tex-pool, the XTP side of items 49 and 55 (`be6a440`), off by default
    (`XTP_TEXTURE_POOL=1`).
- **Item 52 was run.** The Series S A/B (frame-prof session C) gave the
  numbers in the SS tag. The person then chose Series S first.
- **Upstream:**
  - No tracked item was merged or closed. VIctory `main` is still
    `ffe310e`.
  - xcl `main` took #680 (item 92).
  - VIctory issue #1302, a fork's fix for the mip-drop fallback on
    `E_OUTOFMEMORY`, was closed.
- **The testers' tree** is still at `94e0ea5` (16:16 UTC), update 9. Its
  `INTEGRATION.md` was read for item 87. It carries #211, and no UAV clear
  or FidelityFX change.
- **Other projects since 16:25 UTC:**
  - five-star-recomp: 18 commits (item 84).
  - AnyXboxX: 5 commits (items 83 and 87).
  - VItoriaBr: 4 commits (item 88).
  - OCE OpenScarlett: 1 commit.
  - Neonbridge: 40 commits, by title: heap imports kept 2 s, a transient-pool watch.
  - Xbox2Windows: 65 commits; leonida: 22. By title, nothing on Series S or FSR.
  - palmwave, G6Recomp and californiaAlmonds' OpenScarlett: none.

## P2. Pass 2: since 13:40 UTC

### P2.1 Summary

- **Volume.**
  - VIctory reopened pull requests to everyone at about 14:12 UTC. Since
    the mark: about 170 new PRs (#922–#1183) and 92 issues, none merged.
    `main` is still `ffe310e`.
  - xcl: about 210 new PRs (#1244–#1458), and 50 commits on `main`
    (`47efa2e` → `b7aa9f3`): 20 small PRs merged.
  - Everything was triaged by title; about 45 items were read.
- **Video memory, the question asked first.** No project manages
  residency the way XTP's stall needs. No project reaches the world at a
  usable rate either. The best report: 0.4–0.9 fps at 2,300–3,100 draws a
  frame on a 12 GB card (VIctory issue #1086, item 72).
  - xcl puts every GPU-owned texture (render target, depth buffer, UAV) on
    64 KiB pages of video memory, taken from 64 MiB heaps. There is one
    page per page of the title's memory, shared by the textures that alias
    it (item 49). That is one VidMm admission per 64 MiB, not one per
    texture. XTP's pool design (task tex-pool) keeps those targets
    committed.
  - xcl's real backend never calls `MakeResident`, `Evict` or
    `SetResidencyPriority`. It peaks at 14.1 GB of local video memory on a 16 GB card, and its world
    load still ends at the title's stop at `+0x23cf683` (item 50).
  - Series S mode asks for about 2.2 GB less video memory in the world
    (VIctory #952: 6.4–7.1 GB against 8.6–9.1 GB). That would bring XTP's
    8.1 GB of textures under the ~9.5 GB threshold. XTP has the mode and
    #211, so this is an A/B to run now (item 52).
  - Making sampled textures without their top level saved 1 GiB (11.3 →
    10.3 GiB) in #1086's world run (VIctory issue #737, item 53). OCE
    OpenScarlett does the same.
- **The stop at `+0x23cf683` is the title's GPU-progress watchdog.** xcl
  issue #505 decodes the message that site builds before it faults: a
  GPU-progress check (the message itself is the title's, not quoted). VIctory
  (#1118) and xcl (#534, on AMD cards of 12 and 24 GB) end there too, so
  the stop is not specific to XTP's wait on VidMm. Once the stall is
  fixed, XTP should expect the same stop for another reason (item 74).
- **The four PRs named in the task:**
  - #1122 (item 57) and #1124 (item 58): take with credit;
  - #1126 (item 59): take, with the heap-cap items (item 66);
  - #1127 (item 60): ignore. XTP's port of #539 already does more. It is on
    `hangar/port-victory-539-453-592-621-24cfee`, not landed.
- **Wave64 on NVIDIA.** Three findings bear on XTP's port of the stack:
  - VIctory issue #1132: a lone `v_readfirstlane` under a partial EXEC is
    not routed to the console flow in #439, so lanes 32–63 silently read a
    wrong value (item 64).
  - #1086's row 2: silently wrong cross-lane compute kept the world pass
    from ever being submitted. That is a candidate cause of XTP's 1–7 draws
    a frame.
  - VIctory issue #940: numbering wave64 mesh lanes by the host's lane put
    world geometry in the G-buffer on an RTX 5080 (item 65). It is an
    alternative to refusing wave64 mesh programs (#578).
- **Pass 1's first pick for XTP's check changed.** VIctory #689 was closed
  in favour of #985 (item 61).

### P2.2 How to read the mapping

Each item's "Maps to" cell names the backlog item (`docs/analysis/`), then
the findings of today it bears on:

| Tag | Finding |
|---|---|
| STALL | The world-load stall. Past about 9.5 GB resident on this PC, VidMm admits one resident allocation per 0.5–2 s, and XTP makes one per host texture ([stall.md](../analysis/stall.md)). The fix in progress is pooled heaps made ahead of need: task tex-pool (450ec9ba), with `docs/analysis/vram.md` on its branch. |
| VRAM | XTP holds 8.1 GB of host textures by the stall; the PC's VidMm threshold is about 9.5 GB. |
| W64 | NVIDIA and wave64. Wave64 mesh programs hang NVIDIA and are refused by default (#578, `XTP_WAVE64_MESH=allow`). The GPU half of the stack's wiring was reverted at 16:18 UTC (`4394656`), then reapplied (`5eb9213`). |
| DRAWS | The world frame runs at 0.2–0.4 fps with only 1–7 draws per frame. |
| SS | Series S mode (`--console series-s`). #211's planes are in XTP. |

The symptom codes (C, D, W, F, H, K) are pass 1's (section 2).

### P2.3 Video memory: how the others create host resources

xcl is GPL-2.0-only. The mechanisms below are described in this document's
own words from reading xcl `main` at `b7aa9f3` (paths under
`crates/xcl/src/`). They are reimplemented from this description only, and
nothing of the code is copied.

| # | Item | Licence · state | What it does | Maps to | Action and why |
|---|---|---|---|---|---|
| 49 | xcl `hle/d3d12x/vram.rs` ("Pages", "Arenas"), called from `create_placed_resource_x` in `hle/d3d12x/device_x.rs` | GPL-2.0-only · `main` | **Textures at title addresses in pooled video memory.**<br>• A render target, depth buffer or UAV texture the title places at its address becomes a reserved (tiled) resource. Its 64 KiB tiles map onto one 64 KiB page of video memory per 64 KiB page of the title's address. Targets at one address share those pages, so each alias sees the others' writes, and a page exists once however many targets cover it.<br>• Pages come from 64 MiB heaps (1,024 tiles, `DEFAULT`, no flags under resource heap tier 2). A heap is made only when no free or cooled tile is left.<br>• Tile mappings run on a queue of xcl's own, with a fence. Each title queue waits for that fence on the GPU before its next work, so the CPU never waits.<br>• A page no target uses is kept 2 s for the next target at its address, and a freed tile is not reused for 2 s (`IDLE`, `COOL`).<br>• What cannot be tiled goes into an "arena": D32S8, MSAA, or an address not 64 KiB aligned. An arena is a heap of the placement's own size, at its address, shared by the placements inside it and freed 2 s after its last user. Merged 256 MiB arenas had made 15 GB in 2 s.<br>• Sampled textures go on the same pages when tier 2 allows (`place_pages`); otherwise they are committed. All of this needs tiled resources tier 2, where unmapped tiles read zero. | gpu · G17, G19 · STALL, VRAM; dark world | **Reimplement, as tex-pool's second step.** tex-pool pools title-written sampled textures and keeps targets committed, so each target still costs one admission. Pages give one admission per 64 MiB for targets too. Shared pages also save what xcl #1182 counted: 1.85 GB of targets placed over other targets' pages (item 25). Keep tex-pool's choice of making heaps ahead, on a background thread. xcl makes them on demand on the creating thread (`take_tile`), which here would pay the admission on the title's thread. A freshly placed target needs a `DiscardResource` before its first use (xcl #1342, open). |
| 50 | xcl residency and budget | GPL-2.0-only · `main` | xcl's real D3D12 backend makes no residency call (`MakeResident`, `Evict`, `SetResidencyPriority`, budget events): those names appear only in its vtable layout and its null backend. `QueryVideoMemoryInfo` only feeds its activity log. A tester's run (#1248, RX 9070 XT 16 GB): 9.8 GB of target pages and 14.1 GB of local video memory at the peak. Presents fell to 0 at 110–120 s, then the title stopped at `+0x23cf683`. | gpu · G17 · STALL, VRAM | **Nothing to take; a warning for tex-pool.** Pooling alone did not keep xcl's world load going on a 16 GB card. XTP's plan (heaps made resident off the title's thread, a watch on the budget) goes further than xcl. |
| 51 | xcl `hle/d3d12x/sparse.rs`, `gpu/uma.rs` | GPL-2.0-only · `main` | **The title's GPU buffers in video memory.**<br>• A memory reservation where the title keeps GPU data gets an address in a "sparse window". That window is a 4 GiB reserved buffer whose 64 KiB tiles are mapped onto 64 MiB heaps of video memory as the title commits pages.<br>• Before each `ExecuteCommandLists`, what the CPU wrote (write-watch) goes up through a 64 MiB staging ring, on a queue of xcl's own. The title's queues wait for its fence on the GPU.<br>• Only the 64-byte chunks whose hash changed are sent, so bytes the GPU wrote on the same page are not overwritten.<br>• Coherent memory stays in heaps opened over title memory (`OpenExistingHeapFromAddress`, at most 4,095 MiB, fully committed). | gpu · G15b, G16, G17a · VRAM; C | **Reimplement in G16–G17a.** This is palmwave's rule (item 41), built: the title's heaps in VRAM, and only small CPU-read memory imported. VIctory issue #1147 measures why. On an RTX 5080, scattered GPU reads of imported system memory run at 0.3–0.4 GB/s, against 22–24 GB/s from VRAM, and a 2 GB pass removed the device on a 4070 Ti. These heaps count against the 9.5 GB. |
| 52 | Series S: xcl `config.rs`; VIctory [#952](https://github.com/Marvisak/VIctory/pull/952) (Ege-Mert), issue [#904](https://github.com/Marvisak/VIctory/issues/904) (YaoweiXiao) | xcl GPL-2.0-only; #952 GPL-3.0, open, clean, `faccc4e` | • xcl tells the title it has 8,192 MiB of memory in Series S mode (`lockhart`), and 13,824 in Series X mode.<br>• #952 logs the process's local video memory use and budget every 5 s. On an RTX 3060 Laptop (6 GB), in the world: 8.6–9.1 GB in Series X mode, 6.4–7.1 GB in Series S mode. Before loading: 4.5 against 2.5 GB.<br>• #904, RTX 5060 Laptop (8 GB), Series S with #211 and #578: menus and loading art render correctly. The world stays black but stable for 8 min, at 1–3 fps and 95–432 draws a frame, with 7.2 of 8.0 GB of VRAM in use. | gpu · G2; hle (title memory status) · SS, VRAM, STALL, DRAWS | **Run XTP in Series S mode on this PC now, as an A/B of the stall.** No port is needed: the mode and #211 are in XTP. About 2.2 GB less would put 8.1 GB of textures under the threshold. XTP reports 13.5 GiB of title memory in Series X mode and 7,776 MiB in Series S mode (`xtp-gpu/src/console.rs`); its comment says BTP 6 goes by the device type. Take #952's periodic line with credit if XTP's budget log (`8ab1787`) lacks it. |
| 53 | VIctory issue [#737](https://github.com/Marvisak/VIctory/issues/737): `antikohza/VIctory` `feat/texture-mip-drop-budget` (`2df78b6`); OCE OpenScarlett [`f8238f6`](https://github.com/OpenCommunityEdition/OpenScarlett/commit/f8238f6) | #737 GPL-3.0, branch; OCE AGPL-3.0 | `VICTORY_TEXTURE_MIP_DROP=1..3` makes the host copies of pure sampled textures (no RT, DS, UAV or MSAA) without their top levels. They are detiled from the console's level 1 or 2, and views and copies are shifted to match. In #1086's world run: 11.3 → 10.3 GiB. That run needed two fixes: textures under 2048 on either axis are spared, since the loading art turned flat colour without that (`4c84a09`), and copies into dropped levels are routed (`2df78b6`). OCE makes placed 2D textures from 512×512 one level smaller: 27 GB of commit at 65 s instead of 33.5 GB. | gpu · resource.rs, texture.rs · VRAM, STALL | **Take #737 with credit, opt-in, after tex-pool**, as the fallback when the pool still goes over 9.5 GB. It changes the picture, so it stays off by default. |
| 54 | xcl [#1248](https://github.com/ivall/xcl/pull/1248) (AmYisael) | GPL-2.0-only · open | A sampled texture of at most 4 KiB that lies inside one page of the title's memory goes into a 4 KiB slot of a 4 MiB heap, not into a 64 KiB tile of its own. BTP 6 makes about 12,000 16×4 RGBA16F textures of 1–2.5 KB while it loads the world: 768 MiB of tiles for 20 MiB of textures. A tester on an RX 9070 XT measured 0.7 GB less local video memory at the peak, and also about two-thirds of `main`'s frame rate after the first A press, not explained yet. | gpu · tex-pool · VRAM | **Already in tex-pool's design** (its 4 KiB class). Use the count of 12,000 to size that class, and time the class against the 64 KiB one. |
| 55 | Residency priority and eviction: Neonbridge [`2fa28a9`](https://github.com/DesignWithTommy/Neonbridge/commit/2fa28a9), [`d69cf7f`](https://github.com/DesignWithTommy/Neonbridge/commit/d69cf7f), [`9d48cd8`](https://github.com/DesignWithTommy/Neonbridge/commit/9d48cd8), [`6d94818`](https://github.com/DesignWithTommy/Neonbridge/commit/6d94818); OCE OpenScarlett [`3da6373`](https://github.com/OpenCommunityEdition/OpenScarlett/commit/3da6373), [`2c27467`](https://github.com/OpenCommunityEdition/OpenScarlett/commit/2c27467) | Neonbridge: ideas only (it contains xcl code); OCE: AGPL-3.0 | • Neonbridge gives heaps that hold only sampled textures a LOW residency priority, and heaps with targets HIGH.<br>• Neonbridge also evicted heaps unused for 10 s when over budget. Tracking their use cost 35–40 → 15 fps, so that is now off by default.<br>• Neonbridge's pools hand out the fullest pool's tiles first, so sparse pools drain and can be released.<br>• OCE makes a page pool's heap only when its first tile is mapped.<br>• OCE measured that, on NVIDIA, a texture committed in video memory also costs its size in the process's private bytes (688 MB of BC7: +691 MB). The title's textures are therefore held twice. OCE ends the process before the system's commit limit is reached. | gpu · tex-pool, G17 · VRAM, STALL | **Reimplement two rules in tex-pool:** residency priority by content (sampled LOW, targets HIGH), and fullest-pool-first allocation. Leave out per-submission use tracking, whose cost Neonbridge measured. Add the process's commit to the frame line. |
| 56 | VIctory [#982](https://github.com/Marvisak/VIctory/pull/982) (chemokita13) | GPL-3.0 · open, clean, `b4abe5d` | Upload buffers for footprint copies and scratch memory are pooled across list resets, instead of one committed resource per copy. A buffer is reused if it is at most twice the size asked for, and at most 32 are kept. | gpu · host.rs · STALL | **Take with credit.** XTP has the same shortcut (`host.rs:1948`, "one buffer per copy"). Upload heaps are in system memory, outside the local segment's threshold. But each committed creation still queues behind the device's residency lock, which stall.md §2.1 shows held during the stall. Count creations per frame before and after. |

### P2.4 New VIctory items

All are GPL-3.0-or-later, so take with credit unless the row says
otherwise. Numbers are VIctory PRs unless marked "issue". "Clean" means
the PR merges onto `ffe310e`.

| # | Item | State | What it does (measured) | Maps to | Action and why |
|---|---|---|---|---|---|
| 57 | [#1122](https://github.com/Marvisak/VIctory/pull/1122) (tightfall) | open, clean, `9ed6093`, +201/−9 | **Guards on every view of a physical page.**<br>• xmem keeps, for each physical page, the views that show it (`map_alias` and pool mappings).<br>• Guarding a block now guards its aliases too, and a write through one view opens the guards of all of them.<br>• Before, a write through the other view never faulted, and the host copy kept its first contents: stale mesh data, then page faults like #59 and #597.<br>• A test maps one pagefile section twice and checks both directions. A reviewer (adamdotpy) made it fail with the lookup turned off.<br>• RX 9070, on an older stack (`d2a4bae` + #135): world runs of 1:40–4:44 became 10:15, then 17:32 with `TdrDelay=10`. | gpu · watch.rs, G19; hle · xmem · H, K, DRAWS | **Take with credit.** XTP's `watch.rs` has no alias handling; `xmem.rs:952` has `map_alias`. XTP changed `watch.rs` since `ffe310e` (F5, `5553e29`, #464, #490), so the port needs adapting. Add the review's two points: prune the reverse pairs in `title_memory_remapped`, and make the comments say that a block is a page. |
| 58 | [#1124](https://github.com/Marvisak/VIctory/pull/1124) (divyeshganta), from issue [#918](https://github.com/Marvisak/VIctory/issues/918) (cybx, `4c0d57d`) | open, clean, `b3d8f95` | **Batched copy-back.** Copy-back compares and merges as before, but writes each run of joined 64 KiB pieces back with one kernel call. A run that is partly unmapped falls back to piece by piece (DrDarkDK's fix for runs that cross a piece boundary). cybx's run (Ryzen 7 9800X3D, RTX 5070 Ti, `main` plus open PRs): copy-back 555 → 133 ms a frame, frame ~1,033 → ~750 ms. | gpu · shadow.rs (interim, before G15–G18) · C, DRAWS (frame time) | **Take with credit, on top of XTP's own copy-back fix** (`747da15` on `hangar/w1-refresh-each-shadow-range-at-74bc23`, not on `main`), which writes only the GPU's bytes. Document the window the review points at: a title write between the read and the write-back of a long run. Measure it with G2. |
| 59 | [#1126](https://github.com/Marvisak/VIctory/pull/1126) (EatCodeSleep2), for issue [#911](https://github.com/Marvisak/VIctory/issues/911) | open, clean, `bb65bc3`, CI green | **Descriptor tables after a rejected bind.** When a draw or dispatch cannot get a fresh descriptor table, the ranges it reserved during that bind go back, and so do their cache entries. Tables from earlier accepted binds stay. The synthetic D3D12 test fails with the rollback turned off (reviewed). Not measured in the title. A comment notes that a rejected bind's uploads and transitions stay recorded. | gpu · draw.rs, descriptor.rs · D, DRAWS | **Take with credit, together with item 66.** XTP has the same table code and the same 65,536 dynamic slots (`descriptor/host.rs:78`). A rejected bind that leaks slots brings the next "no dynamic slots" drops sooner. |
| 60 | [#1127](https://github.com/Marvisak/VIctory/pull/1127) (ashrafsdrop) | open, clean, `52a5381`, no CI run | 64-bit image atomics as DXIL i64 `atomicBinOp` and `atomicCompareExchange`. | shader · I1 | **Ignore: XTP's port of #539 is ahead** (`d651284` on `hangar/port-victory-539-453-592-621-24cfee`, not landed). <br>• #1127 sets only the INT64 flag, not the SFI0 bits for 64-bit atomics on typed and on heap resources.<br>• It takes its handle from `handle`, which caches by the annotation's first word. XTP's port avoids that bug with a handle of its own, made per lane.<br>• Its test checks the flag bit only, with no validator and no GPU run. XTP's port was validated on the whole pack (22,508 → 23,034 programs) and run on WARP. |
| 61 | [#985](https://github.com/Marvisak/VIctory/pull/985) (Makto94), in place of #689 | #689 closed at 14:54 ("superseded by #985"); #985 open, clean, `211d365` | The frame-wait test keeps its own clock. Each flip must land on a new tick, not before the tick due and not after the ticks that passed, and at least one flip must land exactly on time. Test only. | gpu · frame.rs; tooling · B1 | **Take with credit, first,** in #689's place. XTP's `frame.rs:221` still asserts consecutive ticks. |
| 62 | [#1094](https://github.com/Marvisak/VIctory/pull/1094) (adamdotpy, carrying lines9686-rgb's fix from issue [#1089](https://github.com/Marvisak/VIctory/issues/1089)) | open, clean, `0061ed8` | A translation that fails is cached too: a `VSF1` entry with the reason, under the same key, so it is not tried again on every run. On an RTX 3060 Ti, about 400 programs failed in every run, the big compute ones taking 2–3 s each. For about 1.5 min after world entry the creation workers did only that, and their draws were dropped. | gpu · shader_cache.rs, G12 · DRAWS, STALL (the title waits during the load) | **Take with credit.** XTP's `shader_cache.rs:343` still says "Failures are not stored". The key must include XTP's wave-size target. |
| 63 | [#1129](https://github.com/Marvisak/VIctory/pull/1129) (divyeshganta, carrying Peppacodes' `d09c31b` from issue [#966](https://github.com/Marvisak/VIctory/issues/966)) | open, clean | Pipeline workers get a 64 MiB stack. On an RTX 5080, NVIDIA's compiler recursed past Rust's 2 MiB default and the pipeline came back as `E_OUTOFMEMORY`. With the larger stack it builds. | gpu · pipeline/make.rs · W64, DRAWS (on NVIDIA) | **Take with credit** (8 lines). XTP sets no stack size, and its dev GPU is NVIDIA. Look for `E_OUTOFMEMORY` pipeline refusals in XTP's logs first. |
| 64 | Issue [#1132](https://github.com/Marvisak/VIctory/issues/1132) (N7z, with MichaelSmithAI's comment) | open | Three wave64 implementations tested on one RTX 4060 Laptop, all green. It found two test-harness bugs, and one correctness gap.<br>• On a hybrid laptop, adapter 0 is the iGPU, so the "hardware" tests ran on the Intel GPU.<br>• Test adapters must be filtered by `WaveLaneCountMax ≥ 32`, not by name.<br>• **The gap, in #439:** a lone `v_readfirstlane_b32` under a partial EXEC is not routed to the console flow. Lanes 32–63 then read their own half's first lane (103 instead of 37, on an RTX 4090), and pipeline creation still succeeds. | shader · L1, L2 · W64, DRAWS | **Check XTP now**, with that program as a test: 64 threads, lanes 10–63 active, each holding 3·lane+7, and every active lane must read 37. #1086's row 2 shows what silently wrong cross-lane compute does: the title's own checks never pass, and the world pass is never submitted. That fits 1–7 draws a frame. Take the adapter filter for XTP's GPU tests. |
| 65 | Issue [#940](https://github.com/Marvisak/VIctory/issues/940) (Peppacodes): `Peppacodes/VIctory` `exp/mesh-host-lanes` (`ed9afc0`, on #189) | open | On a 32-lane host, a wave64 mesh program runs as two 32-lane waves. Its lane numbers now come from the host's lane (`WaveGetLaneIndex`), so lane numbers and the masks from host ballots agree within each half. RTX 5080: world geometry in the G-buffer at frame 1550. Without the change, the first world submission ran for 62.8 s. It does not cover the 115 programs with explicit lane reads, and the run still ended later on a 63 s submission. | shader · L1 (mesh); gpu · #578 · W64, DRAWS | **Reimplement in XTP's stack** (the commit is written for #189's lowering). It is an alternative to refusing wave64 mesh programs that have no cross-lane reads. Try it under `XTP_WAVE64_MESH=allow`, with G11's bounds. |
| 66 | [#1133](https://github.com/Marvisak/VIctory/pull/1133) (rivaldodev); issue [#902](https://github.com/Marvisak/VIctory/issues/902) (Xvania7, `fix/issue-887-descriptor-heap-limit`); OCE OpenScarlett [`0fbe1ce`](https://github.com/OpenCommunityEdition/OpenScarlett/commit/0fbe1ce) | #1133 open, clean, `be0fe04`; #902 a branch; OCE AGPL-3.0 | The title asks for 1,245,184 view slots, and NVIDIA caps a shader-visible heap at 1,000,000.<br>• #1133: indices past the cap read the heap's last slot, which stays null, and the cap is part of the cache key. Also, on narrow-wave hosts, a barrier inside a lane-dependent branch keeps its memory fence but drops the group sync: a host wave that never arrives would hang the GPU. RTX 4070 Ti SUPER: the hang came at 60–140 s before, at 145–534 s after.<br>• #902: 131,072 dynamic slots on a capped heap, a ladder of smaller heap sizes, and a fixed wrap-around search. Before it, "no dynamic slots" drops reached 65,536.<br>• OCE: the PC heap is a cache of the title's heap, in pages of 4,096 slots. | gpu · descriptor/host.rs · W64, H, D, DRAWS | **Take #1133's barrier rule with credit** (W64), after a review of when the sync may be dropped. **Choose one answer to the capped heap**: #1133's clamp, #483's guard slot (item 3) or #955. Raise the dynamic headroom as #902 does. |
| 67 | [#1135](https://github.com/Marvisak/VIctory/pull/1135) (from talakasse's `af4fef4`, issue [#915](https://github.com/Marvisak/VIctory/issues/915)); issue [#916](https://github.com/Marvisak/VIctory/issues/916) (alessiodeia, `8cb0222`) | #1135 open, clean; #916 a branch | • #1135: after a texture's first detile, only the changed 64 KiB pages are read or hashed again, and only the levels and slices they hold are detiled.<br>• #916: on an RTX 2070 SUPER, `Staging::refresh_now` spent 308–416 ms a frame hashing 150–156 MB of unwatched textures, none of which had changed. The fix caches "not watched", keeps a remap epoch and uses a faster digest.<br>• DrDarkDK found a race in #916: `xmem::unmap` moves the epoch before Windows removes the view. | gpu · texture.rs · C, DRAWS (frame time) | **Take #1135 with credit; take #916 once its race is fixed.** Check the texture-refresh time in XTP's frame report first. |
| 68 | [#1010](https://github.com/Marvisak/VIctory/pull/1010) (TG0000), [#991](https://github.com/Marvisak/VIctory/pull/991) (matheusfrta), [#1024](https://github.com/Marvisak/VIctory/pull/1024) (Dufftwt) | open, clean | • #1010: copies from title memory run in user mode, with a vectored handler for a page that faults. 256-byte fills: 796 → 81 ns. Not measured in the title.<br>• #991: overlapping write-back ranges merged, and guard openings coalesced.<br>• #1024: copy-back writes only the GPU's bytes, and marks only unchanged pages as current (fixes #721 and #722). | gpu · host.rs, shadow.rs, watch.rs · C | **#1024: compare with XTP's `747da15`**, which makes the same two fixes, and take what it lacks. **#991 after item 58. #1010 once G2 shows what `ReadProcessMemory` costs per fill.** All interim work: Phase 3 removes this code. |
| 69 | [#1013](https://github.com/Marvisak/VIctory/pull/1013) (matheusfrta); xcl [#1273](https://github.com/ivall/xcl/pull/1273), [#1278](https://github.com/ivall/xcl/pull/1278), xcl `main` `25c6f7e` | #1013 open, clean; xcl open, and `main` | • #1013: urgent and background pipeline queues, so live binds do not wait behind preloads; faster structurisation.<br>• xcl: a bind waits only when a worker has started its build or is free to start it.<br>• xcl: compute programs of 4 MB of DXIL or more build in a queue of their own. Three screen-probe programs are 13–29 MB of DXIL, and NVIDIA had not finished one after 14 min, at 4.6 GB.<br>• xcl: a big build the title waits for does not wait for its turn. | gpu · G12 · DRAWS, STALL | **Take #1013's queue split after #275 and #546 (item 5), and reimplement xcl's rules in G12.** Find those three programs in XTP's census (xcl #1278 gives their record numbers in the shader pack, 22,616–22,618). On the 4080 they must never block other builds. |
| 70 | [#1081](https://github.com/Marvisak/VIctory/pull/1081) (iurysf), [#1082](https://github.com/Marvisak/VIctory/pull/1082) (bvtterbroot), issue [#1054](https://github.com/Marvisak/VIctory/issues/1054) (bittertea0323) | open, clean | • #1081: behind a switch, each loop of a translated shader stores its iteration count in system memory. After a device removal, the report names the five busiest loops. On an RTX 4070: `loop 2 of slot 621 ran 512 iterations`, in the vertex program of the list DRED named.<br>• #1082 logs the TDR settings and warns when a GPU hang would freeze the PC: with `TdrLevel=0`, a slow frame becomes a hard freeze (#1054). | gpu · G2, G11; tooling · H | **Take #1081 for G11's diagnosis, and #1082 as it is.** This PC's SSD is fragile, so XTP should say at start when a GPU hang would freeze it. |
| 71 | Issue [#979](https://github.com/Marvisak/VIctory/issues/979) (bittertea0323, `b6a9bdc`, `c8353a8`); [#1130](https://github.com/Marvisak/VIctory/pull/1130) (felicienradio), [#1067](https://github.com/Marvisak/VIctory/pull/1067) (undeadhuman37) | open | "A scalar memory read as data" is the largest offline failure class: 771 programs. A descriptor read only as data now takes its view from the used-resource entry it falls in: a 2D image for 8 dwords, a structured or raw buffer for 4. 771 → 0, and +231 DXIL programs. #1130 and #1067 do the same thing. | shader · R2 · D, DRAWS | **Take one of the three with credit, after S0.** XTP's #539 survey names this as the next blocker, for 104 programs. |
| 72 | Issue [#1086](https://github.com/Marvisak/VIctory/issues/1086) (bittertea0323, branch `try/wave64` `6090ffb`) | open | **The world on a 12 GB card**: RTX 4070 SUPER, Windows 10. It took nine stacked changes: TDR settings, #189's emulate mode, the Agility SDK (#729), #940, mip drop (item 53), 1 MiB windows for title memory (RAM 12.6 → 3.7 GB), and item 71. Result: 2,296–3,111 draws a frame, no pipeline refused, 10.3 GiB of VRAM, 0.4–0.9 fps. On an RTX 4080, the same tree plus #493 and #206 ran 900 s with no device removal. Still in the way: about 512 pipelines rebuilt on every run (library misses, #352). | all · DRAWS, VRAM, W64 | **Evidence: the best world run reported this pass.** Compare XTP's world log with its counts. The 1 MiB windows are a lead for XTP's RAM use. |
| 73 | [#1009](https://github.com/Marvisak/VIctory/pull/1009) (NuraadGit), [#961](https://github.com/Marvisak/VIctory/pull/961) (EduBertozzi), [#970](https://github.com/Marvisak/VIctory/pull/970) (pushkarreddyy), [#1152](https://github.com/Marvisak/VIctory/pull/1152) (pablogonz12), [#1041](https://github.com/Marvisak/VIctory/pull/1041) (gabrielcruzg3), [#956](https://github.com/Marvisak/VIctory/pull/956) (MartinIndra02), [#1160](https://github.com/Marvisak/VIctory/pull/1160) (geiyiliyabumo) | open, clean | • #1009 reserves the space below 4 GiB for the title's 32-bit `XMemVirtualAlloc`. The title asks for 185 MB there, and asserts at `+0x19f4f58` when refused.<br>• #961 draws mesh pipelines through compute, including mesh programs whose output is above D3D12's 32 KiB.<br>• #970 gives typeless views a concrete format by aspect: the DSV-only fix that item 35 asked for.<br>• #1152: no blending on integer targets.<br>• #1041 zeroes PSO padding and deduplicates library stores.<br>• #956 and #1160: multi-argument `ExecuteIndirect` with root constants (G13, xcl #869's case). | loader; gpu · G13, G7, G12 · D, DRAWS | **Read by title and summary only.** #970 goes with item 35. For G13, pick one of #956 and #1160 after a review: it is the GPL-3.0 form of item 24. Take #1009 if XTP meets that assertion, and #1041 if XTP's pipeline library misses (#1086's 512 rebuilds). |
| 74 | Issue [#1118](https://github.com/Marvisak/VIctory/issues/1118) (CaptureLance35); xcl issues [#505](https://github.com/ivall/xcl/issues/505) and [#534](https://github.com/ivall/xcl/issues/534); xcl [#1288](https://github.com/ivall/xcl/pull/1288) (imnicho) | open | **The stop at `+0x23cf683`.**<br>• That site builds a message naming a GPU-progress check, then writes to address 0: it is the title's GPU-progress watchdog (xcl #505; the message is not quoted here).<br>• VIctory #1118, RTX 3070 Ti (8 GB): 8 of 8 runs stop there about 1 min into the world, with the title's own memory 95 % used (its tile heap at 3,521 of 6,236 MB). xcl #534 stops there on AMD cards of 12 and 24 GB.<br>• Just before it, in xcl: the first call of command-list method #101 (a stub in xcl), and xcl's deferred CPU writes putting back old counter values (3L1A5-1).<br>• xcl #1288 stops `Sleep` from counting the time the title is held in a bind. On an RTX 5090 the run went on, but presents stayed at 0 from 82 s. | gpu · G10, G2; hle · STALL | **Reimplement the diagnosis.** XTP's stall ends at this site too (stall.md). Once the VidMm fix is in, check what XTP does with command-list method #101, and in what order bottom-of-pipe writes land. Do not hide the watchdog the way #1288 does: XTP already has an opt-in hook (#454). |

### P2.5 New xcl items and other projects

Ideas only, unless the licence column says otherwise.

| # | Item | Licence · state | What it does | Maps to | Action and why |
|---|---|---|---|---|---|
| 75 | xcl `main` since `47efa2e`: 50 commits and 20 merged PRs, including [#1150](https://github.com/ivall/xcl/pull/1150) | GPL-2.0-only · merged | Small fixes:<br>• clear values of placed resources converted (#1150, one of pass 1's checks in 4.4);<br>• default SRVs of multisampled depth buffers as Texture2DMS (#1265);<br>• allocation info of several resources padded to their alignment (#1354);<br>• partially executed lists first in DRED reports (#1330);<br>• copies within one memory window through a scratch buffer. | gpu · resource.rs; 4.4 | **The checks in 4.4 stay checks.** Nothing new to reimplement. |
| 76 | xcl [#1428](https://github.com/ivall/xcl/pull/1428) (lukastaro) | GPL-2.0-only · open | Dispatches of more than 65,535 groups in X are folded into Y. | gpu · indirect.rs · D | **Check XTP.** D3D12 refuses more than 65,535 groups in one dimension, so such a dispatch is dropped or clamped. |
| 77 | xcl [#1457](https://github.com/ivall/xcl/pull/1457) (ySKELLETONX), [#1323](https://github.com/ivall/xcl/pull/1323) (IvyNotFound) | GPL-2.0-only · open; #1457 draft | • #1457: a heap over title memory is held until the queues are past the work that used its buffers, instead of being freed at the title's last `Release`.<br>• #1323: markers written before and after each dispatch (`WriteBufferImmediate`) name the dispatches a stalled queue has not finished. With DRED on, NVIDIA skipped its cached pipelines there: one 1.99 MB pipeline took 153 s to build. | gpu · G15b, G2 | **Check G15b (`XTP_UMA=1`) for #1457's case.** Keep the DRED effect in mind when timing pipeline builds. |
| 78 | five-star-recomp `main` `e761558` (61 commits) | GPL-3.0-or-later | A shared time budget for first-use work on game threads. Its status page now names the GPU's writes into game memory as its blocker. | gpu · G12, C | **Read next pass.** Only commit titles were read in this one. |
| 79 | New projects: [itsF4LCON/Prodigy](https://github.com/itsF4LCON/Prodigy), [Sicaduj/XboxCompatLayer](https://github.com/Sicaduj/XboxCompatLayer), [wqryx/Leonida](https://github.com/wqryx/Leonida) | GPL-2.0; GPL-2.0; GPL-3.0 | • Prodigy says it is "an independent continuation of XCL" with VIctory parts; BTP 6 reaches its menus.<br>• XboxCompatLayer carries xcl's README and code under another name.<br>• wqryx/Leonida is one commit with leonida's README. | – | **Ignore.** Copies or forks with nothing XTP lacks. Prodigy mixes xcl's GPL-2.0-only code with VIctory's. |

### P2.6 Ignore (new)

| Item | Why |
|---|---|
| VIctory [#1076](https://github.com/Marvisak/VIctory/pull/1076) (legayclightning22) | 1,722 lines in 15 files: a med3 fix, one submission per list, memo bounds and format flags, mixed. A review found a lost UAV capability. Its parts are covered elsewhere (item 70 for TDRs, and G11). |
| VIctory #1121, #994, #1011, #1007, #1066 | Linux, Proton, Apple Silicon and Intel Arc. XTP targets Windows on NVIDIA. |
| VIctory: about 30 PRs by geiyiliyabumo (#1155–#1174 and others) | Synthetic unit tests and HLE slots, opened minutes apart. None touches the world's frame. |
| VIctory #1002, #1033, #1180, #999, #947 | Faster translation (−4 to −16 %): offline time, not frame time. |
| xcl [#1426](https://github.com/ivall/xcl/pull/1426) (bmjask) | A direct RDNA2 → DXIL backend for xcl (18,866 lines). XTP already emits DXIL directly. |
| xcl [#1329](https://github.com/ivall/xcl/pull/1329), [#1326](https://github.com/ivall/xcl/pull/1326) (vakaobr) | A copy of the coherent window in video memory, for hosts without `OpenExistingHeapFromAddress` (macOS). Windows keeps the import. |

### P2.7 Pass 1's items: what changed

- **Landed in XTP since pass 1:**
  - the wave64 stack, item 1 (`2d978f7`, `0b85474`, `d01bc62`). Its GPU
    wiring was reverted at 16:18 UTC (`4394656`), then reapplied (`5eb9213`);
  - #578, item 2 (`575d721`). Wave64 mesh programs are refused by default
    again since `2c65877`;
  - #493, item 3 (`bdd0091`);
  - #595, item 4 (`40ec751`);
  - #464 and #490, item 6 (`f2a4c7c`, `6b8be7e`);
  - #492, item 8 (`19047bd`);
  - #643 and #211, items 10 and 15 (`2775f7f`, in the dark-world merge);
  - #454, item 17 (`55f39e1`, `XTP_WATCHDOG_HOOK`).
- **On an XTP branch, not landed:** #539, #592 and #621 (items 7 and 13),
  on `hangar/port-victory-539-453-592-621-24cfee`.
- **Upstream:**
  - No item was merged.
  - #689 was closed in favour of #985 (item 61).
  - #703 now merges cleanly.
  - #233, #320, #439, #578, #493, #483, #595, #490, #89, #206, #346, #592
    and others were updated after 13:40 UTC (comments or pushes). A porting
    task takes the head its task names, then reads what came after it.
  - xcl #1150 (4.4) was merged.
- **The testers' tree** moved to `94e0ea5` (16:16 UTC). Its
  `INTEGRATION.md` was not re-read in this pass.
- **Before porting a PR:** VIctory issue #1184 lists the 24 mergeable PRs
  whose CI fails (9 of them from #801).

## Pass 1 (12:10–13:40 UTC)

Sections 1 to 7 are pass 1's text. Read their states with sections P2.7, P3.7 and 0.8.

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
  - xcl #1182: BTP 6 maps one physical page under render targets at two
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

| Project | Licence | What XTP may do | Approach | Activity since XTP's import | Head at the pass | BTP 6, by its own reports |
|---|---|---|---|---|---|---|
| [Marvisak/VIctory](https://github.com/Marvisak/VIctory) | GPL-3.0-or-later | take with credit | XTP's base: native x86-64, HLE, RDNA2 → DXIL | 612 PRs: 443 open, 89 merged, 80 closed; last created 07:12 UTC, #710. `main` has not moved. 8.6 k stars. | `ffe310e` | menus 30 fps; the world dark at 0.5–1.8 s per frame, a GPU hang on NVIDIA after 2–5 min (fixed in some PR sets, below) |
| [ivall/xcl](https://github.com/ivall/xcl) | GPL-2.0-only | ideas only | native x86-64, HLE, RDNA2 → HLSL → DXC; title memory GPU-visible | 910 PRs: 552 open, 200 merged, 158 closed; last #1196 at 13:05 UTC. `main` has not moved. | `47efa2e` | menus at a steady 30 fps; gameplay not reached (minimap mesh pipelines refused) |
| [simnJS/AnyPS5](https://github.com/simnJS/AnyPS5) | GPL-2.0 (its README: the original Kyty code by InoriRus is MIT) | process only | PS5 titles on Linux/Windows (AnyKyty, Vulkan) | 195 PRs: 15 open; last #292 at 06:15 UTC. No commit since 2026-11-22 23:46. | `7de61e3` (branch `AnyKyty`) | not a BTP 6 project |

Other projects working on BTP 6 or Xbox Series titles on PC are in
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

*Pass 2:* #689 was closed at 14:54 UTC in favour of #985. Take #985
instead (item 61).

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
| 15 | [#211](https://github.com/Marvisak/VIctory/pull/211) | open, clean | Yaekai | `PresentX` with several planes composites them: in Series S mode, a 1080p scene plus a 2160p UI plane. | gpu · G20 | K | **Take.** It answers half of gpu open question 3: BTP 6 presents two planes in Series S mode (Series X not reported). Series S menus are black without it. |
| 16 | [#431](https://github.com/Marvisak/VIctory/pull/431) | open, clean | AntoninMichelq | Host textures and slot resources are freed only after the submissions that use them, and made on first use. | gpu · G9 | H | **Take after #595, keeping what #595 does not cover.** The longest run reported: 11 min 39 s in the intro cutscene. It halves host texture memory on an 8 GB card. #609 and #575 overlap; ignore them. |
| 17 | [#454](https://github.com/Marvisak/VIctory/pull/454) | open, clean | ruvenesh | An opt-in hook past the title's own watchdog for build 1.0.2.1. The title kills itself when its main thread has not advanced for about 30 s, which slow hosts trigger. | tooling · B15 (game layer); loader | H | **Take, into the game layer, off by default.** Testers need it to stay in a slow world. It is build-specific code addresses, which belongs in `xtp-game`. |
| 18 | [#452](https://github.com/Marvisak/VIctory/pull/452) | open, clean | AntoninMichelq | Spin-wait loops in translated programs stop after 4,096 iterations; the loop limit also applies in release builds. | shader · emit; gpu §6.3 | H | **Take.** Testers run with `…_SHADER_LOOP_LIMIT=32` to stay up. A bounded default in release is what G11 assumes. |
| 19 | [#509](https://github.com/Marvisak/VIctory/pull/509) or [#130](https://github.com/Marvisak/VIctory/pull/130); [#638](https://github.com/Marvisak/VIctory/pull/638) | open, clean | aycow; Sunkiller-qc; a third author (login in the private notes) | #509: an opt-in GPU census by pipeline and shader hash (spans, groups, build time, refusals). #130: frame times, memory and caches to `frames.csv`, a Perfetto trace and an overlay. #638: diagnostics for what a hang waits for and for a black frame. | gpu · G2; tooling · B6, B9 | C D H | **Take #509 for G2.** It names the slow programs that h0useadv's TDR run points at. #130 is large (2,720 lines); take it only if G2 needs a trace. Take #638's switches when debugging the dark world. |
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
| 25 | [xcl #1182](https://github.com/ivall/xcl/pull/1182) | open | 3L1A5-1 | BTP 6 maps one physical page under render targets at two addresses at once. In its first 40 s, 422 targets (2.1 GB) had their pages mapped under 276 others (1.85 GB), and the game used both. GPU-owned texture pages are now keyed by the physical page, not by the address. | gpu · G19; §6.1 cause 4; hle · xmem | K | **Reimplement in G19.** It explains part of the dark world: what one alias writes, the other never shows. It also costs 1.85 GB of VRAM. VIctory #643 covers only aliases at one address. |
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

Searched: GitHub repositories for BTP 6 on PC, Xbox Series
compatibility layers and emulators, RDNA2 shader recompilers, and two port
trackers (named in the private notes, README), whose project lists were
used as leads only.

Stats are a snapshot from about 13:15–13:35 UTC. "Commits" counts
default-branch commits since 2026-11-23. Items marked "code" were read in
the commit or diff; the others are README claims.

### 5.1 Projects

| Project | Licence | What XTP may do | Approach | BTP 6, by its own reports | Activity | Head |
|---|---|---|---|---|---|---|
| [OpenCommunityEdition/OpenScarlett](https://github.com/OpenCommunityEdition/OpenScarlett) | AGPL-3.0-or-later (tools and tests MIT) | reimplement | XTP's design in C++, RDNA2 → HLSL → DXC; not a fork of californiaAlmonds' | game unnamed ("a newer retail game"). Start screen and intro at 30 fps on an RTX 5090; stops while the scene behind the menus streams. 25,601 of 25,889 programs translate. | 43 commits, 1 open PR | `7c85829` |
| [ThatRecompDev/five-star-recomp](https://github.com/ThatRecompDev/five-star-recomp) | GPL-3.0-or-later | take with credit (but C# and HLSL: in practice a design reference) | dynamic loader, HLE, RDNA2 → HLSL → DXC | reaches the story intro's title card, then stops on corrupted memory; "all 25,908 programs compile" | 361 commits, no PRs | `15219c4` |
| [fablinoo/palmwave](https://github.com/fablinoo/palmwave) | GPL-2.0-only (xcl fork) | ideas only | xcl plus patches, some of them byte patches of the title | first mission on an RTX 5080: about 7 k draws and 7 k dispatches a frame, 18 s to 2 min of GPU time per world frame, a uniform colour on screen | 533 commits, no PRs | `5a54236` |
| [LooperSalty/AnyXboxX](https://github.com/LooperSalty/AnyXboxX) | GPL-3.0-or-later | take with credit | its BTP 6 runs are VIctory, branch `LooperSalty/victory` `anyxboxx/stable` (VIctory `main` plus 22 open VIctory PRs) | first cutscene, 5 min in the world without a GPU hang on an RTX 4070 SUPER; black scene, 0.3–0.5 fps | 84 commits, 96 merged and 10 open PRs, highest #139 | `b15d882` |
| [DesignWithTommy/Neonbridge](https://github.com/DesignWithTommy/Neonbridge) | GPL-3.0-or-later, **but** its README says xcl's GPL-2.0-only page pool was copied into `placement/pages.rs` | ideas only (licence conflict inside the project) | TuxBox fork (Apache-2.0) with credited parts of many projects | first mission, black, out of memory after 2 min | 406 commits, no PRs | `63daaa4` |
| [R0Y00/Xbox2Windows](https://github.com/R0Y00/Xbox2Windows) | MIT | take with credit | static recompilation research; shaders Python → HLSL → DXC | a modified diagnostic executable reaches loading. Its README's "26,500 of 26,503 shaders" is a scan; `docs/experiments.md` verifies 865. | 1,898 commits, 1,221 merged and 47 open PRs, highest #1503 (#1517 an hour later) | `309cfed` |
| [ubiquitarianism/leonida](https://github.com/ubiquitarianism/leonida) and forks ([LeonidaReforge](https://github.com/LeonidaReforge/Leonida), [ProjectLeonidaL](https://github.com/ProjectLeonidaL/leonida), [IAmNotSiob/leonida-ce](https://github.com/IAmNotSiob/leonida-ce)) | GPL-3.0-or-later | take with credit | static recompilation, offline RDNA2 → SPIR-V, Vulkan | ubiquitarianism does not boot to a frame. leonida-ce presents black frames, with a race crashing at about 2 min. All 26,503 shaders pass spirv-val. | ubiquitarianism: 2,852 commits, 560 open and 131 merged PRs, highest #739 | `2d73c24` |
| [Kajcsi/G6Recomp](https://github.com/Kajcsi/G6Recomp) | GPL-3.0-or-later | take with credit | early static recompilation and loader | no frame yet | 61 commits | `82ccaa5` |
| [uiliannn/VItoriaBr](https://github.com/uiliannn/VItoriaBr) | GPL-3.0-or-later | take with credit | VIctory at `ffe310e` plus 3 commits | world loads; `DispatchMesh` causes TDRs | 3 commits on VIctory | `dbff821` |
| [californiaAlmonds/OpenScarlett](https://github.com/californiaAlmonds/OpenScarlett) | MIT | take with credit | C++ translation layer | Minecraft at 60 fps; BTP 6 early frames only | 89 commits, highest PR #50 | `46d8e2c` |
| [Batushn/KestrelX](https://github.com/Batushn/KestrelX) | MIT | – | C++ layer | no BTP 6 | 38 commits, highest PR #41 | – |
| [jadeowaowa/scarlett-layer](https://github.com/jadeowaowa/scarlett-layer) | Apache-2.0 | – | C++ layer | Terraria menus | 14 commits | – |
| [sammwyy/xvalkyrie](https://github.com/sammwyy/xvalkyrie) | GPL-3.0 (no "or later" found) | – | Xbox One and Series layer | no BTP 6 | 72 commits, highest PR #11 | – |
| [rpatty-dev/PercyScarlett](https://github.com/rpatty-dev/PercyScarlett) | GPL-2.0-only | – | Rust layer | first `PresentX` | highest PR #7 | – |
| [puffnfresh/rebox](https://github.com/puffnfresh/rebox) ([idvwork/rebox](https://github.com/idvwork/rebox) reposts it) | GPL-2.0-only | – | Linux only: WineGDK and vkd3d-proton, GPU address = CPU address through a Vulkan layer | – | 366 commits | – |
| [luizwidmer/palmetto](https://github.com/luizwidmer/palmetto); a boot test (no licence) and a test bench (MIT), both named after the title (links in the private notes, README) | – | – | runtime without a frame; stand-in DLLs to a first UI screen; a test-harness scaffold | – | – | – |

### 5.2 Items

Numbering continues from section 4.

| # | Item | State | Author | What it does | XTP area · backlog | Sym. | Action and why |
|---|---|---|---|---|---|---|---|
| 41 | [palmwave `80492db`](https://github.com/fablinoo/palmwave/commit/80492db9) | commit (code) | Stellarr Studio | Measures where the title's GPU heaps should live. BTP 6 commits about 4.4 GB of GPU heaps, and xcl puts them in imported host memory that the GPU reads across PCIe. On an RTX 5080 the same dispatch takes **431 ms from host RAM against 0.45 ms from video memory**: descriptor loads cost 4–7 µs a workgroup, with the SMs at 100 % and PCIe busy. An opt-in switch moves heaps above a size to the VRAM windows. | gpu · G15–G17a, risk "Bandwidth" | C H | **Reimplement the rule in G16–G17a.** Imported host memory is only for small, CPU-read buffers (constants, readback, labels). The title's GPU heaps go to VRAM tiles that follow commits. This turns gpu.md's bandwidth risk into a measured fact, and a G16 that imported everything would be slower than the shadows. |
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
| scarlett-layer, xvalkyrie, PercyScarlett, rebox, palmetto, the boot test, the test bench | No BTP 6 progress past the first frames, Linux only, or no licence. Nothing XTP lacks. |

### 5.4 Lures

These names came up in searches, and some are tracked as candidates by
the port trackers. They look like the usual bait: the title's name with
"PC", "emulator" or "Windows beta", installers, near-identical copies.
They were not opened past their search entry. Do not download from them.

Their names carry the title's real name, so the list is in the private
notes (`E:\xtp-data\upstream-private.md`, README): seven repositories,
one of them copied under four owners.

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
