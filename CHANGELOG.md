# Changelog

All notable changes to this project are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05

First tagged release. Rewrite of the original SwiftUI macOS Grok client into
cross-platform Rust + eframe.

### Added

- Multi-provider chat streaming: xAI Grok, OpenAI, Anthropic Claude
  (text + `tool_use` via per-block `input_json_delta` accumulator),
  and any OpenAI-compatible local endpoint (Ollama / LM Studio /
  llama.cpp server).
- Voice mode against xAI Realtime (`wss://api.x.ai/v1/realtime`) with
  30s keepalive ping, 90s receive-side watchdog, 15s per-WS-send
  timeout, and 10s WebSocket-connect timeout against hung handshakes.
- Local persistence via redb (chats + messages + settings) and
  tantivy (full-text search). Optional semantic re-ranking under
  `--features rag` via `fastembed`.
- Optional rubato sinc-interpolated resampling under
  `--features hq-resample` (default is linear).
- Cross-platform single binary: macOS arm64 + x86_64, Linux x86_64,
  Windows x86_64. Cold-start under 5 ms for `--version`.
- Native rendering through `eframe` → `wgpu`; no embedded browser.
- Hardened HTTP client: `https_only` + TLS 1.2 floor by default; only
  `LocalClient` opts into plaintext loopback.
- `cargo xtask` Rust-native release pipeline (no shell scripts).
- Sigstore keyless signing + SLSA build-provenance attestation for
  every release artifact.

### Security

- **HTTP client**: removed the 120 s overall request timeout that was
  killing legitimate long streams; replaced with a 60 s pre-first-byte
  cap via `tokio::time::timeout` around `.send()` only.
- **HTTP client**: `Client::builder().build().expect()` removed —
  the fallback path retries without `min_tls_version` and ONLY a
  `LOOPBACK` policy may degrade to `Client::new()`. `STRICT` clients
  panic rather than silently downgrade.
- **Error responses**: capped at `MAX_ERROR_BODY_BYTES = 16 KiB` to
  prevent a hostile peer from OOM'ing the process via a multi-GB 500.
- **SSE decoders**: refuse to emit events past `[DONE]` /
  `message_stop`, even within a single `feed` call; bounded line
  buffer (4 MiB) surfaces overflow as a typed `StreamTruncated` error;
  parse failures escalate to `ProviderStream` after 3 strikes.
- **WebSocket**: receive-side watchdog (`Arc<AtomicI64>` last-recv
  timestamp, 60 s ticker, 90 s deadline); send-side 15 s timeout to
  catch half-open transports the keepalive ping can't see.
- **Storage**: per-entry decode failures in `list_chats` /
  `list_messages` / `load_settings` log + skip rather than propagate;
  one corrupted row can no longer brick the app.
- **CI**: `permissions: { contents: read }` workflow-wide;
  `persist-credentials: false` on every checkout; release-cache set
  to `save-if: "false"` to prevent cache poisoning of signed artifacts;
  template-injection-safe matrix passthrough via `env:`.

### Testing

- **58 tests** under default features, `--no-default-features`,
  `--features hq-resample`, `--features hotkeys` — green on all four.
- **Property tests** (proptest): decoder no-panic-on-arbitrary-bytes
  (256 cases × 4 KiB inputs); message storage roundtrip preserves
  order + content (64 cases × 1-32 messages).
- **Adversarial regression tests**: dead-WS detection, oversize SSE
  line, rate-limit retry honouring `Retry-After`, body-size cap
  against OOM, settings decode-failure fallback, list-functions
  skip individually-corrupt entries, WS hung-handshake timeout.
- **Fuzz harnesses** (`fuzz/`): cargo-fuzz targets for both
  decoders with seeded corpus from in-tree fixtures.
- **Criterion benches**: redb insert (~16 ms warm), tantivy query
  (~46 µs warm, 1k-doc corpus).

[0.1.0]: https://github.com/mcaney006/grokmacos-better-version/releases/tag/v0.1.0
[Unreleased]: https://github.com/mcaney006/grokmacos-better-version/compare/v0.1.0...HEAD
