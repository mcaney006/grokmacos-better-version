#![no_main]
//! libFuzzer harness for the Anthropic SSE decoder.
//!
//! Same invariants as the OpenAI/xAI counterpart — see
//! `fuzz_targets/sse_decoder.rs`.
//!
//! Run:
//!   cargo +nightly fuzz run anthropic_decoder
//!   cargo +nightly fuzz run anthropic_decoder -- -max_total_time=300

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    grok_insane::__fuzz::drive_anthropic_decoder(data);
});
