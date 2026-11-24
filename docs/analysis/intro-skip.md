# The intro movie: is it on the way to the world, and can tests skip it?

Measured on 2026-11-24 on the person's PC (RTX 4080 SUPER) with build
1.0.2.1 (BLAKE3 `b99f876b…e93`), from the golden save, for the task
"Skip the intro videos when they only cost time". RVAs are relative to the
base of `game_scarlett_master.exe`. Nothing of the title (bytes, names of
its files) is quoted here.

**Answer: yes, it is on the way, and cutting it saves about 13 s per test
session.** The title starts loading the world only when two things are
done: its intro movie has ended, and the profile is accepted. The movie
lasts 15.00 s and starts as soon as the start-up work is done. A test
session accepts the profile within ~2 s of that, so the movie is what it
waits for. With the movie sent to its last frame, the world loading starts
13.0 to 13.3 s earlier: 10.1–10.4 s after launch instead of 23.35–23.39 s.
Whether the world itself then arrives 13 s sooner is not shown yet
(section 6). While loading is held up by VidMm (docs/analysis/stall.md),
the world load takes 160 to 206 s and varies by more than the saving. The
one pair taken into the world shows the same work before and during the
world load, cut or not.

`XTP_SKIP_INTRO=1` does it (section 5). It is off by default, and only
build 1.0.2.1 is known.

## 1. What the videos are

