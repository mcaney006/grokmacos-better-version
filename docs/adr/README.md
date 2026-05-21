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
| [0004](0004-result-queued-sse-decoders.md) | Result-queued SSE decoders surface stream errors as typed `ApiError` | Accepted |
| [0005](0005-redb-plus-tantivy.md) | redb (K/V) + tantivy (full-text) split with startup reconciliation | Accepted |
| [0006](0006-ws-keepalive-and-watchdog.md) | WebSocket keepalive + receive watchdog + connect/send timeouts | Accepted |

To add an ADR, copy an existing file, bump the number, set status to
`Proposed`, and open a PR.
