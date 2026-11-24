# Reaching the world: scripted keys, the golden save and the title's state

How to get Grand Theft Auto VI from launch into the first mission's world,
reproducibly and without a person at the keyboard, on XTP at `main`
`2d0257e` (diagnostics build). Measured on 2026-11-24 on the person's
machine (RTX 4080 SUPER, Ryzen 9 7950X3D, 64 GB, Windows 11).

Scope, as the orchestrator set it: start from a save only. The first-boot
screens of an empty data folder are covered in one line (section 6) for
completeness.

Nothing of the game is in this document: screens are described in words,
title code and data by RVA and value. Run logs and captures stay under
`E:\xtp-data\runs\` (outside the repository).

## 1. Summary

- **Golden data folder** `E:\xtp-data\golden\data`: first boot done, the
  `Player` save written in the world. Every run copies it; nothing writes it.
- **Launcher** `E:\xtp-data\play.ps1` (outside the repository): builds the
  diagnostics build if asked, copies the golden folder into a new run folder,
  sets `XTP_CONFIG` and `XTP_KEYS`, starts the title and logs to
  `<run>\run.log`. `-Measure <s>` stops it and prints the world's frame-rate
  lines.
- **Keys**: `Enter` (pad A) every 2.5 s from 5 s to 50 s. One press counts:
  the one the profile screen accepts. Everything after it (intro, logo,
  loading screen, world) follows without input. There is no "Continue" menu
  in this build's start-up path: the save is loaded straight after the
  intro.
- **Timings vary a lot between runs**: the profile screen accepts A after
  17.6 to 46 s and the world comes 53 to 80 s after launch. Fixed delays
  therefore either press keys in the world or risk staying on the profile
  screen. Section 5 gives title variables (RVA, type, values) that an
  autopilot can read instead.

## 2. The golden data folder

| | |
|---|---|
| Path | `E:\xtp-data\golden\data` (contains `7952987A\`) |
| Made from | run `20261124-1414-B-save`: a fresh data folder taken through the first-boot screens (run `20261124-1410-A-fresh`), then started again from its save and taken into the world, where the title rewrote its `Player` container (`v4`) |
| Contents | `saves\…\SettingsProfileSettings~…\v1`, `saves\…\Player~…\v4`, `storage\settings\settings.xml`, `temp\telemetrycache.dat`; no `crashes.log` |
| Rule | read only; each run copies it (`play.ps1` does), so a run that crashes or saves never changes the next run's start |

The `Player` container is about 2 KB: a profile and progress record, not a
full story save. Starting from it, the title loads the first mission
(`story000` packs) directly.

To rebuild it: run from an empty data folder with the keys of section 6
until the world, let the title save (its `Player` container is rewritten
about 20 to 25 s after the world appears), stop it, and copy the data
folder without `crashes.log`.

## 3. The launcher `E:\xtp-data\play.ps1`

```powershell
E:\xtp-data\play.ps1                          # play: build, copy golden, run
E:\xtp-data\play.ps1 -WorldSeconds 60         # stop 60 s after the world appears
E:\xtp-data\play.ps1 -Measure 180             # stop after 180 s, print world fps lines
E:\xtp-data\play.ps1 -WorldSeconds 60 -Env @{ XTP_GPU_TIMING = '1' }
E:\xtp-data\play.ps1 -Repo E:\XTP-worktrees\<branch> -NoBuild
```

| Parameter | Default | Effect |
|---|---|---|
| `-Measure <s>` | 0 (until closed) | stops the title after that many seconds |
| `-WorldSeconds <s>` | 0 (no) | stops the title that many seconds after it enters the world (section 5's state words, read from the log) |
| `-Repo <dir>` | `E:\XTP` | checkout to build and run (`target\diag\xtp.exe`) |
| `-NoBuild` | off | skips the build |
| `-Golden <dir>` | `E:\xtp-data\golden\data` | data folder to copy |
| `-Keys '<XTP_KEYS>'` | section 4 | replaces the scripted presses (`''` for none) |
| `-Env @{…}` | none | extra environment for the title (`XTP_CAPTURE`, `XTP_WATCH`, …) |
| `-Name <s>` | none | suffix of the run folder |
| `-Sound` | off | unmutes the title (it is muted otherwise) |

What it does, in order, under the machine's rules for game sessions:

1. builds the diagnostics build in `-Repo` through
   `E:\xtp-data\tools\cargo-slot.ps1` (low priority, waits for game
   sessions), with `TMP`, `TEMP` and `CARGO_HOME` on `E:`;
2. takes `E:\xtp-data\measure.lock` by exclusive creation (its own process
   id on the first line; removed in a `finally`; a lock whose process is
   gone is removed as stale; another live holder is waited for);
3. waits up to 90 s for no `rustc.exe` to run, and reports the session as
   not clean if one still runs or appears during the session;
4. copies the golden folder into `E:\xtp-data\runs\play-<yyyyMMdd-HHmmss>[-name]\data`
   and writes that run's `xtp.toml` (game `E:\uwu`, cache
   `E:\xtp-data\cache`, data the copy);
5. sets `XTP_KEYS` and adds the state words of section 5 to `XTP_WATCH`;
6. starts `xtp.exe` through `E:\xtp-data\tools\XtpLaunch.cs` (compiled by
   `Add-Type`): no console window, an empty standard input, output and
   errors to `run.raw.log`, and its first window shown without activation
   (`SW_SHOWNOACTIVATE`), so the window someone works in keeps the focus;
7. mutes the title's audio sessions on every output device (Windows keeps
   the mute per executable path: an `xtp.exe` started by hand after a muted
   run stays muted until a run with `-Sound` or the volume mixer);
8. watches the log for the state words and prints "In the world at +N s";
9. at the end writes `run.log` (colour codes removed) and `times.txt`
   (start, end, timeout, `rustc` count at start and during the run, audio
   state, the title's exit code, front-end end and world times), then
   prints the world's frame-rate lines and any GPU failure or crash.

After the last scripted press, the keyboard (title window focused) and
XInput pads work as usual: nothing else is injected.

Checked: runs `play-20261124-142006-test1` (world at +74 s) and
`play-20261124-142538-C-timing` (+53 s) with the first version; the
session lock, the `rustc` wait, the muting and the exit code with
`play-20261124-143937-F-targets`; world detection and `-WorldSeconds`
with the session of section 4.1.

## 4. The key sequence from the save

`XTP_KEYS=5:Enter,7.5:Enter,…,50:Enter` (19 presses, each held 0.2 s; times
count from the title's first input poll, about 1 s after launch; `Enter` is
pad A for the keyboard stand-in pad and is sent with or without focus).

What the screens do, from the captures (one frame per second, three per
second around the transitions) and the title variables of section 5:

| Step | Screen | Run C | Run test1 | Run B | Run F |
|---|---|---|---|---|---|
| 1 | black, then boot loading (variable `0xab43664` = 0xe) | +2.5 → +5.5 s | | | +2.7 → +5.1 s |
| 2 | profile screen: dark background, two prompts at the bottom right (profile selector on Y; the signed-in profile, named PLAYER by XTP's offline user, on A) | left at +17.6 s | left at ≈ +38 s | on screen +4 s, left at ≈ +46 s | on screen from ≤ +38 s to ≥ +98 s |
| 3 | star-burst transition, intro montage (about 10 s), studio logo (about 3 s) | +18 → +31 s | +38 → +44 s | +46 → +60 s | not captured |
| 4 | loading screen: a slideshow of character art (`0xab43664` = 0xe) | +31.7 → +52.9 s | +44.6 → +73.7 s | ≈ +61 → +80 s | from +116 s; the title stopped itself at +155 s (`run-world.md` 7.3) |
| 5 | the world (first mission); renders black today | +52.9 s | +73.7 s | +80 s | – |

Times are seconds after launch (run C from its log's first line, test1 from
the memory scan's start, B and F from the process start). Run B pressed
Enter every 2.5 s until 95 s, test1 and C until 70 s, F until 50 s.

The profile screen leaves after 17.6 to 116 s. Nothing in the log explains
the spread (no HLE call or file activity precedes the change); the slow
runs are those during which other workers compiled (2 to 4 `rustc` in
runs E and F). In run B the screen ignored ten presses; in run F it left
66 s after the last one. So the press is probably not what ends it: the
title finishes some start-up work first. Section 4.1 tests a run without
any press.

Presses during the intro, logo and loading screen have had no visible
effect (the intro runs its full length in runs C and test1). Whether a
press that lands in the world does anything was not observed: the world is
black.

### 4.1 Without any key

RESULT-G-PLACEHOLDER

## 5. Title state variables for an autopilot

Found by reverse engineering for interoperability (authorised by the
person): a read-only scanner copied the title's `.data` section (RVA
0x67e2000, 0x43ec398 bytes) out of the running process every 0.5 s and kept
every 32-bit word that changes rarely, then kept the words whose changes
fall on the transitions of section 4 (run test1), and checked the same
words in a second scan (run C). The candidates were then logged with
`XTP_WATCH` (polled every 5 ms) in runs C and D. RVAs are relative to the
base of `game_scarlett_master.exe` (build blake3 `b99f876b74d4a77f…`);
another build moves them.

| RVA | Type | Values and meaning | Seen in |
|---|---|---|---|
| `0xab43664` | u32 | `0xe` (14) while a loading screen is up: the boot loading (≈ +2.5 → +5.5 s) and the save → world loading; `0` otherwise. Falls to 0 at the world's first frame. | test1, C, D |
| `0x74e2c1c` | u32 | `1` from the end of boot initialisation (≈ +2.5 s) while the front end runs (profile screen, intro, logo); `0` from the moment the title commits to loading the world (with the loading screen in runs test1 and C, 6 s before it in run D). `0x74f5bb4` (`0xd`) and `0x7453ed4` (`0x101`) cover the same span. | test1, C, D |
| `0x9e58138` | u32 | `1` from boot to the world's first frame, then `0`. | test1, C |
| `0x74dfa9c` | u32 | `0x1000000` outside the game, `0x1000100` from the world's first frame (byte `0x74dfa9d` = 1 in game). | test1, C |
| `0x7528c1c` | u32 | `0` until the profile screen accepts A, then `0xd`, about 0.7 s after the acceptance (candidate: two scans, not yet watched) | test1, C (scans) |
| `0x74e007c` | u32, four u8 flags | `0` → `0x1` at boot, then the three other bytes become 1 between the profile acceptance and the world load, **in an order that differs between runs** (byte `0x74e007f` first in runs test1 and C, last in run D). All four are 1 from the world load on. Not a reliable "profile accepted" signal. | test1, C, D |

An autopilot (in XTP, next to `XTP_KEYS`) can then:

1. press A (0.2 s) every 2.5 s while `0x74e2c1c` is 1 (presses before the
   profile screen is ready are ignored; presses during the intro and logo
   have had no effect), or, more tightly, while `0x7528c1c` is still 0;
2. press nothing once `0x74e2c1c` is 0: the world load needs no input, and
   that flag falls at least 20 s before the world appears, so no press can
   land in the world;
3. report "in the world" when `0x9e58138` falls to 0 (with `0xab43664` back
   to 0 and byte `0x74dfa9d` = 1); a measurement can start its clock there.

Not found yet: a "player controllable" flag. The world renders black, so
there is no visible moment (end of the intro cutscene, first input
accepted) to line a scan up with. The same scan, run once the world shows
an image, should find it: line its changes up with the first frame where
movement input moves the camera.

## 6. From an empty data folder (no longer required)

Run `20261124-1410-A-fresh`: `Enter` every 2.5 s from 5 s to 95 s took an
empty data folder through the first-boot screens (accessibility, language,
display and controls, then a "finish the setup?" screen with A to
continue), the intro and the loading screen into the world at +97 s
(loading from +75 s). Every first-boot screen takes A as its default
choice. The title wrote `SettingsProfileSettings` at the end of the setup
and `Player` in the world.

## 7. Notes for whoever measures

- The machine's rules for game sessions (set by the person on
  2026-11-24): one session at a time under `E:\xtp-data\measure.lock`; at
  most 3 minutes in the world per session and at least 10 minutes between
  two sessions, because a world frame holds the GPU for about a second and
  the desktop waits behind it; render-target dumps only for a few chosen
  frames; compilations through `E:\xtp-data\tools\cargo-slot.ps1`, which
  waits for the lock. `play.ps1` applies the lock, the `rustc` wait, the
  muting and the no-focus start; `-WorldSeconds` keeps the world part
  short.
- Three of the five world sessions of `run-world.md` that lasted over 40 s
  ended in a driver reset 38 to 64 s into the world. A window of about
  35 s after the world appears is what can be counted on today. A reset
  blanks every monitor for a moment; the title then exits with
  `host GPU failure` in its log and in `crashes.log`.
- The title stops itself when no frame completes for somewhere between
  about 17 and 35 s (a write to address 0 at RVA `0x23cf683`; its crash
  reporter writes a 32-35 MB minidump to `T:\`, in the run's data copy,
  and tries to upload it, which XTP's network policy blocks). Diagnostic
  switches that make frames that slow (`XTP_GPU_TIMING`, with or without
  `XTP_D3D12_DRED`) end the run that way.
- `XTP_GPU_TIMING` names the slow commands only with `XTP_D3D12_DRED=1`
  as well; without it each slow list is logged with `commands=0`, and with
  both the world is out of reach (25 s per frame on the loading screen).
- Timings vary with the machine's load: the profile screen left at 17.6 s
  on a quiet machine and at 116 s while other workers compiled. Compare
  runs only when their `times.txt` shows no `rustc`.
- After each batch of sessions, check that `E:\uwu` is unchanged: sizes
  and dates were compared with `E:\xtp-data\uwu-before.tsv` after the
  sessions of this report and matched.
