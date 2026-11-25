# Ray tracing and the dark image: what the refused traces leave, and a "rays miss" stand-in

Task rtdark (558d346d), 2026-11-25. The title is BTP 6, build 1.0.2.1,
Series S mode. Programs are named by hash only (the runtime's hash, as the
logs print it); their roles come from reading the programs and the passes
around them, and are ours. Runs, logs and pass dumps stay under
`E:\xtp-data\`, none in the repository. Times are local (UTC+1).

The question: is the dark world (surfaces black outside emissive lights
and coronas; watch item 277, a black face in shade) the ray-traced
lighting's missing output? The trace programs are refused, so their
dispatches are dropped, and what they write keeps whatever was there: a
shadow mask at 0 reads "in shadow", an irradiance cache at 0 "no indirect
light".

## 1. Summary

- **Nine of the thirteen refused trace programs are dispatched in a world
  frame, and all are dropped** (frame 707 of session readmap3616-eager-a1,
  section 2): the particle collision 30 times, the other eight once or
  twice. The four traced sun-shadow programs are not dispatched at all in
  that frame: the sun's shadows come from the cascades, which run.
- **Most of what the traces write is buffers, not textures**: the
  irradiance cache's and the probes' gathers write ray records (visibility
  buffers) that later passes, which run, shade and copy into textures; the
  irradiance-cache atlas (1280×2560, one 16-bit float channel) holds **0
  over the whole texture** after the copy that fills it, because its
  gather was dropped. The local lights' shadow-mask trace had no work in
  that frame anyway (its indirect arguments read 0 groups: its requests
  come from light draws that are dropped too).
- **The trace programs are not the only holes in the lighting**: the same
  lists drop the tiled lighting's material tiles (`fea2b7d3f17b504f`,
  `bad01f53f53248e5`, `11eeb27c2d15980d`), the light proxies' draws and
  every material's ray-traced hit shading. Some of those were still being
  made in that frame (they translate); others are refused for other
  classes (refusals.md). A stand-in for the traces alone cannot be expected
  to light the world.
- **No ray-tracing switch in the title's parser metadata**: the private
  dump of its parser structures and enums holds gameplay data and a few
  quality levels (particles, lighting quality), none that turns ray tracing
  off or picks a non-RT path. "The ray-tracing builder off" in watch item
  277 was VIctory's own setting, not the title's.
- **A stand-in behind `XTP_RT_STANDIN`** (`crates/xtp-gpu/src/rt_standin.rs`,
  off by default): each dropped dispatch of the thirteen is counted and its
  outputs logged; with the switch on, its texture outputs are cleared to
  "every ray missed" where the dispatch would have run (shadow traces: 1,
  lit), others as `XTP_RT_STANDIN_FILL` says. `ab` alternates it within one
  session for an A/B (section 5).

## 2. The world frame, pass by pass

Session readmap3616-eager-a1 (`E:\xtp-data\runs\readmap3616-eager-a1`, run
log `E:\xtp-data\cache\play\20261125-032744\run.log`), `main` of 04:15,
Series S, `XTP_PASS_DUMP=0`: frame 707, 95 lists, 7,999 ops of which 1,931
were dropped. A dropped op was tied to its program by its pipeline's
address and the run log's `first bind` lines (every one resolved).

| Program | Role | Drops in frame 707 | Where (pass, list) |
|---|---|---:|---|
| `caf96a3229145fa6` | particles colliding with the structure | 30 | P367, L39 (GPU particles) |
| `6d7fe7745ab9c6d8` | screen rays' visibility buffer (reflections) | 2 | P545 L64, P668 L76 (transparent surfaces) |
| `f607780aa8e502d0` | irradiance cache: shadow rays | 2 | P569, P572 L65 |
| `3592426e3e9142d8` | probes: primary gather | 1 | P545 L64 |
| `f22a76fcb967ee59` | screen probes: rays (indirect diffuse) | 1 | P546 L64 |
| `32ed072b9f845048` | local lights: tiled traced shadows | 1 | P548 L64 |
| `36f04a4ed0ad1d9b` | probes: transparent surfaces' gather | 1 | P566 L65 |
| `b58eb3ca6ee46427` | irradiance cache: gather | 1 | P569 L65 |
| `559d9f0184e15fe3` | local lights: shadow mask trace (checkerboard) | 1 | P646 L70 |
| `a1b20bc47e3c9597`, `e8c962358d862e3f`, `17829ac71aab4e8d`, `afeea48bbb9913ad` | sun shadows traced past the cascades | 0 | not dispatched |

The lighting lists, in order (roles ours):

- **L64**: the probes' allocation and scheduling run (8 passes), then the
  screen probes' activation and ray binning run; the screen rays' own
  binning and tiling programs are dropped (being made in that frame, they
  translate), then the three trace programs above. Then the screen-space
  bent cones (writes 160×90 RG16F and 1280×720 RGBA8, both non-zero), the
  light mixer's two passes, the shadow mask's reset and clear
  (`e22a5d3d6857a2f5`, writes a buffer).
- **L65**: the irradiance cache's scheduling, its gather (dropped), the
  copy of the gather into the atlas (`c519f7a7946e3702`, runs: the atlas
  1280×2560 f54 is **0 everywhere**, "lit 0.0 %"), its shadow rays
  (dropped twice), the sky's radiance cache.
- **L66**: the shading bins, the irradiance cache's and the probes' shade
  passes, and some 80 hit-shading programs, one per material, all dropped
  (being made, or refused for an unknown stride or a scalar read as data).
- **L70**: the shadow mask's arguments (runs), its trace (dropped).

What the lighting reads from these is section 4's.

## 3. The title's own switch

The private dump of the title's parser structures (under
`E:\xtp-data\re\`, the structures and enums the title's data parser knows,
build 1.0.2.1) was searched for ray tracing, the structure, the
traces, irradiance, probes, shadows and quality settings. It holds gameplay
data, light definitions (a light's shadow flags and fade distances), and
quality levels for particles and for "lighting" (an enum whose values have
no readable names); nothing that turns the ray-traced passes off or picks
a path without them. The title also never asks the device whether it
supports ray tracing (no feature query reaches XTP's device), so there is
no capability answer to change either. Forcing a non-RT path is not
available from data.

**But the title has non-RT paths in its shader pack.** Beside each of its
structure-tracing gathers it carries a voxel variant and a "fallback"
variant (programs of the same passes, by hash; roles ours):

| Pass | Traced (refused) | Voxels | Fallback |
|---|---|---|---|
| probes, primary gather | `3592426e3e9142d8` (and `aeb93e0888250bd3` without far field) | `1a1c5c02e73751b6`, translates | `81560230fcde2b0a`, translates |
| irradiance cache, gather | `b58eb3ca6ee46427` (and `a74a0746279f960d`) | `64673ea7805c27be`, translates | none |
| transparent surfaces' probes | `36f04a4ed0ad1d9b` (and `3d121c93c7f9a057`) | `d7752c9f275803d8`, translates | `29dc558346584906` (gather and shade), translates |
| screen probes, rays | `f22a76fcb967ee59` (the one frame 707 runs, of 7 structure variants) | `05767b5d544b0c7a`, translates | `a953d5b93efe8424`, refused (wave64 lanes in a group over 64) |
| deferred gather | 4 other variants (whether they trace: not read) | `12f7da9158b1ce24`, translates | `62543c14e97e7ed7`, translates |

("Translates": the precook list of 00:03, `E:\xtp-data\precook\list-b.txt`.)
In frame 707 none of the voxel or fallback programs runs, and no
voxelization program either: the title builds no voxel scene while it
traces, so its choice of path is made once, in code, not per frame from
data. Its voxelization draws go through geometry stages (the merged ES +
GS pairs the translator does not attempt, refusals.md), so the voxel path
would likely stop there; the fallback path needs neither the structure nor
voxels. Finding the title's choice in its code and forcing "fallback" is a
follow-up (section 7); until then the stand-in of section 5.

## 4. What the lighting reads from the traces

From frame 707's dump (the dropped ops' outputs are not in it; the
programs that run around them are):

- **The irradiance cache.** Its gather (`b58eb3ca6ee46427`) writes ray
  records into buffers; the copy that runs after it (`c519f7a7946e3702`)
  reads five buffers (up to 12 MiB) and the atlas, and writes the atlas
  (1280×2560, f54) and four buffers. The atlas is 0 over the whole texture
  after it. Then the cache's shading bins and shade pass run (L66) over
  those records, and its shadow rays (`f607780aa8e502d0`) are dropped.
- **The probes.** Their gather (`3592426e3e9142d8`) is dropped; their copy
  and shading bins run in L66 over what it would have written.
- **The screen probes.** Their rays (`f22a76fcb967ee59`) are dropped
  between the ray binning (runs) and the bent cones (run, 160×90 and
  1280×720 outputs non-zero: they are screen-space, not traced).
- **The local lights' shadow mask.** What the frame clears before the trace
  is a 52-byte buffer of counters (`e22a5d3d6857a2f5`), and the arguments
  of the trace's indirect dispatch, made from it (`495c6013589afd60`),
  read 0 × 1 × 1 groups in frame 707: the trace would have run no group
  that frame. Its requests come from the light proxies' draws, dropped in
  the same list (L64).
- **The sun.** The traced distant shadows are not dispatched; the cascades
  draw (L24).

The gathers' outputs are visibility buffers: per ray, what it hit, shaded
afterwards by the hit-shading programs of each material (L66, dropped in
frame 707) or, for rays that hit nothing, by the sky's fallback programs.
A ray that "missed" is therefore a record the gathers write, in a layout of
the title's; a clear of the whole buffer to a value that reads "missed"
would send every ray to the sky's fallback, which runs. Which value that is
has not been read from the programs yet (section 7). What each dropped
trace's outputs are bound to at the dispatch is what `rt_standin` logs
(`rt: dropped trace, output` lines), and what the session of section 5
records.

## 5. The stand-in, A/B

`XTP_RT_STANDIN=ab:30:5` in one Series S session on the PC, with
`XTP_TEXTURE_POOL=1`, four world frames captured with their pass dumps:
not run at this writing; its results go to the task and to this section in
a follow-up. Without fills for buffers, the stand-in clears only the
shadow traces' texture outputs, and section 4 says most outputs are
buffers: phase B is expected to differ little from A until
`XTP_RT_STANDIN_FILL` names the gathers' visibility buffers with their
"missed" value.

## 6. Method and limits

- Roles are read from what each program reads and writes and from the
  passes around it, not from the title's names, which stay out of the
  repository.
- A pass dump puts a dropped op in the list its thread last recorded for
  (passdump.rs), so a dropped op's pass is the right list and neighbours;
  its outputs come from its program's resource list
  (`rt_standin::Program::writes`) resolved against the title's tables at
  the dispatch, not from the dump.
- A clear writes the whole view; a trace may write only part of it. Buffers
  are left alone by default: what "missed" means in a buffer is the
  program's layout.

## 7. What comes next

- **The A/B of section 5** (queued as `rtdark-ab1`), then section 4's
  outputs from its `rt:` lines.
- **The "missed" value of the gathers' visibility buffers**, read from the
  gathers' and the copies' code, then an A/B with `XTP_RT_STANDIN_FILL`
  naming them (no rebuild needed).
- **The title's own choice of path** (section 3): where its code picks the
  structure, the voxels or the fallback for its indirect light, and a
  title-specific switch that forces the fallback, whose programs translate.
