# Threat Model

This document is the security analysis the engineering team relies on when
reviewing changes. It is intentionally specific (not "best practices") so a
reviewer can check whether a proposed change actually changes the model.

## Assets

| Asset | Where it lives | Value to attacker |
|---|---|---|
| Provider API keys (xAI, OpenAI, Anthropic) | OS keyring (Keychain / Credential Manager / Secret Service) | Billing fraud, prompt impersonation, training-data poisoning. **High.** |
| User chat history | Local redb at `$DATA_DIR/grok-insane.redb` | Personal data; possibly proprietary code or secrets the user shared with the model. **High.** |
| Tantivy search index | Local at `$DATA_DIR/search-index/` | Indirect access to chat content. **Medium.** |
| Voice audio in-flight | RAM only; no persisted recordings | Eavesdropping if the WS is downgraded. **High.** |
| Local OAuth / GitHub tokens | NOT held by this app | n/a |
| Build artifacts (DMG / tar.gz / zip) | GitHub Release | Supply-chain attack vector against every user. **Critical.** |

## Adversaries

1. **Malicious / hostile API provider.** A misbehaving xAI / OpenAI / Anthropic
   could return SSE payloads designed to crash, OOM, or leak the decoder.
2. **MITM on the network path.** Public Wi-Fi, malicious VPN, compromised
   corporate proxy.
3. **Local malware.** Another process on the same machine that wants the
   user's API keys.
4. **Compromised CI.** A poisoned Swatinem cache, a typo-squatted dep, or a
   compromised GitHub Action that ships a backdoored binary.
5. **Curious future maintainer.** Someone who reads the codebase to find
   "interesting" exfiltration paths (logs, telemetry, accidental panics).

## Controls

### Against #1 (hostile provider)

| Risk | Control | Where |
|---|---|---|
| Multi-GB error body OOM | `read_capped_body` streams via `bytes_stream` with a 16 KiB hard cap | `services::chat::read_capped_body` |
| SSE buffer growth | 4 MiB `LINE_BUDGET_BYTES`; overflow → `StreamTruncated` error, buffer cleared | `services::sse::LineByteBuffer` |
| Parse-failure loop | After `SSE_PARSE_FAILURE_LIMIT = 3` consecutive failures the decoder escalates to `ProviderStream` error | `services::chat::SseDecoder`, `services::anthropic::AnthropicDecoder` |
| Stream after terminator | Decoders refuse to emit events past `[DONE]` / `message_stop` even within a single feed() | both decoders' `feed` loop |
| Hung connection | Pre-first-byte `tokio::time::timeout(60s)` on `.send()`; per-WS-send 15s timeout on uplink; 90s receive-side WS watchdog | `services::chat::send_with_rate_limit_retry`, `services::voice` |
| Rate-limit storm | Bounded exponential backoff honouring `Retry-After` (capped at 30s); 3 retry budget; only retries 429, never 500-class | `services::chat::send_with_rate_limit_retry` |
| Arbitrary-byte panic | `proptest` (256 cases × 4 KiB inputs) + in-tree LCG (1000 seeds × 8 KiB) + `cargo fuzz` continuous harness | `src/services/{chat,anthropic}.rs` tests + `fuzz/` |

### Against #2 (network MITM)

| Risk | Control | Where |
|---|---|---|
| Plaintext exfil | `https_only(true)` on every cloud client; build-time refusal to fall back to `Client::new()` for STRICT policy | `services::chat::http_client` |
| TLS downgrade | `min_tls_version(TLS 1.2)`; `rustls` backend (not OpenSSL) | `services::chat::http_client`; Cargo.toml `reqwest` features |
| Local-runner trust | Only `LocalClient` opts into `https_only(false)`; addressed to `127.0.0.1:11434` by default | `services::local::LocalClient` |

### Against #3 (local malware)

