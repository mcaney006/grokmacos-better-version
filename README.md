# GrokInsane

[![CI](https://github.com/mcaney006/grokmacos-better-version/actions/workflows/ci.yml/badge.svg)](https://github.com/mcaney006/grokmacos-better-version/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](#license)

A cross-platform desktop client for xAI Grok, OpenAI, Anthropic, and any local
OpenAI-compatible model. Rewritten from the original SwiftUI macOS app into
pure Rust + egui — a single binary that runs on **macOS, Windows, and Linux**
with the same code.

> No Xcode. No shell scripts. No Electron. One `cargo` build. ~80 MB RAM in
> normal use, sub-100 ms startup, GPU-accelerated UI via wgpu.

---

## Table of Contents

1. [Features](#features)
2. [Screenshots / UX](#screenshots--ux)
3. [Getting Started](#getting-started)
4. [Configuration](#configuration)
5. [Keyboard Shortcuts](#keyboard-shortcuts)
6. [Architecture](#architecture)
7. [Workspace Layout](#workspace-layout)
8. [Developer Workflow (`cargo xtask`)](#developer-workflow-cargo-xtask)
9. [CLI](#cli)
10. [Feature Flags](#feature-flags)
11. [Security Model](#security-model)
12. [Performance](#performance)
13. [Testing](#testing)
14. [Roadmap](#roadmap)
15. [License](#license)

---

## Features

| Area | What you get |
|---|---|
| **Providers** | xAI Grok (streaming) · OpenAI (streaming, shared decoder) · Anthropic (native Messages SSE w/ usage) · any local OpenAI-compatible endpoint (Ollama, LM Studio, llama.cpp server) |
| **Chat UI** | Multi-chat sidebar, full transcript history, sticky-to-bottom auto-scroll, virtualised scroll, Markdown rendering with code blocks via `egui_commonmark`, copy-message, ↻ regenerate |
| **Chat management** | Pin · archive · rename · delete · export to Markdown / Obsidian / JSON from a right-click menu |
| **Voice** | Realtime WebSocket to `wss://api.x.ai/v1/realtime`, full duplex audio via `cpal`, custom animated waveform, persona selector (Ara / Rex / Sal / Eve / Leo), client-side level metering, server-side VAD |
| **Search** | Every message indexed by `tantivy`; instant full-text + fuzzy queries even at 100k+ messages |
| **Storage** | Embedded ACID K/V via `redb`, composite-key range scans by chat & timestamp, no external database needed |
| **Secrets** | API keys live in the OS keyring (macOS Keychain / Windows Credential Manager / Linux Secret Service) with `zeroize` on in-memory copies |
| **RAG** *(opt-in feature)* | Local sentence embeddings via `fastembed`, semantic re-rank over lexical hits, top-k retrieval inserted as a system message |
| **Plugins** *(opt-in feature)* | WebAssembly host (`wasmtime`) — surface area reserved for follow-up |
| **Hotkeys** *(opt-in feature)* | Global system-wide hotkeys via `global-hotkey` |
| **Perf** | Built-in dashboard for frame time, fps, tokens/s, last-request latency, indexed message count, resident memory |
| **Theming** | Custom "cosmic" theme (neon green on dark) by default; Dark and Light fallbacks |
| **Persistence** | Chats, messages, settings, and search index all kept locally — works offline for everything except actual model calls |
| **Build & release** | Pure-Rust `cargo xtask` replaces shell scripts; CI matrix builds macOS, Windows, Linux on every push |

---

## Screenshots / UX

The app launches at **1280×820** with a **220 px side rail**, a **central chat
panel**, and a **floating settings window**. Default theme is a high-contrast
dark surface with `#78FFAF` neon-green accents matching the xAI palette.

```
┌──────────┬───────────────────────────────────────────────────┐
│ + New    │  ◉ My chat about Rust                             │
│  search… │  ────────────────────────────────────────────────  │
│ ─────    │                                                   │
│ ◉ Chat 1 │   USER                                            │
│   Chat 2 │   How do borrow checkers work?                   │
│ 📌 Chat 3│                                                   │
│   …      │   ASSISTANT (markdown rendered, code highlighted) │
│          │   ```rust                                         │
│          │   fn main() { … }                                 │
│          │   ```                                             │
│ v0.1.0   │                                                   │
│          │  ┌─────────────────── compose ──────────────────┐ │
│          │  │ message Grok…                       🎙 🔊 ➤ │ │
│          │  └────────────────────────────────────────────  ┘ │
└──────────┴───────────────────────────────────────────────────┘
```

---

## Getting Started

### Prerequisites

- **Rust 1.78+** — install via [rustup](https://rustup.rs/).
- **Linux only**: ALSA + X11/Wayland + fontconfig dev libs. You can install
  them with one command:
  ```
  cargo xtask install-deps
  ```
  …which runs `apt-get install -y libasound2-dev libxkbcommon-dev libwayland-dev
  libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
  libfontconfig1-dev`. For non-Debian distros install the equivalents.

### Build & run

```
git clone https://github.com/mcaney006/grokmacos-better-version
cd grokmacos-better-version

# debug build with live logs
cargo xtask dev

# or a plain release build
cargo build --release
./target/release/grok-insane
```

### First launch

1. Press <kbd>⌘ / Ctrl</kbd>+<kbd>,</kbd> to open **Settings**.
2. Pick a provider (default: xAI).
3. Paste your API key into the key field and click **Save key** — it lands
   in the OS keyring, never on disk in plaintext.
4. Press <kbd>⌘ / Ctrl</kbd>+<kbd>N</kbd> and start typing.

---

## Configuration

All user-visible config lives in the in-app **Settings** window.

| Setting | Notes |
|---|---|
| **Provider** | xAI · OpenAI · Anthropic · Local. Drives which API key slot is loaded and which client is used. |
| **API key** | Stored in the OS keyring under `com.grokinsane.grok-insane`, one entry per provider. |
| **Model** | Per-provider text field. Defaults: `grok-beta`, `gpt-4o-mini`, `claude-3-5-sonnet-latest`, `llama-3.1-8b`. |
| **Temperature** | `0.0 – 2.0`, half-step slider. |
| **Max tokens** | `64 – 131 072`. |
| **Theme** | Cosmic (default) / Dark / Light, hot-applied. |
| **Font size** | `10 – 24` pt, hot-applied. |
| **TTS enabled** | Toggle Grok speaking replies aloud. |
| **Voice persona** | Ara / Rex / Sal / Eve / Leo. |
| **System prompt** | Optional, prepended to every conversation. |
| **RAG enabled** | Requires `--features rag`. Top-K retrieval over your own chat history. |
| **RAG Top K** | `1 – 12`. |
| **Perf dashboard** | Show frame time / fps / tokens/s / resident memory in settings. |

Settings, chats, and messages are persisted to:

| OS | Data dir |
|---|---|
| macOS | `~/Library/Application Support/grok-insane/` |
| Linux | `~/.local/share/grok-insane/` (XDG) |
| Windows | `%APPDATA%\GrokInsane\grok-insane\data\` |

Run `grok-insane --diag` for the exact paths on your machine.

---

## Keyboard Shortcuts

| Shortcut | Action |
|---|---|
| <kbd>⌘ / Ctrl</kbd>+<kbd>N</kbd> | New chat |
| <kbd>⌘ / Ctrl</kbd>+<kbd>,</kbd> | Toggle Settings |
| <kbd>⌘ / Ctrl</kbd>+<kbd>.</kbd> | Stop generation |
| <kbd>⌘ / Ctrl</kbd>+<kbd>⇧</kbd>+<kbd>V</kbd> | Toggle voice mode |
| <kbd>Enter</kbd> | Send |
| <kbd>⇧</kbd>+<kbd>Enter</kbd> | Newline in composer |
| Right-click chat | Rename / Pin / Archive / Export / Delete |

---

## Architecture

```
┌─────────────────────────── UI thread ───────────────────────────┐
│                                                                  │
│   eframe::App  ─►  app::GrokApp                                  │
│                       │                                          │
│                       ├─►  ui::sidebar                           │
│                       ├─►  ui::chat_view  ─►  egui_commonmark    │
│                       ├─►  ui::settings_view + perf_dashboard    │
│                       ├─►  ui::waveform                          │
│                       └─►  ui::toast                             │
│                                                                  │
│   ▲   channels    ▲   channels                                   │
└───┼───────────────┼──────────────────────────────────────────────┘
    │               │
┌───┴───────────┐ ┌─┴──────────────────────────────────────────────┐
│ Tokio runtime │ │ cpal capture/playback streams (!Send)          │
│  • chat SSE   │ │  • mono PCM16 @ 24 kHz, linear resampler       │
│  • voice WS   │ │  • level meters (Arc<AtomicU32>), VoiceShared  │
└───┬───────────┘ └────────────────────────────────────────────────┘
    │
┌───┴────────────────────────────────────────────────────────────┐
│ Storage layer                                                  │
│  • redb K/V (chats, messages, settings) — composite keys for   │
│    range scans per chat ordered by timestamp                   │
│  • tantivy index over message bodies (TEXT + STORED), kept     │
│    eventually consistent via best-effort writes                │
└────────────────────────────────────────────────────────────────┘
```

Key design decisions:

- **Single binary**, no plugins required to be useful, but plugin host wired
  behind a feature flag for future extension.
- **Send / !Send split for voice**: `cpal::Stream` is `!Send`, so the live
  audio handles stay on the UI thread; channels + atomics (`VoiceShared`) are
  what the async WebSocket task touches.
- **Storage façade** is `Clone + Send + Sync`; an internal `Arc<Inner>`
  protects the tantivy writer with a single `parking_lot::Mutex` because redb
  already serialises writes.
- **No locks in the hot UI path**. Live settings are read via `ArcSwap` so the
  audio engine never blocks on a contended mutex.

---

## Workspace Layout

```
.
├── Cargo.toml                  # workspace + grok-insane package
├── Cargo.lock
├── .cargo/config.toml          # alias: cargo xtask
├── .github/workflows/ci.yml    # matrix CI: macOS / Windows / Linux
├── README.md
├── src/                        # the desktop app crate
│   ├── main.rs                 # entry + CLI (--version, --diag, --reset-db)
│   ├── app.rs                  # top-level eframe::App impl
│   ├── config.rs               # ArcSwap<Settings> handle
│   ├── error.rs                # typed errors
│   ├── models.rs               # Chat / Message / Settings / Provider / Role
│   ├── paths.rs                # XDG / Known Folder / macOS app paths
│   ├── secrets.rs              # OS keyring wrapper (zeroize)
│   ├── theme.rs                # cosmic / dark / light palettes
│   ├── services/
│   │   ├── providers.rs        # ChatProvider trait + ChatRequest/ChatEvent
│   │   ├── chat.rs             # xAI streaming + reusable OpenAI-compat SSE decoder
│   │   ├── openai.rs           # OpenAI client (delegates to chat.rs)
│   │   ├── anthropic.rs        # Anthropic Messages SSE decoder
│   │   ├── local.rs            # local OpenAI-compatible endpoint (Ollama, etc.)
│   │   ├── voice.rs            # tokio-tungstenite Realtime WS client
│   │   ├── audio.rs            # cpal capture + playback, resampler, level meter
│   │   ├── export.rs           # Markdown / Obsidian / JSON exporters
│   │   └── embeddings.rs       # RAG retriever (feature = "rag")
│   ├── storage/
│   │   ├── mod.rs              # redb façade (chats, messages, settings)
│   │   └── search.rs           # tantivy index over message bodies
│   └── ui/
│       ├── chat_view.rs        # transcript + composer + markdown
│       ├── sidebar.rs          # chat list + context menu (rename/pin/archive/export/delete)
│       ├── settings_view.rs    # modal settings window
│       ├── perf_dashboard.rs   # in-app stats
│       ├── waveform.rs         # custom animated bar widget
│       └── toast.rs            # bottom-right toast queue
└── xtask/                      # Rust build/release helpers (no shell scripts)
    ├── Cargo.toml
    └── src/main.rs             # check / fmt / lint / test / dev / dist / bundle / reset / install-deps / ci / clean
```

---

## Developer Workflow (`cargo xtask`)

There are no shell scripts in this repository. Build/release/dev helpers live
in the `xtask/` crate and are invoked with `cargo xtask <command>`.

| Command | What it does |
|---|---|
| `cargo xtask check` | `fmt --check` + `clippy --all-targets -- -D warnings` + `cargo test --workspace`. The exact pipeline CI runs. |
| `cargo xtask fmt` | `cargo fmt --all`. |
| `cargo xtask lint` | `cargo clippy --all-targets --workspace -- -D warnings`. |
| `cargo xtask test` | `cargo test --workspace`. |
| `cargo xtask dev [args]` | Debug build with `RUST_LOG=grok_insane=debug,info`. Forwards any extra args to the binary. |
| `cargo xtask dist` | Release build + copies the binary to `dist/<target>/grok-insane[.exe]`. |
| `cargo xtask bundle` | Release build + per-OS packaging: a real `.app` on macOS (incl. `Info.plist` with `NSMicrophoneUsageDescription`), `grok-insane.exe` on Windows, staged tar source on Linux. |
| `cargo xtask reset` | Calls `grok-insane --reset-db --yes`. |
| `cargo xtask install-deps` | `sudo apt-get install …` for the Linux dev libs (no-op on macOS / Windows). |
| `cargo xtask ci` | `install-deps` + `check` + `dist`. |
| `cargo xtask clean` | `cargo clean` + remove `dist/`. |
| `cargo xtask help` | Print the above. |

Add a new helper by editing [`xtask/src/main.rs`](xtask/src/main.rs); each
helper is a regular Rust function calling `std::process::Command`.

---

## CLI

`grok-insane` itself ships a small CLI surface so the binary is usable
headless (smoke tests, support sessions, CI):

```
grok-insane                  # launch the desktop app
grok-insane --version        # print version
grok-insane --help           # usage
grok-insane --diag           # self-test: paths, redb open, chat/message counts, keyring presence
grok-insane --reset-db --yes # wipe local DB + search index
```

Example `--diag` output:

```
data dir:    /root/.local/share/grok-insane
config dir:  /root/.config/grok-insane
cache dir:   /root/.cache/grok-insane
db path:     /root/.local/share/grok-insane/grok-insane.redb
index path:  /root/.local/share/grok-insane/search-index

chats:       0
messages:    0

api key [xai      ] missing
api key [openai   ] missing
api key [anthropic] missing
api key [local    ] missing
```

---

## Feature Flags

All are off by default — the base build stays slim.

| Flag | Adds |
|---|---|
| `rag` | `fastembed` for local sentence embeddings; the retriever re-ranks lexical hits by cosine similarity to the query. Model weights download on first use. |
| `hotkeys` | `global-hotkey` for system-wide shortcuts (e.g. ⌘⇧V to toggle voice from any window). |
| `plugins` | `wasmtime` host (Cranelift + async). Plugin surface area is reserved; ready to expand. |

```
cargo run --features rag
cargo run --features rag,hotkeys
cargo build --release --features plugins
```

---

## Security Model

- **Secrets** never touch disk in plaintext. API keys live in the OS keyring
  (`com.grokinsane.grok-insane`, one entry per provider). In-memory copies
  are wrapped in `zeroize::Zeroizing` so the buffer is overwritten on drop.
- **No telemetry**, no analytics, no third-party SDKs.
- **TLS** everywhere via `rustls` (no OpenSSL dependency); WebPKI roots are
  bundled at build time.
- **WS auth** rides as a `Bearer` header — same key as the REST API.
- **PCM audio** stays on your machine until you toggle voice mode; once
  enabled it is streamed to the configured xAI Realtime endpoint only.
- **Database** is a single redb file in your local data dir. It can be
  copied, backed up, or wiped with `grok-insane --reset-db --yes`.

---

## Performance

Targets the rewrite is built around (measured on an M2 Air with 50 k cached
messages):

| Metric | Target | Why |
|---|---|---|
| Cold startup | < 150 ms | redb open is lazy; eframe boots without glow. |
| Frame time | < 8 ms (120 fps) | Immediate-mode + GPU compositor + cached markdown. |
| Resident memory | < 80 MB idle | No GC, no JS, no embedded browser. |
| Search latency | < 5 ms / 100 k msgs | Tantivy MMAP segments. |
| Stream-to-screen latency | bounded by network | UI drains its channel each frame; no extra hop. |

Toggle the perf dashboard in **Settings → Show performance dashboard** to
watch live numbers.

---

## Testing

```
cargo xtask test       # the full suite, in workspace
cargo test --bin grok-insane storage::   # one module
```

Current coverage (10 unit tests, all green):

- `storage::tests::settings_roundtrip` — bincode round-trip through redb.
- `storage::tests::chat_and_messages_persist_and_order` — composite-key
  ordering by created_at.
- `storage::tests::delete_chat_removes_messages` — range delete cleanup.
- `storage::tests::delete_message_drops_it_from_history_and_index` —
  redb + tantivy stay in sync.
- `storage::tests::full_text_search_finds_keyword` — tantivy round-trip.
- `services::chat::tests::sse_decoder_extracts_deltas_and_done` — OpenAI-
  compatible SSE state machine.
- `services::anthropic::tests::anthropic_decoder_parses_text_delta_and_stop`
  — Anthropic event stream + usage tracking.
- `services::export::tests::markdown_export_includes_messages_and_title`
- `services::export::tests::obsidian_export_has_front_matter_and_quotes_title_with_colon`
- `services::export::tests::json_export_round_trips`

---

## Roadmap

Already shipped this iteration is in [Features](#features). Next planned:

- [ ] **Streaming TTS playback** — play audio chunks as they arrive instead of
      waiting on `audio.done`.
- [ ] **Interruptible TTS** — barge-in via a key while speaking.
- [ ] **Image input** for vision-capable models (xAI, OpenAI, Anthropic).
- [ ] **File / tool attachments** in the composer.
- [ ] **Plugin manifest** + a couple of reference WASM plugins behind the
      `plugins` feature.
- [ ] **`cargo xtask bundle` improvements** — codesign + notarise on macOS,
      MSIX on Windows, AppImage on Linux.
- [ ] **`cargo dist`** integration once we want signed GitHub Releases.

---

## License

Dual-licensed under either of:

- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.
