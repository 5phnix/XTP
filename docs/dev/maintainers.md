# Maintainers' guide

How pull requests and issues are handled, and the GitHub settings the
process relies on. **Nothing in the settings section is applied by the
repository's files**: each item is a box for a repository admin to tick by
hand. Contributors' side of the process: [CONTRIBUTING.md](../../CONTRIBUTING.md).

## What runs by itself

| Workflow | Trigger | What it does | Required check |
|----------|---------|--------------|----------------|
| `ci.yml` (CI) | PRs, merge queue, pushes to main | jobs `fmt`, `clippy`, `clippy-diagnostics`, `test` (Windows), `policy`, `actionlint` | yes, each job |
| `pr-conventions.yml` (PR conventions) | PR opened, edited, pushed; merge queue | job `conventions`: title and every commit `<area>: <summary>`, Provenance box ticked | yes |
| `pr-intake.yml` (PR intake) | PRs (`pull_request_target`), pushes to main, hourly | jobs `labels` (area + size), `welcome` (first-time contributors), `conflicts` (`conflict` label and comment), `overlap` (comment when open PRs change the same files) | no |
| Dependabot | weekly | grouped `deps:` (cargo) and `ci:` (actions) updates | goes through the same checks |

Security of the automation, to keep when editing it:

- `pull_request_target` jobs (intake) never check out, build or run the PR's
  files; they read it through the API. Code from a PR only runs under
  `pull_request`, with a read-only token and no secrets.
- Every workflow starts from `permissions: {}`; each job asks for what it
  needs. Actions are pinned to a commit SHA, with the version in a comment;
  Dependabot updates both.
- PR titles and descriptions reach scripts through `env:`, never through
  `${{ }}` inside `run:`.
- `actionlint` (in CI) checks every workflow.

## Triage

