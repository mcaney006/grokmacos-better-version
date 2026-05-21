# ADR 0004: Result-queued SSE decoders

## Status

Accepted (2026-05).

## Context

The original decoder for both providers held a `VecDeque<ChatEvent>` of
*successful* events. Failure modes were communicated implicitly:

- A provider `error` event was translated into `ChatEvent::Delta("")`.
- A stream ended without `[DONE]` / `message_stop` synthesised a
  `ChatEvent::Done`.
- A run of malformed JSON was silently swallowed — the decoder would
  keep accepting bytes forever.

The UI couldn't tell the difference between "completed cleanly" and
"the upstream said the model was overloaded then dropped." Every
production support escalation that began with "the response just
stopped" traced back to this.

## Decision

The pending queue is now `VecDeque<Result<ChatEvent, ApiError>>`. Every
decoder surfaces three distinct failure variants:

- `ApiError::ProviderStream { provider, message, request_id }` — the
  upstream emitted an `error` event mid-stream OR the parse-failure
  counter exceeded `*_PARSE_FAILURE_LIMIT = 3`.
- `ApiError::StreamTruncated { provider, message, request_id }` — EOF
  without the protocol's terminator (`[DONE]` for OpenAI/xAI,
  `message_stop` for Anthropic).
- `ApiError::RateLimited { retry_after }` — for the pre-stream
  classification of HTTP 429 (handled by the retry middleware before
  the decoder ever sees bytes).

Both decoders share the same:

- byte-level line buffer (`services::sse::LineByteBuffer`) with a
  4 MiB `LINE_BUDGET_BYTES` overflow guard,
- per-iteration `saw_done` / `saw_stop` check that breaks the
  `while let Some(line) = take_line()` loop on terminal state,
- post-terminal `feed` short-circuit so a buggy caller can't keep
  driving the parser.

The request-id is captured from the response headers at construction
(via `services::chat::extract_request_id`) and embedded in every
error variant for support handoffs.

## Consequences

- The UI distinguishes provider errors, stream truncation, and
  rate-limit storms from clean completion. Each maps to a distinct
  toast message and (optionally) retry behaviour.
- Both decoders are property-tested + fuzz-tested for the invariant
  "no panic on arbitrary bytes, post-terminal state stays terminal."
  See `services::{chat,anthropic}::tests::*_never_panics_proptest`
  and `fuzz/fuzz_targets/{sse,anthropic}_decoder.rs`.
- `match` on stream events is now `Result<ChatEvent, ApiError>`
  rather than `ChatEvent`, which is a wider surface but a more honest
  one.

## Rejected alternatives

- **Out-of-band error channel.** Tried in a draft branch; doubled the
  number of stream handles every consumer had to manage and the UI
  had to multiplex two futures with no obvious ordering guarantees.
  In-stream `Result` lets `Stream::next()` deliver errors in the
  exact byte position they occurred.
- **Synthetic `ChatEvent::Error` variant.** Made the enum match
  asymmetric (every variant except this one is success-shaped) and
  forced callers to pattern-match against `ChatEvent::Error` everywhere
  they really meant `Result::Err`. `Result` is the standard idiom for
  this in async-Stream code.
