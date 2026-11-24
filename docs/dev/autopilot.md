# The autopilot: from the start to gameplay in one command

The autopilot drives the title through its screens by the title's own state,
read from its memory and its presented frames, then hands control back to the
player. It replaces `XTP_KEYS`, whose presses at fixed times break as soon as
start-up or loading gets faster or slower.

```text
cargo xtask play                  # from the golden save to gameplay, then you play
cargo xtask play --measure 60     # 60 s of gameplay, then a frame-rate summary
xtp run <exe> --execute --autopilot autopilot/gameplay.txt
```

The code is in `crates/xtp-hle/src/modules/autopilot/` (`script.rs` the
language, `engine.rs` the state machine, `autopilot.rs` the thread that runs
it against the title) and `xtask/src/play.rs`. Scripts ship in `autopilot/`.

## `cargo xtask play`

1. Builds the diagnostics build (`cargo build --profile diag -p xtp --features
   diagnostics`); cargo does nothing when it is up to date. `--no-build`
   skips it.
2. Copies the golden data folder into a run folder of the cache,
   `<cache>/play/<YYYYMMDD-HHMMSS>/data` (UTC). The golden folder itself is
   never written.
3. Runs `xtp run <game>/game_scarlett_master.exe --execute --autopilot
   <run>/autopilot.txt` with `XTP_DATA_DIR` set to the copy; the rest of the
   configuration is yours (`XTP_CONFIG` or `./xtp.toml`, `[input]` included).
   `--exe` names another executable, `--script` another script (default
   `autopilot/gameplay.txt`).
4. Shows the title's log and keeps it, without colours, in `<run>/run.log`;
   `<run>/autopilot.txt` is the script that ran.
5. Once the autopilot hands control back, the keyboard, the mouse and the pads
   are yours until you close the title.

With `--measure <s>` the script gets `exit after <s>` at its end: the title
ends that many seconds after the autopilot handed control back, so the
measurement covers gameplay only, however long start-up and loading took.
Then it prints the median, lowest and highest of each figure of the
frame-rate lines (`xtp_gpu::host`, one every 5 s) logged during gameplay:
fps, ms/frame, ms preparing, ms on the GPU, ms copying back, draws and dropped
draws per frame. It fails (exit code 1) when gameplay was not reached; when
the script stops on a timeout, it ends the title at once.

Set up once, in `xtp.toml` or the environment:

| Setting | What |
|---|---|
| `[paths].golden_data`, `XTP_GOLDEN_DATA` | a data folder kept as it is: first boot done, a save in the world. `cargo xtask play` refuses to run without it. |
| `XTP_SESSION_LOCK` (optional) | a file that says a game session runs: created with the process id on its first line for the run, removed after; `play` waits while a live process holds it. Build tools that wait for the same file keep the measurements clean. |

Only one title runs at a time: `play` refuses to start while an `xtp.exe`
runs. Keep the title's window in front during loading: the run-world analysis
saw sessions whose window was never activated stop presenting frames during
the world's loading.

## Scripts

One step per line; `#` starts a comment; keywords take any case.

```text
build <BLAKE3 of the title executable>
wait <condition> [timeout <duration>]
press <buttons> [[hold] <duration>]
repeat <buttons> [hold <duration>] every <duration> until <condition> [timeout <duration>]
release
exit after <duration>
```

| Step | What it does |
|---|---|
| `build <hash>` | The title build the script's RVAs belong to: the BLAKE3 of the executable, hex, all 64 digits or at least the first 16. `xtp run` prints it (`build ... (<hash>)`). A script that reads title memory must have one; on another build the autopilot refuses to start and `xtp run` stops before running the title. |
| `wait <condition>` | Waits until the condition holds. With `timeout`, a wait that lasts that long stops the script. Without, it waits as long as it takes. |
| `press <buttons>` | Holds the buttons for 200 ms (or the duration given), then leaves them up 100 ms so that a press right after is a new press. |
| `repeat <buttons> ... every <d> until <condition>` | Presses every `d` (held 200 ms, or `hold`) until the condition holds; it lets go at once when it does, even mid-press. It checks the condition first: when it already holds, nothing is pressed. `every` must be longer than `hold`. |
| `release` | Hands control back to the player: nothing is pressed after it (a script that presses after a `release` is refused). The end of the script does the same. |
| `exit after <d>` | Ends the title `d` after this step (exit code 0), while the script goes on; it comes due even after the script ended or stopped. The earliest of several wins. |

Durations are numbers with `ms` or `s` (`2500ms`, `2.5s`, `2.5 s`). Without a
unit, `hold` and `every` are in milliseconds, the rest in seconds.

Buttons are pad buttons named as in `[input.bindings]`: `a`, `b`, `x`, `y`,
`menu`, `view`, `dpad_up`, `dpad_down`, `dpad_left`, `dpad_right`,
`left_shoulder`, `right_shoulder`, `left_stick_click`, `right_stick_click`;
`a+menu` presses two together. They play on the pad that plays: on top of the
first host pad, or on the keyboard and mouse's stand-in pad when no pad is
connected. Triggers and sticks cannot be pressed yet.