In the whole front end, the title plays **one movie**: Bink 2, 3840×2160,
450 frames at 30/1, so 15.00 s. It is the montage after the profile
screen: a star transition, the artwork montage, then the publisher logo (the
captures of run-world's sessions C and G show the same sequence). The title
reads it into memory and hands it to Bink from there (open flag
`0x04000000`), so it has no path at the Bink level.

Other movies seen, none of them on the way:
- about 60 small movies (512×512, 0.5 to 5 s) are probed when the profile
  is accepted, and never played;
- the loading screen plays a 2560×1440 movie of 217 frames (7.23 s). It
  is opened after the world loading has started, and runs while the world
  loads.

## 2. How the title plays them

The movies go through `Bink2Scarlett.dll`. The title calls it through its
import slots: `BinkOpenWithOptions` `0x6794210`, `BinkNextFrame`
`0x6794208`, `BinkGoto` `0x6794200`, `BinkClose` `0x67941c0`, and others.
XTP's media module (`xtp-hle` `media.rs`) only covers Media Foundation,
which is not involved here.

- A movie is **probed** first: `0x442ea70` opens it, copies its size, frame
  count and rate into the movie object, and closes it. The intro is probed
  three times.
- Then it is **opened for playback**: `0x442efe0`, called from `0x199ef20`,
  which is called from the front end's movie code at `0x3a18620`.
- If that open fails, `0x199ef20` sets the movie object to state `0xb`
  (reason 3).
- The **player** is `0x2ef0e0`. Before each frame it decodes, it calls
  `BinkNextFrame`, then waits for the decode before the next one. Its own
  seek is a `BinkGoto` followed by a decode.
- The intro is opened paused, about 2 s before it starts. It is unpaused
  when the start-up loading screen goes (`0xab43664` back to 0), and paused
  four times in a row when its last frame is shown.

## 3. Does it gate the world?

The title's start-up flags (`0x74e007c`, four bytes) show it. The table
below shows the order of the transitions; section 4 gives the times.

| byte | set when | with the movie | with the cut |
|---|---|---|---|
| `0x74e007c` | start-up done | +2.9 s | +3.3–3.7 s |
| `0x74e007f` | profile accepted (A) | +10.1–10.4 s | +10.1–10.4 s |
| `0x74e007e`, then `0x74e007d` | intro movie ended | +23.35–23.39 s | +8.7–8.9 s |
| `0xab43664` = 0xe, `0x74e2c1c` = 0 | world loading starts | as soon as both are set: +23.35–23.39 s | +10.1–10.4 s |

The world loading starts the instant the later of the two is set:
- with the movie, the movie's end. The loading begins 15.07 to 15.10 s
  after the movie is unpaused, within 0.1 s of the pauses that follow its
  last frame;
- with the cut, the profile acceptance. Here the A presses come every
  2.5 s from 5 s on, and the first one taken is the one at 10 s.

So the movie is not filling a wait: the start-up work is already done when
it starts (it starts when the start-up loading screen goes). The saving in
a session is the movie's end minus the profile acceptance:
- **about 13 s** with prompt presses, as here or with the autopilot (it
  presses A every 2.5 s as soon as the front end is up);
- less when the profile is accepted late. Nothing is saved when the
  profile is accepted after the movie would have ended anyway.

## 4. Measurements

Sessions of `E:\xtp-data\play.ps1` from the golden save, with the same
regime in all of them:
- the window not focused and the sound at volume 0;
- A (Enter) every 2.5 s from 5 s to 145 s;
- stopped 10 s after the world loading started;
- no rustc running at any time.

Times are seconds after the first log line, which comes about 0.2 s after
launch. They are read from state words polled every 5 ms and logged with
timestamps, and from the logged Bink calls. The measurement build wrapped
the Bink imports to log them (`XTP_INTRO_TRACE`, not kept in the
repository).

| # | run (`E:\xtp-data\runs\…`) | intro | front end up | movie unpaused | profile accepted | movie ended | world loading starts |
|---|---|---|---|---|---|---|---|
| 1 | play-20261124-161601-intro-trace | played | 2.97 | 8.32 | 10.36 | 23.39 | **23.39** |
| 2 | play-20261124-162032-intro-trace | played | 2.85 | 8.28 | 10.19 | 23.35 | **23.35** |
| 7 | play-20261124-170441-intro-trace | played | 3.09 | 8.28 | 10.05 | 23.38 | **23.38** |
| 5 | play-20261124-165936-intro-skip-goto | cut (`BinkGoto`) | 3.66 | 8.47 | 10.38 | 8.94 | **10.38** |
| 6 | play-20261124-170212-intro-skip-goto | cut (`BinkGoto`) | 3.33 | 8.28 | 10.12 | 8.75 | **10.12** |
| 8 | play-20261124-171625-intro-final | cut (`XTP_SKIP_INTRO=1`) | – | cut at 8.38 | – | – | ~10 (play.ps1, 1 s steps) |

- Played: the world loading starts at 23.37 s on average (range 0.04 s).
- Cut: it starts at 10.25 s on average (range 0.26 s).
- **Saved: 13.1 s.** The movie itself is 15.07 s; the difference is the
  time from the movie's start to the profile acceptance.
- Sessions 5, 6 and 7 ran the same binary. Sessions 1, 2 and 8 ran other
  builds of the same branch, and their front-end times match.
- Session 8 is the delivered code. It only logs the cut and the movies
  opened in the front end. Its times come from the log and from play.ps1,
  whose summary has 1 s steps.

Two other ways to cut it were tried, and failed:

| # | run | how | result |
|---|---|---|---|
| 3 | play-20261124-162317-intro-skip | the playback open made to fail | **the front end hangs.** The start-up loading screen never goes (`0xab43664` stays 0xe), the profile is never accepted, 3 draws per frame until the stop at 150 s. A failed open does not count as a finished movie. |
| 4 | play-20261124-165433-intro-skip-frames1 | the probe told the movie has 1 frame | **no effect.** The player still plays all 450 frames, and the world loading starts at 23.40 s. |

## 5. The switch: `XTP_SKIP_INTRO=1`

`crates/xtp-game/src/intro.rs`. It is installed by one line in
`apps/xtp/src/run.rs`, right after the title's imports are bound: the slots
it wraps are only filled then. It works in every build, not only the
diagnostics one. Off by default.

- **Build check.** The build must be 1.0.2.1, matched by its whole BLAKE3.
  Any other build gets a warning and nothing is changed. All the sites are
  checked before any slot is written: every import slot inside the image
  and bound, and the state words inside it.
- **`BinkOpenWithOptions` is wrapped.** A movie opened while the start-up
  front end is up (`0x74e2c1c` = 1) and before any world loading (byte
  `0x74e007d` = 0) is marked with its frame count. Only the intro and its
  probes match.
- **`BinkNextFrame` is wrapped.** The first call on a marked movie becomes
  `BinkGoto(last frame)`, the call the title's own seek makes, between two
  of its decodes. The player decodes that frame, and the movie has ended.
- **`BinkClose` is wrapped.** It forgets the mark: probes are closed
  without being advanced, and a handle can be handed out again.
- **The log.** `the intro movie will be cut (XTP_SKIP_INTRO)` at start,
  then `intro movie cut: sent to its last frame` once. With
  `xtp_game=debug`, each movie opened in the start-up front end.

Tests:
- `cargo test -p xtp-game`:
  - known and unknown builds;
  - the front-end rule;
  - the marks;
  - an install on a fabricated image whose slots point at stand-ins for
    Bink, called as the title calls them. The intro is cut once; a probe's
    reused handle, and movies of the loading screen and of the world, are
    left alone; an empty slot or a cut image is refused with nothing
    written; a second install is refused.
- `XTP_SAMPLE_GAME=<title folder> cargo test -p xtp-game --test
  intro_sample`: on the real executable, each slot RVA is the import table's
  slot for that `Bink2Scarlett.dll` function, and the state words lie in
  writable data.

To use it:
- `cargo xtask play` and `xtp run`: set `XTP_SKIP_INTRO=1` in the
  environment.
- The PC's session scripts, outside the repository, set it by default
  since this delivery; `-Intro` keeps the movie:
  - `E:\xtp-data\play.ps1`;
  - `E:\tools\stall-probe\session.ps1` (`-EnvPairs "XTP_SKIP_INTRO="`
    also removes it).

## 6. Into the world with the cut

Two sessions went into the world. Both used `E:\tools\stall-probe\session.ps1`
with:
- the autopilot (A every 2.5 s from the front end until the world);
- `XTP_WATCHDOG_HOOK=1`, without which no session reaches the world today;
- `xtask play --measure 60`, VidMm counters, and a 360 s limit.

Times are seconds after the first log line. The world loading start is
taken as the opening of its first pack, about 0.4 s after the state word
changes.

| run | intro | world loading starts | world | world load | "resource at an address": front end / world load | VidMm local resident, max |
|---|---|---|---|---|---|---|
| stall-T2-watchdog-hook, 16:47 | played | 24.96 | **184.6** | 159.6 s | 4 626 / 2 431 | 9.72 GiB |
| stall-T3-intro-cut, 17:19 | cut | 11.19 | **217.1** | 205.9 s | 4 626 / 2 461 | 10.06 GiB |

- With the cut, the world loading started 13.8 s earlier, as in section 4.
- The world load itself then took 46 s longer.
- The work is the same: the front end makes the same 4 626 textures before
  the world load starts, cut or not (in 11 s instead of 25 s), and the
  world load makes the same number.
- So the cut does not move the front end's work into the world load. The
  difference is the world load's own spread under the VidMm stall (one
  allocation admitted per ~1 s past ~9.5 GiB resident), which one pair
  cannot measure.
- Both reached the world and stayed in it until stopped. Neither crashed.
- In T3 the title's watchdog would have fired twice (once in the world
  load, once in the world), as in T2.

Whether the world arrives 13 s sooner is to be measured again, in pairs,
once the world load is no longer held up by VidMm.

## 7. Limits

- The saving depends on when the profile is accepted. Without any key
  press, run-world saw the front end end on its own (session G, +36.6 s).
  When the profile is accepted on its own is not measured here.
- What the cut changes on screen and in sound: the movie's pictures and
  sound after its first frame are not shown or played. Nothing in the
  front end or the world load that we saw depends on them, and the person
  found the cut sessions right.
- Another build needs new RVAs: the slots and the two state words.
