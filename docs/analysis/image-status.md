# Image status: the light loop, the staging release and the stride table together

Task imagecheck (7683b613), 2026-11-25, on the person's PC (RTX 4080
SUPER, Series S mode). The fixes that landed for the black world frame, measured
together for the first time:

- the light-loop preparation `11eeb27c2d15980d` now translates (440d7e4, task
  a8d52dcc);
- the staging of textures picked by index is released after upload (c44a716,
  task 86c766da);
- the stride table and the run-time views, switched on with
  `XTP_STRIDE_TABLE=1 XTP_RUN_TIME_VIEWS=1` (still off by default, task
  5d4d8f6f).

Build: `main` at c44a716, diagnostics build made on the build farm. The
sessions ran it from a worktree at that commit. The baseline is session
`imgsrc-1` of docs/analysis/pix-world-frame.md §11, with the stride table
off and none of the three fixes. Programs are named by hash only. Captures,
dumps and logs stay under `E:\xtp-data\`, none is in the repository.

## 1. Verdict

- **The world is recognisable for the first time.**
  - `imgsrc-1` presented a black frame.
  - Main with the stride table on presents the opening's night street. You
    see the skyline with lit windows and signs, the street lamps, power lines
    and traffic lights, car lights, and the characters from behind (§4.4).
  - It is still very dark. The characters show as flat pale shapes with an
    outline.
- **Geometry and albedo are mostly back** (§4.1).
  - The main depth covers 57 to 84 % of the screen, against 20 %.
  - The G-buffer albedo is non-zero on 40 to 60 % of the screen, mean 0.04
    to 0.07, against 12 % at mean 0.004.
- **The light loop runs**, and the tile light lists are no longer empty.
  - `11eeb27c2d15980d` runs once a frame.
  - `s1+80` is non-zero on 8 to 15 %, against 0 %; `s0+968` is no longer
    uniformly 0xffffffff.
  - But only 2 of the 13 slices of `s1+80` hold anything.
- **The lighting still writes about 0.** It covers the geometry and replaces
  the material resolve's colour with almost nothing:
  - mean 0.001, at most 0.011 over 57 % of the screen in frame 618;
  - 6.7 % non-zero in frame 621;
  - 0 % in frame 624.
  
  The diffuse light `s0+1008` is still 0 in every frame measured on main.
  That zero light chain is what keeps the image dark (§5.2).
- **The stride table and run-time views make the world run at 0.5 to 0.7
  frames per second**, against 3.9 to 5.8 with them off.
  - Preparing a frame takes 1.3 to 1.4 s.
  - Most of that is refreshing host copies of buffers at every command list:
    110,000 to 220,000 refreshes per frame copy 8 to 17 GB (§4.3, §5.1).
  - This is what breaks the image now. It cannot be turned on by default as
    it is.
  - With `-Measure 40` and the brief's `XTP_CAPTURE_WORLD=2000:60:4:1`, no
    world frame was captured at all.
- **Textures:**
  - The staging is no longer the wall: 40 to 53 MiB live after the load,
    against the 1 GiB cap.
  - 14,900 to 16,300 index-picked slots still wait, because video memory
    sits at 89 to 90 % of its budget from the first seconds in the world
    (§4.2, task cba81604).

## 2. Sessions

| Session | What | Result |
|---|---|---|
| `lightloop-1` | the light-loop worker's own session | **never ran**: its queue waiter (pid 60920) died when the worker's pane closed, and its worktree was removed. The ticket was orphaned; there is no run or capture folder |
| `imagecheck-1` | the brief's settings | the title stalled 5 min in `xtp_pipeline::cache::Index::scan` (§6.3) and was stopped at the 300 s limit, no frame |
| `imagecheck-2` | the brief's settings | ran, 0.7 fps; no frame reached 2,000 draws in 40 s, so `XTP_CAPTURE_WORLD=2000:60:4:1` never armed |
| `imagecheck-3` | `XTP_CAPTURE_WORLD=1000:12:3:5` | frames 597, 602, 607 (PNG), dumps 598, 603 |
| `imagecheck-4` | `XTP_CAPTURE_WORLD=1000:22:3:3`, `-Measure 60` | the night street: frames 617, 620, 623 (PNG), dumps 618, 621, 624 |

Every session used the brief's settings:

- launch: `E:\tools\stall-probe\queue-session.ps1 -Measure 40 -Limit 300`
  (60 for `imagecheck-4`);
- switches: `XTP_STRIDE_TABLE=1 XTP_RUN_TIME_VIEWS=1 XTP_TEXTURE_POOL=1`,
  `XTP_FRAME_PROBE=11eeb27c2d15980d,1cd9bc634417a981,db4835a364449a45`;
- added for the measurements: `XTP_CAPTURE`, `XTP_PASS_DUMP=0` and
  `XTP_PASS_INPUTS=1cd9bc634417a981` (the lighting pixel, as in
  `imgsrc-1`).

The night-street shot starts about 22 frames after the first frame with
1,000 draws. The same held in `staging2-2`, the staging-fix worker's session
of the same settings.

The light-loop numbers (step 1 of the task) therefore come from these
sessions on `main`, with the stride table on.

Files:

- captures and dumps: `E:\xtp-data\pix\imagecheck-{3,4}\`;
- session files: `E:\xtp-data\runs\stall-imagecheck-*\`;
- title logs: `E:\xtp-data\cache\play\20261125-094059\`, `-094408\` and
  `-094629\`.

## 3. The light loop against imgsrc-1

The lighting pixel `1cd9bc634417a981`'s inputs just before its first
rectangle (pix-world-frame.md §11.3 for what each slot is).

| Input | `imgsrc-1` 675 | 598 | 603 | 618 | 621 | 624 |
|---|---|---|---|---|---|---|
| `s1+80` tile light lists 160×90×13, non-zero | 0 % | 8.3 % | 14.7 % | 15.4 % | 11.8 % | 12.2 % |
| `s1+80` slice 0 / slice 1, non-zero | 0 / 0 % | 8 / 100 % | 92 / 100 % | 100 / 100 % | 39 / 94 % | 45 / 94 % |
| `s1+80` slices 2–12 | 0 | 0 | 0 | 0 | 0 | 0 |
| `s0+968` 7680×90, entries other than 0xffffffff (from the mean) | 0 % | 1.9 % | 0.2 % | 0 % | 7.1 % | 1.4 % |
| `s0+1008` diffuse light, RGB | 0 | 0 | 0 | 0 | 0 | 0 |
| `s0+456` the other light, RGB at most | 0.008 | 0.0009 | 0.0011 | 0.0012 | 0.0013 | 0.0018 |
| exposure (`s0+168`, word 0) | 1.25 | 8.40 | 6.30 | 4.56 | 3.91 | 3.38 |

- **`11eeb27c2d15980d` runs**, one 20×12 dispatch per frame. It is absent
  from `imgsrc-1`'s dumps, where it was refused.
- **Slice 0 of `s1+80`** often holds 0xffffffff, a "no light" marker.
  **Slice 1** holds small counts or masks. The other 11 slices stay 0.
- **`s0+1008` stays 0** in all five frames on `main`.
  - Its two R9G9B9E5 views alternate from frame to frame (`0x97b110000`,
    `0x97bf50000`). Both are 0.
  - The only non-zero one was in `staging2-2`'s frame 593 (at most 0.116,
    mean 0.0003).
- **The exposure buffer** sits between 3.4 and 8.4, against its bounds
  (8, 0.125). The auto exposure is pushed up by a dark scene; in
  pix-world-frame §4 it reached 214.

## 4. The combined frame

### 4.1 Depth, albedo, lighting

From the dumps. "Non-zero" is the dump's "lit": the share of sampled texels
with a channel other than 0. Means are over all sampled texels.

| Frame | Ops (dropped) | Depth pre-pass | Depth before the lighting | Albedo `s0+584` non-zero | Albedo mean RGB | Lighting output non-zero | Lighting mean, max |
|---|---|---|---|---|---|---|---|
| `imgsrc-1` 675 | 8,298 (632) | 8.3 % | 20.1 % | 12.0 % | 0.004, 0.003, 0.002 | 0.2 % | 0.0000, 0.0014 |
| 598 | 6,743 (871) | | 91.7 % (≤ 0.03: a cut) | 91.7 % | 0.019, 0.017, 0.014 (≤ 0.035) | 12.0 % | 0.0000, 0.0000 |
| 603 | 5,646 (363) | 8.3 % | 8.3 % | 0 % | 0 | 0 % | 0 |
| 618 | 8,440 (678) | 44.0 % | 57.1 % | 39.5 % | 0.071, 0.047, 0.041 | 57.1 % | 0.0012, 0.011 |
| 621 | 7,516 (285) | 79.5 % | 84.2 % | 59.9 % | 0.049, 0.045, 0.041 | 6.7 % | 0.0010, 7.7 |
| 624 | 12,400 (1,243) | | 64.4 % | 46.6 % | 0.054, 0.042, 0.031 | 0.0 % | 0 |

- **Frame 598 is a cut.** Its depth holds values of at most 0.03.
- **Frame 603 is the shot of silhouettes against a dusk sky**, with an empty
  G-buffer.
- **618 to 624 are the night street.**
- **In frame 621, the lighting covers the geometry and writes about 0 there.**
  The material resolve `f0c91b4d2578f5bf` (P467) leaves the scene colour
  non-zero on 84.2 % of the screen, the depth coverage, at mean 0.002. The
  lighting's 232 rectangles (P605, blending off) leave it non-zero on 6.7 %.
  So they ran over the geometry and wrote about 0 there.
- **The material-class plane** (R24G8) is written by the material resolve
  itself (P467, 58.8 % non-zero), before the lighting tests it EQUAL.
- **The two culling programs of pix-world-frame §6.3**, `48426bae580a080c`
  and `969c1cef8b783b39`, run every dispatch of their own.
  - The 162 to 1,102 ops dropped in `48426bae580a080c`'s pass are not its
    own. cxstrideon noted on 25 November that a pass's unnamed ops can
    belong to other programs. Here they are dispatches of the ray-tracing
    structure builder, dropped as "BVH builder held".
  - Its 52 programs are admitted together (`pipeline/builder.rs`, VIctory
    #2118). One of them, `0ddab4b63b1394e0`, does not translate.

### 4.2 Textures picked by index and video memory

From the title's 5-second lines ("textures shaders pick by index", "host
textures").

| | `imgsrc-1` (1 GiB staging, never freed) | `main`, `imagecheck-3` and `-4` |
|---|---|---|
| slots waiting | 12,500 to 19,000 | 14,900 to 16,300 |
| slots served | 13,200 to 14,300 | 15,600 to 19,600 |
| staging live | 1,023 MiB, at its cap, the whole time | 38 to 53 MiB after the load (29,000 to 31,500 released) |
| video memory | 12,300 to 13,000 MiB of 15,090 (81–86 %) | 12,500 to 13,400 MiB of 14,550 to 14,900 (89–90 % after the load) |
| host textures | 26,200 to 34,700 alive, 8,400 to 10,700 MiB | 21,900 to 25,100 alive, 9,400 to 11,500 MiB |

- "video memory near its budget, the rest wait" comes within 1 to 2 s of
  the world loading.
- The staging fix works: 2,000 to 5,000 more slots are served.
- The wall is now video memory.
- The three defects Codex's review found in c44a716 are on `main` until task
  2a48667f lands:
  - a valid texture overwritten by a blank buffer;
  - a served texture left stale during loading;
  - CPU copies growing.

  They can blacken or age textures in places. These sessions cannot tell
  them apart from the waiting slots.

### 4.3 Frame time

Median of the frame lines in gameplay (`cargo xtask play` summary).

| Session | Switches | fps | ms/frame | preparing | GPU | buffer copies per frame |
|---|---|---|---|---|---|---|
| `imgsrc-1` (07:58) | off | 3.9 | 254.7 | 73.9 | 166.2 | 14,000–16,000 refreshes, 58–67 MB |
| `rt641f0d` (10:09, another branch) | off | 5.8 | 172.2 | 43.8 | 110.0 | 10,000–14,000 refreshes, 38–61 MB |
| `imagecheck-2` | on | 0.7 | 1,833 | 1,439 | 207 | 180,000 refreshes, 12.8 GB copied (its plain part in 858 ms) |
| `imagecheck-3` | on | 0.7 | 1,833 | 1,392 | 284 | 122,000–195,000 refreshes, 8.3–13.1 GB copied (612–924 ms) |
| `imagecheck-4` | on | 0.6 | 1,721 | 1,257 | 242 | 113,000–221,000 refreshes, 7.8–16.7 GB copied (591–1,083 ms) |

Other costs per frame with the switches on:

- guards and write watch: 160 to 300 ms;
- the snapshot and CPU compare: 195 to 220 ms.

### 4.4 The captures

Each listed frame `n` gives `frame-<n>.png` (what was presented) and
`passes-<n+1>.txt` (the dump). At 0.5 fps the two can differ:
`frame-617.png` is black while `passes-618` has a full G-buffer. So each
PNG is described as seen.

| File | What it shows |
|---|---|
| `E:\xtp-data\pix\imgsrc-1\frame-675.png` | before: black |
| `E:\xtp-data\pix\imagecheck-3\frame-602.png` | the opening: utility poles, buildings, a road sign and a character's back, black silhouettes against a grey dusk sky, letterboxed |
| `E:\xtp-data\pix\imagecheck-3\frame-607.png` | the night street: skyline with lit windows and signs, street lamps, headlights and traffic lights; the foreground characters are large flat pale shapes with an outline |
| `E:\xtp-data\pix\imagecheck-4\frame-620.png` | the same shot: power lines, lamp posts, lit windows, a car's tail lights; the characters are flat pale grey with darker strands at the edges |
| `E:\xtp-data\pix\imagecheck-4\frame-623.png` | a wider street: skyline, power lines, lamp posts, a bare tree, traffic lights; the road and the near buildings are almost black |

`E:\xtp-data\pix\staging2-2\frame-597.png` (the staging-fix branch, same
settings) shows the night street with a car and a lit facade.

## 5. What zeroes or breaks the image next

In order of effect on what the person sees.

### 5.1 The run-time views refresh 8 to 17 GB a frame (breaks)

- **The cost is in `draw.rs` `picked_buffers` and `picked_again`.**
  - Since the Codex review of 54ab53d, every command list, not once a frame,
    refreshes what every run-time buffer view of the heap reads.
  - At about 40 lists per frame, that is 110,000 to 220,000 refreshes.
  - One 174 MiB buffer (`0x1d430000+0xae00000`) alone was copied 45 times a
    frame, 8.2 GB.
- **The world then runs at 0.5 to 0.7 fps**, against 3.9 to 5.8 with the
  switches off.
- **Not A/B'd here.** The two switches were on together. The per-list
  refresh is `XTP_RUN_TIME_VIEWS`' code path, which makes it the likely
  cost, not a proven one. cxstrideon (task 5d4d8f6f) has the on/off pair on
  the L4.
- **Next step:** refresh a run-time view's span once per submission, or once
  per title write (the write watch already knows), instead of once per list.
  Then measure the stride table and run-time views apart.

### 5.2 The light given to the lighting is 0 (zeroes the lighting)

- **The lighting covers the geometry and writes about 0** (§4.1, frame 621).
- **The diffuse light `s0+1008` is 0 in every frame** (§3). Its writer chain
  is pix-world-frame §11.3: `34fb51b8a0312e82` ← `2dc364df5d3278b8` ←
  `8cb27ccf2d50c606` and `17e736204a05a105`. Task 71e45f7c (lightchain)
  follows it.
- **The tile light lists fill 2 slices of 13.**
- **The ray-traced lighting is absent.**
  - The structure builder is held, so no acceleration structure is built.
  - So the RT probes, irradiance gathers and local-light shadow traces of
    pix-world-frame §5.3 cannot run (RT tasks 641f0de3, 0ac4924f).
- **This is the person's "très sombre".**
  - Direct light: the lists are partly filled, but the lighting result is
    about 0.
  - Indirect light: 0.
  - What lights the picture is the sky, the emissive windows, signs and
    lamps, the fog and the post passes. The exposure is pushed to 3.4–8.4.

### 5.3 Frames where the lighting draws nothing at all (lead)

- **Frames 618 and 624 have the same light inputs**: probes, `s0+1008`,
  `s0+456`, and the lists at 12–15 %.
- **Yet the lighting leaves 57.1 % of the screen non-zero in 618 and 0.0 %
  in 624.** Two candidates:
  - The rectangles' EQUAL test against the material-class plane missed in
    624 (task 7873b786, "redrawn only at list close").
  - The tile classification gave no rectangles. `s1+48` and `s1+56` (10×6)
    hold small counts in 618 and mask-like values up to 6×10⁸ in 621 and
    624.
- **To confirm** with twins' counters (7873b786), or `XTP_PASS_INPUTS` on
  the classification.

### 5.4 The characters: drawn, but unlit and flat

- **All the character programs of refusals.md translate and run** in frame
  621:
  - the G-buffer meshes `ef5409053e14aa82`, `093c64546c55554e`,
    `5bd95fdec765c0d6` and `f841ad74ec6e7367`, in P277: 248 of 250 ops; the
    2 dropped ones are indirect mesh dispatches without a pipeline;
  - the skinning `36e010b53acffd77` and `2bba29bbd7c019bc`: 110 of 110;
  - the skinned shadow depth `910b4be059bfb28f` and `321d411b20a35ef9`, in
    the shadow passes: all ops run.
- **They are not in the depth pre-pass.** P277 raises the depth from 79.5 to
  82.5 %, so they write their own depth there.
- **Their albedo** is `saturate(T) × B` (pix-world-frame §11.2). Their
  instance colour is white with alpha 0, so the tint has no weight. `T` and
  `B` are textures picked by index, so a character whose slots wait (§4.2)
  reads 0.
- **Whatever their G-buffer holds, the lighting writes about 0 on them**, as
  everywhere (§5.2).
- **What makes them pale and outlined is drawn after the lighting.** The
  passes after it take the scene colour from 6.7 % to 100 % non-zero:
  - the sky `348e1e8c414c7eca` (P656) and its dome draws (P657): 23 %;
  - then the fog application `577248728544d74b` (P673) and the draws after
    it: 100 %.
- **Which of those gives the flat fill and the outline is not shown by these
  dumps.** The dump samples rows across the whole target, not the
  characters' pixels.
- **Next:** `XTP_PRESENT_TARGET=1280x720:f27` (the albedo) and
  `1280x720:f10@1cd9bc634417a981` (the scene colour after the lighting) in
  one short session, to see the characters' pixels.

### 5.5 Missing meshes: what is still dropped

Frame 621, and the title's 5-second cause lines at 09:47:22 and 09:47:44.

| Cause | Ops per frame | What |
|---|---|---|
| BVH builder held | 817 to 1,092 | ray-tracing structures (§4.1) |
| indirect dispatch, does not translate | 70 to 72 | about 20 compute programs, `caf96a3229145fa6` (particle collision) 17 to 18.5 of them |
| indirect mesh dispatch, does not translate | 10.5 to 33 | mesh `45f081396c2c59ba`: "a first-lane read of a value lanes may hold differently … wave64 mesh group" (refusals.md row 15, distant LODs) |
| being made | 3 to 50 | pipelines still compiling |
| no dynamic slots for its tables | 32, then 64 dropped (running count) | at 09:47:52, about 60 s into the world (peak 178,326 of 196,608 slots), after the captures; `imgsrc-1` hit it about 2 min in (task c81d63d7) |

- **The stride table's raw views run out.** They reach 65,536 of 65,536
  about 30 s into the world, with `retiring=0`.
  - From then on, a buffer read through the table reads zeros: 13 to 19
    refusals in these sessions.
  - It grows over a longer session.
  - Task 6cc97dd7 (stride table round 3: retiring at the fence) covers it.
- **Depth coverage** is in §4.1: 57 to 84 % on the night street.

### 5.6 Video memory and albedo (darkens)

- **15,000 to 16,300 index-picked slots wait** on video memory at 89–90 % of
  its budget (§4.2, task cba81604).
- **The albedo is non-zero on 40 to 60 %, but its mean stays 0.04 to 0.07.**
  - That is low for a street.
  - Samuel's watch item 29 names the next lead, without it being handled
    here: VIctory fork #684 (3467f1aeb) makes three compute outputs
    R32_FLOAT, for +2.02 points of non-zero base colour on the same GPU.

### 5.7 Shadows and exposure

- **Shadows.** The shadow-map passes run every op, and the characters' skinned
  depth runs there now (§5.4). Their R16 typeless targets are read as half
  floats by the dump, so their contents are not measured here. Nothing shows
  them zeroing the image.
- **Exposure.** It sits at 3.4 to 8.4, near its bound of 8. It follows the
  missing light rather than causing it.

## 6. Notes

### 6.1 For the person's three points

| Point | Answer |
|---|---|
| Characters transparent with an outline | Drawn and not dropped: G-buffer meshes, skinning and shadow depth all run. Unlit like everything else; their textures may be among the waiting slots. Their fill and outline come from passes after the lighting (§5.4). |
| Missing meshes | Depth 57–84 % against 20 %. Still dropped: 10–33 mesh dispatches of `45f081396c2c59ba` (wave64), about 70 compute dispatches, the ray-tracing builder. Slots run out within the first minute: the stride table's raw views at about 30 s, then the dynamic slots at about 60 s (§5.5). |
| Darkness | Direct light: 2 of 13 list slices filled, and the lighting writes about 0. Indirect light (`s0+1008`): 0 in every frame. Ray-traced light: absent (§5.2). |

### 6.2 Sessions and queue

- **The queue lost `lightloop-1`.** A waiter of
  `E:\tools\stall-probe\queue-session.ps1` dies with its worker's pane.
- **The queue let two sessions start together.** `Test-Busy` looks for
  `xtp.exe`, and the precook runs as `xtp-precook.exe`: `imagecheck-1` and
  `rt641f0d` both started at 09:59. `measure.lock` still kept their games
  one at a time.

### 6.3 The shader cache scan

`imagecheck-1` spent its whole 300 s in `Cache::open_with_limit` →
`Index::scan` → `disk::indexed_size`.

- **Evidence.** Stacks taken with `cdb -pv`
  (`E:\xtp-data\runs\imagecheck-1-try1\stacks-42228.txt`); the process used
  12 s of CPU in 4.5 min.
- **The scan opens every file of `E:\xtp-data\cache\shaders`** to read its
  header: 157,805 files, 8.5 GiB, the 8 GiB limit of `shader_cache.rs`.
- **Usually this takes 3.5 to 17 s.** It took over 5 min right after a
  23-min precook that wrote 26,168 translations and made 90,037 pipelines.
  E: is an NVMe drive.
- **The cause is not shown.** It may be a cold file cache, or on-access
  scanning of new files.
- **Next:** do not open every entry at start. Possible ways: an index file,
  sizes from metadata, a background scan, or pruning dead translator
  generations.

### 6.4 Tools

Two short scripts summarised the dumps and logs. They are outside the
repository (session scratch), like the PIX clients of pix-world-frame §8:

- one prints a pass dump's depth and albedo changes, the lighting pass, its
  inputs and the late outputs;
- the other prints a run log's frame, texture and video-memory lines.
