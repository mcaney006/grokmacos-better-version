# GrokInsane

A cross-platform desktop client for xAI Grok, rewritten from the ground up in
Rust + egui. Replaces the original SwiftUI macOS app with a single binary that
runs on macOS, Windows, and Linux.

## Highlights

- **Native everywhere.** One Rust codebase, no Xcode, GPU-accelerated UI via
  `eframe` + `wgpu`.
- **Streaming chat.** OpenAI-compatible Server-Sent Events parser feeds the UI
  token-by-token as Grok generates.
- **Voice mode.** WebSocket client for `wss://api.x.ai/v1/realtime` plus a
  cross-platform `cpal` audio engine for capture + playback, with a live
  waveform widget and selectable personality (Ara / Rex / Sal / Eve / Leo).
- **Embedded search.** Every message indexed by `tantivy`; full-text and
  fuzzy lookups stay fast even at 100k+ messages.
- **Embedded ACID store.** All chats, messages, and settings live in `redb`
  next to the app, no external database needed.
- **Secure secrets.** API keys stored in the OS keyring (Keychain / Credential
  Manager / Secret Service).
- **Optional local RAG.** Build with `--features rag` to add semantic
  retrieval over your own chat history via `fastembed`.
- **Performance dashboard.** Toggle from settings to see frame time, tokens/s,
  request latency, and resident memory.

## Build & run

Prerequisites:

- Rust **1.78+** (`rustup install stable`)
- Linux only: `libasound2-dev`, `libxkbcommon-dev`, `libwayland-dev`,
  `libxcb*-dev`, `libfontconfig1-dev`

```bash
# debug build
cargo run

# release build
cargo build --release
./target/release/grok-insane
```

Optional feature flags:

```bash
cargo run --features rag            # local semantic retrieval (downloads model)
cargo run --features hotkeys        # global system hotkeys
cargo run --features plugins        # WebAssembly plugin host (experimental)
```

## First-time setup

1. Launch the app.
2. Press <kbd>⌘ / Ctrl</kbd>+<kbd>,</kbd> to open Settings.
3. Choose a provider (default: xAI), paste your API key, click **Save key**.
4. Start chatting. <kbd>⌘ / Ctrl</kbd>+<kbd>N</kbd> for a new chat.

## Keyboard shortcuts

| Shortcut | Action |
|---|---|
| <kbd>⌘ / Ctrl</kbd>+<kbd>N</kbd> | New chat |
| <kbd>⌘ / Ctrl</kbd>+<kbd>,</kbd> | Toggle settings |
| <kbd>⌘ / Ctrl</kbd>+<kbd>.</kbd> | Stop generation |
| <kbd>⌘ / Ctrl</kbd>+<kbd>⇧</kbd>+<kbd>V</kbd> | Toggle voice mode |
| <kbd>Enter</kbd> | Send |
| <kbd>⇧</kbd>+<kbd>Enter</kbd> | Newline |

## Repository layout

```
src/
├── main.rs            entry point + tracing setup
├── app.rs             top-level eframe::App
├── config.rs          live settings handle
├── paths.rs           OS-specific app directories
├── secrets.rs         keyring wrapper
├── theme.rs           custom egui theme
├── error.rs           strongly-typed error enums
├── models.rs          Chat, Message, Settings, …
├── storage/
│   ├── mod.rs         redb façade
│   └── search.rs      tantivy index
├── services/
│   ├── chat.rs        xAI streaming client
│   ├── voice.rs       realtime WS client
│   ├── audio.rs       cpal capture + playback
│   ├── providers.rs   ChatProvider trait
│   └── embeddings.rs  optional RAG
└── ui/
    ├── chat_view.rs   transcript + composer
    ├── sidebar.rs     chat list
    ├── settings_view.rs
    ├── perf_dashboard.rs
    ├── waveform.rs    custom widget
    └── toast.rs       toast queue
```

## License

Dual licensed under MIT or Apache-2.0.
