# Development

Local commands and what they exercise. Run any of these without
arguments to see usage; everything here is a thin wrapper over
`cargo`/`cargo nextest`/`cargo clippy`.

## TL;DR

```bash
cargo xtask ci            # what GitHub Actions actually runs
cargo xtask preflight     # everything ci does, plus audit + sbom + dist
cargo xtask release       # preflight + reproducible + bundle + dmg (macOS)
```

If `cargo xtask ci` passes locally, the `build` matrix job in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) should also
pass. The two run the same stages (`fmt`, `clippy`, `nextest`, `build`)
through the same Rust entry point (`xtask/src/ci_driver.rs`).

## Toolchain

`rust-toolchain.toml` pins Rust 1.94.0. CI also pins 1.94.0. If
`cargo xtask ci` succeeds with a different toolchain on your machine,
it tells you nothing about whether CI will succeed.

```bash
rustup show
# expect: active toolchain: 1.94.0-<host>
```

## Stages CI runs

| Stage | Command | What it catches |
| --- | --- | --- |
| `fmt` | `cargo fmt --all -- --check` | Formatting drift |
| `clippy` | `cargo clippy --locked --workspace --all-targets -- -D warnings` | Lint warnings + lockfile drift |
| `nextest` | `cargo nextest run --workspace --locked` | Test failures, lockfile drift |
| `build` | `cargo build --release --workspace --locked` | Release build breaks |
| `audit` | `cargo audit` + `cargo deny check` | Known-vuln deps + license drift |
| `hygiene` | `cargo machete` | Unused dependencies |
| `sbom` | `cargo cyclonedx` | (release only) SBOM generation |
| `reproducible` | two clean release builds + SHA256 diff | Build determinism (local only — not in CI) |

Run an individual stage with `cargo xtask ci --stage <name>`.

## Features matrix

Optional features each opt in to a dep that the default build avoids:

```bash
cargo xtask ci --stage clippy   # default features
cargo clippy --workspace --all-targets --features hq-resample -- -D warnings
cargo clippy --workspace --all-targets --features hotkeys     -- -D warnings
cargo clippy --workspace --all-targets --features rag         -- -D warnings  # downloads ONNX runtime
cargo clippy --workspace --all-targets --all-features         -- -D warnings  # downloads ONNX runtime
```

`rag` and `--all-features` pull `fastembed → ort-sys`, which fetches
a pre-built ONNX Runtime tarball at build time. That needs network
access; sandboxed environments without it can't build those feature
combinations locally and should rely on the GitHub-hosted runners.

## Audit (supply chain)

```bash
cargo xtask audit              # cargo-audit (advisories) + cargo-deny (advisories + bans + sources + licenses)
cargo xtask hygiene            # cargo-machete (unused deps)
```

A green `audit` does NOT mean you're safe; it means the RustSec
advisory DB doesn't currently flag anything in your lockfile. New
advisories land daily — the scheduled
[`audit.yml`](.github/workflows/audit.yml) workflow runs at 13:00 UTC.

## Reproducibility

```bash
cargo xtask reproducible
```

Clean-builds the workspace twice with `SOURCE_DATE_EPOCH` set from
the tip commit's authored timestamp, then SHA-256s both binaries.
If they differ it prints the hashes and `bail!`s — a typical cause
is a build dep that bakes the current timestamp into the binary.
This is NOT run in CI today (the release pipeline only sets
`SOURCE_DATE_EPOCH`; it does not verify byte-equivalence). See
[`SECURITY.md`](SECURITY.md) for the honest scoping.

## Release dry-run

```bash
cargo xtask release            # full pipeline: check + hygiene + audit + sbom + reproducible + dist + bundle + dmg
```

On macOS this stages a `.app`, codesigns it, attempts notarization
(if `APPLE_*` envs are set), and packages the DMG. On Linux/Windows
the macOS-only steps are no-op stubs. The output lands in
`dist/<triple>/`.

To verify the release workflow's GitHub-Actions wrapper without
actually tagging:

```bash
# Push to a throwaway tag, watch the workflow, then delete the tag.
git tag -a v0.0.0-dryrun -m "dry run"
git push origin v0.0.0-dryrun
# … wait for workflow …
git push --delete origin v0.0.0-dryrun
git tag --delete v0.0.0-dryrun
```

There is intentionally no `--dry-run` flag on the release workflow
itself: a real release IS the dry-run — every artifact must be
producible, signable, and attestable before the GitHub Release
publish step. Tag pushes are the only way to drive it.

## Doctor

```bash
cargo xtask doctor             # advisory: lists missing tools, doesn't fail
cargo xtask doctor --strict    # fails if required + optional tools are missing
```

Use the `--strict` form on a release-bench machine to confirm
`codesign`, `xcrun`, `cosign`, `cargo-cyclonedx`, etc. are all
present BEFORE tagging.

## When something fails

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| `cargo fmt --all -- --check` fails | unformatted code | `cargo xtask fmt` |
| clippy fails with `--locked` | Cargo.lock not committed | `cargo update` then commit lockfile, or `cargo add` then commit |
| nextest hangs | a test is waiting on the network / a real provider | check tests don't hit production endpoints; mocks live in `src/services/voice.rs::tests::stub_server` and `chat.rs::tests::spawn_mock_http` |
| `xtask reproducible` diverges | non-deterministic codegen, embedded timestamps | use `diffoscope dist/build1.bin dist/build2.bin` (or any binary diff) to locate; common offenders are `vergen`, `built`, anything that calls `chrono::Utc::now()` in `build.rs` |
| `xtask audit` fails on `rag` | a feature-gated transitive has an advisory | check `xtask/src/commands.rs::audit` — known-acceptable advisories live there, gated to the `rag` feature only |
| macOS release falls back to ad-hoc | `APPLE_*` secrets unset or partially set | see `docs/RELEASE_CHECKLIST.md`; missing-all → ad-hoc; missing-some → loud fail |
