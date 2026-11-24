# Security policy

## Reporting a vulnerability

Report it **privately**, through GitHub:
[Security → Report a vulnerability](https://github.com/MiamiGame/XTP/security/advisories/new).
Do not open a public issue, pull request or discussion about it.

Include what is affected (commit, file, workflow), how to reproduce it, and
what an attacker gains. Attach nothing from the game: describe the input
instead (see [docs/asset-policy.md](docs/asset-policy.md)).

What to expect:

- an acknowledgement within **3 working days**;
- an assessment, and a fix plan or a reasoned refusal, within **14 days**;
- the fix developed in a private fork of the advisory, released, then the
  advisory published with credit to you unless you prefer otherwise.

Please give us 90 days, or until a fix is released if sooner, before you
disclose it publicly.

## Supported versions

There are no releases yet: only the latest `main` is supported, and fixes
land there.

## Scope

XTP is **not a sandbox**. It runs the title's own machine code natively, in
its process, with the rights of the user who starts it. That the title's code
can do anything the user can is how XTP works, not a vulnerability. XTP is
meant to run only the user's own dump of a title they own.

In scope, for example:

- XTP writing outside the folders it is configured with, or writing derived
  data inside the source tree despite `xtp_core::ensure_outside_repo`;
- memory-safety bugs in our parsers (`xtp-pe`, `xtp-formats`, `xtp-shader`,
  `xtp-pipeline`) reachable from a crafted file, beyond what the title can
  already do;
- the title or a crafted file reaching the host in ways `xtp-hle`'s
  pass-through policy (`HostPolicy`) is meant to deny, such as the network,
  which is offline by design;
- the repository's automation: a workflow that runs pull-request code with a
  write token or secrets, script injection through titles or descriptions,
  an action not pinned to a commit, a hook or `xtask` command that runs
  something it should not;
- a dependency with a known vulnerability that affects XTP.

Out of scope: anything that requires game files to be distributed, or a way
around the title's own protection (XTP never circumvents DRM, see
CONTRIBUTING.md).