| Risk | Control |
|---|---|
| API keys in process memory | `Zeroizing<String>` wrappers on every secret; OS-keyring backed storage |
| Keys on disk | Never written to redb / tantivy / logs |
| Keys in tracing | EnvFilter explicitly downgrades `reqwest`, `hyper`, `rustls`, `tungstenite`, `tokio_tungstenite` to `warn` to suppress request/header dumps |
| Keys in error messages | `HeaderValue::from_str` errors do NOT include the value (verified against the `http` crate); `BadStatus.body` truncated; `ApiError` Display strings never carry the key |

### Against #4 (compromised CI / supply chain)

| Risk | Control | Where |
|---|---|---|
| Cache poisoning of release artifact | `Swatinem/rust-cache` set to `save-if: "false"` on `release.yml` build job — read-only cache | `.github/workflows/release.yml` |
| Token persistence in artifacts | `persist-credentials: false` on every `actions/checkout` across all 4 workflows | every checkout |
| Excessive workflow permissions | `permissions: { contents: read }` at workflow scope; each job opts in explicitly to anything more | every workflow |
| Template injection via matrix | `${{ matrix.* }}` values pass through `env:` into `run:` blocks; never inlined | `ci.yml` features matrix, `release.yml` verify step |
| Unpinned actions | Every `uses:` pinned to a full commit SHA with a trailing `# vN` comment | every workflow |
| Stale advisory ignores | `cargo deny` warns on unnecessary skips; xtask audit list documented as "graph vs lockfile-only" | `deny.toml`, `xtask/src/commands.rs` |
| Yanked / withdrawn deps | `cargo audit` runs daily via `audit.yml` schedule + on every Cargo.lock change | `audit.yml` |
| Empty / partial release | Per-row pre-upload artifact verifier in `release.yml` build job + pre-publish verifier in release job | `release.yml` |
| Unsigned distribution | Sigstore keyless via cosign on every artifact; SLSA build-provenance attestation | `release.yml` |
| Unverified dep license | `cargo deny check licenses` blocks any license outside the allow-list | `deny.toml` |
| Build reproducibility regression | `cargo xtask reproducible` builds twice and diffs hashes; runs in release pipeline | `xtask/src/commands.rs::reproducible` |

### Against #5 (curious maintainer)

| Risk | Control |
|---|---|
| Adding an unsafe block | `#![forbid(unsafe_code)]` at the crate root; can NOT be `#[allow]`'d at an inner scope |
| Adding a panic path | `#![deny(clippy::unwrap_used)]`, `#![deny(clippy::expect_used)]`; the few legitimate sites are `#[allow]`'d at the line with an explanation |
| Logging a secret | Reqwest / hyper / rustls / tungstenite log levels capped at `warn` in `init_tracing` |
| Smuggling in a new provider with HTTP fallback | `HttpPolicy::LOOPBACK` is the only way to opt out of `https_only`; review-visible via grep |

## Out of scope (deliberately)

- **Disk encryption of redb.** The chat history is at rest on the user's
  machine. We trust the user's OS-level disk encryption (FileVault,
  BitLocker, LUKS). Adding application-level encryption with a passphrase
  is a future consideration; today's threat model assumes the disk is the
  user's trust boundary.
- **Sandboxing the desktop process.** Eframe + cpal + tokio expect
  conventional process privileges. Hardening via Seccomp / AppContainer
  would require significant restructuring.
- **Network-level rate limit coordination across multiple windows of
  the same app.** Each window retries independently under 429.

## Verifying a release

```bash
# Signature + transparency log
cosign verify-blob \
    --bundle <artifact>.cosign-bundle \
    --certificate-identity-regexp 'https://github.com/mcaney006/grokmacos-better-version/.github/workflows/release\.yml@.*' \
    --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
    <artifact>

# SLSA build provenance
gh attestation verify <artifact> --owner mcaney006

# File integrity
sha256sum -c SHA256SUMS
```
