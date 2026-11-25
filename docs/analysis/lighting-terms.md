# Lighting terms: what the deferred lighting is given, term by term

Task lightterms (42c20033), 2026-11-25, on the person's PC (RTX 4080
SUPER, Series S mode). Follow-up of image-status.md: after the deferred
lighting the scene colour is non-zero on 6.7 % of the screen (frame 621),
or 0 % (frame 624), while the material resolve left it non-zero on 64 to
84 %. This file splits the lighting pixel's output into its terms, measures
each one and names their producers. Programs are named by hash only. Dumps,
probe files and captures stay under `E:\xtp-data\`; none is in the
repository.

## 1. Verdict

- **The street lamps are switched off by a ray-traced shadow that does not
  run** (§4.4).
  - The lamp loop multiplies each lamp of one light type by a shadow test
    against a per-light distance `t` in `s0+80`.
  - `s0+80` is the output of `32ed072b9f845048`, the local lights' RT
    shadow trace. XTP drops that program (refused, "an address not from a
    root parameter"), so `t` stays 0.
  - The title writes `t + 0.04` where a shadow falls and FLT_MAX elsewhere
    (from rtsplice2's reading of the program, task 880a2519). With
    `t = 0`, the lamp lights only what lies behind it along its axis:
    nothing it should light.
  - The lamps' other two shadow inputs, `s0+968` and `s0+1160`, are outputs
    of `559d9f0184e15fe3`, the local-light shadow-ray trace, dropped too.
  - **The fix is the RT activation of those two programs**, in that order.
    It is not a stand-in.
- **The directional light (the moon) has no colour** (§4.3), in every frame
  of every session probed: 17 frames of 6 sessions, the dusk shot
  included.
  - The record's colour `s1+0` +300 is 0. The title's CPU writes it into
    its constant ring.
  - The scale `s0+232` +84 is 0. The GPU computes it from a constant the
    CPU writes as 0 (`cb[5].z` of `fb9c7f430da14c4c`).
  - The direction is always the same, and the whole directional shadow
    chain is never dispatched (task lightsrc, d012488d). That fits a light
    the title switched off.
  - **The scene is at night, so that may be legitimate.** Task lightsrc
    looks for the CPU writer.
- **The indirect light `L` is 0** because no probe ray is traced
  (light-chain.md §4, the DXR work). It is the only ambient term this
  program has: it reads no sky SH of its own (§4.6).
- **"2 of 13 slices" is not a fault.** The tile light lists hold one range
  word and 12 mask words of 32 lights each.
  - The scene uses lights 0 to 96 (§7), so slices 1 to 4 can hold bits and
    5 to 12 stay empty.
  - Frames 621 and 624 fill slices 0 to 4. Frame 618 filled only 0 and 1.
- **The three full-screen inputs at 0 are secondary** (§5). `s0+760`,
  `s0+808` and `s0+1280` have no writer:
  - none in the frame;
  - none among the dropped programs probed.

  They feed only a directional-light block that the zero colour multiplies
  anyway.
- **Frames 618 and 624 are the material-class plane** (twin task 7873b786,
  §6):
  - in 618 the lighting writes nothing;
  - in 624 it writes 0 everywhere.

## 2. Sources

- **The archived sessions' probes and dumps**, all with
  `XTP_FRAME_PROBE` on `1cd9bc634417a981` (pix-world-frame.md §11.1):
  - `imgsrc-1` and `imgsrc-2` (frames 674–676, 661–663);
  - `staging2-2` (587, 592, 597);
  - `imagecheck-3` (597, 602, 607);
  - `imagecheck-4` (617, 620, 623; pass dumps 618, 621, 624, with
    `XTP_PASS_INPUTS=1cd9bc634417a981`).
- **The lighting pixel's translation**: PSO 20525 of the PIX capture
  `world-1` (`E:\xtp-data\pix\world-1\pso\20525-ps.txt`, `dxc -dumpbin`
  text, 14,911 lines, 211 blocks).
  - A data-flow reader (session scratch, outside the repository) names
    each load by its table slot, as the probe does: `sT+N` is table `T`'s
    dword `N`, at heap index `(user data + 4N) >> 5`.
  - It expands a value into the loads it depends on. Block numbers below
    are that file's.
- **`fb9c7f430da14c4c`'s translation**: PSO 19091 of the same capture
  (`xtp-pix pso`, then `dxc -dumpbin`).
- **Sessions `lightterms-1` and `lightterms-2`** on this branch (§7).

## 3. The lighting pass and its program

The pass (P605 in frame 621) is 232 indirect rectangle draws:
- the vertex program is `25ebc92075b62822`;
- there are 182 pixel programs, one per material class and light type;
- depth is tested EQUAL against the material-class plane, blending off
  (pix-world-frame.md §5.1).

Of the 182, 180 read the light record `s1+0` and the buffer `s0+232` at
the same addresses (frame 621's ops).

`1cd9bc634417a981` is one of them. Its output:

```
RGB = e × (D' × albedo + S)
D   = L × (1 + k) + X0 + Σ lamps
D'  = D mixed toward its luminance by a saturation factor (s0+1424 reg 30.w)
```

| Term | What | Read from |
|---|---|---|
| `e` | exposure | `s0+168` word 1 |
| `albedo` | G-buffer base colour | `s0+584` |
| `L` | screen-probe indirect light | `s0+1008` (R9G9B9E5 view), scaled by `s0+168` word 0 |
| `k` | sky-occlusion weight on `L` | `s1+72` (640×360 R16, gathered), `s0+1240`, depth |
| `X0` | directional light, before the lamp loop (§4.3) | colour `s1+0` +300 × `s0+232` +84 × ramp `s0+1224`; direction `s1+0` +272 |
| `Σ lamps` | the tile's lights (§4.4) | lists `s1+80`, records `s0+216`, shadow slots `s1+24`, shadows `s0+968`, `s0+1160`, `s0+80`, shadow maps picked by index |
| `S` | specular (§4.5) | indirect `s0+456` × BRDF table `s0+1056` × `s0+752`; the directional light's; the lamps' |

## 4. The terms, measured

### 4.1 Exposure `e`

`s0+168` = [4.33, 0.231, 8, 0.125] in frame 620. The program multiplies by
word 1:

| Frame (`imagecheck-4`) | 617 | 620 | 623 |
|---|---|---|---|
| `e` | 0.197 | 0.231 | 0.269 |

It is small but not 0. It follows the missing light (image-status.md
§5.7).

### 4.2 Indirect light `L`

`L` is 0 in every frame (image-status.md §3, light-chain.md §4): the
probe rays are not traced. `k` only scales `L`.

### 4.3 The directional light

Two blocks compute it before the lamp loop. Both multiply everything they
add by the light's colour.

- **Diffuse** (block 5787):
  `X0 += s0+232[+84] × s1+0[+300..308] × ramp(s0+1224) × V/π × saturate(N·L) × (0.02 + 0.98 × …)`.
  - It runs where `V > 0`.
  - `V` is a visibility: `s1+72` (640×360 R16, 99 % non-zero), gathered,
    times a cloud term (`s0+672`, 512×512 BC1, and `s0+1240`).
- **Specular and transmission** (blocks 4175–5150):
  - they read the same colour, the BRDF table `s0+800`, `s0+576`
    (128×128 RGBA16F) and `s0+808`;
  - they run where a depth-weighted, 13-tap filter of `s0+760` is above 0,
    with `s0+1424` reg 3.x on (§5).

**The light record** `s1+0` is 340 bytes:
- in the title's constant ring (allocation `0x12f50000`, 182 MB; mapped,
  read-write), at a new address each frame;
- no op writes it through a UAV;
- the host copy equals title memory at record and at submit.

In all probed frames:

| Bytes | Value |
|---|---|
| +272 (direction), +288 again | (0.5522, −0.53, −0.6436) |
| +300 (colour, w) | **(0, 0, 0, 0)** |
| +328 | (1, 1, 1) |
| +0 | an identity matrix |

**The scale** `s0+232` is 384 bytes at `0x992738370`. The compute
`fb9c7f430da14c4c` writes it (one 1×1×1 dispatch, P259 of frame 621),
reading:
- the exposure;
- a CPU block of 2,044 bytes;
- a 128×128 RGBA16F sky texture;
- its constant view `s1+16`, 128 bytes the CPU writes in the same ring.

It writes the buffer as an array of float3:

| Element (bytes) | What it stores | Value (frame 620 or 622) |
|---|---|---|
| 0–6 (0–83) | sums over the sky texture: SH, R < G < B | 0.001 to 0.010 |
| **7 (84–95)** | **`cb[5].x × cb[5].z`** | **0** |
| 11 (132–143) | a CPU word × (a CPU word × exposure × the sky texel + …) | (0.0031, 0.0039, 0.0056) |
| 12 (144–155) | a loop's result | (0.41, 0.52, 0.75) |
| 13 (156–167) | `cb[7].w` | 0.1 |
| 15 (180–191) | `cb[5].x × cb[5].w` | 0 |
| 17 (204–215) | `cb[5].y × (…)` | (0.0031, 0.0039, 0.0056) |
| 20 (240–251) | `cb[4].w` | 0 |

`lightterms-2` read the constant view (frame 622): `cb[5]` =
(0.1, 1, 0, 0). **The zero is `cb[5].z`, byte 88**; `.w` is 0 too. The
lighting program reads only element 7.

| Session | Frames | `s1+0` +300 | `s0+232` +84 |
|---|---|---|---|
| `imgsrc-1` | 674–676 | 0 | 0 |
| `imgsrc-2` | 661–663 | 0 | 0 |
| `staging2-2` | 587, 592, 597 | 0 | (not read) |
| `imagecheck-3` | 597, 602 (dusk), 607 | 0 | (not read) |
| `imagecheck-4` | 617, 620, 623 | 0 | 0 |
| `lightterms-2` | 622, 627 | 0 | 0 (`cb[5].z` = 0) |

**Both zero factors are values the title's CPU writes.** Evidence for and
against the light being off on purpose:
- the scene is at night;
- the direction never changes;
- the directional shadow chain is never dispatched (classification,
  cascade resolve, RT directional trace; task lightsrc);
- the tiled lighting's per-tile reveal texture is 0. `5741294cf872ffb9`
  writes an 80×45 R8 tile mask (0 %), `f11c3ddeaeb7d602` reduces it to the
  10×6 R8 (0 %), and the rectangles' vertex program reads that as `s1+64`.
  The directional variant therefore gets no rectangle, as in VIctory's
  findings;
- but the dusk shot has it off too.

Task lightsrc (d012488d) looks for the CPU code that writes the record and
`cb[5]`.

### 4.4 The lamps

**The lists** (light-chain.md §3.2):
- slice 0 of the 160×90×13 texture is the range of mask words, or
  `0xffffffff` for a tile with no light;
- slices 1 to 12 are mask words of 32 lights each, 384 lights at most.

| Frame (`imagecheck-4`) | Slice 0 | 1 | 2 | 3 | 4 | 5–12 |
|---|---|---|---|---|---|---|
| 618 | 100 % (all `0xffffffff`) | 100 % | 0 | 0 | 0 | 0 |
| 621 | 39.1 % | 94.0 % | 5.0 % | 8.1 % | 6.7 % | 0 |
| 624 | 44.8 % | 93.6 % | 3.9 % | 9.5 % | 6.5 % | 0 |

These are the non-zero shares of the pruning's output, just before the
lighting (`XTP_PASS_INPUTS`).
- `lightterms-2` read the whole record array (`s0+216`, 384 × 512 bytes):
  the last non-zero record is 96. Slices 5 to 12 (lights 128 to 383) have
  nothing to hold.
- In frame 621, slice 4's largest word is 0x18018: lights 99, 100, 111
  and 112.
- The lists are filled: the light loop is translated (440d7e4), and its
  inputs are full (light-chain.md §5).

**The shadow test** (block 10658). It is entered for lights whose record
word +24 has bits 31–30 equal to 1. Record 0 is such a light in frame 620
(`0x4401b3a1`), and not in 617 or 623 (`0x0021bc00`).

```
d      = dot(a, P_light − P_pixel)       a: a per-light vector; P_light: record +0..+8
shadow = (−d ≤ s0+80[light].x) ? 1 : 0   and the light is in the tile's list,
                                          and its visibility below is not 0
lamp  ×= shadow × attenuation × visibility(s0+968, s0+1160)
```

- **`s0+80` is `32ed072b9f845048`'s output.** In `lightterms-2`'s
  frame 622, its UAV `s1+16` and the lighting's `s0+80` are the same
  resource: a 3,072-byte buffer, one 8-byte entry per light.
  - It is 0 at record, at submit and on the GPU.
  - It is 0 in `imagecheck-4` too.
  - The program is dropped ("an address not from a root parameter", W12).
- **With `t` = FLT_MAX the test always passes**: nothing is in shadow.
  The title writes FLT_MAX where no shadow falls, and `t + 0.04` where one
  does (rtsplice2's reading of the program).
- **With `t` = 0 the lamp lights only pixels with
  `dot(a, P_pixel − P_light) ≤ 0`**: behind it along its axis, not what it
  shines on.
- **The visibility comes from the tile's value in `s0+968`**, per light
  and per tile (7680×90, the light's shadow slot `s1+24` × 160 tiles):
  - a value up to 255 is divided by 255 or 15;
  - above 255, a per-pixel mask is read in the atlas `s0+1160`
    (2048×1824 R8_UINT).

  `lightterms-2`'s probe shows both are UAVs of `559d9f0184e15fe3`, the
  local-light shadow-ray trace, dropped too:
  - its `s1+96` (7680×90 R32_UINT, `0xaf7f40000`) is `s0+968`;
  - its `s1+80` (2048×1824 R8_UINT, `0xaf8700000`) is `s0+1160`.

  What reaches the lighting is what the classification `0a5ecfee6b79e13b`
  / `5cf00dc39042edaf` wrote. The denoise `0ea6fac38d096770` runs with 0
  groups, because the trace's counter stays 0.

So the lamps light only where their type skips the test, or where a pixel
happens to fail `d > 0`. That is the 6.7 % of frame 621, up to
(4.0, 7.1, 7.7). The reference's lit roadway needs both traces.

### 4.5 Specular `S`

`S` = the indirect specular + the directional light's specular + the
lamps':
- the indirect specular reads `s0+456`, the other R9G9B9E5 view of the
  probe chain: RGB at most 0.0011 to 0.0018 (image-status.md §3);
- the directional light's specular has the zero colour of §4.3;
- the lamps' specular has the shadow factor of §4.4.

### 4.6 Ambient and sky

This program has no ambient term of its own. Outside the lamps and the
directional light, its only light is `L`, the screen probes' indirect
light, whose rays would bring the sky in.
- The sky SH that `fb9c7f430da14c4c` computes (`s0+232` elements 0–6) is
  not read by this program: of that buffer it reads only element 7.
- The reference's teal ambient therefore reaches this program through `L`
  (the DXR work), and nowhere else.

## 5. The three full-screen inputs at 0

Read by `1cd9bc634417a981` and written by nothing in frames 618, 621, 624
or 622: no render target, depth or UAV at their host textures or title
addresses. None of these dropped programs has a UAV at those addresses
either (`lightterms-2`):
- `559d9f0184e15fe3`, `32ed072b9f845048`, `3592426e3e9142d8`;
- `aab1b3c7248ee5de`, `36f04a4ed0ad1d9b`, `b58eb3ca6ee46427`;
- `f607780aa8e502d0`, `82a2ad6d07417d36`, `90a0692af4320129`.

| Slot | Format | Size | Title memory | What the program does with it |
|---|---|---|---|---|
| `s0+760` | R8_UNORM | 1280×720 | `0xad6e80000` | the value squared, or a 13-tap depth-weighted filter of it when the tile's flag bit 14 (`s0+32`) is set; above 0, it enables §4.3's specular and transmission block |
| `s0+808` | RGBA8_UNORM | 1280×720 | `0xb31c40000` | sampled once in that block, as a colour (transmission) |
| `s0+1280` | R8_UNORM | 1280×720 | `0xaf9190000` | loaded at the pixel, `max()` with another value, then `lerp(1, s1+0[+328], …)` in that block |

The addresses are the same in `imagecheck-4` and `lightterms-2`. These
inputs belong to the directional light, so they wait on §4.3. Task
lightsrc reads `s0+808`'s memory as reused after the lighting, a transient
target.

## 6. Frames 618 and 624: the material-class plane

| Frame | Material resolve (scene colour non-zero, max) | Lighting (non-zero, max) | Plane R24G8 non-zero: after the resolve → after the lighting |
|---|---|---|---|
| 618 | 57.1 %, 0.011 | 57.1 %, 0.011 (unchanged) | 73.8 % → 100 % |
| 621 | 84.2 %, 10.8 | 6.7 %, 7.7 | 58.8 % → 66.7 % |
| 624 | 64.4 %, 0.24 | 0.0 %, 0 | 91.7 % → 68.5 % |

- **Frame 618: no rectangle passed the EQUAL test.** The resolve's colour
  is left as it was.
- **Frame 624: the rectangles covered the screen and wrote 0**, the lamps'
  tiles included.

The plane changes between the resolve and the lighting in both frames.
That is twin task 7873b786's subject (the plane redrawn only at list
close), not a missing light source.

## 7. Sessions `lightterms-1` and `lightterms-2`

Both used `queue-session.ps1` with imagecheck-4's switches:
`XTP_STRIDE_TABLE=1 XTP_RUN_TIME_VIEWS=1 XTP_TEXTURE_POOL=1`,
`XTP_PASS_DUMP=0`, `XTP_PASS_INPUTS=1cd9bc634417a981`, and the probe
added here (§9).

- **`lightterms-1`** (`-Measure 60 -Limit 300`) measured nothing.
  - Its precook took 1,360 s: a new translation key, 26,503 records and
    80,806 pipelines.
  - The title then wrote three log lines, the last "copy-back map
    loaded", and was stopped at 300 s.
  - It is image-status.md §6.3's shader-cache scan after a large precook,
    seen a second time.
- **`lightterms-2`** (`-Measure 90 -Limit 420`) reached gameplay after
  36 s, at 0.5 fps.
  - Switches: `XTP_CAPTURE_WORLD=1000:22:4:5`;
    `XTP_FRAME_PROBE` on the lighting pixel, `fb9c7f430da14c4c` and nine
    dropped programs; `XTP_FRAME_PROBE_KIB=192`;
    `XTP_DIAG_DIR_LIGHT=0,0,0/0.1,0.12,0.2/1,1.2,2/8,9.6,16` with
    `XTP_DIAG_DIR_LIGHT_FRAMES=5`.
  - Frames 622 and 627 were captured, the night street. Probes 622 and
    627; pass dumps 622 (a partial frame), 623 (a cut with an empty scene
    colour) and 628 (no lighting pass).
  - Results: §4.3's `cb[5]`, §4.4's light count and shadow buffers, §5's
    dropped programs.
- **The `XTP_DIAG_DIR_LIGHT` A/B shows nothing yet.**
  - The writes went in at every frame: the log has one line per frame,
    with the colour and both writes done.
  - The two captured frames had the colours 0 (622) and (0.1, 0.12, 0.2)
    (627). Both show the same night street with a black roadway; a cold
    moon of 0.1 is far below what `e × albedo / π` would make visible.
  - The strong colours fell on frames 610 to 618, before the captures
    armed.
  - Whether the ramp `s0+1224` (16×1, written by `a5b1c338a9607317`, not
    read by the probe) is non-zero is therefore not known.
  - A session that captures frames 630–639 (`XTP_CAPTURE_WORLD` with a
    later start, or a single strong colour) would show it.
- **Files**:
  - captures, probes and dumps: `E:\xtp-data\pix\lightterms-2\`;
  - session files: `E:\xtp-data\runs\stall-lightterms-{1,2}\`;
  - title logs: `E:\xtp-data\cache\play\20261125-103248\` and
    `20261125-110942\`.

## 8. What to fix

1. **The lamps** (§4.4): the RT activation of `32ed072b9f845048`
   (rtsplice2, task 880a2519, its walk and its place in SITES), then of
   `559d9f0184e15fe3`.
   - Check: one session with `XTP_FRAME_PROBE=1cd9bc634417a981`.
     `s0+80` must hold FLT_MAX or `t + 0.04` per light, and the lighting
     output must be non-zero beyond the 6.7 % of frame 621, on the
     roadway under the lamps.
   - No stand-in for `t`: the person's decision, nothing provisional.
2. **The directional light's colour** (§4.3, task lightsrc d012488d).
   - Find the CPU code that writes the light record's colour (`s1+0` +300)
     and `cb[5].z` of `fb9c7f430da14c4c`'s view `s1+16`, and why both are
     0.
   - At night that may be the title's intent. If so, nothing is to fix
     there.
3. **The indirect light `L`**: the probe rays' trace and the BVH, the RT
   tasks (light-chain.md §6.2). It is the ambient this program has.
4. **The material-class plane** (§6, task 7873b786): the frames where the
   rectangles miss (618) or cover the lamps' tiles with 0 (624).
5. **The three full-screen inputs** (§5): nothing to do while the
   directional light is off.

## 9. Diagnostics added (`frameprobe.rs`)

- **Constant views.** `XTP_FRAME_PROBE` now reads constant views as it
  reads buffer views:
  - title memory at record and at submit, and the host copy on the GPU
    just before the op;
  - all of their bytes shown, up to 4 KiB.

  Before this, it printed only their address. The colours, directions and
  feature switches above are in such views (`cb[5]` of §4.3).
- **`XTP_DIAG_DIR_LIGHT=<r>,<g>,<b>[,<scale>][/…]`** (diagnostics builds
  only; off by default, and never meant to be on by default).
  - Just before each op whose stage reads the light record (`s1+0`,
    340 bytes) and the scale buffer (`s0+232`, 384 bytes), it writes a
    colour of ours at the record's bytes 300–311 and the scale (1 unless
    given) at byte 84.
  - The writes go into their host copies, on the GPU at that place in the
    list (`WriteBufferImmediate`). Title memory is not written. The next
    refresh, or the scale's writer, puts the title's values back.
  - Several colours take turns, `XTP_DIAG_DIR_LIGHT_FRAMES` frames each
    (4 by default), so that one session gives an A/B. The log names each
    frame's colour.
  - It only shows what the image would look like with a directional
    light. It fixes nothing.
