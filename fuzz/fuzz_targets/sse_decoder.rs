#![no_main]
//! libFuzzer harness for the OpenAI / xAI SSE decoder.
//!
//! Invariants under fuzz:
//!   * `feed` + `eof` never panics, never hangs, never allocates beyond
//!     the line-buffer budget (4 MiB).
//!   * After `eof`, the event stream is drainable in finite steps.
//!   * Once the decoder reports a terminal state, no further events
//!     surface regardless of input.
//!
//! Run:
//!   cargo +nightly fuzz run sse_decoder
//!   cargo +nightly fuzz run sse_decoder -- -max_total_time=300
//!
//! Corpus seeds live in `fuzz/corpus/sse_decoder/` — golden fixtures
//! from the in-tree replay tests give libFuzzer something useful to
//! mutate from on first run.

use libfuzzer_sys::fuzz_target;

// The decoder is private to the chat module, so we drive it via the
// public XaiClient::stream path? No — that path requires HTTP. The
// fuzz target needs direct access. We expose a tiny stable surface
// via `grok_insane::__fuzz` (gated, see lib.rs) so fuzz harnesses
// don't drift if the internal type renames.
fuzz_target!(|data: &[u8]| {
    grok_insane::__fuzz::drive_sse_decoder(data);
});
