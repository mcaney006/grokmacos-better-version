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

1. Built reproducibly with `SOURCE_DATE_EPOCH` from the tip commit's
   authored timestamp.
2. Signed via **Sigstore keyless** (cosign) — verifiable against the
   public Rekor transparency log.
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
- **WebSocket**: 10s connect timeout, 15s send timeout, 90s receive
  watchdog.
- **Secrets**: OS-keyring storage, `Zeroizing` in-process, tracing
  filters that suppress framework-level log dumps.
- **Storage**: per-entry decode failures skipped with a log line —
  one corrupted row can never brick the app.
- **CI/CD**: least-privilege `permissions`, `persist-credentials:
  false` everywhere, read-only release cache, template-injection-safe
  matrix passthrough, every action SHA-pinned.

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | ✅ |
| < 0.1 | ❌ |
