# Release Checklist

Tags drive releases. Pushing `v<X.Y.Z>` to `origin` triggers
[`.github/workflows/release.yml`](../.github/workflows/release.yml),
which fans out to four platform jobs, signs the artifacts via Sigstore
keyless, generates a SLSA build-provenance attestation, and creates the
GitHub Release.

## Pre-tag

- [ ] Bump `version = "X.Y.Z"` in `Cargo.toml` `[workspace.package]`.
- [ ] Update [`CHANGELOG.md`](../CHANGELOG.md): rename `[Unreleased]` to
      `[X.Y.Z] — YYYY-MM`, document Added / Changed / Fixed / Security /
      Testing for the new version. The link references at the bottom of
      the file are part of the format; bump those too.
- [ ] Run the full local gauntlet:
      ```bash
      cargo xtask preflight                           # fmt + clippy + test + audit + sbom + dist
      cargo nextest run --workspace --features hq-resample
      cargo nextest run --workspace --no-default-features
      ```
- [ ] Commit + push to `main`. CI must be green before tagging.

## Tag + push

```bash
git tag -a v<X.Y.Z> -m "v<X.Y.Z>"
git push origin v<X.Y.Z>
```

If you need to retag at a later commit (e.g., the first push exposed a
CI bug):

```bash
git tag -fa v<X.Y.Z> -m "v<X.Y.Z>"
git push origin v<X.Y.Z> --force-with-lease
```

## During the release workflow

The workflow has two phases:

1. **build** matrix — four parallel jobs (macOS arm64 / macOS x86_64 /
   Linux x86_64 / Windows x86_64). Each one:
   - Builds the release binary via `cargo xtask ci --stage build`.
   - macOS only: `cargo xtask dmg` — codesign + notarize + DMG packaging.
   - Generates a CycloneDX SBOM per target.
   - Packages (`tar.gz` / `zip`) the platform's artifact.
   - Signs every shipable file with cosign keyless.
   - Verifies the artifact set is complete (pre-upload guard); fails if
     anything is missing.

2. **release** job — runs only on tag pushes (`refs/tags/*`). Downloads
   the merged artifact set, verifies completeness (pre-publish guard),
   generates a SLSA build-provenance attestation across the merged
   artifacts, and creates the GitHub Release with the `generate_release_notes`
   block populated by `.github/release.yml`.

If either guard fails, the workflow fails; no Release is published.

## Post-tag verification

Once the workflow finishes, verify the published artifacts from the
release URL:

```bash
# 1. Cryptographic signature + transparency log entry (Rekor).
cosign verify-blob \
    --bundle <artifact>.cosign-bundle \
    --certificate-identity-regexp 'https://github.com/mcaney006/grokmacos-better-version/.github/workflows/release\.yml@.*' \
    --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
    <artifact>

# 2. SLSA build provenance (needs gh CLI logged in).
gh attestation verify <artifact> --owner mcaney006

# 3. File integrity.
sha256sum -c SHA256SUMS

# 4. (macOS) Gatekeeper assessment on the DMG.
spctl --assess --type install <artifact>.dmg
```

## Apple Developer ID

First v0.1.x DMG will be ad-hoc signed because Apple Developer ID
secrets aren't configured. macOS users see a Gatekeeper prompt on first
launch. To enable real signing + notarization later:

```bash
gh secret set APPLE_CERTIFICATE_P12_BASE64       # base64-encoded .p12
gh secret set APPLE_CERTIFICATE_PASSWORD         # password for the p12
gh secret set KEYCHAIN_PASSWORD                   # ephemeral keychain pw
gh secret set APPLE_DEVELOPER_ID_APPLICATION     # "Developer ID Application: Name (TEAMID)"
gh secret set APPLE_ID                            # appleid@example.com
gh secret set APPLE_TEAM_ID                       # 10-char Team ID
gh secret set APPLE_APP_SPECIFIC_PASSWORD        # app-specific pw for notarytool
```

The keychain bootstrap step runs on every macOS job, then exits early
inside `run:` if `APPLE_CERTIFICATE_P12_BASE64` is empty. Missing secrets
fall back to ad-hoc signing (Gatekeeper warns; `right-click → Open` is
the escape hatch); setting only some of the three required secrets
(`APPLE_CERTIFICATE_P12_BASE64`, `APPLE_CERTIFICATE_PASSWORD`,
`KEYCHAIN_PASSWORD`) fails the job loudly so a misconfigured release
doesn't silently ship unsigned.

## Yanking a release

If you discover the release is broken:

1. Delete the GitHub Release (the tag stays, that's deliberate — the
   tag is the cryptographic root of the SLSA attestation).
2. Tag a new patch version (`v<X.Y.Z+1>`) with the fix.
3. Note the yank in the new version's `CHANGELOG.md` entry.

Don't rewrite history on `main` or force-push the original tag after
the Sigstore attestation is published. The Rekor log entry references
the original commit; rewriting confuses verification.
