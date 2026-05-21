// === Crate-wide hardening lints ============================================
//
// Posture: every rule that catches a real-world bug class is on, even if it
// occasionally produces noise. We tune `allow` lists in narrowly-scoped
// `#[allow(...)]` attributes on the exact items that need them — never at
// the crate root unless the rule is genuinely wrong for our domain.
//
// `forbid(unsafe_code)`: stronger than `deny` — an inner `#[allow]` can NOT
// re-enable it. Any future unsafe block requires lifting this attribute in a
// PR where it shows up in the diff. There is no unsafe code in this crate.
#![forbid(unsafe_code)]
// And if any unsafe ever IS reachable (e.g., via a build.rs we don't
// control), require explicit `unsafe { ... }` blocks inside `unsafe fn`
// bodies. Already the default in edition 2024; explicit here as a guard.
#![deny(unsafe_op_in_unsafe_fn)]
// Network input + serialized bytes from disk should never be wrapped through
// `unwrap`/`expect` blindly. These two lints catch bare panic surface in our
// own code. `#[allow]` is acceptable in tests + benches via `#[cfg(test)]`.
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
// Catch lifetimes that are never used elsewhere — usually a refactor leftover.
#![warn(single_use_lifetimes)]
// `let _ = ...` on a Drop-impl type is sometimes wrong, but in this
// codebase the dominant pattern is intentional fire-and-forget channel
// sends (`let _ = tx.send(...)`). Forcing a `.ok()` or `drop()` wrap
// would be noisier without catching real bugs. Not warned at crate level.
// `clippy::panic` is similarly noisy: every test uses `panic!` in
// `match other => panic!` arms. Per-site `#[allow]` where panicking IS
// the right outcome (paths::dirs final fallback).
// Catch infinite or pointless type recursion in trait bounds.
#![warn(trivial_bounds)]
// `as` casts hide lossy conversions; use `From` / `TryFrom` where exact.
#![warn(clippy::cast_lossless)]
// Catch `.iter().map(|x| f(x))` when `.map(f)` works.
#![warn(clippy::redundant_closure_for_method_calls)]
// Inline format-arg captures (`format!("{x}")`).
#![warn(clippy::uninlined_format_args)]
// `clippy::missing_errors_doc` and `clippy::missing_panics_doc` would
// be ideal lints to enforce, but adding `# Errors`/`# Panics` rustdoc
// sections to every `Result`-returning fn in the codebase is a separate,
// large undertaking. Tracked rather than blanket-allowed here.
// The `Err` variants of our error enums embed `reqwest::Error` (and friends),
// which are intentionally large. We accept the size hit in exchange for clean
// `?`-propagation; callers immediately surface the error to a toast and the
// `Result` does not stay live on the stack.
#![allow(clippy::result_large_err)]
#![allow(clippy::items_after_test_module)]
#![allow(clippy::field_reassign_with_default)]

//! GrokInsane library crate — exposes the storage, services, models, and
//! utility modules to both `src/main.rs` (the desktop binary) and the
//! `benches/` and `tests/` targets. The binary itself is just a thin
//! `fn main` plus CLI dispatch over what's defined here.

pub mod app;
pub mod config;
pub mod error;
pub mod models;
pub mod paths;
pub mod secrets;
pub mod services;
pub mod storage;
pub mod theme;
pub mod ui;

/// Stable surface for `cargo-fuzz` harnesses. **Not** a public API —
/// gated behind the `__fuzz` feature so it can't accidentally be
/// linked from production binaries. The harnesses in `fuzz/` drive
/// the SSE decoders against arbitrary bytes; this module is the only
/// place that names those internal types from outside the parent
/// module, so renaming a private decoder never silently invalidates
/// the fuzz corpus.
#[cfg(feature = "__fuzz")]
#[doc(hidden)]
pub mod __fuzz {
    pub fn drive_sse_decoder(_bytes: &[u8]) {
        // Routing implemented in services::chat::__fuzz_drive.
        crate::services::chat::__fuzz_drive(_bytes);
    }

    pub fn drive_anthropic_decoder(_bytes: &[u8]) {
        crate::services::anthropic::__fuzz_drive(_bytes);
    }
}
