# Tests, developer tooling and the game-specific layer

Analysis of what XTP inherits from VIctory (imported at `ffe310e`), what xcl does in the same
areas, and the test strategy XTP should follow, ending in an ordered backlog.

xcl is GPL-2.0-only and is a **reference for behaviour only**: nothing below copies its code.
Where an xcl file or function is named, it is so a worker can read what it does and then write
XTP's own version. Crate names use the `xtp-*` names that the parallel rename introduces
(`crates/xtp-game`, `apps/xtp`, `xtask`); environment variables are written `XTP_*` for the same
reason (today they are `VICTORY_*`).

## 1. Scope and sources

### XTP (VIctory at ffe310e)

| Area | Files |
|---|---|
| Unit tests | `#[cfg(test)] mod tests` in about 90 files, 352 `#[test]` functions in total (shader translator, formats, HLE modules, loader, GPU objects) |
| Integration tests | `crates/xtp-loader/tests/load_probe.rs`, `crates/xtp-loader/tests/title_dlls.rs`, `crates/xtp-hle/tests/no_dll_planting.rs` |
| Synthetic title | `tests/fixtures/hle-probe` (a `cdylib`, not a workspace member, built on demand by `load_probe.rs`) |
| Game-data and tool tests | `crates/xtp-formats/src/bcpack.rs` and `xma.rs` (`#[ignore]` tests reading `*_BCPACK_*`, `*_XMA_*` variables); `crates/xtp-shader/src/emit/diff_tests.rs` and `emit/mod.rs` (print "skipped" without DXC or an SM 6.6 device) |
| Repository tooling | `xtask/src/main.rs` (`policy`, `install-hooks`), `xtask/src/policy.rs`, `.githooks/pre-commit`, `.cargo/config.toml` (the `cargo xtask` alias), `.victory-root` (marker) |
| CI | `.github/workflows/ci.yml`: `windows-latest`, clippy without and with `--all-features`, `cargo test --workspace --locked` |
| Runtime tooling | `crates/xtp-core/src/{config.rs,logging.rs,policy.rs}`, `crates/xtp-loader/src/{crash.rs,stacks.rs,thunks.rs}`, the diagnostics switches listed in `README.md` ("Debugging") |
| CLI | `apps/xtp/src/{main.rs,inspect.rs,census.rs,shader.rs,translate.rs,run.rs}` |
| Game-specific layer | `crates/xtp-game/src/lib.rs` (`KnownBuild`, `KNOWN_BUILDS` (1.0.2.1), `identify`), `crates/xtp-game/src/sgd5.rs` (the engine's shader file), `crates/xtp-game/src/watchdog.rs` (the opt-in watchdog hook) |
| Policy | `docs/asset-policy.md`, `xtp.example.toml` |

### xcl (E:/xtp-refs/xcl, read only)

| Area | Files |
|---|---|
| Unit tests | `#[cfg(test)] mod tests` in the runtime crate |
| Integration and e2e tests | `crates/xcl/tests/*.rs` (15 files: `e2e.rs`, `console_dlls_e2e.rs`, `gpu_pso_e2e.rs`, `bcpack_parity.rs`, `xg_oracle.rs`, …) |
| Synthetic titles | `tests/testtitle` (a console *executable*, about 2 900 lines with `gpu.rs` and `audio.rs`), `tests/testgamedll` (a "shipped with the game" DLL), `tests/data/testgame` (its game folder, `MicrosoftGame.config` included) |
| Checked-in test data | `tests/data/bcpack/*.bin` (+ `generate.py`), `tests/data/llvm-amdgpu/*.txt` (third-party, with its licence) |
| Game-data tests | `XCL_SAMPLE_RPF`, `_DLLS`, `_TTPF`, `_BCPACK`, `_BK2`, `_BVH4`, `_AWC`, `_XG`, `_DXC`, `_FSPX` |
| GPU-dependent tests | `gpu/recomp/runner.rs` `skip_test`, `XCL_REQUIRE_GPU` |
| Scripts | `scripts/test.ps1`, `scripts/test-native-stderr.tests.ps1`, `scripts/smoke.ps1`, `scripts/pr_check.py` + `scripts/test_pr_check.py`, `scripts/gen_*.py` (generators of checked-in tables) |
| Runtime tooling | `crates/xcl/src/report.rs`, `log.rs`, `config.rs`, `preflight.rs`, `launcher.rs` (`summarize`, `pick_exe`), `host.rs` (`XCL_RUN_SECONDS`, `XCL_ACTIVITY`), `stubs.rs` (per-stub call counts) |
| CLI | `crates/xcl/src/main.rs` (`run`, `inspect`, `disasm`, `shaders`, `rpf`, a double-click menu), `inspect.rs` |
| Game-specific code | none in a layer of its own; `rpf.rs` / `rpf_probe.rs` (RPF9 archives) and `shaders.rs` (shader packs) sit in the runtime crate |
| Process | `CONTRIBUTING.md` ("What a PR needs to be merged", "Tests"), `AGENTS.md`, `HANDOFF.md` |

### AnyPS5 (E:/xtp-refs/AnyPS5), for the CI and run-report notes only

`docs/dev/GAME_TESTING.md` ("Reporting a result"), `tools/check_skips.py` (skips classified as
device limitations), `tools/run_report_summary.py`, `tools/title_run.py`. Another task turns
these into XTP's contributor process; section 5.6 lists only what that CI should run.

## 2. Summary

VIctory is stronger on **correctness and safety**: an asset-policy allowlist enforced by a
test, a fixture of ours built on demand, a pure and tested config, a detailed crash handler. xcl
is stronger on **contributors at scale**: one convention for tests on game data, counted skips,
a synthetic console title run end to end through the CLI, a `report.json` for every run, and a
timed smoke run compared against `main`. XTP keeps VIctory's foundations and rewrites, after
xcl's behaviour, what makes results **comparable between contributors**: sample convention,
skip marker, run report, timed smoke run, test title, preflight. The scripts become `cargo
xtask` commands in Rust, tested like the rest.

## 3. Capability matrix

| Capability | XTP today (VIctory) | xcl | Verdict |
|---|---|---|---|
| Unit tests | 352, in-file, pure where possible (config takes an injected `env` closure) | in-file | **Keep VIctory** |
| Integration tests with a fixture we wrote | `hle-probe` `cdylib`: loader, binding, TLS, `DllMain`, delay loads, title DLLs, in process | `testtitle` exe + `testgamedll` through `xcl run`, out of process | **Keep** `hle-probe`; **new work**, after xcl: an executable test title run by `xtp run` |
| Checked-in test data | none (policy forbids binaries; one exception: BCPack tables) | binaries under `tests/data/` up to 1 MB, some third-party | **Keep VIctory**: data is generated by the test |
| Tests on the contributor's game files | `#[ignore]` + ad hoc variables; `--ignored` without the variable panics | `XCL_SAMPLE_*`: unset passes doing nothing, set but unreadable fails; `test.ps1` lists the unset ones | **Rewrite after xcl** (`XTP_SAMPLE_*`, a helper, also read from `xtp.toml`) |
| Host-dependent tests (GPU, DXC, dxil.dll) | `eprintln!("…; skipped")` + `return`, wording varies, not counted | `skip_test(reason)`: one marker line, counted by `test.ps1`, `XCL_REQUIRE_GPU` turns skips into failures | **Rewrite after xcl** |
| Test runner | `cargo test --workspace` | `scripts/test.ps1` (build, tests, skip summary) + a test of the script | **New work**: `cargo xtask test` |
| Differential / oracle tests | shader diff tests (console DXC vs PC DXC, drawn on the host GPU) | console DXC as test generator, XG oracle, BCPack parity | **Keep VIctory**, move onto the sample convention |
| Smoke runs | none: `run --execute` runs until the title exits | `smoke.ps1`: fixed time (`XCL_RUN_SECONDS`), activity lines, an `xcl-smoke v1` block | **Rewrite after xcl** (`xtp run --seconds`, `cargo xtask smoke`) |
| Comparison against `main` | none | `pr_check.py`: frames ≥ 80 % of main's, peak commit ≤ 125 %, no crash, no device removal | **Rewrite after xcl** (`cargo xtask smoke-compare`); enforcement belongs to the process task |
| Crash reports | vectored + unhandled filters, unwound frames as `module+rva`, the title's last debug output, `crashes.log`, panics logged, `*_CRASH_SAVE`, `*_STACKS` | crash text stored in `report.json`, kept while another thread writes it | **Keep VIctory**; add the crash to the run report |
| Run reports | stdout only: bind report per image (hle / host / title / missing); one global unimplemented-call counter | `report.json`: imports by source, stubs called (count, first caller), modules, unmapped paths, shader / D3D12 / audio stats, environment checks, exit code, crash; summary printed at exit | **Rewrite after xcl** |
| Logging | `tracing`, `XTP_LOG` filter, capped at info without `diagnostics`, stderr only | own logger: a file per run, uptime + thread id, flush on warnings, info echoed to stderr | **Keep VIctory**, **add** a file layer with xcl's line shape |
| Config | one `xtp.toml`, env > file > default, every path checked outside the repo, `xtp config` | per-title `xcl.toml` created on first run, many runtime knobs with env overrides, a boot record for the host process | **Keep VIctory**; per-title settings only when a knob needs them |
| Preflight | `xtp config` checks paths only | `preflight.rs`: DXC, XGameRuntime, GPU (SM 6.6, mesh, wave width, WARP), levels with a fix text, stored in the report | **Rewrite after xcl** (`xtp doctor`) |
| CLI | `inspect`, `census`, `lib-exports`, `shader-info`, `translate`, `run <exe>`, `config` (clap) | `run <folder|exe>`, `inspect --json`, `disasm`, `shaders`, `rpf`, a double-click menu | **Keep VIctory**; add `--json`, `run <folder>`, `doctor` |
| Asset policy | `cargo xtask policy` (allowlist, size, magic, NUL, encoded runs), pre-commit hook, a test in `cargo test`, `ensure_outside_repo` at runtime | file-name and extension regex on the PR's file list | **Keep VIctory** (stricter, and runs offline) |
| CI | Windows, clippy ×2, tests | `CI` + `PR checks` (description, smoke blocks) | **Keep VIctory**, extend (5.6) |
| Game-specific layer | `xtp-game`: build identities (BLAKE3), SGD5 reader, policy on what may be stored | none as a layer; RPF9 and shader-pack scanning inside the runtime | **Keep VIctory's separation**; **new work** for hooks, signatures, build data |

## 4. Sub-areas and verdicts

### 4.1 Unit tests: keep VIctory

VIctory's unit tests are pure where it matters: `Config::resolve` takes the environment as a
closure, the policy check is a pure `check(path, content)`, formats are tested on data the test
builds (`sgd5::tests::record_bytes`), and `docs/asset-policy.md` says "assert on properties,
never on snapshots". Keep these conventions (5.5). One caution, from
`crates/xtp-hle/tests/no_dll_planting.rs`: a test that changes process-wide state (current
directory, environment) needs a test binary of its own or a child process (as xcl's
`gpu_pso_e2e.rs` `sample_pack_read_errors_fail_instead_of_skipping` does), or it races the
other tests of its binary.

### 4.2 Synthetic test titles: keep `hle-probe`, add a test title (new, after xcl)

`hle-probe` tests the loader in process: it is a `cdylib` importing from DLLs that do not exist
on a PC (`raw-dylib`), built into `CARGO_TARGET_TMPDIR` by the test, and called through its
exports. It covers mapping, binding (HLE, host, title DLL, delay load, missing import), TLS
callbacks and `DllMain`. It stays: it is fast and pinpoints loader regressions.

What XTP lacks is xcl's second level: a console **executable** of ours started by the real CLI,
so the whole path a title takes is tested (launcher, console drives, storage, run report, exit
code). Its protocol is worth reproducing in our own code: each check prints `[PASS]` or
`[FAIL]`, the last line states the failure count, the exit code is that count; the e2e test
(`crates/xcl/tests/e2e.rs`, `test_title_runs_and_game_folder_is_untouched`) asserts no failure,
a complete run, a byte-for-byte unchanged game folder, writes where the runtime puts them, and
the expected `report.json`.

For XTP: `tests/fixtures/test-title` (not a workspace member, like `hle-probe`), driven by
`apps/xtp/tests/test_title.rs`. Its game folder, `MicrosoftGame.config` included, is **written
by the test** into a temporary directory: the policy rejects `.config` files and needs no
exception for it. Each subsystem PR then adds its checks to the title (a rule for contributors,
not a backlog item).

### 4.3 Checked-in test data: keep VIctory

xcl commits binary test data under `tests/data/` (BCPack streams with their generator, a
third-party instruction listing). XTP should not: the allowlist is the simplest rule to enforce
and to explain, and every input needed so far can be built by the test (`record_bytes`, the
probe fixture). Where expected values come from a real console or SDK tool, the test reads them
from the contributor's files (4.4), as VIctory's BCPack and XMA parity tests already do.
Generated constant tables follow the existing exception process.

### 4.4 Tests on the contributor's game files: rewrite after xcl

Today these tests are `#[ignore]` and read variables with `expect`, so `cargo test -- --ignored`
panics on a machine without the files, and a contributor who has the files must know each
variable name and remember `--ignored`. xcl's convention is better for many contributors:

- one family of variables, `XCL_SAMPLE_<KIND>`, each naming a file or folder the contributor
  owns;
- unset: the test passes without doing anything and says so;
- set but unreadable (missing, a folder instead of a file): the test **fails** and names the
  variable and the path, so a typo is never mistaken for a pass (xcl tests the skip itself, in a
  child process);
- the runner prints which variables are unset.

XTP's version (backlog B1–B3): `XTP_SAMPLE_<KIND>` variables, also readable from a `[samples]`
table in `xtp.toml` (env wins, as for paths today), and defaults derived from `game_dir` where
the file has a known place in the dump (the executable, an archive). A helper in a test-support
crate returns the path or skips with the standard marker (4.5). `#[ignore]` stays for benchmarks
only.

Tests on samples assert properties (every SGD5 record unpacks to its stated size, every
container translates or is counted as unsupported, the BLAKE3 of the executable is a known build
or is reported) and never print or compare game bytes.

### 4.5 Host-dependent tests and the skip marker: rewrite after xcl

The CI machine has no GPU (WARP only), no console SDK and no game. A test that cannot run there
must say so in a form a tool can count. xcl prints one line, `SKIPPED <test>: <reason>`, from a
single function, and its runner adds the reasons up; `XCL_REQUIRE_GPU=wave64,mesh` turns the
matching skips into failures on a machine that is supposed to have the feature.

XTP: one helper (`xtp_testkit::skip`) used by every such test, a fixed set of reasons (`no
sample XTP_SAMPLE_<KIND>`, `no GPU`, `no SM 6.6`, `no DXC`, `no dxil.dll`, `no console DXC`),
and `XTP_REQUIRE=<reason>,…` to make the listed ones fail. The shader diff tests and the `emit`
tests move onto it.

### 4.6 Test runner: new work in xtask

xcl's `scripts/test.ps1` builds, runs the suite with a fixed thread count, collects the skip
lines and prints a summary; it needed a script test of its own (`test-native-stderr.tests.ps1`)
because Windows PowerShell 5.1 treats native stderr as errors. XTP writes this once in Rust:
`cargo xtask test` runs `cargo test --workspace --locked`, passes its output through, and ends
with the counts a PR must report (passed, failed, ignored, skipped by reason, samples not set).
Its parser is unit-tested on captured libtest output.

### 4.7 Smoke runs: rewrite after xcl

A runtime change has to be checked on a real title, comparably between contributors. In xcl the
runtime ends the title after N seconds, GPU drained, so log and report are complete
(`XCL_RUN_SECONDS`), and logs an activity line every 2 s (`XCL_ACTIVITY`); `smoke.ps1` runs the
release build with networking off and prints a fixed block (format version, commit and dirty
flag, GPU, title, seconds, exit, frames, presents per 2 s, device removals, crashes, peak
commit); `pr_check.py` compares a `main` block with the PR head's (same title and time) and
refuses under 80 % of main's frames, over 125 % of its peak commit, any crash or device removal
on the PR, or a killed run.

XTP keeps the idea and fixes its weak point: xcl's block is scraped from log text with regular
expressions. XTP's smoke tool reads only the run report (4.9), which has a schema, and the block
it prints is generated from it. `xtp run --seconds N` (a flag, not only an env var), `cargo
xtask smoke` and `cargo xtask smoke-compare` are backlog B9–B11. The title is identified by its
build identity (`xtp-game`), not by its folder name.

### 4.8 Crash reports: keep VIctory

`crates/xtp-loader/src/crash.rs` already does more than xcl: it reports first-chance faults that
end a process, unhandled exceptions of any kind (C++ exceptions included), our own panics,
unwound frames as `module+rva`, the memory an access hit, and the title's last debug output; it
appends all of it to `crashes.log` in the title's data folder, and emulates the AMD-only
instructions a title may use on Intel CPUs. `*_STACKS` and `*_CRASH_SAVE` cover hangs and engine
state. What is missing is a link to the run report: the first crash text (and where it was
logged) should be kept in `report.json`, even when the crash happens while the report is being
written. xcl's `report.rs` `write_final` / `lock_within` show the behaviour to reproduce: a
bounded wait for the writer, never an endless one, because the thread holding the lock may be
the one that crashed.

### 4.9 Run reports: rewrite after xcl

VIctory prints its bind reports to stdout and keeps one global counter of unimplemented calls:
enough at a terminal, not for a hundred contributors filing results. xcl writes `report.json`
at the end of every run and on a crash, adds the exit code and environment checks, and prints a
summary ending with the report and log paths. What makes it a to-do list: imports by how they
were resolved, stubs called with count and first caller, unmapped paths, presented frames.

XTP's report (B5–B7), written by `apps/xtp` into the title's data folder:

- `schema` (an integer, bumped on incompatible changes), XTP version and commit, the build
  identity of the executable (BLAKE3, known label or unknown), title id, console model,
  command-line switches that change behaviour (`--strict`, `--unimplemented`), the `XTP_*`
  variables set;
- per image: hle / host / title / missing counts, the missing symbols;
- unimplemented imports called: `dll!symbol`, calls, first caller as `module+rva` (needs
  per-thunk counters in `xtp-loader`);
- how the run ended (exit code, time limit, crash with its text, killed), duration, frames
  presented, peak committed memory, device removals;
- the preflight checks (4.11).

Privacy: paths are written relative to `game_dir` or the data folder, never with the user's
profile path; the report holds no bytes from the game, only names of console APIs and our own
counters.

### 4.10 Logging: keep VIctory, add a file

`tracing` with `XTP_LOG` and the `diagnostics` cap is the right base and is used across every
crate; xcl's hand-written logger brings nothing to the API. What xcl does better is where logs
go: one file per run in the title's data folder, every line with the time since start and the
thread id, warnings and errors flushed at once (so a crash does not lose them), and the path
printed at the end. XTP adds a file layer to `xtp_core::logging::init` with that line shape,
opened once before title code runs (xcl's `log.rs` note: opening it later could re-enter hooked
file APIs), and refuses a path inside the repository through `ensure_outside_repo`.

### 4.11 Config and preflight: keep VIctory's config, rewrite the preflight

Keep `xtp.toml` with env > file > default and the outside-the-repository check; add `[samples]`
(4.4). xcl's per-title `xcl.toml` and its boot record answer needs XTP does not have yet (a
separate host process, per-title GPU memory knobs); add per-title settings only when one is
needed.