One maintainer is on triage each week (see [review rotation](#review-rotation)).
Every working day:

1. **New pull requests**: the bot has labelled them. Check the labels and the
   Provenance statement; approve the first CI run of first-time contributors
   after reading the diff for anything that should not run (Settings require
   it, see below); request a reviewer if CODEOWNERS found none; close those
   that break [the rules](../../CONTRIBUTING.md#the-rules), with the rule
   quoted.
2. **`size/XL` without prior agreement**: ask for a split; do not review.
3. **New issues** (`needs triage`): reproduce or ask for the missing run
   report, set the `area/...` label, remove `needs triage`. Mark small,
   well-pointed ones `good first issue` and say where to start in the issue.
4. **Anything from a game** in an issue, PR or comment: hide or delete it
   at once and say why. In the repository, follow
   [docs/asset-policy.md](../asset-policy.md), "If something slips through".
5. **Names in issue and PR texts**: `cargo xtask policy` checks files only.
   Read the titles, bodies and comments of new issues and PRs, and their
   commit messages, for what CONTRIBUTING.md ("Writing about titles") leaves
   out: names of commercial titles, their publishers and their settings,
   local game paths, title file names. Edit them out and delete the old
   revision from the edit history (GitHub shows edits to everyone).
6. **Waiting too long**: PRs past the turnaround of CONTRIBUTING.md
   ("Review") get a reviewer or a comment saying when.

## Labels

Create these before enabling the intake (the workflows may add labels but are
not allowed to create them; a missing label makes the job fail).

| Label | Colour | Set by | Meaning |
|-------|--------|--------|---------|
| `area/core` ... `area/game` (9: core, pe, loader, hle, gpu, shader, formats, pipeline, game) | `1d76db` | labeler | touches `crates/xtp-<area>` |
| `area/app` | `1d76db` | labeler | `apps/` |
| `area/xtask` | `1d76db` | labeler | `xtask/` |
| `area/tests` | `1d76db` | labeler | `tests/` |
| `area/ci` | `1d76db` | labeler, Dependabot | `.github/`, `.githooks/` |
| `area/docs` | `1d76db` | labeler | `docs/`, Markdown |
| `area/deps` | `1d76db` | labeler, Dependabot | `Cargo.lock` |
| `area/build` | `1d76db` | labeler | workspace manifest, toolchain, repository files |
| `policy` | `b60205` | labeler | changes the rules, licence or automation: a maintainer reviews |
| `size/XS` | `c2e0c6` | intake | under 10 changed lines |
| `size/S` | `9be9a8` | intake | 10 to 99 |
| `size/M` | `fbca04` | intake | 100 to 399 |
| `size/L` | `f9a03f` | intake | 400 to 999 |
| `size/XL` | `d93f0b` | intake | 1000 or more |
| `conflict` | `e11d21` | intake | conflicts with its base branch |
| `overlap` | `c5def5` | intake | another open PR changes the same files |
| `first-time contributor` | `7057ff` | intake | the author's first PR here |
| `bug` | `d73a4a` | issue form | |
| `compatibility` | `0e8a16` | issue form | a compatibility report |
| `enhancement` | `a2eeef` | issue form | |
| `needs triage` | `ededed` | issue forms | not looked at yet |
| `good first issue` | `7057ff` | maintainers | small, with a pointer to the code |

The area list is `AREAS` in `xtask/src/conventions.rs`; a test keeps it equal
to `.github/labeler.yml`. Adding a crate means adding its area in both and
its label here.

## Review rotation

- Each area has a code-owner team (see [Code owners](#code-owners)); GitHub
  requests a review from it, and the team's review assignment setting
  (round robin, 1 reviewer, "never assign certain members" for people away)
  spreads the load.
- Triage rotates weekly among the maintainers; keep the schedule in the
  pinned issue "Maintainer rotation".
- Reviewers follow the turnaround of CONTRIBUTING.md ("Review"). Someone who
  cannot, reassigns rather than holding the PR.
- A review checks the rules and provenance first, then that the change is
  one topic, generic (only `xtp-game` knows the title), tested without game
  files, and documented.

## Merging

- Merge only through the merge queue, which re-runs the required checks on
  top of the latest `main` and squashes with the PR title as message.
- Before queueing: one approval after the last push, conversations resolved,
  checks green, the title is a good commit header (it is checked, but reread
  it: it is what `main`'s history will say).
- Never merge a PR with a `policy` label without a second maintainer having
  read it.
- Revert rather than fix forward when `main` breaks and the fix is not
  obvious: `git revert`, as a PR titled `Revert "<title>"`.

## Releasing

There are no releases yet. When there are:

1. Choose the commit on `main`; CI must be green on it.
2. Write the notes from the squashed titles since the previous tag
   (`git log --format=%s <previous>..<commit>`), grouped by area, and update
   the README's status table in a PR first.
3. Tag `vX.Y.Z` (signed), push the tag, and publish a GitHub release with the
   notes. Release builds contain only our code: never attach anything built
   from or containing game data.
4. Bump `version` in `[workspace.package]` in a `build:` PR.

## GitHub settings to apply

Tick each box once done. Nothing here is changed by the repository's files.

### Ruleset "Protect main" (Settings → Rules → Rulesets)

Today: PR required, 1 approval, approval after the last push, resolved
conversations, no deletion or force push, admins can bypass. Add:

- [ ] **Require status checks to pass**, source GitHub Actions, these
      checks by job name: `fmt`, `clippy`, `clippy-diagnostics`, `test`,
      `policy`, `actionlint`, `conventions`. (Leave "require branches to be up
      to date" off: the merge queue does that.)
- [ ] **Require merge queue**: merge method *squash*; build concurrency 5;
      minimum group size 1, maximum 5; wait 5 minutes for a group to fill;
      status check timeout 60 minutes; "only merge non-failing pull
      requests".
- [ ] **Require review from Code Owners**, once CODEOWNERS has real owners.
- [ ] **Require linear history** (squash merges are linear anyway).
- [ ] Bypass list: keep admins, but "for pull requests only", so that an
      admin's push to `main` still goes through a PR.
- [ ] If a CI job is renamed, update this list in the same PR (the header of
      `ci.yml` says so).

### Pull requests (Settings → General)

- [ ] Allow **squash merging** only; default message "Pull request title and
      commit details" (the description holds the template, not history).
      Disable merge commits and rebase merging.
- [ ] Always suggest updating pull request branches.
- [ ] Allow auto-merge.
- [ ] Automatically delete head branches.

### Actions (Settings → Actions → General)

- [ ] Workflow permissions: **Read repository contents** (the default token is
      read-only; jobs ask for more themselves). Do not allow Actions to create
      or approve pull requests.
- [ ] Fork pull request workflows: **Require approval for first-time
      contributors** (at least; "for all outside collaborators" while the
      contributor list is unknown).
- [ ] Allowed actions: actions by GitHub, plus `Swatinem/rust-cache@*`;
      **require actions to be pinned to a full-length commit SHA**.

### Security (Settings → Code security)

- [ ] Private vulnerability reporting: **on** (SECURITY.md points to it).
- [ ] Dependabot alerts and security updates: on (version updates come from
      `.github/dependabot.yml`).
- [ ] Secret scanning and push protection: on.
- [ ] Code scanning default setup for GitHub Actions workflows (optional).

### Labels

- [ ] Create every label of [Labels](#labels), with its colour and
      description.

### Code owners

`.github/CODEOWNERS` is a draft with every rule commented out.

- [ ] Create the teams (this needs an organisation account): `maintainers`,
      and one per area as needed, e.g. `xtp-core`, `xtp-loader` (also
      `xtp-pe`), `xtp-hle`, `xtp-gpu`, `xtp-shader`, `xtp-formats`,
      `xtp-pipeline`, `xtp-game`. Give each write access and enable review
      assignment (round robin, 1 reviewer).
- [ ] In CODEOWNERS, replace each placeholder (`<gpu-owners>`, ...) with its
      team (`@<org>/xtp-gpu`) or with user handles, uncomment the lines, and
      check them under Settings → Code owners errors.
- [ ] Then require Code Owners review in the ruleset (above).

### Decisions left to the project owner

- [ ] **Code of Conduct contact**: CODE_OF_CONDUCT.md has a placeholder for
      the address reports go to; set it.
- [ ] **Screenshots in issues**: the issue forms currently ask for no
      screenshots or video of the title, since docs/asset-policy.md lists
      screenshots among derived material. Relax the forms if you decide
      issues may carry them.
- [ ] **Turnaround times** in CONTRIBUTING.md ("Review"): 2 working days to a
      first response, 5 to a review (10 for `size/L`). Adjust to what the
      maintainers can hold.
- [ ] Optional: enable Discussions for questions, and add it to
      `.github/ISSUE_TEMPLATE/config.yml`.
