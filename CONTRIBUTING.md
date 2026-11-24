# Contributing to XTP

Thank you for helping. XTP runs the Xbox Series build of BTP 6
natively on Windows, from files each user supplies. Many people work on it at
once, so the process below is strict and mostly automatic: a pull request that
follows it is labelled, checked and routed to a reviewer without anyone
having to ask.

New to the code? Read [docs/dev/START_HERE.md](docs/dev/START_HERE.md) first.

Contents: [the rules](#the-rules) ·
[setup](#setup) · [build and test](#build-and-test) ·
[commits and PR titles](#commits-and-pull-request-titles) ·
[pull requests](#pull-requests) · [review](#review) ·
[code](#code) · [reporting](#reporting-bugs-progress-and-vulnerabilities)

## The rules

These are not negotiable. A pull request that breaks one is closed, and if
something slipped into `main`, history is rewritten (see
[docs/asset-policy.md](docs/asset-policy.md), "If something slips through").

1. **Nothing from the game.** No assets, no part of the executable, nothing
   derived from them (converted data, disassembly, shader dumps, extracted
   tables, captures, screenshots), in any form: files, string literals, test
   fixtures, snapshots, docs, or attachments to commits. What *is* allowed is
   listed in [docs/asset-policy.md](docs/asset-policy.md): our own code that
   understands formats, build identities (BLAKE3, CodeView GUID), offsets and
   byte-pattern signatures of functions we hook, small constants we write,
   and synthetic test data our code generates.
2. **No keys.** No encryption or signing keys, licence keys, certificates or
   anything that decrypts protected content, whether whole or in part.
3. **No console SDK material.** No code, headers, libraries, documentation
   or samples from the Microsoft GDK for Xbox or any other console SDK, and
   nothing transcribed from them. Interfaces are written from what the title
   does and from public documentation.
4. **Nothing that circumvents DRM.** XTP runs the user's own decrypted dump.
   It does not decrypt, unpack, crack or bypass licence checks of any
   protection; code that does is not accepted.
5. **Licence: GPL-3.0-or-later.** Everything you contribute is licensed under
   the GNU General Public License, version 3 or any later version (see
   [LICENSE](LICENSE)). By opening a pull request you agree to that and confirm
   you have the right to.
6. **xcl and AnyPS5 are references only.** xcl (GPL-2.0-only) and AnyPS5
   (GPL-2.0) may be read to understand a problem; **no code is copied or
   translated from them**, not even a function, because their licences are
   not compatible with ours (see [NOTICE.md](NOTICE.md)). Write your own
   implementation; say in the pull request what you read.
7. **State where your code comes from.** Every pull request ticks a box under
   [Provenance](#provenance). The "PR conventions" check fails without it.
8. **Tests never need game files to pass.** A test that needs the title or
   other real files reads the paths of *your own* files from an `XTP_*`
   environment variable and skips cleanly when it is unset, which is always
   the case in CI. See [Tests](#tests).

### Provenance

The pull request template asks you to tick one of:

- **All my own work**: you wrote it for XTP, or the change only moves code
  already in XTP.
- **Some of it comes from elsewhere**: list each source with its licence and
  the files concerned. Its licence must be compatible with GPL-3.0-or-later
  (MIT, Apache-2.0, BSD, zlib, ISC, GPL-3.0, LGPL-3.0, ...), and a file that
  carries code under another licence keeps that licence's notice at its top.
  GPL-2.0-only code (xcl among others) is not compatible; neither is code
  without a licence.

Also say in the description whether an AI tool wrote part of the change.
That is allowed; you answer for every line, its provenance included, as if
you had typed it.

## Setup

You need:

- Windows 10 or 11, x64, a CPU with AVX2, and for running the title a
  Direct3D 12 GPU with shader model 6.6 (mesh shaders for the world).
  Building and the tests need no GPU.
- [rustup](https://rustup.rs). The toolchain version, the
  `x86_64-pc-windows-msvc` target and the `rustfmt` and `clippy` components
  are pinned in `rust-toolchain.toml`; rustup installs them on first use.
- The MSVC linker and the Windows SDK: Visual Studio 2022 Build Tools with
  the "Desktop development with C++" workload.
- Git.

Then:

```powershell
git clone https://github.com/<you>/XTP.git      # your fork
cd XTP
git remote add upstream https://github.com/MiamiGame/XTP.git
cargo xtask install-hooks                        # policy check + commit message check
copy xtp.example.toml xtp.toml                   # then point it at your files, outside the repo
```

`install-hooks` points git at `.githooks/`: `pre-commit` runs the asset policy
on what you stage, `commit-msg` checks your message against the
[conventions](#commits-and-pull-request-titles). They take a second once
xtask is built. Do not bypass them with `--no-verify`: CI runs the same checks.

## Build and test

```powershell
cargo build                                       # debug build
cargo build --release -p xtp                      # what you run the title with
cargo build --profile diag -p xtp --features diagnostics   # with debugging aids
cargo test --workspace                            # needs no game data
cargo xtask pr-check                              # everything CI checks, before you push
```

`cargo xtask pr-check` runs, in order, exactly what the required checks run
and prints a summary:

| Step | CI job | Command |
|------|--------|---------|
| fmt | `fmt` | `cargo fmt --all --check` |
| clippy | `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| clippy-diagnostics | `clippy-diagnostics` | the same with `--all-features` |
| test | `test` | `cargo test --workspace --locked` |
| policy | `policy` | `cargo xtask policy` |
| conventions | `conventions` | every commit since your branch left `main` |

Options: `--title "<area>: <summary>"` also checks the title you plan to give
the pull request, `--base <commit>` compares with another commit, `--skip
<step>` leaves a step out (CI still runs it), `--fail-fast` stops at the first
failure. CI also runs `actionlint` on the workflows.

### Tests

- Every change in behaviour comes with tests that run without game files:
  synthetic inputs built by the test, or our own fixtures under
  `tests/fixtures/` (code we wrote, e.g. `hle-probe`, a stand-in title).
- A test that needs real files takes their path from an environment
  variable and skips when it is unset. Either mark it
  `#[ignore = "needs <what>: set XTP_<NAME>"]` and read the variable inside,
  or return early:

  ```rust
  #[test]
  fn decodes_real_streams() {
      let Some(dir) = std::env::var_os("XTP_XMA_STREAMS") else {
          xtp_testkit::skip("no-sample XTP_XMA_STREAMS");
          return;
      };
      // ...
  }
  ```

  `xtp_testkit::skip` writes `SKIPPED <test>: <reason>` directly to stderr so
  skips stay visible under default `cargo test` capture; setting
  `XTP_REQUIRE=<reason,...>` turns matching skips into a panic. Run the ignored
  ones with `cargo test -p <crate> -- --ignored`. Document a new variable in
  the test's doc comment and in the pull request.
- Such tests assert on properties (it decodes, sizes match, the hash of our
  output is stable for your input), never on snapshots of game content, and
  they write nothing below the repository (`xtp_core::ensure_outside_repo`).

## Commits and pull request titles

Every commit message and every pull request title starts with a header:

```
<area>: <summary>
```

- `<area>` is one of: `core`, `pe`, `loader`, `hle`, `gpu`, `shader`,
  `formats`, `pipeline`, `game`, `testkit` (the crates `crates/xtp-<area>`),
  `app` (`apps/xtp`), `xtask`, `tests`, `ci` (`.github/`, `.githooks/`),
  `docs`, `deps` (dependency updates), `build` (workspace manifest, toolchain,
  `.cargo/`, repository settings). A topic may follow (`hle/audio`), and a
  change that spans areas names them all (`gpu, shader: ...`).
- `<summary>` says what the change does, in the imperative ("clamp indirect
  draw arguments", not "clamped" or "clamps"), with no final period.
- The header is at most 72 characters. A body, if any, follows a blank line
  and says why.
- `fixup!` / `squash!` commits and `Revert "..."` are accepted as git writes
  them; merge commits are not checked.

`cargo xtask conventions --title "..."` checks a header; the `commit-msg` hook
checks each commit; the "PR conventions" check checks the title and every
commit of the pull request. Pull requests are squashed when they merge, so
the title becomes the commit on `main`: make it a good one.

Examples:

```
gpu: clamp indirect draw arguments to the buffer size
hle/audio: decode Opus streams on the worker pool
shader, gpu: translate 64-bit image atomics
docs: explain the run report
```

**Branch names**: `<area>/<short-topic>` in your fork, e.g.
`gpu/indirect-clamp`. Never open a pull request from your fork's `main`.

## Pull requests

### One topic per pull request

A pull request does one thing: a fix, a feature, a refactoring. A refactoring
that a feature needs goes in its own pull request first. Formatting or
renaming outside the lines you change goes in its own pull request too.
Follow-ups are new pull requests, not more commits on a merged one.

Before starting anything big, open an issue (Feature or improvement) and agree
on the approach, and check that no open pull request does the same work: the
automatic overlap comment tells you when another open PR changes your files.

### Size

Changed lines are additions plus deletions, `Cargo.lock` aside. A label is
added automatically:

| Label | Changed lines | |
|-------|---------------|---|
| `size/XS` | under 10 | |
| `size/S` | 10 to 99 | |
| `size/M` | 100 to 399 | the target: aim to stay below 400 |
| `size/L` | 400 to 999 | allowed with a reason in the description; reviewed more slowly |
| `size/XL` | 1000 or more | reviewed only once split, or if a maintainer agreed to it beforehand in an issue |

### Opening one

1. Rebase on (or merge) the latest `main`, and run `cargo xtask pr-check`.
2. Open the pull request against `main` and fill in the template completely:
   what and why, area, tests, **provenance**, checklist. Use a **draft**
   pull request for work in progress; mark it ready when it is.
3. The automation then, within minutes:
   - adds `area/...` and `size/...` labels (and `policy` when the project's
     rules or automation change);
   - runs **CI** (`fmt`, `clippy`, `clippy-diagnostics`, `test`, `policy`,
     `actionlint`) and **PR conventions** (`conventions`), all required;
   - labels `conflict` and comments while the PR conflicts with `main`;
   - comments and labels `overlap` when other open PRs change the same files;
   - welcomes you if it is your first contribution (a maintainer then
     approves the first run of the checks: GitHub requires it).
4. Keep the checks green. A failing check is yours to fix; ask in the pull
   request if you do not understand why it fails.

## Review

- **Who**: the code owners of the area (`.github/CODEOWNERS`) are asked
  automatically; a maintainer assigns someone if no one is. One approval is
  needed, given after your last push (a push dismisses earlier approvals),
  and every review conversation must be resolved.
- **Turnaround**: a first response (triage, labels, a reviewer) within
  **2 working days**; a review within **5 working days** for `size/M` and
  smaller, **10** for `size/L`; a new look after you address comments within
  **3 working days**. Past that, comment on the pull request to ask: it is
  not rude.
- **Answering review**: push new commits (do not force-push during a review
  unless asked: reviewers follow what changed), reply to each comment, and
  let the reviewer resolve conversations they opened.
- **Merging**: when approved and green, a maintainer adds the pull request to
  the merge queue. The queue tests it on top of the latest `main` and squashes
  it, with its title as the commit message.
- **Inactive pull requests**: one waiting on its author for 30 days may be
  closed; reopen it when you come back.

Reviewers check, beyond the code: the [rules](#the-rules) and the provenance
statement, that the change is generic platform code (only `xtp-game` knows
the title), tests, and docs.

## Code

- `rustfmt` formats everything; clippy warnings are errors.
- Every `unsafe` block has a `// SAFETY:` comment saying why it is sound
  (the `undocumented_unsafe_blocks` lint); `unsafe fn` bodies use explicit
  `unsafe` blocks.
- Platform code (every crate but `xtp-game`) must work for any Xbox Series
  title. What only BTP 6 needs (formats, build identities,
  hooks) goes in `xtp-game`.
- Log with `tracing`; return errors (with `anyhow` or `thiserror`, as the
  crate already does) rather than panicking on bad input.
- Anything that writes derived data calls `xtp_core::ensure_outside_repo`.
- A new dependency needs a reason in the description, a licence compatible
  with GPL-3.0-or-later, and goes in `[workspace.dependencies]`.
- Update the docs in the same pull request: `README.md` (status, commands,
  variables), `docs/architecture.md` when the structure changes.

## Reporting bugs, progress and vulnerabilities

- Bugs, game progress and feature requests: use the
  [issue forms](https://github.com/MiamiGame/XTP/issues/new/choose). They ask
  for a run report (see [docs/dev/START_HERE.md](docs/dev/START_HERE.md#run-report)).
  Attach nothing from the game.
- Security vulnerabilities: privately, as described in [SECURITY.md](SECURITY.md).
- Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).