xcl's preflight is worth reproducing: before the title starts, check what the PC has of what a
run needs, give each missing piece a level (ok, note, warning, error) and a fix text, print
them, repeat the warnings at the end, and store them in the report. The checks are a pure
function of what was found, so they are unit-tested without the hardware. For XTP: a D3D12
device (and whether it is WARP), Shader Model 6.6, `dxil.dll` (the validator the translator
uses), the CPU features the title needs, the configured paths, and whether `game_dir` holds a
known build. `xtp doctor` prints them; it absorbs `xtp config`.

### 4.12 CLI: keep VIctory, extend

VIctory's clap-based CLI covers inspection, import census, shader corpus statistics and
translation, and running. Additions after xcl's behaviour: `--json` on `inspect` and `census`
(machine-readable output for tests, scripts and issue reports; xcl `inspect.rs` `run(path,
json)`), `run` taking a game folder and picking the executable from `MicrosoftGame.config` or
the only candidate (xcl `launcher.rs` `pick_exe`, `resolve_target`, and the e2e test
`launcher_uses_configured_exe_in_an_ambiguous_folder`), and `doctor`. xcl's double-click menu is
a user feature, not tooling: out of scope here.

### 4.13 Asset policy, hooks and CI: keep VIctory

VIctory's `cargo xtask policy` is an allowlist with content checks, runs in the pre-commit hook
and as a test in `cargo test`, so it holds even for a contributor who never installed the hook.
xcl's PR check matches file names against a list of game extensions, which is weaker (an unknown
extension passes). Keep VIctory's; only the marker file is renamed (`.xtp-root`) by the rename
task. CI is covered in 5.6.

