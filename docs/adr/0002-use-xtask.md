# ADR 0002: Use `cargo xtask` instead of shell scripts

## Status

Accepted (2026-05).

## Context

The original repo carried `build.sh`, `build-dmg.sh`,
`create-dmg-now.sh`, `create-dmg-from-archive.sh`, `package-dmg.sh`,
`deploy-notarized.sh`, `release.sh`, `release_repackage.sh`. They
diverged from each other, called into each other in undocumented ways,
and only ran reliably on the machine of whoever wrote them.

We needed:

1. One automation surface, identical locally and in CI.
2. Cross-platform from day one (macOS, Windows, Linux).
3. Type-checked at compile time.
4. Reviewable in a PR diff like any other change.

## Decision

Adopt the [`cargo xtask`](https://github.com/matklad/cargo-xtask)
pattern: a workspace-member binary at `xtask/` is the only entry point
for build / release / dev automation. The `.cargo/config.toml` alias
`cargo xtask = "run --quiet --package xtask --"` makes it the canonical
verb.

xtask only shells out to system binaries that genuinely belong to the
platform (`codesign`, `hdiutil`, `xcrun`, `apt-get`, `rustup`,
`cargo`). It never invokes a hand-written `.sh` file in this repo —
because there aren't any.

## Consequences

**Positives**

- Same command set on every contributor's machine and in CI.
  `cargo xtask check` produces identical behaviour everywhere.
- Compile errors catch refactors before they reach CI.
- Adding a new command (e.g. `sbom`, `audit`, `dmg`) is a Rust function,
  not a new shell file with its own coding conventions.
- No PATH assumptions, no shell-dialect fights between bash and
  PowerShell.

**Negatives**

- A few seconds of compile overhead on the first invocation per branch.
  Mitigated by `Swatinem/rust-cache` in CI.
- One-line tasks are slightly more ceremony than a bash one-liner —
  acceptable cost for the consistency win.

## Conventions

- New commands live in `xtask/src/main.rs`, dispatched from the central
  `match` in `run()`. Add a one-line entry to `print_help()` so the
  command appears in `cargo xtask help`.
- macOS-only steps gate at the function boundary with two functions
  (one `#[cfg(target_os = "macos")]`, one `#[cfg(not(...))]`) — never
  inline `cfg` blocks inside a single body, because clippy under
  `-D warnings` fights them.
