<!--
Thanks for the PR.

Keep this short. The goal is for a reviewer (or future-you) to understand
the change in 30 seconds — what changed, why, how it was verified.
Detailed design discussion belongs in the linked issue or an ADR.
-->

## What changed
<!-- One sentence. No lore dump. -->

## Why
<!-- What problem does this solve? Closes #N if applicable. -->

## Verification
- [ ] `cargo xtask check` passes locally (fmt + clippy `-D warnings` + tests)
- [ ] Manually exercised the changed path (`cargo xtask dev` or `./target/debug/grok-insane`)
- [ ] Docs / README updated if user-facing behaviour changed
- [ ] No new `unwrap`/`expect` in production code (test code is fine)
- [ ] No new `unsafe` (crate forbids it)

## Behavior changes
<!-- CLI flags, settings keys, file formats, key bindings, breaking changes. Or "None". -->

## Performance impact
<!-- Frame time / memory / startup. "Not relevant" is a valid answer. -->

## Release notes
<!--
One line that would belong in the changelog if this lands. The release
workflow groups by label (see .github/release.yml), so label this PR
accordingly: feature / bug / perf / security / docs / deps / refactor.
-->

## Screenshots or logs
<!-- Optional, but very helpful for UI changes. -->
