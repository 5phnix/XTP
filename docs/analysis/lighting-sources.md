# Lighting sources: who should write the lighting pass's zero inputs

Task lightsrc (d012488d), 2026-11-25, beside task lightterms (42c20033,
docs/analysis/lighting-terms.md). The deferred lighting pixel
`1cd9bc634417a981` reads inputs that are 0 with no producer in the frame.
This page asks, for each of them, who should write it on the console and
why nothing does in XTP.

Programs are named by hash only, their roles in our own words. Code is
named by RVA (image base 0x140000000). The title's bytes, program names,
dumps and debugger logs stay outside the repository, under
`E:\xtp-data\re\lighting\` and `E:\xtp-data\pix\`.

## 1. Verdict

- **On the console, nothing writes these inputs in this scene.**
  - The three zero textures are the directional light's screen-space
    shadows. No pass writes them, and none is refused or dropped.
  - The title does not record the passes that would write them, because
    its directional light is off (§3).
  - The light is off because its intensity factor M is 0 (§4.6).
- **That 0 is the opening scene's own content.** The scene:
  - sets the clock to 03:00, which makes the moon the directional light;
  - holds the clock;
  - forces a weather whose time-cycle modifier sets the moon's
    multiplier to 0. The light's update then computes M = 0 (§4.8 to
    §4.11).

  No HLE call or XTP state is on that path. The 1970 date is the title's
  own start-up default, and the host's local time feeds another object.
- **XTP's fault on the way was elsewhere and is gone.** On the older base
  of most of these sessions, 18 tasks of the frame's graph never ran,
  among them the copy of the light's state (§4.7). With main of
  2026-11-25 they run (§4.9).
- **What follows** (§5):
  - no CPU-side fix;
  - this scene's image rests on the local lights (RT lamps) and the
    ambient and indirect terms;
  - the directional light is to be checked in a daytime or clear-night
    place.

## 2. The zero inputs

From the pass dumps' "before it" lines of the lighting pass, and from
lightterms' reading of the pixel's DXIL (task 42c20033).

| Slot | Format, size | Title memory | Host flags | What the pixel does with it |
|---|---|---|---|---|
| `s0+760` | R8_UNORM 1280×720 | 0xad6e80000 | render target and UAV | gates a directional-light block (and a transmission term) |
| `s0+808` | RGBA8_UNORM 1280×720 | 0xb31c40000 | render target and UAV | sampled once, inside that block |
| `s0+1280` | R8_UNORM 1280×720 | 0xaf9190000 | render target only | loaded per pixel, then a `max()` with another value |
| `s1+0` +300..+311 | 3 floats in a 340-byte record | CPU constant ring | none (CPU-written) | the directional light's colour: it multiplies the moon terms |

The record `s1+0` holds the light's direction at +272 and again at +288,
(0.5522, −0.5300, −0.6436), the same in every frame of every session; the
colour at +300 is 0. The global 2,044-byte block `s0+176` holds the same
direction, negated. lightterms found a second CPU-written zero, the
intensity factor in a constant buffer of the ambient-cube compute
`fb9c7f430da14c4c` (its `s0+232` element 7).

## 3. The GPU side: nothing writes the three textures

### 3.1 No writer, no alias

- **Every dump, every session.** The three addresses are 0 before the
  lighting in all 23 dumps that have the pass's inputs: `imgsrc-1` 674–676,
  `imgsrc-2` 661–663, `lightchain-1` 674–677, `staging2-2` 588–598,
  `imagecheck-3` 598 and 603, `imagecheck-4` 618–624, `chars-albedo-1` 608
  and 613. No pass output, render target, UAV or depth target of any dump
  is at those addresses. Neither is any output of the 19 dumps of `rtab-2`,
  in either arm of its `XTP_RT_PATH` A/B (rt-fallback.md §8).
- **No placed alias hides a writer.** No output of any dump overlaps the
  three ranges, with two exceptions that are not writers:
  - `s0+1280` sits right after the material-class depth texture
    (0xaf8ec0000), whose 16-bit depth and stencil planes take 0x2d0000
    bytes on the console. The R8 target starts at +0x2d0000 and ends where
    the tile light lists start (0xaf9280000). The dumps' overlap comes from
    sizing that depth texture at 4 bytes a texel. The target has its own
    memory.
  - The second half of `s0+808`'s memory is written by a 640×360 R32
    texture, but in a later list, after the lighting (P774 and P792 of
    `imagecheck-4` 618, the lighting being P681). The target is transient
    memory, which is normal.
- **The dropped dispatches named as candidates are not writers.**
  `82a2ad6d07417d36` and `90a0692af4320129` skin meshes for the
  ray-tracing structure and write vertex buffers. `aab1b3c7248ee5de`
  schedules the indirect-light probes. None writes a 1280×720 R8 or RGBA8
  texture (refusals.md rows 135, 136 and 56 give the same roles).

### 3.2 The directional shadow system never runs

The three textures are what the directional light's screen-space shadow
system writes before the lighting. In every dump of every session, the
only programs of that system that run are the shadow map draws. The other
programs are never dispatched:
- the shadow tile classification (raster or ray-traced tiles);
- the compute and pixel passes that resolve the cascades into a
  screen-space shadow mask;
- the ray-traced directional shadow traces, their denoise and copy.

The 1,722 "first bind" lines at frame 595 of `imagecheck-4` are the
title's warm-up of its pipelines (all within 0.1 s), not dispatches. The
classification and resolve programs translate and get host pipelines
there (for example `9991e29feaa26e3e`, `191ecaed66eea3b1`). The
ray-traced directional traces are never even made into pipelines. None of
them is refused or dropped: the title's CPU does not record them.

That is the title's own choice when its directional light is off. The
light's colour, which the CPU writes, is 0 (§2). So the three textures are
consequences, and the cause is on the CPU (§4).

## 4. The CPU side

### 4.1 The light is the moon, above the horizon

The title's global constant block (3,424 bytes at title memory
0x99276ed80, the same address in every session; the lighting reads it as
`s0+1416`) holds the directional light's direction towards the light at
+0x490, (−0.552, 0.530, 0.644), and a smoothed copy at +0x4a0. Its z is
up: the light is 40° above the horizon. The sky's 240-byte block
(0x992738280) holds that vector and a second one, (0.686, −0.091,
−0.722), 46° below the horizon: the sun, at night. So the directional
light is the moon, high in the sky. Its colour being 0 is not a light
below the horizon.

In the global block, +0x4c0 holds (0, 0, 0) and then 10000, likely the
light's colour and a scale; +0x4e0 holds (632.7, 1125.3, 1496.3).

### 4.2 The time-of-day and weather data arrive

The data that drives the time of day lives in XML documents of the
`common.rpf` archive, which the title reads through DirectStorage
(`dstorage.rs`). Session `lightsrc-2` traced every archive read
(`XTP_DSTORAGE_TRACE=.rpf`, §6): 74,493 reads, 861 of them from that
archive, none failed.
- The document at archive offset 0x1b1000 (268 bytes of zlib) inflates
  to 1,370 bytes of XML, 2.8 s after the start.
- The one at 0x207000 is read in four pieces (0x207000 to 0x209040), all
  inflated to XML.
Both land at addresses, not in physical pages. The time-of-day data is
read and served: a lost or late read is not the cause.

### 4.3 Paths the HLE does not map

Each session logs the same paths as "unmapped console path":
- **About ten `memory:$<address>,<length>,0:str:<n>(<id>)` paths.** That
  is the engine's own name for a block of memory it reads as a file. The
  title's `_wstat64` asks for their attributes, always from the same
  loading thread (title code at RVA 0x1a99400). The HLE passes the path
  to the host, which fails, and the console's system has no such device
  either. The blocks are binary resources of the engine's parser, not the
  time-of-day XML of §4.2.
- **One pack alias path**, asked through `GetFileAttributesW`. It is the
  engine's mount name, which the console's system does not know either.

### 4.4 Device methods the title calls

The title calls two methods of the device that XTP leaves unwritten. Both
create a pipeline state from a description and return `E_NOTIMPL`. The
title then goes on without the object.
- Slot 0x218, from RVA 0x24800db, is the deserialisation of a compiled
  compute pipeline (series-s.md §6). The title falls back to its normal
  compilation.
- Slot 0x1c8, from RVA 0x247757e, creates an `ID3D12PipelineState` too
  (same interface id).

Neither touches the light's colour.

### 4.5 Who writes the colour

Found with sessions `lightsrc-2` to `lightsrc-5` (§6) and static reading
of the executable. RVAs, image base 0x140000000.

- **The record and the global block live in the title's `.data`.** The
  340-byte records form a persistent array at RVA 0x8fca6c0 (stride
  0x154), indexed by the render thread's buffer index. The global block
  has its own copy at RVA 0x8492070. At each draw, the engine's generic
  state flush (RVA 0x248c418, writing through RVA 0x4d9f059) copies them
  into the constant ring, and from there to the fixed addresses the GPU
  reads. That is why hardware breakpoints on the fixed addresses never
  fire (`lightsrc-4`).
- **One setter writes the record's direction and colour**
  (+0x110 to +0x150): RVA 0x50d30e0. It copies a 0x44-byte source whose
  colour is at +0x1c.
- **One function builds that source**: RVA 0x50ccf90, with two arguments,
  an intensity (float) and a switch (byte).
  - The direction is a global, at RVA 0x8fdbdb0, with the opposite of the
    one at RVA 0xab4ba40 (the to-light vector of §4.1).
  - colour = intensity × (switch ? K : 0) × M × (1, 1, 1), M a global at
    RVA 0x8fdbdd4. When the byte at RVA 0xab4baa4 is not 1, the colour
    comes from three bytes at RVA 0xab4baa8 instead.
  - **A switch at 0 gives a colour of 0.**
- **Three entries reach it:**
  - (A) RVA 0x25f0630 passes the switch at 0: the light off. The frame
    set-up at RVA 0x25efcf0 calls it unconditionally, then runs further
    steps chosen by a flag table (RVA 0x620226c) indexed by a mode byte of
    its object (+0xb984).
  - (B) RVA 0x35cac70: the switch at 1, a constant intensity.
  - (C) RVA 0x50cc340: the switch at 1, the caller's intensity. It has 15
    callers. Two of them, at RVA 0x329ced0 and 0x329d4a0 beside the
    cascade-shadow code, pass an intensity of 0 when bits 1–2 (or bit 2)
    of a flags byte are set, and a constant otherwise.

So each frame turns the light off first. Only (B), or (C) with a non-zero
intensity, turns it back on.

### 4.6 Why the colour is 0: the factor M

Sessions `lightsrc-6` and `lightsrc-7`, with debugger breakpoints in the
world (§6), on the older XTP base (§4.9 says what changed with main).

- **The light is turned on, not off.** In 10 s, (A) never runs, nor (B),
  nor the two flag-testing callers. (C) runs 71 times with an intensity of
  1.0, mostly from RVA 0x9b2b80, and the setter 70 times. The source it
  copies holds the direction and a colour of (0, 0, 0).
- **The factor M is 0.** M is the global at RVA 0x8fdbdd4. The float
  before it (RVA 0x8fdbdd0) is 1.0. The byte at RVA 0xab4baa4 is 0, so the
  colour is intensity × K × M × (1, 1, 1) = 0. The game clock reads
  03:00:00, held still during the opening.
- **M is the field +0x44 of the directional light's state object.** The
  object has two copies of 0x760 bytes:
  - the update copy, at RVA 0x98e6450;
  - the render copy, at RVA 0x8fdbd90.

  Two predicates also test M to decide whether the directional light is
  on, both "M ≠ 0 and bit 0x40 of the flags at +0x7c": RVA 0x2009be0 and
  RVA 0x20335a0. With M at 0 they answer no, which is why the
  directional shadow system never runs (§3.2).
- **Both copies hold exactly their constructor's values.** The constructor
  (RVA 0x4e3ad10, called from the static initialisation at RVA
  0x354c7b0) clears the object, then sets:
  - the colour to (1, 1, 1, 1);
  - the direction to a constant of the binary, the (0.552, −0.530,
    −0.644) of §2;
  - 1.0 at +0x40.

  M, at +0x44, keeps the 0 of the clear. In 20 s of the world nothing
  writes M in either copy: hardware write breakpoints on both fire 0
  times. Watched from the image's mapping on (§4.8), M is written during
  loading, 1.0 then back to 0, and not again in the world. The sun and
  moon pass gives the moon that same direction at 03:00 (§4.9), so §4.1's
  reading stands.
- **The override path is idle.** M is set through the light state's
  setter (RVA 0x548a390): by the system's initialisation (RVA 0x573fdb0,
  M = 0) and by the directional light's update (RVA 0xbb6ad0, §4.8).
  Right after, the update calls RVA 0xbb6c60, which applies a script-set
  override: set or scale M, by day or by night, as the clock-based
  predicate at RVA 0x4db8060 decides. Its override struct is in its
  default state (nothing set, scales of 1.0).
- **The render copy is refreshed by a task of the frame's task graph.**
  The copy from the update copy to the render copy (RVA 0x51cae60, 0x760
  bytes) is reached from RVA 0x0307190, called by RVA 0x3d95e30. That
  function is a task registered with 111 others by RVA 0x1f04cd0 (task
  creation RVA 0x1c86590, dependencies RVA 0x12ec180).

On that base the copy task never ran (§4.7). With main it runs, and M is
still 0 for a reason of the scene's own (§4.9 to §4.11).

### 4.7 The frame's task graph: which tasks run

Session `lightsrc-8`: software breakpoints on the 27 distinct functions of
the 112 tasks that RVA 0x1f04cd0 registers (111 resolved), for 10 s in
the world.

- **9 functions run**: RVA 0xf9dc80, 0x3d95df0, 0x3d95da0, 0x4380 (the
  generic function of 85 tasks), 0x3d95e60, 0x3d95e40, 0x3d768d0,
  0x1195980 and 0x3d960b0.
- **18 never run.** Among them:
  - the light state's copy, RVA 0x3d95e30;
  - its neighbour RVA 0x3d95e20, which calls a large update (RVA
    0x2611090) that reads the light state;
  - RVA 0x3d95dd0, 0x3d95e00, 0x3d96050, 0x3d96080, 0x3d96180, 0x3ad960,
    0x654c80, 0xf47c40, 0xa31c00, 0xb87cd0, 0xd9ff90, 0xb68f10, 0xabe2f0,
    0xa04590, 0x4f6cd0 and 0x624ca0.
- **Their registration looks like that of the tasks that run.** Each task
  is created by RVA 0x1c86590 with a name hash, between calls to RVA
  0x12ec180 that take a small identifier and a flag. They look like
  resource or phase declarations. What keeps the 18 from being scheduled
  is not visible in the graph's construction.

The game clock, read beside the light state, holds day 1, month 0, year
1970, 03:00:00, and day of week 4 (a Thursday). That is 1 January 1970,
the Unix epoch, which looked like a date made from a zero timestamp. It is
the title's own start-up default (§4.8), and the scene sets the time
(§4.11). With main, all of these tasks run (§4.9).

### 4.8 Who writes the clock and M, and what M is made of

Session `lightsrc-9b`: hardware write breakpoints from the image's
mapping on, on the clock's year, day and hours and on M (update copy),
with the stacks; breakpoints on the host time functions, kept only for
calls from the title. `XTP_WATCH` (5 ms polling, no debugger) on the same
words.

- **The clock is set by the title's own code, not from a host date.**
  - The title's start-up (RVA 0x51acd6e and 0x51acd74, reached from RVA
    0x3013726) writes day 1, month 0, year 1970.
  - Then the hours go to 12 (RVA 0x4ebb751, from RVA 0x5d27453).
  - The clock's tick then advances it minute by minute. It is the task at
    RVA 0x15120b0, which adds time through RVA 0x51ad0e0 and writes at RVA
    0x4e8fec6.
  - Later the clock reads 03:00:00, set by the opening scene. That write
    was not caught (§4.11).
  - The title calls `GetLocalTime` (RVA 0x7877f6, reached from RVA
    0x78771e and 0x22afc3d): a host call, not an HLE one. No clock word
    is written from its result. 1970 is the clock's start-up default;
    nothing found sets a date from the host or the console.
- **M changes during loading.** In `XTP_WATCH`'s order:
  - M is 0 from the system's initialisation (RVA 0x573fdb0 through the
    setter RVA 0x548a390);
  - the hours go to 12;
  - M goes to 1.0, written by the directional light's update (RVA
    0xbb6ad0, entered through RVA 0xbb6ac0 from the job executor RVA
    0x63cd0);
  - the hours go to 3;
  - M goes back to 0. This writer was not caught: the breakpoints had
    been removed by then.
- **How the update computes M (RVA 0xbb6ad0, static).**
  - It picks one of two parameter sets, A or B: B when the weight at
    RVA 0xab4a3b0 is above 0.
  - It takes that set's intensity (A at RVA 0xab48d4c, B at RVA
    0xab48d70).
  - It multiplies the intensity by the multiplier at RVA 0xab4a390, and
    by a setting (read through the pointer at RVA 0xab7f990, +0x194, when
    its bytes +0x18d and +0x18f allow it).
  - It sets M to 0 if the one-shot flag at RVA 0xab5cdb3 is set, if the
    product is under a threshold, or if the colour is black.
- **What the inputs held**, read once in the world at 03:00:
  - intensity A: 1.0;
  - sun weight (RVA 0xab4a3a8): 0;
  - moon weight (RVA 0xab4a3ac): 1.0;
  - third weight (RVA 0xab4a3b0): 0;
  - flag: 0;
  - setting: 1.0;
  - **multiplier: 0**.

  So M = 1.0 × 0 = 0.
- **Where the multiplier comes from (static).**
  - The sun and moon pass (RVA 0x4994f0) computes the sun and moon
    directions and the three weights.
  - It stores the largest weight as the multiplier, then dims it.
  - The time-cycle pass (RVA 0x496bf0) then multiplies it (at RVA
    0x496e39) by one entry of the time-cycle block at RVA 0xab468c0:
    +0x2490 when the sun weighs most, +0x2494 when the moon does.
  - The same pass zeroes intensity A when the predicate at RVA 0x497020
    answers yes. That predicate follows a global object (RVA 0x9f148d8,
    +0xc8, +0x20, +0xd8) and tests bit 2 of a flags word, which looks
    like an "interior" test. Here intensity A is 1.0, so it answered no.

So the light is off because the time-cycle multiplier is 0 at 03:00
with the moon dominant. Whether that 0 is the night's data or a
time-cycle block that was never filled is session `lightsrc-10`'s
question (§4.9).

Two dead ends, for the record:

- **MONITORX and MWAITX.** The title's code holds 4 of them, all in one
  function (RVA 0x4fb0ad0). That function waits for a 64-bit GPU fence
  value to reach a target, re-reading the value after each MWAITX.
  XTP's handler (`crates/xtp-loader/src/crash.rs`: MONITORX does
  nothing, MWAITX yields) keeps that loop correct, and `l4-world.md`
  counts a single MONITORX in a whole run. No task waits there.
- **Task registration.** Task creation (RVA 0x1c86590) copies read and
  write resource masks into the task. The calls to RVA 0x12ec180 that
  precede it fill those masks. They are access declarations for
  ordering, not run conditions. The run conditions are in the job
  executor (RVA 0x63cd0). Before calling a job's function (its +0x50),
  it tests three things:
  - the job's 16-bit mask at +0xe4 against the context's at +0x5e;
  - the job's bits at +0xe1, ANDed with the context's byte at +0x70,
    against the job's +0xe2;
  - an optional predicate object at +0xe8.

### 4.9 With the current main: the tasks run, the clock stops at 03:00

Session `lightsrc-10`, with XTP rebuilt on the main branch of
2026-11-25 (219 commits after the base of sessions 2 to 9):

- **Phase A**, from the image's mapping: hardware write breakpoints on
  the hours, the year and M in both copies, and a breakpoint on the
  light's update.
- **Then 60 s in the world with no debugger**, the clock and M watched
  by `XTP_WATCH`.
- **Phase B, 40 s**:
  - the inputs and the time-cycle block read;
  - a daytime date written into the clock;
  - breakpoints on the light-sync tasks, the update, M, the hours and
    the sun weight;
  - then a clean detach.

Findings:

- **The frame's tasks run now.** In the world, the light state's copy
  task (RVA 0x3d95e30) ran 1,513 times in 40 s, its neighbour (RVA
  0x3d95e20) 1,615 times. With the older XTP of §4.7 they never ran. The
  18 idle tasks were an XTP fault that main no longer has.
- **Before the world, the clock runs.** It stands at 12:00 and advances
  one second per second, and the light's update follows every step: M
  is 1.0 in both copies.
- **The opening scene stops it at 03:00.**
  - Its script sets 03:00:00 and the clock then holds still. In 60 s
    without a debugger and 40 s with one, no hours, minutes or seconds
    word changes.
  - The sun and moon pass no longer runs: no write of the sun weight in
    40 s. The light's update no longer runs either: no call in 40 s.
- **The last pass, at 03:00, made M = 0.**
  - The moon weight is 1.0 and the sun weight 0.
  - The time-cycle block's moon multiplier (block +0x2494, RVA
    0xab48d54) is 0, where the sun multiplier (+0x2490) is 1.0.
- **A date written into the clock changes nothing.** 25 Nov 2026,
  14:00, written in the world: in 40 s, no update, no write of M, the
  hours or the sun weight. The scene has the clock paused, and a raw
  write into it does not restart the time-of-day system.

No HLE call or XTP fault is left on M's path. M is 0 because the
opening scene holds its night at 03:00 and the time-cycle's moon
multiplier is 0 there. Whether that 0 is the scene's data (an overcast
night, as on the console) or a time-cycle value that was not loaded is
the last open point.

The time-cycle block (RVA 0xab468c0) is the time-cycle manager. It holds
a list of modifiers looked up by name hash (RVA 0x494d20) and applied by
RVA 0x52a9890 (RVA 0x496970 applies the scene's ones), and the current
values from +0x2480. No instruction writes the moon multiplier
RIP-relative. It is written through the block's base, by the blend or by
a modifier. Session `lightsrc-11` names the writer (§4.10).

### 4.10 Who writes the moon multiplier's 0: a weather time-cycle modifier

Session `lightsrc-11`: hardware write breakpoints on the time-cycle
block's sun and moon multipliers, the hours and M, attached during
loading and again at the world + 15 s for 30 s.

- **The time-cycle blend gives the moon multiplier 1.0 at 03:00.**
  - The time-cycle pass is a task, RVA 0x5dcc40.
  - It reads the clock and blends the time-cycle data of two weather
    entries (indices at RVA 0xab46190 and 0xab46194 in the table pointed
    to by RVA 0xab45eb8, factor at RVA 0xab46198) at that time (RVA
    0x5ddda0).
  - It copies the 0x1600 bytes of the result to RVA 0xab48d40.
  - At 03:00 that blend holds realistic values (0.456, 0.628, 2.32, 0.889,
    0.673…) and a moon multiplier of 1.0. The weather data are loaded.
- **Right after, a modifier sets it to 0.**
  - The task at RVA 0x494ca0 applies the two lists of active time-cycle
    modifiers (RVA 0xab47f60 and 0xab47f70, through RVA 0x52a99c0 and
    0x52a9c70) over the block.
  - One of them writes the moon multiplier to 0 and the parameter after
    it to 0, and resets the colours to (1, 0.76, 0.898, 1).
  - The light's update (RVA 0xbb6ad0) then computes M = 0 and stores it
    in both copies.
- **The modifiers come from the weather.**
  - The lists are filled at every step by one function (RVA 0x43428e0).
  - Its only direct callers are 16 sites in the blend itself (RVA
    0x5ddda0): up to 8 modifiers per weather entry, for the current and
    the next entry, weighted by the blend factor.
  - The task at RVA 0x494ca0 empties the lists after applying them.
- **The clock's tick has explicit holds.** The tick (RVA 0x15120b0) does
  nothing while any of these is non-zero:
  - the byte at RVA 0xabbe2e0;
  - the byte at RVA 0x7446278 (read through RVA 0x4f7b6a0);
  - the byte at RVA 0xabbe2e1.

  It holds the clock (rebases it without advancing) when:
  - bit 0 of the bytes at RVA 0x92e0031 or 0x8004560 is set (RVA
    0x3013890);
  - or the dword at RVA 0x7f028dc is 1;
  - or the byte at RVA 0x7f028ea is 1.
- **The host's local time does not feed the game clock.** The title reads
  `GetLocalTime` at RVA 0x7877e0. It stores the result into another date
  object (through RVA 0x7876e0), not into the clock at RVA 0xab5cdc0.

### 4.11 The opening scene sets the night, holds the clock and forces the weather

Session `lightsrc-12`:

- `XTP_WATCH` (no debugger) on the clock, the six hold flags of §4.10,
  the weather indices, the moon multiplier and M.
- A software breakpoint on the clock's "set time" (RVA 0x4ebb720) during
  loading.

When the opening scene starts, right after loading (clock at 12:00,
weather entries 0x15 and 0x11, moon multiplier 1.0, M = 1.0), three
things happen together:

1. **The clock goes to 03:00.**
2. **A hold flag is raised.** The byte at RVA 0x8004560 goes to 1, and
   the clock's tick holds the clock from then on. That byte is set by
   methods of the engine's scene-playback object: RVA 0x249490 sets it
   when bit 2 of a flags byte of the scene (its object +8, then +0x158)
   is set. RVA 0x24a620 and 0x2503a9 set it as well; RVA 0x23d90e,
   0x24b878 and 0x2506b3 clear it.
3. **The weather is forced.** Both entries of the time-cycle blend become
   0x20. The blend still gives a moon multiplier of 1.0, but a modifier
   of that weather sets it to 0 (§4.10), and the light's update makes M
   = 0.

Setting the time, holding the clock and forcing the weather when a
scripted scene starts is the scene setting itself up. Every value on the
path comes from the title's code and data. No HLE call and no XTP state
takes part, and the host's local time feeds another object (§4.10).

The instruction that writes 03:00 was not caught. "Set time" also
initialises dozens of other time-of-day objects, and its 96 calls before
the world used up the session's breakpoint budget. A breakpoint
conditioned on the clock (rcx = RVA 0xab5cdd0) would name it.

## 5. What to do

- **No fix on the CPU side.** The directional light of the opening scene
  is off by the scene's own content: night at 03:00, clock held, a weather
  whose time-cycle modifier zeroes the moon. The three textures of §2 are
  then rightly unwritten, because the directional shadow system follows
  M (§3.2). The 18 idle tasks of §4.7 were XTP's, and main no longer has
  that fault (§4.9). No HLE file needs a change for this.
- **What lights this scene.** With neither sun nor moon, its image
  depends on the local lights (the RT lamps work, tasks rt3a and rt3c)
  and on the ambient and indirect terms. The directional light is
  tested elsewhere:
  - a daytime or clear-night place of the world;
  - or later, once the scene releases the clock and the weather.
- **For the next sessions.**
  - Read the hold byte at RVA 0x8004560 and the weather indices at RVA
    0xab46190 and 0xab46194. They tell at once whether a scene holds the
    time and the weather.
  - M at RVA 0x98e6494 tells whether the directional light is on.
- **Not done here:**
  - the name of the exact instruction that sets 03:00 (§4.11);
  - the name of the weather 0x20 and of the modifier that zeroes the
    moon;
  - whether the exposure target that the CPU maps is written into memory
    that XTP tracks. XTP copies a tracked GPU result back to title memory
    at the CPU's first touch (`readmap.rs`); the copy-back map only holds
    hints. The question needs that target's title address.

## 6. Method

Sessions ran on the PC through the session queue
(`E:\tools\stall-probe\queue-session.ps1`) with a diagnostics build
(`--profile diag --features diagnostics`).

| Session | What it did |
|---|---|
| `lightsrc-1` | Cancelled, to add the traces below. |
| `lightsrc-2` | DirectStorage and unmapped-path traces (§4.2, §4.3). |
| `lightsrc-3`, `-4` | Hardware breakpoints on the fixed constant addresses: they never fire (§4.5). |
| `lightsrc-5` | First writes into write-watched pages, then a memory scan for the light's record (§4.5). |
| `lightsrc-6`, `-7` | Breakpoints on the light's setter, builder and entries, and on M (§4.6). |
| `lightsrc-8` | Breakpoints on the 27 task functions of the frame's graph (§4.7). |
| `lightsrc-9` | Lost: the old base's shader-cache scan held the start past the session's limit. |
| `lightsrc-9b` | Writers of the clock and M from the image's mapping (§4.8). Its date test is void (below). |
| `lightsrc-10` | Rebuilt on main: tasks, clock, inputs, a date test (§4.9). |
| `lightsrc-11` | Writers of the time-cycle's sun and moon multipliers (§4.10). |
| `lightsrc-12` | Clock holds, weather indices, "set time" (§4.11). |

- **Switches.**
  - `XTP_DSTORAGE_TRACE` (`dstorage.rs`, diagnostics builds) logs each
    DirectStorage read whose source name contains the given text, or
    every read with `1`: source, offsets, sizes, destination, result and
    the first bytes.
  - `XTP_UNMAPPED_CALLERS` (`files.rs`, diagnostics builds) adds the
    title's callers to each "unmapped console path" warning.
  - `XTP_WATCH` (existing) logs the u32 at each given RVA whenever it
    changes, polled every 5 ms, with no debugger.
- **Debugger.** `cdb` attaches with `-pd` to the running game, right
  after the run log's "image mapped" line for the image whose preferred
  base is 0x140000000.
  - The PEB's `BeingDebugged` is cleared at attach.
  - Hardware write breakpoints (`ba w4`) and software breakpoints log the
    value and the stack, under a hit budget.
  - The session ends with a break-in (`DebugBreakProcess`, after setting
    `BeingDebugged` back), then clears every breakpoint and DR7 on every
    thread, and detaches with `.detach`.
- **Two pitfalls.**
  - In `lightsrc-9b`, the debugger was ended by killing it after `g`
    while an event was still pending. That left all 144 of the game's
    threads suspended. The date written afterwards went into a stopped
    game. Resuming the threads then hit the autopilot's time limit, which
    closed the title. Since `lightsrc-10` the drivers detach with
    `.detach`, and resume the threads if all are left suspended.
  - With hardware breakpoints, a stray single-step event can stop `cdb`
    early (`lightsrc-11`'s loading phase). Software breakpoints, or
    `XTP_WATCH` alone, avoid it.
- **Static reading.** Python with `pefile`, `capstone` and `numpy` over
  the executable: functions by `.pdata`, RIP-relative references, stores
  through a block's base, and call sites. Only RVAs and behaviour are
  recorded here. The scripts and logs stay under
  `E:\xtp-data\re\lighting\`.