### 4.14 The game-specific layer: keep the separation, build it out (new work)

`xtp-game` is the one crate that knows Grand Theft Auto VI. Today it holds the SGD5 shader-file
reader (with tests on synthetic records) and an empty table of known builds looked up by BLAKE3;
`xtp run` prints the build. `docs/asset-policy.md` already says what may live there: build
identities, per-build offsets of the functions we hook under names we choose, byte signatures,
small interoperability constants.

xcl has no such layer: what is specific to one title (RPF9 archive scanning, shader packs, title
notes in `HANDOFF.md`) is spread through the runtime. XTP should keep the boundary, and test it
at three levels:

1. **Unit, in CI**: signature matching (found, absent, ambiguous) and offset tables checked
   against synthetic images built by the test; build lookup.
2. **Samples, local**: on the contributor's executable, every hook of the identified build is
   located exactly once by its signature and agrees with its per-build offset; SGD5 and archive
   readers on the contributor's files.
3. **Smoke, local**: the title reaches a stated point (frames presented) in the smoke run.

The format readers that only this game uses (SGD5 today, the RPF9 archive reader when it is
needed) belong in `xtp-game` too; xcl's `rpf_probe.rs` shows one behaviour worth keeping, a
bounded-memory header probe for archives of several gigabytes.

