# Architecture Decision Records

Lightweight write-ups of architectural choices that future contributors
(human or model) would otherwise have to reverse-engineer. ADRs preserve
**why** something is the way it is; the code already shows **what**.

Format: short. Status → Context → Decision → Consequences.

| # | Decision | Status |
|---|---|---|
| [0001](0001-rewrite-in-rust.md) | Rewrite the macOS Swift app in cross-platform Rust | Accepted |
| [0002](0002-use-xtask.md) | Use `cargo xtask` instead of shell scripts | Accepted |
| [0003](0003-release-model.md) | Release model: signed + notarised + SBOM'd + Sigstore-signed via tag push | Accepted |

To add an ADR, copy an existing file, bump the number, set status to
`Proposed`, and open a PR.
