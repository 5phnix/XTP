<!--
Title: `<area>: <summary>`, e.g. `gpu: clamp indirect draw arguments`
(areas and rules: CONTRIBUTING.md, "Commits and pull request titles").
Keep every heading below; "PR conventions" checks the Provenance section.
-->

## What and why

<!-- What changes, and why. One topic per pull request. -->

Fixes: <!-- #issue, or none -->
Depends on: <!-- #PR, or none -->

## Area

<!-- The crates or parts this touches; labels are added automatically. -->

## Tests

<!--
What you ran and what you checked. Tests that need game files read them from
XTP_* variables and skip when unset: name the variables and what you ran
them on (title build, Series X or S mode), not the files themselves.
"Not tested on the title" is a valid answer: say so.
-->

- [ ] `cargo xtask pr-check` passes on my machine
- [ ] New or changed behaviour has tests that need no game files, or I explain below why it cannot
- Ran on the title: <!-- what you ran, the stage reached, GPU and driver; or "no" -->

## Provenance

<!-- Tick the box that applies (CONTRIBUTING.md, "Provenance"). -->

- [ ] I wrote all of this change myself for XTP, or it only moves code already in XTP.
- [ ] Some of it comes from elsewhere. Each source, its licence and the files concerned are listed here, and every licence is compatible with GPL-3.0-or-later:
<!-- source, licence, files -->

AI assistance: <!-- none, or the tool and what it produced; you answer for every line either way -->

## Checklist

- [ ] No game data, keys, console SDK code or headers, and nothing that circumvents DRM ([asset policy](../blob/main/docs/asset-policy.md))
- [ ] No code copied from xcl or AnyPS5 (references only: read them to understand, write your own)
- [ ] I agree to license this contribution under GPL-3.0-or-later
- [ ] Title and every commit follow `<area>: <summary>`; one topic; under 400 changed lines, or the reason is above
- [ ] Docs updated where behaviour, commands or variables changed (README, docs/)