## 5. XTP's test strategy

### 5.1 Tiers

| Tier | What | Where it runs | Needs |
|---|---|---|---|
| T0 static | `cargo fmt --check`, clippy (default and all features), `cargo xtask policy` | CI and pre-commit | nothing |
| T1 unit | `#[cfg(test)] mod tests`, pure, data built by the test | CI and locally | nothing |
| T2 fixtures | `hle-probe` (in process), the test title through `xtp run` (out of process) | CI and locally | Windows |
| T3 host-dependent | translator output validated by `dxil.dll`, pipelines created, draws on WARP or a GPU, diff tests against a console DXC | locally; in CI what WARP and the runner can do | a GPU, the Windows SDK, the console SDK's DXC: skipped with the marker otherwise |
| T4 samples | tests reading `XTP_SAMPLE_*` (formats, translator on the game's shaders, build identity and signatures) | locally, on the contributor's files | the contributor's dump: skipped with the marker otherwise |
| T5 smoke | `cargo xtask smoke` on the game, a fixed time, from `main` and from the branch | locally, required for runtime changes | the game and a GPU |
| T6 run reports | `report.json` of every `xtp run`, compared by `smoke-compare` and attached to bug reports | locally | as T5 |

### 5.2 What runs in CI (GitHub-hosted Windows, no game data)

T0, T1, T2 and the part of T3 the runner can do. Every T3 and T4 test that cannot run skips with
the marker; `cargo xtask test` lists the skips, so a reviewer sees what the run did not cover.
Nothing in CI ever reads a game file, and no CI job may be given one (no secrets holding dumps,
no caches of derived data).

### 5.3 What runs on the contributor's machine

- Always: `cargo xtask test` (T0–T4 with what the machine has), whose summary goes into the PR.
- With the game: set `game_dir` (and `[samples]` for files not under it) in `xtp.toml`, outside
  the repository; the same command then runs T4.
- For a change to the runtime (any crate `xtp run` links, outside tests): two `cargo xtask
  smoke` runs, one built from `main`, one from the branch's head with a clean tree, same build
  of the game and same duration, and `cargo xtask smoke-compare` on the two reports.
- For a bug report: the `report.json` and the log of the failing run (relative paths, no game
  bytes).

### 5.4 How to skip cleanly

1. A test skips only through `xtp_testkit::skip(reason)`; it prints `SKIPPED <test>: <reason>`
   on stderr and returns. It never `#[ignore]`s itself to pass, and never weakens an assertion
   when the input is missing.
2. A sample variable that is **set** but cannot be read fails the test with the variable and the
   path in the message.
3. `#[ignore]` is for benchmarks and long runs, with the command to run them in the reason
   string.
4. `XTP_REQUIRE=<reason>,…` turns the listed skips into failures, for a machine that must cover
   them (a maintainer's GPU box).
5. Skips are reported, never counted as coverage: the summary of `cargo xtask test` lists them
   by reason.

### 5.5 Rules for writing tests

- A bug fix comes with a test that fails before it; new logic comes with a test of it. Docs,
  logging and renames need none.
- Inputs are built by the test or read from the contributor's files; nothing derived from the
  game is written into a test, a snapshot or an assertion message.
- Assert properties (sizes agree, every record unpacks, a hook is found once), not dumps.
- Process-wide state is changed only in a test binary of its own or in a child process.
- Tests write only to temporary directories or the configured cache and data folders;
  integration tests set `XTP_DATA_DIR` and `XTP_CACHE_DIR` to temporary ones.

### 5.6 What the CI should run (input for the contributor-process task)

On `windows-latest`, on every pull request and push to `main`:

1. `rustup toolchain install` (pinned by `rust-toolchain.toml`), the cargo cache;
2. `cargo fmt --all --check` (missing today, and the tree is not rustfmt-clean: 35 files; B0);
3. `cargo clippy --workspace --all-targets --locked -- -D warnings`, and the same with
   `--all-features`;
4. `cargo xtask test` (which runs `cargo test --workspace --locked`, the policy test included,
   and prints the skip summary); until B4 lands, `cargo test --workspace --locked`;
5. once B10 exists, the smoke tool on the test title (`--seconds 5`), to prove the timed run and
   the report work on a machine without a GPU.

A description check (the process task's, after xcl's `pr_check.py` and AnyPS5's `check_*.py`)
can require the test summary and, for runtime changes, two smoke blocks; it should call `cargo
xtask smoke-compare` (B11) so the thresholds live in one tested place.

## 6. Backlog

Ordered: each item can start once its dependencies have landed. Sizes: S (a few hours), M (one
worker session), L (one long session; none is larger). "xcl reference" is for behaviour only:
read it, do not copy it.

### B0. Make the tree rustfmt-clean (S; depends: the crate rename)

- **Goal**: `cargo fmt --all --check` passes, so CI can require it (5.6). Today 35 files differ
  (mostly `xtp-gpu`, `xtp-hle`, `xtp-formats`). One formatting-only commit, landed when no other
  code branch is open, right after the rename, since it touches every busy file.
- **Files**: the 35 files `cargo fmt --all --check` lists; nothing else.
- **xcl reference**: none (xcl is not rustfmt-clean and forbids whole-file formatting: the
  situation XTP avoids by fixing it once).
- **Acceptance**: `cargo fmt --all --check` exits 0; `cargo clippy` and `cargo test` unchanged;
  the diff is whitespace and line breaks only (`git diff -w --stat` close to empty).

### B1. Test-support crate with the skip marker (S; depends: none)

- **Goal**: one way to skip: `xtp_testkit::skip(reason)` prints `SKIPPED <test>: <reason>` and
  returns; `XTP_REQUIRE` turns listed reasons into a panic.
- **Files**: `crates/xtp-testkit/{Cargo.toml,src/lib.rs}` (new, dev-dependency only), workspace
  `Cargo.toml`.
- **xcl reference**: `crates/xcl/src/gpu/recomp/runner.rs`, `skip_test`.
- **Acceptance**: unit tests: the marker names the current test; with `XTP_REQUIRE=no-gpu` the
  helper panics for that reason and not for another (run in a child process of the test binary).

### B2. Sample files: `XTP_SAMPLE_*` and `[samples]` (M; depends: B1)

- **Goal**: `xtp_testkit::sample("SGD5")` returns the path from `XTP_SAMPLE_SGD5`, else
  `[samples].sgd5` in `xtp.toml`, else a default under `game_dir` when one is defined; unset
  skips (B1); set but unreadable panics naming the variable and the path.
- **Files**: `crates/xtp-testkit/src/lib.rs`, `crates/xtp-core/src/config.rs` (`samples` table),
  `xtp.example.toml`.
- **xcl reference**: `crates/xcl/tests/gpu_pso_e2e.rs`,
  `sample_pack_read_errors_fail_instead_of_skipping`; `scripts/test.ps1` (the list of unset
  variables).
- **Acceptance**: a test re-running its own binary three times: unset passes with the marker, a
  missing path fails with the variable and path in stderr, a folder where a file is expected
  fails; `Config::resolve` tests for the new table (env over file).

### B3. Move existing game-data and tool tests onto B1/B2 (M; depends: B2)

- **Goal**: no test needs `--ignored` or panics on a missing variable; the BCPack and XMA parity
  tests, the shader diff tests and the `emit` tests skip through B1/B2 with the standard
  reasons.
- **Files**: `crates/xtp-formats/src/bcpack.rs`, `crates/xtp-formats/src/xma.rs`,
  `crates/xtp-shader/src/emit/diff_tests.rs`, `crates/xtp-shader/src/emit/mod.rs`, `README.md`
  (variable names).
- **xcl reference**: `CONTRIBUTING.md`, "Tests" (the rule, not the text).
- **Acceptance**: `cargo test --workspace` on a machine without the files: same tests pass, the
  former `#[ignore]` data tests now run and print `SKIPPED`; benchmarks stay `#[ignore]`; with
  the variables set, the parity tests run and pass.

### B4. `cargo xtask test` with the summary (M; depends: B1)

- **Goal**: one command for contributors and CI: runs `cargo test --workspace --locked`, streams
  its output, ends with passed / failed / ignored counts, skips by reason and the sample
  variables not set; exit code of the tests.
- **Files**: `xtask/src/main.rs`, `xtask/src/test.rs` (new).
- **xcl reference**: `scripts/test.ps1`; `scripts/test-native-stderr.tests.ps1` (what went wrong
  with a script runner).
- **Acceptance**: unit tests of the summary parser on captured libtest output (several test
  binaries, skips, a failure); a failing test makes the command exit non-zero.

### B5. Per-symbol counts for unimplemented imports (M; depends: none)

- **Goal**: each unimplemented-import thunk records its call count and its first caller
  (`module+rva`), readable as a list, besides the existing global counter.
- **Files**: `crates/xtp-loader/src/thunks.rs`, `crates/xtp-loader/src/lib.rs`,
  `crates/xtp-loader/tests/load_probe.rs`.
- **xcl reference**: `crates/xcl/src/stubs.rs` (per-stub counters, first caller); `report.rs`
  `build` (`called_stubs`, sorted by count).
- **Acceptance**: `load_probe` calls `probe_missing` twice and asserts one entry
  `victory-probe-missing.dll!VIctoryProbeNotImplemented` (renamed with the fixture) with 2 calls
  and a first caller inside the probe image.

### B6. Run report: schema, writer and summary (M; depends: B5)

- **Goal**: `xtp run` writes `report.json` (4.9) into the title's data folder at the end of the
  run, then prints a short summary ending with the report and log paths.
- **Files**: `apps/xtp/src/report.rs` (new), `apps/xtp/src/run.rs`, `apps/xtp/Cargo.toml`
  (`serde_json`).
- **xcl reference**: `crates/xcl/src/report.rs` (`Report`, `build`, `load`);
  `crates/xcl/src/launcher.rs` `summarize`.
- **Acceptance**: unit tests: a report round-trips through JSON; paths are written relative to
  `game_dir`; an older `schema` is refused by `load`. `xtp run <hle_probe.dll>` without
  `--execute` writes a report with its bind counts (integration test).

### B7. Crash, end and log in the report; the log file (M; depends: B6)

- **Goal**: the first crash text is kept in the report even when the crash happens while it is
  written (bounded wait); how the run ended is recorded; `xtp_core::logging` also writes
  `<data>/logs/xtp.log` (uptime, thread id, flushed from warnings up).
- **Files**: `apps/xtp/src/report.rs`, `crates/xtp-loader/src/crash.rs`,
  `crates/xtp-core/src/logging.rs`, `apps/xtp/src/run.rs`.
- **xcl reference**: `crates/xcl/src/report.rs` `write_final`, `lock_within` and its test
  `a_crash_is_kept_while_another_thread_writes_the_report`; `crates/xcl/src/log.rs` `init`,
  `write`.
- **Acceptance**: unit test: a crash recorded while another thread holds the writer is in the
  next write; logging test: a warning reaches the file with a thread id; a log path inside the
  repository is refused.

### B8. The XTP test title and its e2e test (L; depends: B6)

- **Goal**: `tests/fixtures/test-title`, a console executable of ours (`raw-dylib` imports of
  console DLLs), with first checks (runtime init, a write to `T:`, a storage write, a read of
  `G:`), `[PASS]`/`[FAIL]` lines and the failure count as exit code; an e2e test that builds it,
  writes its game folder (`MicrosoftGame.config` included) into a temporary directory, runs `xtp
  run --execute` with temporary data and cache folders.
- **Files**: `tests/fixtures/test-title/**` (new), `apps/xtp/tests/test_title.rs` (new),
  `Cargo.toml` (`exclude`).
- **xcl reference**: `tests/testtitle/src/main.rs` (the PASS/FAIL protocol),
  `crates/xcl/tests/e2e.rs` `test_title_runs_and_game_folder_is_untouched`.
- **Acceptance**: the e2e test asserts no `[FAIL]`, the final line, exit code 0, the game folder
  unchanged (a hash of every file before and after), the written files where the runtime puts
  them, and the report's exit code and missing-import list; it passes in CI.

### B9. Timed runs and activity lines (M; depends: B7, B8)

- **Goal**: `xtp run --seconds N` ends the title after N seconds after the GPU has drained, with
  the report complete and its end recorded as the time limit; `--activity` logs every 2 s the
  presents and committed memory since the last line.
- **Files**: `apps/xtp/src/main.rs`, `apps/xtp/src/run.rs`, `crates/xtp-hle/src/runtime.rs`, the
  frame counter in `crates/xtp-gpu`.
- **xcl reference**: `crates/xcl/src/host.rs` (`XCL_RUN_SECONDS`, `XCL_ACTIVITY`).
- **Acceptance**: the test title gains a "wait until ended" mode; the e2e test runs it with
  `--seconds 2` and asserts the run ends within a bound and the report says "time limit".

### B10. `cargo xtask smoke` (M; depends: B9)

- **Goal**: builds nothing itself; runs a given `xtp.exe` (default the release build) on a game
  for N seconds with networking off, reads the report and prints an `xtp-smoke v1` block: commit
  (dirty or not), GPU, build identity, seconds, how it ended, frames, presents per 2 s (last 3),
  crashes, device removals, peak commit; saves the report beside it.
- **Files**: `xtask/src/smoke.rs` (new), `xtask/src/main.rs`.
- **xcl reference**: `scripts/smoke.ps1`.
- **Acceptance**: unit test of the block from a synthetic report; in CI, the smoke run of the
  test title (`--seconds 5`) prints a block.

### B11. `cargo xtask smoke-compare` (S; depends: B10)

- **Goal**: compares a `main` report and a branch report: same build and duration, branch frames
  at least 80 % of main's, peak commit at most 125 %, no crash or device removal on the branch,
  the branch's commit clean; prints each problem, exit non-zero if any.
- **Files**: `xtask/src/smoke.rs`.
- **xcl reference**: `scripts/pr_check.py` `check` (`FRAMES_KEPT`, `COMMIT_GROWTH`),
  `scripts/test_pr_check.py`.
- **Acceptance**: unit tests for every rule on pairs of synthetic reports.

### B12. Preflight and `xtp doctor` (M; depends: B6)

- **Goal**: checks (4.11) with a level and a fix text, printed before `xtp run` starts and at
  its end when not ok, stored in the report; `xtp doctor` prints them and the config (replaces
  `xtp config`).
- **Files**: `apps/xtp/src/doctor.rs` (new), `apps/xtp/src/main.rs`, `apps/xtp/src/run.rs`,
  `apps/xtp/src/report.rs`.
- **xcl reference**: `crates/xcl/src/preflight.rs` (`Level`, `Check`, `Found`, `checks`);
  `crates/xcl/src/launcher.rs` `check_cpu_features`.
- **Acceptance**: unit tests of the pure check function on made-up findings (complete PC, WARP
  only, no SM 6.6, no `dxil.dll`, path inside the repository); `xtp doctor` exits 0 in CI.

### B13. `--json` for `inspect` and `census` (S; depends: none)

- **Goal**: a stable JSON output for both commands (file, machine, image base, imports with how
  each resolves; census totals and missing pairs).
- **Files**: `apps/xtp/src/inspect.rs`, `apps/xtp/src/census.rs`, `apps/xtp/src/main.rs`.
- **xcl reference**: `crates/xcl/src/inspect.rs` `run` (`Out`, `Entry`).
- **Acceptance**: integration test: `xtp inspect --json` on the built `hle_probe.dll` parses,
  lists `xgameruntime.dll` as HLE and the missing probe DLL as missing.

### B14. `xtp run <game folder>` (S; depends: none)

- **Goal**: given a folder, take the executable `MicrosoftGame.config` names, else the only
  executable; on ambiguity, list the candidates and stop.
- **Files**: `apps/xtp/src/run.rs`, `crates/xtp-hle/src/runtime.rs` (manifest reading next to
  `Title::from_manifest`).
- **xcl reference**: `crates/xcl/src/launcher.rs` `pick_exe`, `resolve_target`,
  `read_game_config`; `crates/xcl/tests/e2e.rs`
  `launcher_uses_configured_exe_in_an_ambiguous_folder`.
- **Acceptance**: unit tests on temporary folders with configs written by the test: configured,
  single, ambiguous, none.

### B15. Game layer: build data, signatures and hooks (M; depends: none)

- **Goal**: `KnownBuild` gains the CodeView GUID/age and a table of named hook sites (offset per
  build, byte signature with wildcards); a scanner locates a signature in a mapped image (found
  once, absent, ambiguous); `xtp inspect` and the run report name the build.
- **Files**: `crates/xtp-game/src/lib.rs`, `crates/xtp-game/src/signature.rs` (new),
  `apps/xtp/src/inspect.rs`.
- **xcl reference**: none (xcl has no per-game layer): new work under `docs/asset-policy.md`
  ("Allowed").
- **Acceptance**: unit tests on byte buffers built by the test for each scanner outcome; lookup
  by hash ignores case; the policy test still passes (signatures are short, no long encoded
  runs).

### B16. Game layer: tests on the contributor's dump (S; depends: B2, B15)

- **Goal**: T4 tests for `xtp-game`: on `XTP_SAMPLE_SGD5`, every record unpacks to its stated
  size and most have a name; on the executable under `game_dir`, the build is identified or
  reported unknown, and for a known build every hook signature is found exactly once at its
  stated offset.
- **Files**: `crates/xtp-game/tests/samples.rs` (new).
- **xcl reference**: `crates/xcl/tests/console_dlls_e2e.rs` `setup` (one shared lookup of the
  sample per test binary).
- **Acceptance**: without the files the tests print `SKIPPED` and pass; with them they pass on
  the contributor's dump; no assertion message contains bytes from it.

### B17. Engine archive reader (M, when a runtime feature needs it; depends: B2)

- **Goal**: list the records of the game's RPF9 archives (name, sizes) with a bounded-memory
  header probe for archives over 1 GB; `xtp shader-info` finds shader records through it instead
  of scanning raw bytes.
- **Files**: `crates/xtp-game/src/rpf.rs` (new), `apps/xtp/src/shader.rs`.
- **xcl reference**: `crates/xcl/src/rpf_probe.rs` (bounded probing), `crates/xcl/src/rpf.rs`
  (listing behaviour).
- **Acceptance**: unit tests on archives synthesised by the test (records, a truncated header,
  an oversized count refused); a T4 test on `XTP_SAMPLE_RPF` asserting every listed record lies
  inside the file.

### B18. `cargo xtask ci`: the CI sequence locally (S; depends: B4)

- **Goal**: one command running what CI runs (5.6: fmt check, clippy twice, `cargo xtask test`),
  stopping at the first failure, so a contributor can check a branch before pushing.
- **Files**: `xtask/src/main.rs`.
- **xcl reference**: `AGENTS.md`, "Build and test" (the commands a contributor is expected to
  run).
- **Acceptance**: on a clean tree `cargo xtask ci` runs the steps in order and exits 0; a unit
  test with an injected step runner shows a failing step stops the sequence with its exit code.

### Order at a glance

```text
B0 (after the rename, once)
B1 ─┬─ B2 ─┬─ B3
    │      ├─ B16 (with B15)
    │      └─ B17
    └─ B4 ── B18
B5 ── B6 ─┬─ B7 ── B9 (with B8) ── B10 ── B11
          ├─ B8
          └─ B12
B13, B14, B15: no dependency, any time
```

B0 goes first, alone. The first wave after it (B1, B5, B13, B14, B15) can run in parallel; B6
and B8 are the critical path to the smoke runs that runtime PRs will need.
