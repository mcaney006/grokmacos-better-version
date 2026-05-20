// === Crate-wide hardening lints ============================================
// `forbid` is stronger than `deny`: even an inner `#[allow]` cannot relax it.
// If any future contributor (human or model) needs unsafe, they have to lift
// this attribute deliberately in a PR, where it shows up in the diff.
#![forbid(unsafe_code)]
// Network input + serialized bytes from disk should never be wrapped through
// `unwrap`/`expect` blindly. These two lints catch bare panic surface in our
// own code.
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
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
