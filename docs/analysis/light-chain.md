# The light chain: where the world's light becomes 0

Task lightchain (71e45f7c), 2026-11-25, on the person's PC (RTX 4080 SUPER,
Series S mode). Follow-up of pix-world-frame.md §11: the deferred lighting
`1cd9bc634417a981` computes RGB = e × (D × albedo + S), and D, the diffuse
light, is 0 on every texel. D = L × (1 + k) + X: L is the screen probes'
indirect light (`s0+1008`), X the loop over the local lights of the
pixel's tile (`s1+80`, a 160×90×13 R32_UINT texture). Both are 0.
Programs are named by hash only. Dumps and probe files stay under
`E:\xtp-data\pix\`; none is in the repository.

## 1. Verdict

- **The local lights stop at `11eeb27c2d15980d`.** That program turns the
  light clusters into the per-tile light masks the lighting loops over,
  the 160×90×13 texture. XTP refuses it ("GFX10 code at 0x484: a
  descriptor's address or format read as data"). Its 80×45 dispatch is
  dropped in every frame of both sessions examined: the pass of
  `bad01f53f53248e5` runs 1 op of 2.
  - The texture is never written. It is 0 in all 13 slices.
  - Had the program run, slice 0 of a tile would hold the range of its
    non-empty mask words, or `0xffffffff` for a tile with no light. A 0
    reads as "words 1 to 1", and word 1 is 0.
  - The pruning `307e048e4cdd99d4` and the lighting therefore find no light
    in any tile.
- **Its inputs are full** (session `lightchain-1`, §5):
  - the light records: 58 of the 384 slots are in use;
  - the clusters: 31,802 to 40,356 non-zero bytes of 1,530,880 on the GPU,
    with masks such as `0x5f`, `0x11c`, `0x308` in the clusters near the
    camera;
  - 8 to 26 light proxies are rasterized into those clusters a frame.

  The texture it would write is the one the pruning writes and the
  lighting reads: the same host texture, at title memory `0xaf9280000`.
- **The refusal is a false positive** (§3.3):
  - The console code keeps the 12 mask words in a 16-register array
    (v3–v18) that it indexes through M0. It fills the array's unused tail
    (v15–v18) with four dwords of its output's descriptor.
  - M0 never leaves 0–11: both of its bounds are clamped with
    `v_med3_u32 …, 0, 11`.
  - XTP lowers an M0-relative move as a choice among every register up to
    the program's VGPR count. The descriptor dwords therefore enter the
    choice and the loop's join, and the program is refused.
  - The fix is in §6.1 (worker lightloop, task a8d52dcc, translates it).
- **The screen probes' light L is 0 because no probe ray is traced** (§4):
  - The rays' trace `f22a76fcb967ee59` is refused (an address not from a
    root parameter, W12) and dropped, although its own arguments ask for
    17 groups.
  - The 5,652,480-byte buffer it would write stays 0. The probes'
    injection `6bb493a39685ddcc` reads that buffer, and the probe atlas it
    writes (2048×120 R32) is 0.
  - `39f0e9aeb156a61c`, which activates the rays, is refused too (a
    descriptor chosen by control flow). The ray-miss pass
    `b48d48f8304e5bd3` gets arguments of 0 groups.
  - From there to D's texture, every pass runs on zeros: `8cb27ccf2d50c606`
    and `17e736204a05a105` (atlas filters), `2dc364df5d3278b8` and
    `940c2635dc1312f4` (final gather), then `34fb51b8a0312e82` (temporal
    resolve).
- **The fork's lead (watch item 328) is already XTP's state.**
  - `6d7fe7745ab9c6d8` is refused in XTP (an address not from a root
    parameter) and dropped in every frame. `0873f80dd84a3c5c` is never
    bound in our sessions.
  - "The pair skipped", which the fork's issue found lit, is already how
    XTP runs, and the world is still dark. That pair is not what zeroes
    the light here, so the A/B was not run.
  - `XTP_SKIP_PIPELINES` is read once, so it cannot alternate in one
    session anyway.

## 2. Sources

- **Sessions `imgsrc-1` and `imgsrc-2`** of task imgsrc
  (pix-world-frame.md §11):
  - pass dumps of frames 674–677 and 661–664;
  - the title's logs, with the pipelines' `first bind` lines (a dropped op
    is tied to its program through them);
  - the precook's record list, for which programs the translator refuses
    and why.
- **The translated compute programs of the chain**, taken from the PIX
  capture `world-1` (`xtp-pix pso`, PSOs 20022–20363, then
  `dxc -dumpbin`).
- **The console code of `11eeb27c2d15980d`**: `xtp shader-info --disasm`
  on its record alone, cut out of the pack into a scratch file.
- **Session `lightchain-1`** (PC, 09:57, this branch's diagnostics build).
  Its probe and dump files are in `E:\xtp-data\pix\lightchain-1\`, and the
  title's log in `E:\xtp-data\cache\play\20261125-085744\`. Settings:
  - `XTP_CAPTURE_WORLD=2000:60:4:1`, `XTP_PASS_DUMP=0`;
  - `XTP_PASS_INPUTS=6bb493a39685ddcc`;
  - `XTP_FRAME_PROBE` on `11eeb27c2d15980d`, `307e048e4cdd99d4`,
    `7c90a988167650a1`, `d3e5b737ea7cc150`, `e18931c72af42e46`,
    `39f0e9aeb156a61c`, `f22a76fcb967ee59`, `b48d48f8304e5bd3` and
    `6bb493a39685ddcc`, the dropped ones included (§7);
  - `XTP_FRAME_PROBE_KIB=2048`, `XTP_FRAME_PROBE_MIB=128`,
    `XTP_TEXTURE_POOL=1`.

  Frames 674–677 were probed and dumped. The image is as in imgsrc: the
  session changes nothing in what runs.

## 3. The tile light lists

### 3.1 The chain (frame 675 of `imgsrc-1`)

| Pass | Program | What it does | Ran |
|---|---|---|---|
| P193 L20 | `d3e5b737ea7cc150` | clusters the lights in world space (5×5×30 groups) | yes |
| P505 L62 | `3173882c28a20b75` | cluster depth bounds, 40×23 | 1 of 3: `e18931c72af42e46` (refused, W2) and `32ed072b9f845048` (refused, W12) dropped |
| P506, P507 | `a7eeed0646be1ab7`, `0f0b95cf52ddeb26` | clear the cluster masks and the shadow-mask bounds | yes |
| P508 L62 | vertex `a67d0bcde30cead3`, `7f4b62129572a14d`, pixel `5ad3a82b1c3cc63a` | rasterize the light proxies into the 40×23×32 clusters (1,530,880 bytes, 13 words a cluster) | 2/2: 36 indices × 3 instances and 42 × 15, 18 lights |
| P509 L62 | `7c90a988167650a1` | derives a second cluster buffer from the first (5,980 groups) | yes |
| P512 L62 | `bad01f53f53248e5` | material tiles, 80×45 | yes |
| **P512 L62** | **`11eeb27c2d15980d`** | **per-tile light masks, 160×90×13** | **dropped: does not translate** |
| P513 | `fea2b7d3f17b504f` | material tiles, 10×6 | yes |
| P613 L69 | `0a5ecfee6b79e13b`, `5cf00dc39042edaf` | shadow-mask rays per tile and light (reads the masks) | yes; their counter stays 0 |
| P614 | `495c6013589afd60`, then `559d9f0184e15fe3` | indirect arguments from that counter, (0, 1, 1); the shadow-ray trace | the trace is dropped (refused, W12) |
| P615 | `0ea6fac38d096770` | shadow-mask denoise into the 7680×90 visibility | runs with 0 groups: all `0xffffffff` |
| P616 L69 | `307e048e4cdd99d4` | prunes the masks of shadowed lights | yes: writes nothing, 0 in all 13 slices |
| P618 L70 | `1cd9bc634417a981` and 182 others | deferred lighting (232 rectangles) | yes; reads the masks at `s1+80`: 0 |

The same holds in every dumped frame of three sessions:
- the pass of `bad01f53f53248e5` runs 1 op of 2 (`imgsrc-1` 674–676,
  `imgsrc-2` 661–663, `lightchain-1` 674–677);
- the pruning's output is 0 in every slice wherever it was sampled (in
  676 and 663 the dump's read-back budget was spent before it).

### 3.2 The masks' format, from their two readers

**The reader, `307e048e4cdd99d4`** (translated DXIL of PSO 20363):
- It reads slice 0 of the texture for each 8×8-pixel tile. `0xffffffff`
  means no light. Otherwise the low and high halves bound the mask words
  to read, slices lo + 1 to hi + 1.
- Each set bit of a mask word is a light: index bit + 32 × word, a
  512-byte record in the light array (`s0+216`, 196,608 bytes: 384
  lights).
- The pruning clears a light's bit where the 7680×90 shadow visibility
  says the tile is in its shadow, and writes the word back.
- A slice 0 of 0 reads as words 1 to 1, and word 1 is 0: no light.

**The writer, `11eeb27c2d15980d`**, writes exactly that format (its
console code, §3.3). For a tile the material tiles mark as lit:
- it ORs the masks of the clusters between the tile's depth bounds (13-word
  clusters read with `buffer_load_format_x`, clamped to 12 mask words);
- it drops the lights its flags exclude (bits 17, 21 and 26 of word 6 of a
  light record);
- it stores slice 0 = lo | hi << 16 and the 12 mask words.

For an empty tile it stores `0xffffffff` in slices 0–3.

### 3.3 Why `11eeb27c2d15980d` is refused

Its console code (1,920 bytes, 336 instructions), in short:

- 0x0a0: loads the descriptor of its output, the 160×90×13 UAV, into
  s12–s19.
- 0x19c–0x1f8: makes a 16-register array v3–v18 from s0–s15.
  - v3–v14, the 12 mask words, come from s0–s11, which it has just set
    to 0.
  - v15–v18 come from s12–s15: the output descriptor's dwords 0–3, which
    the program never uses as numbers.
- 0x2c8–0x484: the loop over a tile's clusters and lights.
  - It reads and writes the array element `m0` (`v_movrels_b32 v31, v3`;
    `v_movreld_b32 v3, v29`), with m0 = v28 taken lane by lane
    (`v_readfirstlane_b32`).
  - v28 runs from v21 to v19, both `v_med3_u32 …, 0, 11`. m0 is never above
    11, so v15–v18 are never read there.
  - The back edge is at 0x484, the address the refusal names.
- 0x580–0x6e8: before the last stores, v15–v17 are overwritten with v14,
  and v18 with slice 0's value. The descriptor dwords are dead.

XTP lowers an M0-relative move (`emit/lower/valu.rs`, `v_movrels_b32` and
`v_movreld_b32`) as a choice among every register from the encoded one up
to the program's VGPR count. That choice reads v15–v18, whose values are
descriptor dwords with no host value (`smem::DESCRIPTOR_WORD`). The loop's
join of those registers cannot take the undefined value (`flow.rs`, the
descriptor-word arm of the joins), because they count as read on the
loop's edge. So the program is refused.

## 4. The screen-probe light (D's view `s0+1008`)

| Pass (frame 675, `lightchain-1`) | Program | What it does | Ran | What the probe reads |
|---|---|---|---|---|
| L138 | `39f0e9aeb156a61c` | activates the probes' rays | dropped: a descriptor chosen by control flow (R4) | would write 0x97bf42500 (44,160 B), 0xaeaf30000 (1,884,160 B) and 0xaea1d0000 (942,080 B) |
| L138 | `f22a76fcb967ee59` | traces them against the BVH | dropped: an address not from a root parameter (W12) | its arguments ask for 17 groups (`00000011 00000001 00000001`); would write 0xaeb720000 (5,652,480 B), 0xaeb550000 (1,884,160 B) and 0xaea1d0000 |
| L64–L65 | about 40 programs | shade the rays' hits | about half dropped: a structured buffer of unknown stride (W5, the stride table is off) | |
| P606 L66 | `b48d48f8304e5bd3` | rays that missed (indirect) | ran with arguments `0, 0, 1`: no group | reads 0xaeb550000: 0 of 1,884,160 bytes non-zero |
| P607 L66 | `6bb493a39685ddcc` | injects the rays into the 2048×120 atlas | ran | reads 0xaeb720000 (the trace's records): 0 non-zero in the 2 MiB read; 0xaeb1f0000 (the miss pass's output): 0 of 1,884,160. Writes the R16F atlas (11.9 % non-zero, at most 0.28) and the R32 radiance at 0xaeb460000: 0 % |
| P608, P611 | `8cb27ccf2d50c606`, `17e736204a05a105` | filter the atlas | ran | 0 % |
| P616 L67 | `2dc364df5d3278b8`, `940c2635dc1312f4` | final gather (28,800 groups; 0 groups) | ran | R11G11B10 outputs 0 % |
| P619 L67 | `34fb51b8a0312e82` | temporal resolve into D's texture | ran | 0 % (pix-world-frame.md §11.3) |

The first value lost is the trace's: the injection reads 0 because
`f22a76fcb967ee59` does not run. This is the ray-tracing path that
rt-standin.md describes. The person's decision is to run it on DXR
(real ray-tracing hardware), so it is out of this task's scope.

## 5. The bytes of the local lights (`lightchain-1`, frames 674–677)

The table gives the host copies on the GPU, at the place where each op
stands. In each frame 9 or 10 ops were probed, 4 of them dropped.

| What | Where | Bytes |
|---|---|---|
| light records (`11eeb27c2d15980d`'s and `307e048e4cdd99d4`'s `s0+216`) | placed view of 196,608 bytes at 0x95f8a0000 | 11,322 non-zero, from +0x1 to +0x73fc: records 0–57 |
| clusters, as the proxies wrote them (`11eeb27c2d15980d`'s `s0+280`, `7c90a988167650a1`'s `s0+56`) | 1,530,880 bytes at 0x95f8ef000 | 38,105 / 40,356 / 31,802 / 34,963 non-zero; first clusters `00000001 0000005f`, `00000001 0000011c`, `00000001 00000308` … |
| light proxies (P523 of 675) | indexed draws of pixel `5ad3a82b1c3cc63a` | 3 + 14, 3 + 15, 1 + 7, 5 + 1 + 20 instances |
| the masks `11eeb27c2d15980d` would write (`s1+32`, UAV) | host 160×90×13 f42, title memory 0xaf9280000 | (dropped) |
| the masks the pruning rewrites (`307e048e4cdd99d4`'s `s1+8`, UAV) | the same host texture, title memory 0xaf9280000 | 0 in all 13 slices after it (P631) |
| the shadow-offset table the pruning reads (`s1+0`) | 3,072 bytes at 0x95f707d00 | 214 non-zero: `ffff0001` (no shadow) and offsets |

## 6. What to fix

### 6.1 `11eeb27c2d15980d`: M0-relative moves over descriptor dwords

Worker lightloop (task a8d52dcc) holds this one.

- **Where:**
  - `crates/xtp-shader/src/emit/lower/valu.rs`, the arm of
    `v_movrels_b32`, `v_movreld_b32` and `v_movrelsd_b32`;
  - `emit/lower/flow.rs`, the descriptor-word arm of the joins, beside
    `UNPROVEN_DESCRIPTOR_JOINS`.
- **What:** take an undefined value for a register that holds a
  descriptor dword with no host value
  (`Val::Missing(smem::DESCRIPTOR_WORD)`) and is read only as one of the
  candidates of an M0-relative move.
  - This applies in the move's choice, and at the joins before it (here
    the loop header), which must not count such a candidate as a read.
  - Better, where it can be proven: when M0 is a first-lane read of a value
    clamped with `v_med3_u32 …, lo, hi` (here 0 and 11), stop the choice at
    the last register M0 reaches. No undefined value is needed then.
  - Otherwise, count the program as unproven, as the console-flow join
    shortcut does: a program that does read the descriptor through M0 gets
    an undefined value instead of a refusal.
- **Test:** the shape of 0x19c–0x484, an array filled from s0–s15 with
  s12–s15 a loaded descriptor, written and read through M0 in a loop whose
  index is clamped to 0–11.
  - It translates, and the DXIL validates.
  - The variant that reads the tail through M0 does not translate
    silently: it is refused, or counted as unproven.
- **Check:**
  1. `xtp translate` of the pack, on the farm or the L4:
     - `11eeb27c2d15980d` translates;
     - no program moves from translated to refused;
     - record how many other programs the change takes (refusals.md lists
       one program under "a descriptor's address or format read as data").
  2. Then one PC session with
     `XTP_FRAME_PROBE=11eeb27c2d15980d,307e048e4cdd99d4` and
     `XTP_PASS_INPUTS=1cd9bc634417a981`:
     - the pass of `bad01f53f53248e5` runs 2 of 2;
     - slice 0 of the 160×90×13 holds `0xffffffff` on tiles with no light
       and word ranges elsewhere;
     - the mask words hold the street lamps' bits;
     - the lighting has output where they light geometry.

### 6.2 The screen probes

Two tasks already hold the programs this chain needs:
- the probe rays' trace (`f22a76fcb967ee59`) and their activation
  (`39f0e9aeb156a61c`): R4, task d956084e, for "an address not from a root
  parameter" and "a descriptor chosen by control flow";
- the hit shading: the stride table, task 5d4d8f6f.

Even translated, the trace needs the BVH, which is the DXR work.

Until then, nothing downstream of the injection can be non-zero. Fixing
the passes after it (the filters, the gather, the resolve) would not
light anything.

## 7. The probe's additions (`frameprobe.rs`)

In diagnostics builds, `XTP_FRAME_PROBE` (pix-world-frame.md §11.1) gains
the following.

- **Dropped dispatches.** A dispatch of a listed program that has no host
  pipeline (its program does not translate) is probed as an op marked
  `DROPPED (no host pipeline): what it would read and write`.
  - Its tables are those its console program declares
    (`pipeline::Shader::native`), resolved from the roots the title bound
    for it.
  - The resolution is done by `draw.rs` (`State::probe_dropped`), called
    where the dispatch is dropped, direct or indirect.
  - The host copies' bytes are copied where it would have run.
- **Indirect arguments.** An indirect op of a listed program, run or
  dropped, also gets its argument records (`indirect arguments:`), read the
  same three ways as a buffer view.
- **Texture views** name the host texture's depth or layers (`160x90x13`)
  and the title address of its memory. Two programs' views of one texture
  can then be matched in one report.
- **Sizes and snapshots.**
  - `XTP_FRAME_PROBE_KIB` sets the bytes read of each range: 64 KiB by
    default, 16 MiB at most.
  - `XTP_FRAME_PROBE_MIB` sets a frame's read-back: 48 MiB by default, 512
    at most.
  - Each snapshot gives the offsets of its first and last non-zero bytes.
    When the first lies past the 512 bytes shown, the words are shown from
    it.