### Conditions

| Condition | Holds when |
|---|---|
| `time <op> <duration>` | the time since the autopilot started (the title's start) compares so |
| `elapsed <op> <duration>` | the time since this step began compares so |
| `frames <op> <n>` | the frames the title presented since it started compare so |
| `u8\|u16\|u32\|u64 <rva> <op> <value>` | the title value at that RVA of its executable compares so |
| `<width> <rva> changed` | that value differs from what it was when this step began |
| `<width> <rva> nonzero` | that value is not 0 |
| `not c`, `c and d`, `c or d`, `( c )` | as usual; `not` binds tightest, then `and`, then `or` |

`<op>` is one of `==`, `!=`, `<`, `<=`, `>`, `>=`. Numbers are decimal or hex
(`0x74e007c`); values are unsigned, little-endian as the title stores them.
An RVA must be aligned to its width (read one byte of a word with `u8`) and
inside the title's image; the autopilot checks both before the title starts.

### The log

Each step is logged when it begins (`line 7: wait ...`), and each wait or
repeat when it ends, with how long it took, the frame count and the values
its condition reads:

```text
INFO xtp_hle::modules::autopilot: line 14: wait u32 0x74e2c1c == 0 or time >= 150s
INFO xtp_hle::modules::autopilot: line 14: met after 34.1 s (frame 1022, u32 0x74e2c1c = 0x0)
INFO xtp_hle::modules::autopilot: line 27: control handed back to the player
ERROR xtp_hle::modules::autopilot: line 22: timed out after 180.0 s (frame 2140, u32 0x9e58138 = 0x1, u32 0xab43664 = 0xe, u8 0x74dfa9d = 0x0); the script stops here and presses nothing more: the title is yours
```

`cargo xtask play` looks for "control handed back to the player" and "the
script stops here"; keep `HANDED_BACK` and `STOPS` in `engine.rs` and their
copies in `play.rs` alike.

## `autopilot/gameplay.txt`

From the golden save, nothing needs a press: the front end (profile screen,
intro, logo) ends by itself once the title has done its start-up work, 17.6 to
116 s after the start depending on the machine's load, and the save loads into
the world. The script waits for each stage by the title's flags, presses A
every 2.5 s only if the front end is still up after 150 s (a save that waits on
the profile screen), and hands control back at the world's first frame.

The values it reads (build `b99f876b…`, all in `.data`; the run-world analysis
found and validated them, docs/analysis/run-keys.md):

| RVA | Type | Values |
|---|---|---|
| `0x74e007c` | 4 byte flags | byte `0x74e007c` = 1 once start-up init is done; `0x74e007f` = 1 once the profile screen is passed; `0x74e007e`, then `0x74e007d` = 1 once the world starts loading |
| `0x74e2c1c` | u32 | 1 while the front end is up, 0 from the start of the world's loading |
| `0xab43664` | u32 | `0xe` during a loading screen (start-up, save to world), 0 otherwise; 0 again at the world's first frame |
| `0x9e58138` | u32 | 1 from start-up to the world's first frame, then 0 |
| `0x74dfa9c` | u32 | `0x1000000` out of the game, `0x1000100` in it (byte `0x74dfa9d` = 1) |

"In the world" is the world's first frame. A flag for "the player can move"
is not known yet; the world shows no image yet to find one by.

## Finding the RVAs again after a title update

A new executable has a new hash: `xtp run` prints it, and the scripts of the
old build are refused. Their values usually move with the update; find them
again as they were found:

1. Run the new build from the golden data folder a first time and keep a
   timeline of what the screen shows (`XTP_CAPTURE`, or notes with times).
2. Look for values in the executable's `.data` that change when the screen
   changes. XTP has no scanner built in: the run-world analysis read every
   u32 of `.data` twice a second with a local patch of `XTP_WATCH` and kept
   those whose changes lined up with the timeline. Start from the old RVAs:
   an update often shifts a whole block by the same amount, and the new
   values sit near `old RVA + shift`, with the same values and transitions.
3. Validate each candidate with `XTP_WATCH=<rva>,...` (diagnostics build: it
   logs the u32 at each RVA when it changes, polled every 5 ms) on a second
   run, ideally a slower or faster one: the same values must change at the
   same moments.
4. Prefer flags that change once per stage over counters, combine two
   signals for the stage that matters (the world: the loading flag back to 0
   and the in-game byte set), and read a byte with `u8` rather than a word
   whose other bytes move.
5. Write the new `build` line and RVAs in the script, run `cargo xtask play`
   three times in a row, and give the values in the commit (RVAs, types and
   values only: never the title's bytes or code).

The parser and the engine have unit tests over a fake title
(`cargo test -p xtp-hle autopilot`); `autopilot_shipped_scripts_parse` checks
that every script in `autopilot/` parses.
