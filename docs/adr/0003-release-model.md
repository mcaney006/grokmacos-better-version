# ADR 0003: Release model

## Status

Accepted (2026-05).

## Context

A release of a desktop app needs to answer four questions for an end
user before they run the binary:

1. **Did the maintainer publish this?** (authenticity)
2. **Has it been altered after publication?** (integrity)
3. **What does it contain?** (inventory)
4. **Where did it come from?** (provenance)

Apple notarisation answers (1) and (2) for macOS users only. We need
the same guarantees for Linux and Windows, and we want them to be
verifiable by tooling rather than by trust in a brand.

## Decision

Tag pushes (`v*`) trigger `.github/workflows/release.yml`, which
produces, per target triple:

| Artefact | What it proves |
|---|---|
| Signed + notarised `.dmg` (macOS) | Gatekeeper-acceptable. Maintainer's Apple Developer ID. |
| `.tar.gz` / `.zip` | Plain platform binary. |
| `*.sbom.json` (CycloneDX 1.5) | Full dependency inventory at build time. |
| `*.cosign-bundle` | Sigstore keyless detached signature with a Rekor transparency-log entry. |
| Build-provenance attestation (separate, via `actions/attest-build-provenance`) | SLSA v1.0 statement binding the artefact to this repo, this workflow, this commit. |

Build artefacts are produced under a GitHub Actions `release`
environment, gated by required reviewers configured on the GitHub side.
Workflow permissions follow `read-all` by default; only the jobs that
need `id-token: write`, `attestations: write`, `contents: write` get
them, scoped per job.

`SOURCE_DATE_EPOCH` is fixed to the commit's authored timestamp so
two CI runs of the same tag are bit-for-bit identical (modulo Apple
signature blobs which incorporate a notarisation timestamp).

## Consequences

**Positives**

- Anyone can verify any artefact independently using widely-deployed
  open-source tooling — `cosign verify-blob`, `gh attestation verify`,
  `grype sbom:...` — without contacting us.
- A compromised CI build cannot quietly retroactively republish; every
  signed artefact is recorded in the public Rekor log.
- The SBOM is signed and attested too. "Trust me bro, this is the SBOM
  for that binary" stops being a viable attack path.

**Negatives**

- The release pipeline now has six concrete dependencies on third-party
  actions, each of which we SHA-pin. Dependabot keeps them current.
- The `release` environment gate adds latency. That's a feature.
- Apple notarisation is still a single point of failure for the macOS
  path; if Apple revokes our Developer ID, the existing DMG remains
  valid (notarisation tickets are stapled) but new releases break
  until we rotate.

## Verification

Documented in `README.md`, "Verifying a release". Verifying a release
requires three commands; failing any one means do not run the binary.
