# Contributing

Thanks for the interest. This document is short on purpose — the
codebase is small and the conventions are simple.

## Local setup

```bash
# Linux only: install ALSA + X11/Wayland dev libs
cargo xtask install-deps

# One-shot health check (runs the same pipeline CI does)
cargo xtask check
```

Toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml). If you
have `rustup` installed, the first `cargo` invocation will fetch the right
version automatically — no manual step.

## Day-to-day commands

Everything goes through `cargo xtask`. No `make`, no shell scripts.

| Command | What it does |
|---|---|
| `cargo xtask dev` | Run the app with debug logging. |
| `cargo xtask doctor` | Verify your local environment can build + run the app. |
| `cargo xtask check` | `fmt --check` + `clippy -D warnings` + `cargo test`. |
| `cargo xtask preflight` | Everything `check` does, plus `audit` + `sbom`. Run before opening a release PR. |
| `cargo xtask fmt` | Apply formatting. |
| `cargo xtask audit` | Supply-chain scan (`cargo-audit` + `cargo-deny`). |
| `cargo xtask sbom` | Generate a CycloneDX SBOM. |
| `cargo xtask dist` | Release build into `dist/<host-triple>/`. |
| `cargo xtask reset` | Wipe local app data. |
| `cargo xtask clean-deep` | Remove `target/`, `dist/`, and caches. |

Full list: `cargo xtask help`.

## Before opening a PR

```bash
cargo xtask preflight
```

That runs fmt + clippy + tests + cargo-audit + cargo-deny + SBOM
generation locally so CI doesn't catch something on the first push.

## PR expectations

Every PR should include:

1. **What** changed — one sentence.
2. **Why** — what problem it solves; link the issue if one exists.
3. **Verification** — how you confirmed it works (the template lists the
   usual boxes).
4. **Behaviour changes** — CLI flags, settings keys, file formats, key
   bindings. Or "none".

Add a label that matches one of the categories in
[`.github/release.yml`](.github/release.yml) — that's what groups your
change in the auto-generated release notes. `feature`, `bug`, `perf`,
`security`, `docs`, `deps`, `refactor`, `chore`.

## Code conventions

- **No `unsafe`.** The crates carry `#![forbid(unsafe_code)]`.
- **No `unwrap`/`expect` in production code.** Both are denied at the
  binary crate root. Tests can opt back in with a scoped `#[allow]`.
- **Async background work goes through Tokio**; UI work stays on the
  eframe thread. Channels mediate the two — see the streaming pattern
  in [`src/app.rs`](src/app.rs).
- **Storage writes** go through helpers in
  [`src/storage/mod.rs`](src/storage/mod.rs); never serialise to redb
  with bincode directly outside that module.
- **Errors** are typed (`thiserror`). Anything that bubbles up to the UI
  ends up in a toast via `Toaster::error`.

## Architecture decisions

Significant choices are written up as ADRs in
[`docs/adr/`](docs/adr/). If your change reverses an ADR or makes
another one necessary, write a new file there before the PR ships.

## Release process

Releases come from tags. To cut one:

```bash
git tag v0.1.1
git push origin v0.1.1
```

The release workflow takes over: signed + notarised macOS DMG, Linux
tarball, Windows zip, CycloneDX SBOMs per target, Sigstore signatures,
and a SLSA build-provenance attestation are all attached to the GitHub
Release. Do not manually upload artifacts.

Releases run inside a protected `release` environment in GitHub
Actions; configure approvers in **Settings → Environments → release**
so signing requires a human OK.

## Security disclosures

Do **not** file a public issue. Open a private advisory at
<https://github.com/mcaney006/grokmacos-better-version/security/advisories/new>.
