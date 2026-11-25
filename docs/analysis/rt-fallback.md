# The title's choice between ray tracing and its fallback, and how to force it

Task rtfallback (d5499893), 2026-11-25. The title is BTP 6, build 1.0.2.1.
Static analysis of its executable only (Ghidra 12.2 headless on the cloud L4
VM, raw-byte queries on the same machine): RVAs (image base 0x140000000),
pass numbers and behaviour in our own words, no code. Programs are named by
hash (the runtime's 16-digit hash, as the logs print it), their roles from
rt-standin.md and refusals.md. The queries and their outputs stay outside
the repository, under `E:\xtp-data\re\` (section 6).

The question (rt-standin.md §3): the pack carries non-ray-traced variants
of the gathers that leave the irradiance cache and the probes at 0, and
they translate, but none runs; the title's data has no switch for it.
Where does the code pick the traced path, and can XTP make it pick the
other?

## 1. Summary

- **One predicate decides, everywhere: RVA `0x4fd8550`, "is ray-tracing
  scene `i` ready".** It reads only the title's own state: its ray-tracing
  manager (the pointer at RVA `0x8527dc8`), the manager's scene `i`, and
  whether that scene has something to trace (section 2). It asks neither
  the system nor the device: there is no capability XTP could answer "no"
  to, which is why the device never sees a feature query (rt-standin.md
  §3).
- **The probe system takes its fallback when the predicate says no.** Its
  primary gather runs pass 17 of its technique, the fallback gather
  `81560230fcde2b0a` (translates), instead of binding the structure and
  running pass 10 or 11 (`aeb93e0888250bd3`, `3592426e3e9142d8`, refused);
  the transparent surfaces' probes run pass 73, the fallback gather and
  shade `29dc558346584906` (translates), instead of pass 68 or 69
  (`3d121c93c7f9a057`, `36f04a4ed0ad1d9b`). The irradiance cache has no
  fallback: without the predicate its gather (pass 42 or 43) and its
  shadow rays (pass 61 or 62) are not dispatched at all (section 3).
- **The voxel variants are not this choice.** None of the probe system's
  16 dispatch steps selects the voxel passes (16, 46, 71) on either side
  of the predicate: the voxel path, if the title still uses it, is chosen
  elsewhere, and forcing "no ray tracing" does not reach it.
- **46 call sites ask the same predicate** (45 calls, one tail jump): the
  indirect diffuse (screen probes) system, the probe system, the lighting
  functions after them, and about twenty others (section 4). Making the
  predicate answer "no" switches each of them to its non-traced branch at
  once, which is what the task asks; what each of the others does on that
  branch is section 4's open list.
- **The switch**: `XTP_RT_PATH`, off by default (section 5). It makes the
  predicate and its twin (`0x4fd8570`, the same test on a given manager)
  answer 0 by a title-specific patch by RVA in the per-build hooks of
  `crates/xtp-game` (`rt_path.rs`). `fallback` does so for a whole run;
  `ab` alternates it with the title's own answer within one run. The A/B
  of the irradiance and probe buffers and of the world's luminance comes
  next (section 7).

## 2. The predicate

`0x4fd8550(i)` returns a byte, true when:

1. the ray-tracing manager exists (the 8-byte pointer at RVA `0x8527dc8`
   is not null), and
2. its scene `i` exists (entry `i` of the 8-byte table the manager points
   to at +0x08), and
3. either one of the manager's two counts at +0x9c and +0xac is not 0, or
   the object at +0xd8 exists and its word at +0x114 is not 0.

Every caller read so far passes `i = 0`.

**Its twin, `0x4fd8570(manager, i)`**, makes the same test (points 2 and
3) on the manager it is given. The wrappers that bind the structure for a
pass ask both (`0x4fd8630`, `0x4fd86a0`, `0x4fd8c10`: first `0x4fd8550`,
then `0x4fd8570` on the manager at `0x8527dc8`); the binding of the probe
system's structure (`0x200e810`) asks the twin alone, for scene
`1 + far`, where `far` is the byte at RVA `0x8527e02` (read by
`0x5154180`): the "far field" variant of each traced pass is chosen by the
same byte (pass `base + far`). Thirteen other call sites ask the twin
directly (`0x049be10`, `0x04e7570`, `0x06b6bd0`, `0x06b6ef0` two,
`0x15ccdc0`, `0x200e810`, `0x326d970`, `0x3598fe0`, `0x4fd8660`,
`0x4fd8740`, `0x4fd8c50`, `0x5154190`, `0x5374ff0`); the last asks it
about another manager (the pointer at RVA `0x84d5f40`), then hands over
to the predicate by a tail jump, for the scene `0x5154160` names. No
copy of the test made in line was found among the callers read.

## 3. The probe system's dispatch steps

The probe system's effect is loaded by name at start-up (`0x5708f50`), its
pointer kept at RVA `0x876c688` and its technique's index at `0x876c690`.
Each step binds a program as the effect's pass `first + k`, `first` read
from the technique's entry (the rule of title-cpu-image.md §2.3); in this
effect pass `k` is the effect's `k`-th program record, which every pass
number below confirms against the programs' roles. The steps are the 16
functions from `0x35744e0` to `0x3581fb0`; those that ask the predicate:

| Step | Predicate true | Predicate false |
|---|---|---|
| `0x35744e0`, primary probes' gather | binds the structure (`0x200e810`), pass 10 + far: `aeb93e0888250bd3` / `3592426e3e9142d8` | pass 17: `81560230fcde2b0a` |
| `0x35797d0`, irradiance cache's gather, then its shadow rays | the structure, pass 42 + far (`a74a0746279f960d` / `b58eb3ca6ee46427`); pass 61 + far (`5f13c9550dc4ab39` / `f607780aa8e502d0`) | neither dispatched |
| `0x357a8f0`, irradiance cache's shadow rays (second site) | pass 61 + far | not dispatched |
| `0x3581760`, transparent surfaces' probes | pass 68 + far: `3d121c93c7f9a057` / `36f04a4ed0ad1d9b` | pass 73: `29dc558346584906` |
| `0x3574bf0`, `0x357acb0` | move the traced passes' textures into the state those passes need | skip that |
| `0x3575930` | binds the structure (`0x5154190`) and the traced passes' inputs | skips them |
| `0x3580e20`, `0x3581fb0` (both also want the byte at `0x876aa10` to be 0) | the transparent surfaces' traced passes and their inputs | return before them |

The transparent surfaces' step also takes its fallback when the byte at
RVA `0x876aa10` is 1, whatever the predicate; one function writes it
(`0x0cd3b40`), not read yet.

## 4. The other callers

Call sites of `0x4fd8550`, by the `.pdata` function holding them:

- the indirect diffuse (screen probes) system, whose effect is at RVA
  `0x876a740` (loaded by `0x0cc4b00`): `0x3567d20`, `0x3568230`,
  `0x356b6b0` (two), `0x356bd00`, `0x356f2b0`;
- the probe system (section 3) and the steps after it: `0x35744e0`,
  `0x3574bf0`, `0x3575930`, `0x35797d0` (two), `0x357a8f0`, `0x357acb0`,
  `0x357b990`, `0x3580e20`, `0x3581760`, `0x3581fb0`, `0x3582a00`,
  `0x3583000`, `0x3584170`, `0x358df90`, `0x35915a0` (two), `0x3598fe0`,
  `0x359a320` (two);
- elsewhere: `0x027f8b0`, `0x027fec0`, `0x04e69f0`, `0x06b6cd0`,
  `0x06b6d50`, `0x06b6eb0` (two), `0x0a52d90`, `0x0ad53a0`, `0x0c4ce60`,
  `0x15b4ab0`, `0x16c8260`, `0x202f8d0`, `0x4fd82b0`, `0x4fd8630`,
  `0x4fd86a0`, `0x4fd8c10`, `0x5374ff0` (a call and a tail jump),
  `0x55798b0`.

What the screen probes do without the predicate is the first thing to
read: their fallback rays (`a953d5b93efe8424`) are refused (wave64 lanes
in a group over 64, rt-standin.md §3), so that hole would stay.

## 5. Forcing the fallback: `XTP_RT_PATH`

The cleanest switch the code allows: the entries of the predicate and of
its twin made to return 0 ("no scene ready"), so that every caller takes
the branch it takes before the title's ray-tracing scene exists. Nothing
is faked on the GPU side: the structure is still built (its builder's
dispatches are dropped as now), nobody asks for it. Code that reads the
manager's fields itself, without either function (the manager's own
functions, among the 37 references to its pointer), is not affected.

Written by task rthook (65fa56c1), 2026-11-25: `crates/xtp-game/src/rt_path.rs`,
applied from `apps/xtp/src/run.rs` once the image is mapped, before any
title code runs (next to the watchdog and assertion hooks).

- **Values**:
  - unset or `bvh`: nothing is written, the title answers itself;
  - `fallback`: both functions answer 0 for the whole run;
  - `ab`, `ab:<phase s>` or `ab:<phase s>:<settle s>`: the A/B clock of
    `xtp_core::ab` alternates the two. Phase A (`XTP_RT_PATH=0` in the
    A/B lines) is the title's own answer, phase B (`=1`) the fallback. It
    shares the phases of any other switch set to `ab` (`XTP_RT_STANDIN`,
    for one).
- **What is written**: each function's first two bytes become a short
  jump to the function's own "return 0". For the predicate that is
  `0x4fd8562`, the path it takes without a manager; for the twin,
  `0x4fd8581`, the path without scene `i`. Nothing else is written.
- **Why two bytes and not four**: the predicate's first instruction is two
  bytes long, the twin's four, and both functions are leaves (no frame, no
  unwind data). A jump that covers no more than the first instruction is
  one aligned, atomic two-byte store. While title threads run:
  - no thread is ever inside a written byte;
  - a thread that took the jump is in the title's "return 0", which is
    never written, so taking the jump out cannot strand it.

  A "return 0" of our own written over four bytes (two instructions) would
  break the second point. A thread stopped between those two instructions
  would resume, once the patch is taken out, in the middle of the
  predicate's second instruction. So `ab` can flip the path in a running
  session.
- **Checks before writing**: the build's BLAKE3, then an FNV-1a hash of
  each function's bytes from its entry through its "return 0" (0x15 and
  0x14 bytes). A mismatch writes nothing. The title's bytes stay outside
  the repository. The opt-in test `rt_path::tests::sites_in_the_sample_game`
  (`XTP_SAMPLE_GAME=<title folder>`) checks the hashes against a dump.
- **In `ab`**: a thread of ours (`xtp-rt-path`) reads the clock every 5 ms
  and flips both entries after the present that ends a phase. That is not
  a frame boundary of the title, so the frame across a flip can see both
  answers. The phase's settling time (3 s by default) leaves that frame
  out of the measurements.
- **Log lines**:
  - `ray-tracing scene predicate answers no (XTP_RT_PATH=fallback)`;
  - `ray-tracing path (XTP_RT_PATH=ab): phase A …, phase B …`, then
    `ray-tracing scene predicate: <path> (A/B arm <A|B>)` at each flip;
  - a refusal says why: `ray-tracing path not switched (XTP_RT_PATH): …`
    (unknown build, bytes that are not the expected ones).
- **Expected**: in the probe and transparent probe passes, the fallback
  programs in the "first bind" lines and no drop of `3592426e3e9142d8`,
  `36f04a4ed0ad1d9b`, `b58eb3ca6ee46427` or `f607780aa8e502d0`; the
  irradiance cache's atlas stays 0 (no gather), so whether the light
  mixers (`ab619d0b7b7684d2`, `6a62009bc6a4146e`) still read it on that
  branch decides what the image gains.

## 6. Method and material

- **Strings and references**: the effects' names are plain strings in
  `.rdata`, each referenced once, by its system's start-up function; a
  RIP-relative reference scan of the code then gives every function using
  the effect's pointer (40 sites for the probe system).
- **Decompilation**: Ghidra 12.2 headless on a project imported without
  analysis (title-cpu-image.md §5), on the L4 VM at idle priority.
- **Queries**: `E:\xtp-data\re\scripts\ghidra\ReaFind.java` (strings with
  their references, references, direct callers, immediates) and
  `ReaQuery.java` (decompilation), driven from the PC by
  `E:\xtp-data\re\out\rtfallback\start-find.ps1` and `start-q.ps1`;
  outputs in `E:\xtp-data\re\out\rtfallback\vm\`; the probe effect's
  program records in order in `E:\xtp-data\re\out\rtfallback\ilp-records.tsv`.

## 7. What comes next

- **The hook**: written (section 5), `XTP_RT_PATH`, off by default.
- **The A/B on the same frames**, one Series S session with
  `XTP_RT_PATH=ab` (not run at this writing). In each phase record:
  - the "first bind" lines of the fallback programs;
  - the `rt: dropped traces in a frame` lines: the probe gathers and the
    cache's shadow rays should fall to 0;
  - in the pass dump, the probes' writes at pass 17 and the transparent
    probe volume's at pass 73, and the light mixers' inputs (non-zero?);
  - the irradiance cache on its own: its gather is not dispatched on this
    branch, so whether its atlas stays at 0 and whether the mixers still
    read it are measured apart from the probes;
  - the luminance chain, G-buffer → lit HDR → presented, as in
    world-black.md §10.1, and whether the street gets brighter.

  Upstream (watch 25, item 348): on an RTX 4070 SUPER both probe
  structures stayed at 0 over 553 readings, even with their address
  fixed. Nobody upstream has measured the forced predicate.
- **The default**: `fallback` becomes the default only if the A/B shows a
  clearly better image with no crash and no assertion; otherwise it stays
  opt-in, and this page says why.
- **How to confirm the path changed** (watch 23, items 328–330): no
  upstream test has shown the title's non-traced path running. The first
  write into the probe outputs by `81560230fcde2b0a`, then into the
  transparent probe volume by `29dc558346584906`, is the proof; the voxel
  volume is not expected to be written (section 1).
- **A darker room is possible**: on an RTX 5080, with the builder off,
  two ray passes darkened a room once the reflection tiles were made.
  Here the irradiance cache loses its gather on the fallback branch, and
  the screen probes' fallback rays are refused: the A/B says whether the
  light mixers' reads of the cache and of the screen probes make the
  image darker than with the traced passes dropped, and which callers of
  section 4 then need reading.
- **Section 4's callers**, starting with the screen probes and the
  lighting functions after the probe system, and the byte at `0x876aa10`.
