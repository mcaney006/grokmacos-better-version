# ADR 0001: Rewrite the macOS Swift app in cross-platform Rust

## Status

Accepted (2026-05).

## Context

The original `xai-grok-macos` was a SwiftUI + AppKit project. It worked,
but it had four structural limitations:

1. **macOS-only.** No Windows or Linux build path existed.
2. **Fragile packaging.** Release artefacts were assembled by a chain of
   `build.sh`, `create-dmg-*.sh`, `release.sh` scripts with hand-rolled
   error handling.
3. **No native cross-platform abstraction** for the things the app needs
   most: audio capture / playback, real-time WebSocket, an embedded
   search index, an embedded ACID store.
4. **Hard to extend safely.** Mixing voice state into a ViewModel left
   shared mutable state across threads; race conditions were
   load-bearing.

## Decision

Replace it with a Rust workspace targeting macOS, Windows, and Linux
from one codebase:

- `eframe` + `egui` + `wgpu` for the UI (immediate-mode, GPU-rendered,
  trivially cross-platform).
- `tokio` + `reqwest` (streaming SSE) + `tokio-tungstenite` for the
  network layer.
- `redb` (ACID K/V) + `tantivy` (Lucene-style full-text) for storage
  and search.
- `cpal` + linear resampler for audio I/O.
- `keyring` + `zeroize` for credentials.

A small `xtask/` crate replaces every shell script (see
[ADR 0002](0002-use-xtask.md)).

## Consequences

**Positives**

- One binary per OS, built by one `cargo build --release` invocation.
- `#![forbid(unsafe_code)]` is enforceable across our crate, which
  Swift's escape hatches couldn't match.
- Static type-checking covers the streaming SSE / WS state machines
  that were prone to runtime mistakes in the original.
- Significantly smaller memory footprint and faster cold start.

**Negatives**

- Lose SwiftUI's first-class macOS UI feel — we hand-built a theme
  (`src/theme.rs`) that approximates it, but a sufficiently advanced
  macOS user can still tell.
- Apple ecosystem integrations (Sparkle auto-update, Sandbox, etc.) now
  require explicit work; they were "free" with the Swift SDK.
- Anyone who knew the Swift codebase no longer knows this one.

## Reversibility

Low-cost for the first month (Swift code lived in git history at
commit `0f881e1`); decreasing thereafter as Rust-side features land.
