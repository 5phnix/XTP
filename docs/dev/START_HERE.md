# Start here

A map for new contributors: what each part of XTP does, where to begin, how
to run a title and how to report what you see. The rules and the
pull request process are in [CONTRIBUTING.md](../../CONTRIBUTING.md); the
design is in [docs/architecture.md](../architecture.md).

## In one paragraph

The title's x86-64 code runs unchanged inside our process. `xtp-loader` maps
its executable and DLLs and binds their imports; `xtp-hle` implements the
console APIs those imports name; `xtp-gpu` turns the console's D3D12.X into
host D3D12, and `xtp-shader` recompiles console shaders to DXIL when the title
creates a pipeline. Data formats (textures, audio) are decoded by
`xtp-formats`; what a single title needs lives in `xtp-game`.
Nothing from any game is in the repository ([asset policy](../asset-policy.md)).

## Map of the crates

| Crate | What it does | Look at first |
|-------|--------------|---------------|
| `crates/xtp-core` | configuration (`xtp.toml`, `XTP_*` paths), logging, the guard that refuses to write derived data inside the repository | `config.rs`, `policy.rs` |
| `crates/xtp-pe` | read-only PE32+ and import-library parsing | `image.rs`, `implib.rs` |
| `crates/xtp-loader` | prepares, maps, binds and starts the title image; threads, TLS, crash reports | `prepare.rs`, `resolve.rs`, `crash.rs` |
| `crates/xtp-hle` | the console API surface, one module per area (`modules/`: kernel, files, storage, audio, gameinput, xgamesave, xuser, ...) and the host pass-through policy | `modules/mod.rs`, `registry.rs` |
| `crates/xtp-gpu` | D3D12.X objects, command lists and GPU memory on host D3D12; pipelines, descriptors, indirect execution, captures | `lib.rs` (the model), `list.rs`, `pipeline.rs`, `host.rs` |
| `crates/xtp-shader` | console shader containers (DXBC, DXIL with Xbox additions, GFX10 code) to host DXIL | `lib.rs`, `gfx10/`, `emit/` |
| `crates/xtp-formats` | the GPU's tiled texture layouts, BCPack, XMA and Opus audio; pure functions, bytes in and bytes out | `texture.rs`, `xma.rs` |
| `crates/xtp-pipeline` | offline conversion of the user's data, content-addressed cache | `cache.rs` |
| `crates/xtp-game` | BTP 6 only: its formats, build identities, hooks | `lib.rs` |
| `crates/xtp-testkit` | dev-dependency only: visible, countable test skips (`xtp_testkit::skip`, `XTP_REQUIRE`) | `lib.rs` |
| `apps/xtp` | the `xtp` command: `inspect`, `census`, `shader-info`, `translate`, `run`, `config` | `main.rs`, `run.rs` |
| `xtask` | repository tooling: `policy`, `conventions`, `pr-check`, `install-hooks` | `main.rs` |
| `tests/fixtures/hle-probe` | a stand-in title we wrote, built by the loader's tests | `src/` |

Areas in commit headers and `area/...` labels follow this table
(`xtp-gpu` is `gpu`, `apps/xtp` is `app`).

## Where to start

- **Read** [docs/architecture.md](../architecture.md), then the module doc
  comment (`//!`) at the top of the crate you are interested in.
- **Build and test** once (`cargo test --workspace`): it needs no game files
  and tells you your setup works.
- **Pick by interest**:
  - *Graphics*: `xtp-shader` (untranslated shaders drop draws in the world)
    and `xtp-gpu` (frame time, queries, depth bounds, HDR). The README's
    status table lists what is missing.
  - *System*: `xtp-hle`. An import we do not implement yet is named when the
    title calls it (`--unimplemented trap|zero`); implementing one is a
    well-bounded first change.
  - *Formats*: `xtp-formats`, pure code that is easy to test with synthetic
    data.
  - *Tooling and docs*: `xtask`, `apps/xtp`, `docs/`.
- **Ask** in an issue before large work; check open pull requests for the
  same files (the bot comments when they overlap).

## Running a title

Point `game_dir` (or `XTP_GAME_DIR`) at a title folder outside the
repository. The repository does not contain, link to or explain how to get
any title's files, and the tools refuse to write below it.

