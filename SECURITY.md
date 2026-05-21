# Security Policy

## Reporting a vulnerability

Please open a **private** vulnerability report via GitHub's
[Security Advisories](https://github.com/mcaney006/grokmacos-better-version/security/advisories)
rather than a public issue. We will respond within **72 hours** and aim
to ship a fix within **14 days** for confirmed high-severity issues.

If you can't use GitHub's UI, email
[`mcaney006@users.noreply.github.com`](mailto:mcaney006@users.noreply.github.com)
with `[security]` in the subject. Include:

- Affected version (`grok-insane --version`)
- Reproducer (smallest possible)
- Impact: data loss / RCE / privilege escalation / credential leak / DoS
- Whether you intend to disclose publicly and on what schedule

We do **not** currently run a bug bounty.

## Supply chain

Every release is:

1. Built with `SOURCE_DATE_EPOCH` exported from the tip commit's
   authored timestamp (`xtask ci --stage compute-source-date-epoch`).
   This is a necessary, not sufficient, condition for byte-identical
   reproducibility — we do not currently re-build the artifact in CI
   and verify the hashes match. The `cargo xtask reproducible` command
   runs two clean builds locally and `bail!`s if they diverge; that is
   what we'd point at to verify the claim end-to-end.
2. Signed via **Sigstore keyless** (cosign) — verifiable against the
   public Rekor transparency log. SHA256SUMS is also signed so the
   file hashes themselves are anchored to the workflow's OIDC
   identity, not just trusted by virtue of sitting next to the
   binaries.
3. Backed by a **SLSA build-provenance attestation** linking the
   artifact to the exact GitHub Actions run.
4. Accompanied by a **CycloneDX SBOM** listing every transitive crate
   and version.

Verify:

```bash
cosign verify-blob \
    --bundle <artifact>.cosign-bundle \
    --certificate-identity-regexp 'https://github.com/mcaney006/grokmacos-better-version/.github/workflows/release\.yml@.*' \
    --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
    <artifact>

gh attestation verify <artifact> --owner mcaney006
sha256sum -c SHA256SUMS
```

## Threat model

See [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md) for the full
analysis: assets, adversaries, and the specific control that mitigates
each risk class with file/line references.

## Hardening summary

- **No unsafe code.** `#![forbid(unsafe_code)]` at the crate root.
- **Network**: `https_only` + TLS 1.2 floor on every cloud provider;
  rustls (not OpenSSL); pre-first-byte timeout only on streaming
  endpoints; capped error-body read.
- **Auth headers** (`Authorization: Bearer …`, `x-api-key`,
  WebSocket `Authorization`) are marked sensitive via
  `HeaderValue::set_sensitive(true)`, so HTTP-middleware loggers that
  honour the flag print `Sensitive` instead of the raw token.
- **WebSocket**: 10s connect timeout, 15s send timeout, 90s receive
  watchdog.
- **Secrets**: OS-keyring storage, `Zeroizing` in-process, tracing
  filters that suppress framework-level log dumps.
- **Local provider host allowlist**: the Local provider — the only
  one allowed to send plain HTTP — refuses any host that isn't
  loopback (127.0.0.0/8, ::1, `localhost`). RFC 1918 / link-local /
  `*.local` are accepted only when `GROK_INSANE_ALLOW_LAN_LOCAL=1`
  is set in the environment. Public-internet hosts are NEVER
  accepted from this provider. See `src/services/local.rs` for the
  enforcement.
- **SSE parse-failure logging**: payload bytes are NOT logged at
  WARN. We log payload length + a per-process keyed hash
  fingerprint so log readers can correlate repeated failures of
  the same event without disclosing user content. Raw payloads are
  trace-only (`RUST_LOG=…=trace`).
- **Storage**: per-entry decode failures skipped with a log line —
  one corrupted row can never brick the app.
- **CI/CD**: least-privilege `permissions`, `persist-credentials:
  false` everywhere, read-only release cache, template-injection-safe
  matrix passthrough, every action SHA-pinned.

## What this app does NOT protect against

Honest list. Treat every claim above as scoped to what its
implementation actually does, not to what the word "secure" suggests.

- **A compromised desktop session**: the OS keyring, in-process
  memory, and any file the user can read are all reachable to
  malware running as that user. There is no anti-tamper / anti-
  debugging story, and adding one would be theatre.
- **A locally-running malicious LLM**: the Local provider host
  allowlist only prevents prompts from going to a public host. A
  hostile process listening on 127.0.0.1 (e.g. an attacker who can
  open a loopback socket) will receive prompts the user routes to
  it. Don't run untrusted code on the same machine.
- **Process-memory leakage**: `Zeroizing<String>` clears the buffer
  on drop, but egui's `TextEdit` reallocates its backing buffer as
  the user types, and those old allocations are not tracked. A
  core dump or live debugger attached during key entry sees the
  key.
- **Side-channel timing**: API key comparisons happen inside the
  provider, not in our code; we never branch on key bytes
  locally.
- **A determined endpoint operator** (any cloud LLM provider you
  point this at) can log your prompts. That is the provider's
  privacy policy, not a property of this client.

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | ✅ |
| < 0.1 | ❌ |