1. Copy `xtp.example.toml` to `xtp.toml` (git-ignored) and set `game_dir`,
   or set the variables instead:

   | Variable | Meaning |
   |----------|---------|
   | `XTP_GAME_DIR` | the title folder |
   | `XTP_CACHE_DIR` | derived data (default `%LOCALAPPDATA%\xtp\cache`) |
   | `XTP_DATA_DIR` | saves and settings (default `%LOCALAPPDATA%\xtp\data`) |
   | `XTP_TEST_TITLES` | other Xbox Series titles, `;`-separated, for platform tests |
   | `XTP_CONFIG` | another config file |

2. Check the paths: `cargo run -p xtp -- config` (it reports any path inside
   the repository as a policy violation).
3. Run:

   ```powershell
   cargo build --release -p xtp
   target\release\xtp.exe run "<title folder>" --execute
   ```

   For debugging aids, build `--profile diag --features diagnostics`; the
   README's Debugging section lists the `XTP_*` switches.
   `xtp precook` translates every shader of the title's `shaders.rpf` (and
   makes its compute pipelines) into the cache before a run, so the first
   run after a translator change does not wait for them; `cargo xtask play`
   does it before each session (`--no-precook` skips it).
4. Tests that need real files skip unless their variable is set (each test's
   doc comment names it). For example, in PowerShell:
   `$env:XTP_XMA_STREAMS = '<dir>'; cargo test -p xtp-formats -- --ignored`.

### A/B a switch

To compare a switch's two values on the same scene in one session, set it
to `ab` instead of `0` or `1` (`XTP_FRAMES_IN_FLIGHT=ab`), or to
`ab:<phase s>:<settle s>` (`ab:30:5`). Phase A runs its `0`, phase B its
`1`, in turn, 20 s each by default, switched at presents; the first 3 s of
each settle and are not measured. Each frame-rate line is tagged with the
phase it measured (`[A/B B1]`, `[A/B A2 settling]`), each phase ends with a
line of its settled presents and frame rate, and `cargo xtask play` ends
with the median per arm of the lines past settling. Switches set to `ab`
together alternate together. A switch takes `ab` once it is read through
`xtp_core::ab::switch` and listed in `xtp_core::ab::SWITCHES`; any other
`XTP_*` set to `ab` stops the run at start. Switches that change how shaders
are translated or pipelines made (`XTP_DXIL_OPT`, the shader loop limit and
budget, `XTP_STRIDE_TABLE`, `XTP_HOST_LANES`, `XTP_WAVE64_MESH`,
`XTP_ROOT_SRV_DIRECT`) cannot alternate: a program is translated once per
run and the shader cache keys on them, so phase B would run A's
translations, or translate everything again in the middle of the run.
Compare two sessions for those.

### Run report

Bug reports and compatibility reports ask for this block. Every line is
ours to share (no game content):

```text
xtp commit    : output of `git rev-parse --short HEAD` (say if you have local changes)
build         : release | diag
title build   : known | unknown (the `build` line of `xtp run`, without its hash)
console mode  : series-x | series-s (`--console`)
windows       : edition, version and build (`winver`)
cpu, memory   : model; RAM and paging file size
gpu, driver   : model; driver version
command       : the exact `xtp run ...` command line, the title's path written `<title folder>`
result        : how far XTP got (no start, first frames, 2D, 3D, a crash), and the frame rate
crashes.log   : the last entry of crashes.log in the data folder, or "none"
```

Leave out anything the title itself printed and every file it uses or
produces. Replace your user name in paths if you prefer.

## Good first issues

Issues labelled
[`good first issue`](https://github.com/MiamiGame/XTP/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
are small, self-contained and come with a pointer to the code. Typical ones:

- implementing one console API that a title calls and we do not serve yet,
  with a test that needs no title;
- a missing case in a pure decoder of `xtp-formats`, tested on synthetic
  data;
- a clearer error message or a new option in the `xtp` command;
- docs: anything in this file or the README that was wrong or unclear when
  you followed it.

Comment on an issue before working on it so that two people do not do the
same work; a maintainer assigns it to you.
