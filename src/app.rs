//! Top-level `eframe::App` implementation. Owns:
//! * persistent `Store` (redb + tantivy);
//! * a tokio runtime for network + voice tasks;
//! * the live `Settings` handle;
//! * an in-memory chat cache for the active chat;
//! * channels that funnel async results back into the UI thread.

use crate::config::SettingsHandle;
use crate::error::ApiError;
use crate::models::{Chat, Message, PerfStats, Provider, Role, Settings, WireMessage};
use crate::secrets;
use crate::services::audio::VoiceAudio;
use crate::services::embeddings::Retriever;
use crate::services::providers::{ChatEvent, ChatRequest};
use crate::services::voice::{VoiceEvent, VoiceSession};
use crate::storage::Store;
use crate::theme;
use crate::ui::chat_view::ChatViewState;
use crate::ui::settings_view::SettingsState;
use crate::ui::sidebar::{SidebarAction, SidebarState};
use crate::ui::toast::Toaster;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

pub struct GrokApp {
    store: Store,
    runtime: Runtime,
    settings: SettingsHandle,

    chats: Vec<Chat>,
    active_chat: Option<Uuid>,
    messages: Vec<Message>,

    sidebar: SidebarState,
    chat_view: ChatViewState,
    settings_view: SettingsState,
    toaster: Toaster,

    search_hits: Vec<crate::storage::search::Hit>,
    /// Search debounce. When the user types in the sidebar search box,
    /// every keystroke fires a `SidebarAction::Search(q)`. Hitting
    /// tantivy on each keystroke produces a stair-step of partial
    /// queries — `"r"`, `"ru"`, `"rus"`, `"rust"` — when only the final
    /// one is meaningful. We stash the query + when it was last
    /// changed; the next frame past `SEARCH_DEBOUNCE_MS` executes it.
    pending_search: Option<(String, Instant)>,

    // Streaming state
    stream_rx: Option<UnboundedReceiver<StreamMsg>>,
    cancel_flag: Arc<AtomicBool>,
    streaming_message_id: Option<Uuid>,
    stream_started_at: Option<Instant>,
    stream_input_tokens: u32,
    stream_output_tokens: u32,
    /// When we last persisted the in-flight streaming message to redb.
    /// `None` means "never since the current stream started" → flush on
    /// first delta. See `STREAM_PERSIST_DEBOUNCE` for the cadence.
    last_stream_persist: Option<Instant>,

    // Voice state (audio owns !Send cpal::Streams, so it lives on the UI thread only).
    voice_audio: Option<VoiceAudio>,
    voice_session: Option<VoiceSession>,
    voice_rx: Option<UnboundedReceiver<VoiceEvent>>,
    // One-shot delivery for the result of `VoiceSession::open(...)`. The
    // open happens on the tokio runtime so the UI never blocks; the result
    // lands here on completion and `drain_voice` picks it up.
    voice_open_rx: Option<tokio::sync::oneshot::Receiver<Result<VoiceSession, ApiError>>>,
    /// Handle to the spawned `VoiceSession::open` task so a mid-connect
    /// cancel (user rapid-toggles voice off) can `.abort()` it. Without
    /// this the task ran to its 10s WS connect timeout in the
    /// background, holding a TCP socket and TLS session for nothing.
    voice_open_task: Option<tokio::task::JoinHandle<()>>,

    // Perf
    stats: PerfStats,
    last_frame: Instant,
    frame_ema_ms: f32,
    sys: System,
    pid: Pid,
    last_mem_refresh: Instant,

    // Command palette (Cmd+K)
    palette: crate::ui::palette::PaletteState,
}

/// How often we flush a streaming assistant message to redb. Smaller
/// values reduce data loss on crash; larger values reduce I/O during
/// streaming. 500 ms is a good middle ground — at typical 30-60 tok/s
/// rates that's ~15-30 tokens of write amortisation, and the user
/// experiences zero perceptible UI delay.
const STREAM_PERSIST_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug)]
#[allow(dead_code)]
enum StreamMsg {
    Delta(Uuid, String),
    Usage(Uuid, u32, u32),
    Done(Uuid),
    Error(Uuid, String),
}

impl GrokApp {
    pub fn new(cc: &eframe::CreationContext<'_>, store: Store, runtime: Runtime) -> Self {
        let settings = store.load_settings().unwrap_or_default();
        let settings_handle = SettingsHandle::new(settings.clone());
        theme::apply(&cc.egui_ctx, settings.theme, settings.font_size);

        let chats = store.list_chats().unwrap_or_default();
        let active_chat = chats.first().map(|c| c.id);
        let messages = active_chat
            .and_then(|id| store.list_messages(id).ok())
            .unwrap_or_default();

        let settings_view_state = SettingsState {
            open: false,
            api_key_buffer: load_existing_key(settings.default_provider),
            api_key_dirty: false,
        };

        let mut chat_view = ChatViewState::default();
        chat_view.tts_enabled = settings.tts_enabled;

        Self {
            store,
            runtime,
            settings: settings_handle,
            chats,
            active_chat,
            messages,
            sidebar: SidebarState::default(),
            chat_view,
            settings_view: settings_view_state,
            toaster: Toaster::default(),
            search_hits: Vec::new(),
            pending_search: None,
            stream_rx: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            streaming_message_id: None,
            stream_started_at: None,
            stream_input_tokens: 0,
            stream_output_tokens: 0,
            last_stream_persist: None,
            voice_audio: None,
            voice_session: None,
            voice_rx: None,
            voice_open_rx: None,
            voice_open_task: None,
            stats: PerfStats::default(),
            last_frame: Instant::now(),
            frame_ema_ms: 16.0,
            sys: System::new_with_specifics(
                RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing().with_memory()),
            ),
            pid: Pid::from(std::process::id() as usize),
            last_mem_refresh: Instant::now() - Duration::from_secs(10),
            palette: crate::ui::palette::PaletteState::default(),
        }
    }

    fn refresh_messages(&mut self) {
        self.messages = match self.active_chat {
            Some(id) => self.store.list_messages(id).unwrap_or_default(),
            None => Vec::new(),
        };
    }

    fn new_chat(&mut self) {
        let s = self.settings.get();
        let mut chat = Chat::new(
            s.default_provider.id(),
            provider_model(&s, s.default_provider),
        );
        chat.temperature = s.temperature;
        chat.system_prompt = s.system_prompt.clone();
        if let Err(e) = self.store.upsert_chat(&chat) {
            self.toaster.error(format!("could not create chat: {e}"));
            return;
        }
        self.active_chat = Some(chat.id);
        self.chats.insert(0, chat);
        self.messages.clear();
    }

    fn send_user_message(&mut self, body: String) {
        if self.streaming_message_id.is_some() {
            return;
        }
        // Empty + whitespace-only prompts: silently dropped before this
        // fix, which made `Enter` on an empty composer eat the keypress
        // with no feedback. Now we tell the user and bail before
        // allocating a Message / opening a chat row.
        let trimmed = body.trim();
        if trimmed.is_empty() {
            self.toaster.warn("empty message — type something to send");
            return;
        }
        // Oversize prompts: refuse anything past `MAX_PROMPT_BYTES`. The
        // real ceiling is the provider's token limit (4k-200k depending
        // on model), but tokens != bytes and we'd rather fail fast
        // than ship 4 MB of pasted-clipboard noise across the network.
        // 256 KiB is comfortably more than any human-typed prompt and
        // still well below any provider's hard cap.
        const MAX_PROMPT_BYTES: usize = 256 * 1024;
        if body.len() > MAX_PROMPT_BYTES {
            self.toaster.error(format!(
                "prompt is {} KB — capped at {} KB; trim or split it",
                body.len() / 1024,
                MAX_PROMPT_BYTES / 1024,
            ));
            return;
        }
        if self.active_chat.is_none() {
            self.new_chat();
        }
        let Some(chat_id) = self.active_chat else {
            return;
        };

        let user_msg = Message::new(chat_id, Role::User, body.clone());
        if let Err(e) = self.store.insert_message(&user_msg) {
            self.toaster.error(format!("could not save message: {e}"));
            return;
        }
        self.messages.push(user_msg);

        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            if c.title == "New Chat" {
                c.title = Chat::derive_title(&body);
            }
            c.updated_at = chrono::Utc::now();
            let _ = self.store.upsert_chat(c);
        }

        let s = self.settings.get();
        // Resolve provider + model from the active chat. One `find` pass
        // instead of two — the previous version walked self.chats twice
        // and re-cloned independently.
        let chat = self.chats.iter().find(|c| c.id == chat_id);
        let provider = chat.map_or_else(|| "xai".to_owned(), |c| c.provider.clone());
        let model = chat.map_or_else(
            || provider_model(&s, s.default_provider).to_owned(),
            |c| c.model.clone(),
        );

        let messages: Vec<WireMessage> = self.messages.iter().map(WireMessage::from).collect();
        // RAG augmentation was previously a synchronous call here on the UI
        // thread. With `--features rag` that's hundreds of milliseconds of
        // ONNX inference; eframe stalls for the duration. Now we capture
        // the inputs and run augmentation inside the spawned tokio task
        // (via spawn_blocking, since fastembed isn't async-aware).
        let rag_enabled = s.rag_enabled;
        let rag_top_k = s.rag_top_k.max(1) as usize;
        // Last use of `body` — move it into the rag query rather than clone.
        let rag_query = body;
        let store_for_rag = if rag_enabled {
            Some(self.store.clone())
        } else {
            None
        };

        let api_key = match secrets::get_api_key(&provider) {
            Ok(Some(k)) => (*k).clone(),
            Ok(None) => {
                self.toaster
                    .warn("no API key configured — open settings to add one");
                return;
            }
            Err(e) => {
                self.toaster.error(format!("keyring: {e}"));
                return;
            }
        };

        let mut assistant_msg = Message::new(chat_id, Role::Assistant, String::new());
        let assistant_id = assistant_msg.id;
        assistant_msg.provider = Some(provider.clone());
        assistant_msg.model = Some(model.clone());
        if let Err(e) = self.store.insert_message(&assistant_msg) {
            self.toaster
                .error(format!("could not save placeholder: {e}"));
            return;
        }
        self.messages.push(assistant_msg);

        let (tx, rx) = unbounded_channel::<StreamMsg>();
        self.stream_rx = Some(rx);
        self.streaming_message_id = Some(assistant_id);
        self.chat_view.streaming = true;
        self.cancel_flag.store(false, Ordering::SeqCst);
        let cancel = self.cancel_flag.clone();
        self.stream_started_at = Some(Instant::now());
        self.last_stream_persist = None;
        self.stream_input_tokens = 0;
        self.stream_output_tokens = 0;

        let temperature = s.temperature;
        let max_tokens = s.max_tokens;
        let system_prompt = s.system_prompt.clone();
        let provider_id = match provider.as_str() {
            "xai" => Provider::Xai,
            "openai" => Provider::OpenAi,
            "anthropic" => Provider::Anthropic,
            _ => Provider::Local,
        };

        // Drop guard that fires StreamMsg::Done if the task ends without
        // having sent a terminal message itself. Belt-and-braces: a panic
        // inside run_completion (e.g., a future bug we haven't seen) would
        // otherwise leave `streaming_message_id` set forever and freeze
        // the UI's send button. With the guard, every code path — clean,
        // error, or panic — drops the guard which flushes a Done so the
        // UI can recover.
        struct DoneOnDrop {
            tx: tokio::sync::mpsc::UnboundedSender<StreamMsg>,
            id: Uuid,
            fired: std::sync::atomic::AtomicBool,
        }
        impl DoneOnDrop {
            fn mark_fired(&self) {
                self.fired.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        impl Drop for DoneOnDrop {
            fn drop(&mut self) {
                if !self.fired.load(std::sync::atomic::Ordering::SeqCst) {
                    // Best-effort: receiver may already be gone if the
                    // UI tore down the chat. That's fine — UI then has
                    // no streaming state to clean up.
                    let _ = self.tx.send(StreamMsg::Done(self.id));
                }
            }
        }

        self.runtime.spawn(async move {
            // RAG augmentation runs here (off the UI thread). We use
            // spawn_blocking because fastembed::TextEmbedding is sync and
            // CPU-heavy; we want it on a dedicated worker, not stalling
            // a tokio I/O thread.
            let messages = if let Some(store) = store_for_rag {
                let q = rag_query.clone();
                match tokio::task::spawn_blocking(move || {
                    augment_with_rag_blocking(store, messages, &q, rag_top_k)
                })
                .await
                {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "rag worker panicked; sending without context");
                        Vec::new() // fall through with empty so request still goes
                    }
                }
            } else {
                messages
            };

            let request = ChatRequest {
                model: model.clone(),
                messages,
                temperature,
                max_tokens: Some(max_tokens),
                system_prompt,
            };

            let guard = DoneOnDrop {
                tx: tx.clone(),
                id: assistant_id,
                fired: std::sync::atomic::AtomicBool::new(false),
            };
            let result = run_completion(
                provider_id,
                api_key,
                request,
                cancel,
                tx.clone(),
                assistant_id,
            )
            .await;
            match result {
                Ok(()) => {
                    // run_completion always sends StreamMsg::Done on the
                    // clean path before returning, so suppress the guard.
                    guard.mark_fired();
                }
                Err(e) => {
                    let _ = tx.send(StreamMsg::Error(assistant_id, e.to_string()));
                    // The Error message itself terminates the UI's
                    // streaming state; suppress the guard's Done so the
                    // UI doesn't receive Done-after-Error and overwrite
                    // the error.
                    guard.mark_fired();
                }
            }
            // If we never reach either arm (panic from run_completion),
            // `guard` drops with `fired = false` and the destructor
            // sends a Done so the UI unblocks.
        });
    }

    // RAG used to live as a method here; it now runs off the UI thread via
    // `augment_with_rag_blocking` (free function below).

    fn cancel_stream(&mut self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    fn drain_stream(&mut self) {
        let Some(mut rx) = self.stream_rx.take() else {
            return;
        };
        let mut should_clear = false;
        // Bound the per-frame drain. Channel is unbounded (producer runs
        // in tokio::spawn, UI consumes here at ~60Hz) but if a provider
        // burst-emits N deltas during a single frame, processing them
        // ALL in one drain pass would stretch the frame budget. Each
        // delta is a small mutation but the markdown-render path further
        // up is non-trivial, and a 1000-delta burst on a thinking-style
        // model can blow past 16 ms. Cap at a generous bound and let
        // the next frame pick up the tail — egui's continuous repaint
        // is already requested whenever a stream is in flight.
        const DRAIN_BUDGET: usize = 256;
        let mut buffered: Vec<StreamMsg> = Vec::with_capacity(DRAIN_BUDGET.min(32));
        for _ in 0..DRAIN_BUDGET {
            match rx.try_recv() {
                Ok(msg) => buffered.push(msg),
                Err(_) => break,
            }
        }
        // Debounce redb writes. The previous version did
        // `store.update_message(m)` per token — at 100 tok/s that's 100
        // ACID write transactions/sec, each re-serialising the entire
        // growing message body via bincode. Total wall time was O(N²) in
        // the response length. We now persist at most every
        // STREAM_PERSIST_DEBOUNCE and force one final write on terminal
        // events (Done / Error). Crash recovery loses at most the last
        // ~500 ms of streamed text, which is acceptable for a chat client.
        let mut dirty_id: Option<Uuid> = None;
        for msg in buffered {
            match msg {
                StreamMsg::Delta(id, delta) => {
                    if let Some(m) = self.messages.iter_mut().find(|m| m.id == id) {
                        m.content.push_str(&delta);
                        dirty_id = Some(id);
                    }
                }
                StreamMsg::Usage(_, input, output) => {
                    self.stream_input_tokens = input;
                    self.stream_output_tokens = output;
                }
                StreamMsg::Done(id) => {
                    if let Some(m) = self.messages.iter_mut().find(|m| m.id == id) {
                        m.tokens = Some(self.stream_output_tokens);
                        let _ = self.store.update_message(m);
                    }
                    if let Some(start) = self.stream_started_at.take() {
                        // Saturate u128 → u32 via min before truncating.
                        // `From` would be the lossless way for the
                        // `u32::MAX` operand but here we need the
                        // saturating-truncate semantics.
                        let elapsed_ms =
                            u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX);
                        self.stats.last_request_ms = elapsed_ms;
                        let secs = (elapsed_ms.max(1) as f32) / 1000.0;
                        self.stats.tokens_per_sec = self.stream_output_tokens as f32 / secs;
                    }
                    dirty_id = None; // already flushed above
                    should_clear = true;
                }
                StreamMsg::Error(id, err) => {
                    if let Some(m) = self.messages.iter_mut().find(|m| m.id == id) {
                        if m.content.is_empty() {
                            m.content = format!("⚠ {err}");
                        } else {
                            // `write!` against String is infallible; the
                            // only way `write_str` returns Err for a
                            // String impl is via allocator failure,
                            // which would panic regardless. `let _`
                            // documents the intentional discard.
                            let _ = write!(m.content, "\n\n⚠ stream ended early: {err}");
                        }
                        let _ = self.store.update_message(m);
                    }
                    self.toaster.error(err);
                    dirty_id = None;
                    should_clear = true;
                }
            }
        }
        // Debounced persistence of streamed deltas. We use
        // `update_message_no_index` here — the message is still being
        // streamed, indexing each partial body is wasted work (tantivy
        // commits a new segment per call), and a half-written message
        // has no search utility. The terminal Done/Error arms above
        // call `update_message` with the indexing path so the final
        // body lands in the search index.
        if let Some(id) = dirty_id {
            let now = Instant::now();
            let should_flush = self
                .last_stream_persist
                .map(|t| now.duration_since(t) >= STREAM_PERSIST_DEBOUNCE)
                .unwrap_or(true);
            if should_flush {
                if let Some(m) = self.messages.iter().find(|m| m.id == id) {
                    let _ = self.store.update_message_no_index(m);
                    self.last_stream_persist = Some(now);
                }
            }
        }
        if !should_clear {
            self.stream_rx = Some(rx);
        } else {
            self.streaming_message_id = None;
            self.chat_view.streaming = false;
        }
    }

    /// 200 ms after the user stops typing in the sidebar search box,
    /// fire the actual tantivy query. Hits per-frame are cheap (one
    /// Instant comparison) so this lives on the same drain cadence as
    /// streams + voice.
    fn drain_pending_search(&mut self) {
        const SEARCH_DEBOUNCE_MS: u128 = 200;
        if let Some((ref q, when)) = self.pending_search {
            if when.elapsed().as_millis() >= SEARCH_DEBOUNCE_MS {
                let q = q.clone();
                self.pending_search = None;
                // 40 is the same per-query cap the old inline call used
                // — enough to populate the sidebar's "matches in" set
                // without paging.
                self.search_hits = self.store.search(&q, 40).unwrap_or_default();
            }
        }
    }

    fn drain_voice(&mut self) {
        // Step 1: pick up the result of an in-flight `VoiceSession::open`
        // if one finished since the last frame. Non-blocking; if it's not
        // ready yet we put the receiver back and try again next frame.
        if let Some(mut open_rx) = self.voice_open_rx.take() {
            match open_rx.try_recv() {
                Ok(Ok(session)) => {
                    self.voice_session = Some(session);
                    self.chat_view.voice_active = true;
                }
                Ok(Err(e)) => {
                    self.toaster.error(format!("voice: {e}"));
                    self.voice_audio = None;
                    self.voice_rx = None;
                    self.chat_view.voice_active = false;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    // Still connecting; check again next frame.
                    self.voice_open_rx = Some(open_rx);
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    // Sender dropped without sending — treat as failure.
                    self.toaster.error("voice: connect aborted");
                    self.voice_audio = None;
                    self.voice_rx = None;
                    self.chat_view.voice_active = false;
                }
            }
        }

        let Some(mut rx) = self.voice_rx.take() else {
            return;
        };
        let mut buffered: Vec<VoiceEvent> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            buffered.push(event);
        }
        let mut closed = false;
        for event in buffered {
            match event {
                VoiceEvent::Connected => {
                    self.toaster.info("voice session connected");
                }
                VoiceEvent::PartialTranscript(text) | VoiceEvent::FinalTranscript(text) => {
                    if !text.is_empty() {
                        if !self.chat_view.input.is_empty() && !self.chat_view.input.ends_with(' ')
                        {
                            self.chat_view.input.push(' ');
                        }
                        self.chat_view.input.push_str(&text);
                    }
                }
                VoiceEvent::AssistantTextDelta(delta) => {
                    if !delta.is_empty() {
                        if let Some(chat_id) = self.active_chat {
                            self.append_assistant_voice_delta(chat_id, delta);
                        }
                    }
                }
                VoiceEvent::AssistantTextDone => {}
                VoiceEvent::SpeechStarted | VoiceEvent::SpeechStopped => {}
                VoiceEvent::Error(e) => {
                    // An error from any of the WS health paths
                    // (keepalive ping send failure, uplink send timeout,
                    // receive watchdog deadline, downlink WS error) means
                    // the session is no longer usable. Show the toast
                    // AND tear the session down — leaving voice_active=
                    // true with a broken WS would let the user think
                    // they're still recording when nothing's reaching
                    // the server. UI teardown happens via the same path
                    // as Closed.
                    self.toaster.error(format!("voice: {e}"));
                    closed = true;
                }
                VoiceEvent::Closed => {
                    closed = true;
                }
            }
        }
        if closed {
            self.toaster.info("voice session closed");
            self.voice_session = None;
            self.chat_view.voice_active = false;
            self.voice_audio = None;
        } else {
            self.voice_rx = Some(rx);
        }
    }

    fn append_assistant_voice_delta(&mut self, chat_id: Uuid, delta: String) {
        let last_assistant = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.chat_id == chat_id && matches!(m.role, Role::Assistant));
        if let Some(m) = last_assistant {
            m.content.push_str(&delta);
            let _ = self.store.update_message(m);
        } else {
            let mut m = Message::new(chat_id, Role::Assistant, delta);
            m.provider = Some("xai-voice".into());
            let _ = self.store.insert_message(&m);
            self.messages.push(m);
        }
    }

    fn toggle_voice(&mut self) {
        if let Some(session) = self.voice_session.take() {
            session.close();
            self.voice_rx = None;
            self.voice_open_rx = None;
            // Abort the open task too, in case a session opened between
            // the open spawn and this close (timing window is small but
            // we don't want it hanging on to a runtime slot).
            if let Some(t) = self.voice_open_task.take() {
                t.abort();
            }
            self.chat_view.voice_active = false;
            self.voice_audio = None;
            return;
        }
        // If a previous toggle is mid-connect, ABORT the spawned task,
        // don't just drop the receiver. Previously the task ran to its
        // 10s WS connect timeout in the background while the user had
        // already moved on — pure waste of a tokio worker and an open
        // TCP/TLS handshake.
        if self.voice_open_rx.is_some() {
            self.voice_open_rx = None;
            if let Some(t) = self.voice_open_task.take() {
                t.abort();
            }
            self.voice_audio = None;
            self.voice_rx = None;
            self.chat_view.voice_active = false;
            return;
        }

        let api_key = match secrets::get_api_key("xai") {
            Ok(Some(k)) => (*k).clone(),
            Ok(None) => {
                self.toaster.warn("add an xAI API key to use voice");
                return;
            }
            Err(e) => {
                self.toaster.error(format!("keyring: {e}"));
                return;
            }
        };
        let audio = match VoiceAudio::new() {
            Ok(a) => a,
            Err(e) => {
                self.toaster.error(format!("audio: {e}"));
                return;
            }
        };
        let shared = audio.shared();
        self.voice_audio = Some(audio);

        let persona = self.settings.get().voice_persona;
        let (forward_tx, forward_rx) = unbounded_channel::<VoiceEvent>();
        self.voice_rx = Some(forward_rx);

        // Open the WebSocket on the runtime, NOT on the UI thread. The
        // previous code called `handle.block_on(VoiceSession::open(...))`,
        // which froze the entire eframe loop for the duration of the TCP
        // connect + TLS handshake — up to the full 10-second timeout if the
        // network was down. Now the UI returns immediately; the session
        // delivers itself through `voice_open_rx`, and `drain_voice` picks
        // it up on the next frame.
        let (session_tx, session_rx) = tokio::sync::oneshot::channel();
        self.voice_open_rx = Some(session_rx);
        self.chat_view.voice_active = true; // optimistic — flipped back on failure
        let handle = self.runtime.handle().clone();
        let join_handle = handle.spawn(async move {
            let outcome = VoiceSession::open(api_key, persona, shared).await;
            let _ = session_tx.send(outcome.map(|mut session| {
                // Forward events from the session into the UI-bound channel
                // on the same runtime task that opened it. Spawning a
                // separate forwarder kept the original code simpler but
                // doubled the channel hop; in-task is fine.
                let events_rx = std::mem::replace(&mut session.events, mpsc_dummy());
                tokio::spawn(async move {
                    let mut events_rx = events_rx;
                    while let Some(ev) = events_rx.recv().await {
                        if forward_tx.send(ev).is_err() {
                            break;
                        }
                    }
                });
                session
            }));
        });
        self.voice_open_task = Some(join_handle);
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context, toggle_voice: &mut bool) {
        ctx.input_mut(|i| {
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::N,
            )) {
                self.new_chat();
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::Comma,
            )) {
                self.settings_view.open = !self.settings_view.open;
                if self.settings_view.open {
                    let provider = self.settings.get().default_provider;
                    self.settings_view.api_key_buffer = load_existing_key(provider);
                    self.settings_view.api_key_dirty = false;
                }
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::Period,
            )) && self.streaming_message_id.is_some()
            {
                self.cancel_flag.store(true, Ordering::SeqCst);
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::V,
            )) {
                *toggle_voice = true;
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::K,
            )) {
                self.palette.open();
            }
        });
    }

    fn handle_palette_action(
        &mut self,
        ctx: &egui::Context,
        action: crate::ui::palette::PaletteAction,
    ) {
        use crate::ui::palette::PaletteAction as A;
        match action {
            A::None => {}
            A::NewChat => self.new_chat(),
            A::OpenSettings => self.settings_view.open = true,
            A::ToggleVoice => self.toggle_voice(),
            A::ToggleTts => {
                let new_state = !self.chat_view.tts_enabled;
                self.chat_view.tts_enabled = new_state;
                let updated = self.settings.update(|s| s.tts_enabled = new_state);
                let _ = self.store.save_settings(&updated);
            }
            A::ToggleRag => {
                let updated = self.settings.update(|s| s.rag_enabled = !s.rag_enabled);
                let _ = self.store.save_settings(&updated);
                self.toaster.info(format!(
                    "RAG {}",
                    if updated.rag_enabled { "on" } else { "off" }
                ));
            }
            A::Theme(mode) => {
                let updated = self.settings.update(|s| s.theme = mode);
                theme::apply(ctx, mode, updated.font_size);
                let _ = self.store.save_settings(&updated);
            }
            A::Provider(p) => {
                let updated = self.settings.update(|s| s.default_provider = p);
                let _ = self.store.save_settings(&updated);
                self.settings_view.api_key_buffer = load_existing_key(p);
                self.toaster
                    .info(format!("provider: {label}", label = p.label()));
            }
            A::SelectChat(id) => {
                self.active_chat = Some(id);
                self.refresh_messages();
            }
            A::ExportActiveChat => {
                if let Some(id) = self.active_chat {
                    self.export_chat(id, crate::services::export::Format::Markdown);
                }
            }
            A::Quit => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn toggle_pin(&mut self, id: Uuid) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == id) {
            c.pinned = !c.pinned;
            c.updated_at = chrono::Utc::now();
            if let Err(e) = self.store.upsert_chat(c) {
                self.toaster.error(format!("pin: {e}"));
            }
        }
        // Re-sort so pinned chats bubble to the top of the rail.
        self.chats.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
    }

    fn toggle_archive(&mut self, id: Uuid) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == id) {
            c.archived = !c.archived;
            c.updated_at = chrono::Utc::now();
            if let Err(e) = self.store.upsert_chat(c) {
                self.toaster.error(format!("archive: {e}"));
            }
        }
    }

    fn rename_chat(&mut self, id: Uuid, title: String) {
        let title = if title.is_empty() {
            "New Chat".to_string()
        } else {
            title
        };
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == id) {
            c.title = title;
            c.updated_at = chrono::Utc::now();
            if let Err(e) = self.store.upsert_chat(c) {
                self.toaster.error(format!("rename: {e}"));
            }
        }
    }

    fn export_chat(&mut self, id: Uuid, format: crate::services::export::Format) {
        let Some(chat) = self.chats.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        let messages = match self.store.list_messages(id) {
            Ok(m) => m,
            Err(e) => {
                self.toaster.error(format!("export read: {e}"));
                return;
            }
        };
        let body = crate::services::export::export(&chat, &messages, format);
        // Filename sanitisation:
        // 1. Replace every non-alphanumeric with `-` so titles can't smuggle
        //    `..`, `/`, or NUL bytes into the path.
        // 2. Cap at 64 chars so a pathological title can't blow past the OS
        //    filename limit (255 bytes on most FS, less on some).
        // 3. Collapse repeated `-` to keep the output readable.
        let safe_title: String = chat
            .title
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .take(64)
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let safe_title = if safe_title.is_empty() {
            "chat".to_string()
        } else {
            safe_title
        };
        let filename = format!(
            "grok-insane-{}-{}.{}",
            safe_title,
            chat.created_at.format("%Y%m%d-%H%M%S"),
            format.extension()
        );
        let dest = crate::paths::data_dir().join("exports").join(&filename);
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                self.toaster.error(format!("mkdir: {e}"));
                return;
            }
        }
        match std::fs::write(&dest, body) {
            Ok(()) => {
                let display = dest.display();
                self.toaster.info(format!("exported to {display}"));
            }
            Err(e) => self.toaster.error(format!("write: {e}")),
        }
    }

    /// Truncate the chat to the message immediately before the targeted
    /// assistant reply, then re-run completion. The targeted reply is removed
    /// from history so the new generation takes its place.
    fn regenerate(&mut self, target: Uuid) {
        if self.streaming_message_id.is_some() {
            self.toaster.warn("wait for current generation to finish");
            return;
        }
        let Some(idx) = self.messages.iter().position(|m| m.id == target) else {
            return;
        };
        let target_msg = self.messages[idx].clone();
        if !matches!(target_msg.role, Role::Assistant) {
            return;
        }
        let chat_id = target_msg.chat_id;

        // Drop the targeted assistant message + everything after it. We need a
        // *user* turn to resume from, so find the preceding user message.
        let resume_from = self.messages[..idx]
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::User))
            .cloned();
        let Some(user_msg) = resume_from else {
            self.toaster
                .warn("no prior user message to regenerate from");
            return;
        };
        let last_user_idx = self
            .messages
            .iter()
            .position(|m| m.id == user_msg.id)
            .unwrap_or(idx);

        // Hard-delete the assistant reply + any later messages.
        for m in self.messages.split_off(last_user_idx + 1) {
            if let Err(e) = self.store.delete_message(&m) {
                tracing::warn!(error = %e, "delete during regenerate failed");
            }
        }
        // Also remove the user turn we're replaying — `send_user_message`
        // re-inserts it so we don't end up with a duplicate.
        if let Err(e) = self.store.delete_message(&user_msg) {
            tracing::warn!(error = %e, "delete user during regenerate failed");
        }
        self.messages.retain(|m| m.id != user_msg.id);

        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.updated_at = chrono::Utc::now();
            let _ = self.store.upsert_chat(c);
        }
        let body = user_msg.content;
        self.send_user_message(body);
    }

    fn tick_perf(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32() * 1000.0;
        self.last_frame = now;
        self.frame_ema_ms = self.frame_ema_ms * 0.9 + dt * 0.1;
        self.stats.frame_ms = self.frame_ema_ms;
        self.stats.fps = if self.frame_ema_ms > 0.0 {
            1000.0 / self.frame_ema_ms
        } else {
            0.0
        };
        if let Ok(n) = self.store.count_messages() {
            self.stats.messages_indexed = n;
        }
        // Refresh process memory at most once a second — it's the only system
        // call in the hot loop, so we don't want to do it every frame.
        if now.duration_since(self.last_mem_refresh) >= Duration::from_secs(1) {
            self.sys.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::Some(&[self.pid]),
                true,
                ProcessRefreshKind::nothing().with_memory(),
            );
            if let Some(proc_) = self.sys.process(self.pid) {
                self.stats.mem_bytes = proc_.memory();
            }
            self.last_mem_refresh = now;
        }
    }
}

/// Synchronous RAG augmentation, designed to run inside
/// `tokio::task::spawn_blocking`. Builds a "Relevant prior context"
/// system message from the top-k tantivy hits (and, with `--features rag`,
/// semantic re-ranking via fastembed) and prepends it to `messages`.
fn augment_with_rag_blocking(
    store: crate::storage::Store,
    mut messages: Vec<WireMessage>,
    query: &str,
    top_k: usize,
) -> Vec<WireMessage> {
    let retriever = Retriever::new(store);
    let hits = retriever.retrieve(query, top_k).unwrap_or_default();
    if hits.is_empty() {
        return messages;
    }
    let mut context = String::from("Relevant prior context:\n");
    for h in &hits {
        let _ = writeln!(context, "- {}", h.snippet.replace('\n', " "));
    }
    messages.insert(
        0,
        WireMessage {
            role: "system".into(),
            content: context,
        },
    );
    messages
}

fn provider_model(s: &Settings, p: Provider) -> &str {
    match p {
        Provider::Xai => &s.xai_model,
        Provider::OpenAi => &s.openai_model,
        Provider::Anthropic => &s.anthropic_model,
        Provider::Local => &s.local_model,
    }
}

/// Load the API key for `provider` from the OS keyring into a plain
/// `String` for the settings text-edit buffer. The key is wiped via
/// `SettingsState::clear_securely` when the dialog closes; we don't
/// hold it in a `Zeroizing<String>` because egui's `TextBuffer` trait
/// isn't implemented for any wrapper and the intermediate buffer
/// reallocations during typing would leak old copies regardless.
fn load_existing_key(provider: Provider) -> String {
    match secrets::get_api_key(provider.id()) {
        // `Zeroizing<String>` from the keyring is wiped when it drops
        // at the end of this match arm. The plain-String copy we
        // return lives in `api_key_buffer` and gets scrubbed by
        // `clear_securely` on dialog close.
        Ok(Some(k)) => (*k).clone(),
        _ => String::new(),
    }
}

fn mpsc_dummy() -> UnboundedReceiver<VoiceEvent> {
    let (_, rx) = unbounded_channel();
    rx
}

async fn run_completion(
    provider: Provider,
    api_key: String,
    request: ChatRequest,
    cancel: Arc<AtomicBool>,
    tx: UnboundedSender<StreamMsg>,
    assistant_id: Uuid,
) -> Result<(), ApiError> {
    // One structured span per stream so production debugging
    // ("user's stream hung last night at 11pm") has a stable correlation
    // key. The span fields are `assistant_id`, `provider`, and `model`;
    // the api_key is deliberately NOT a field (would land in tracing
    // output as soon as anyone bumped the log level to TRACE).
    let span = tracing::info_span!(
        "chat_stream",
        assistant = %assistant_id,
        provider = ?provider,
        model = %request.model,
    );
    let _enter = span.enter();
    tracing::info!(prompt_msgs = request.messages.len(), "stream open");
    let client = crate::services::providers::make_client(provider, api_key);
    let stream = client.stream(request).await?;
    let started = std::time::Instant::now();
    consume_chat_stream(stream, cancel, tx, assistant_id).await;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "stream closed"
    );
    Ok(())
}

/// Consume a `ChatEvent` stream into `StreamMsg`s on `tx`. Extracted from
/// `run_completion` so the cancellation + event-translation logic is
/// testable without needing to mock a real `ChatProvider`. The cancel
/// flag is checked **before** each event so a cancel set between the
/// previous tx.send and the next stream.next is honoured promptly.
///
/// Termination contract: this function returns when ANY of:
///
/// - the stream yields `None` (clean EOF),
/// - an event is `Ok(ChatEvent::Done)` (provider terminator),
/// - an event is `Err(_)` (any stream error),
/// - the cancel flag is `true` on entry to the loop body.
///
/// In every termination path, exactly ONE terminal `StreamMsg::Done` or
/// `StreamMsg::Error` is sent. The caller's `DoneOnDrop` guard is the
/// only thing that fires `Done` if this function panics.
async fn consume_chat_stream(
    stream: crate::services::providers::EventStream,
    cancel: Arc<AtomicBool>,
    tx: UnboundedSender<StreamMsg>,
    assistant_id: Uuid,
) {
    use futures_util::StreamExt;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            let _ = tx.send(StreamMsg::Done(assistant_id));
            return;
        }
        match item {
            Ok(ChatEvent::Delta(delta)) => {
                let _ = tx.send(StreamMsg::Delta(assistant_id, delta));
            }
            Ok(ChatEvent::ToolUse { id, name, input }) => {
                // No dedicated tool-call UI yet — inject the call as a
                // fenced `tool-use` code block so it renders inline in
                // the markdown view and shows up verbatim in the chat
                // log. When the proper tool-runner ships, this passes
                // through a structured StreamMsg::ToolUse instead.
                //
                // Backtick injection guard: if the model returns a tool
                // input whose string fields contain ``` we'd break out
                // of our fence and the trailing characters would render
                // as markdown prose (potentially rendering attacker-
                // controlled HTML if commonmark allows it). Use a
                // tilde-fence and escape any literal tilde-fences in
                // the serialised input. Both `id` and `name` are bound
                // by the wire schema to ASCII identifiers; we sanitise
                // them defensively anyway.
                tracing::info!(%id, %name, ?input, "anthropic tool_use");
                let safe_input = input
                    .to_string()
                    .replace("~~~", "~ ~ ~")
                    .replace("```", "` ` `");
                let safe_name = name.replace(['\n', '\r', '`', '~'], " ");
                let safe_id = id.replace(['\n', '\r', '`', '~'], " ");
                let rendered =
                    format!("\n~~~tool-use\n{safe_name} ({safe_id}): {safe_input}\n~~~\n");
                let _ = tx.send(StreamMsg::Delta(assistant_id, rendered));
            }
            Ok(ChatEvent::Usage { input, output }) => {
                let _ = tx.send(StreamMsg::Usage(assistant_id, input, output));
            }
            Ok(ChatEvent::Done) => {
                let _ = tx.send(StreamMsg::Done(assistant_id));
                return;
            }
            Err(e) => {
                let _ = tx.send(StreamMsg::Error(assistant_id, e.to_string()));
                return;
            }
        }
    }
    // Stream EOF with no terminator — the decoders convert this to an
    // explicit `Err(StreamTruncated)` already, so we should never reach
    // here in production. Belt-and-braces: send Done so the UI clears
    // its `streaming_message_id`.
    let _ = tx.send(StreamMsg::Done(assistant_id));
}

impl eframe::App for GrokApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.tick_perf();
        self.drain_stream();
        self.drain_voice();
        self.drain_pending_search();

        // Pull live audio level for the waveform.
        if let Some(audio) = self.voice_audio.as_ref() {
            let shared = audio.shared();
            self.chat_view.mic_level = shared.level_in.level();
            self.chat_view.tts_speaking = shared.speaking.load(Ordering::Relaxed);
        } else {
            self.chat_view.mic_level *= 0.85;
        }

        let mut want_toggle_voice = false;
        self.handle_shortcuts(&ctx, &mut want_toggle_voice);
        if want_toggle_voice {
            self.toggle_voice();
        }

        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(220.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::RAIL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER)),
            )
            .show_inside(ui, |ui| {
                // Sidebar filter, zero-allocation in the dominant case
                // (no active search). The previous version did
                // `self.chats.clone()` every frame even when no search
                // was active — a full Vec<Chat> + deep-string clone at
                // 60 Hz, growing linearly with chat count.
                //
                // Now: pass the borrowed slice straight through unless
                // a search is active, in which case we build the
                // filtered list ONCE per frame with one shared
                // lowercased needle. The cross-reference set against
                // `search_hits` is a HashMap keyed by chat_id rather
                // than the previous O(N²) "any in from_titles" check.
                let filtered: Vec<Chat>;
                let chats_view: &[Chat] = if self.sidebar.search_text.trim().is_empty() {
                    &self.chats
                } else {
                    let needle = self.sidebar.search_text.to_lowercase();
                    let referenced: std::collections::HashSet<Uuid> =
                        self.search_hits.iter().map(|h| h.chat_id).collect();
                    filtered = self
                        .chats
                        .iter()
                        .filter(|c| {
                            referenced.contains(&c.id) || c.title.to_lowercase().contains(&needle)
                        })
                        .cloned()
                        .collect();
                    &filtered
                };
                let action = crate::ui::sidebar::render(
                    ui,
                    &mut self.sidebar,
                    chats_view,
                    self.active_chat,
                    &mut self.toaster,
                );
                match action {
                    SidebarAction::None => {}
                    SidebarAction::Select(id) => {
                        self.active_chat = Some(id);
                        self.refresh_messages();
                    }
                    SidebarAction::Delete(id) => {
                        // Cancel any in-flight stream targeting the chat
                        // we're about to delete. Without this, the
                        // streaming task's debounced `update_message`
                        // would write the assistant message back to
                        // redb AFTER the chat row + its message range
                        // had been removed — creating an orphan
                        // message that points at a chat_id that no
                        // longer exists. Tantivy would similarly hold
                        // a stale entry until the next reindex.
                        //
                        // The streaming task observes `cancel_flag` at
                        // the top of every loop iteration; setting it
                        // here racing-but-correctly with `delete_chat`
                        // is enough because the next `consume_chat_stream`
                        // poll yields `StreamMsg::Done` (the cancelled
                        // branch) which clears `streaming_message_id`
                        // via `drain_stream`.
                        if self.active_chat == Some(id) && self.streaming_message_id.is_some() {
                            tracing::info!(
                                chat = %id,
                                "cancelling in-flight stream because its chat was deleted"
                            );
                            self.cancel_flag.store(true, Ordering::SeqCst);
                        }
                        if let Err(e) = self.store.delete_chat(id) {
                            self.toaster.error(format!("delete failed: {e}"));
                        } else {
                            self.chats.retain(|c| c.id != id);
                            if self.active_chat == Some(id) {
                                self.active_chat = self.chats.first().map(|c| c.id);
                                self.refresh_messages();
                            }
                        }
                    }
                    SidebarAction::NewChat => self.new_chat(),
                    SidebarAction::Search(q) => {
                        // Don't hit tantivy on every keystroke — debounce.
                        // `drain_pending_search` (called per frame) fires the
                        // actual query once typing settles.
                        if q.trim().is_empty() {
                            // Clear immediately; no point waiting on empty.
                            self.search_hits.clear();
                            self.pending_search = None;
                        } else {
                            self.pending_search = Some((q, Instant::now()));
                        }
                    }
                    SidebarAction::TogglePin(id) => self.toggle_pin(id),
                    SidebarAction::ToggleArchive(id) => self.toggle_archive(id),
                    SidebarAction::Rename(id, title) => self.rename_chat(id, title),
                    SidebarAction::Export(id, format) => self.export_chat(id, format),
                }
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(theme::SURFACE))
            .show_inside(ui, |ui| {
                let chat = self
                    .active_chat
                    .and_then(|id| self.chats.iter().find(|c| c.id == id))
                    .cloned();
                let action = crate::ui::chat_view::render(
                    ui,
                    &mut self.chat_view,
                    chat.as_ref(),
                    &self.messages,
                );
                if let Some(text) = action.send {
                    self.send_user_message(text);
                }
                if action.stop {
                    self.cancel_stream();
                }
                if let Some(text) = action.copy {
                    ctx.copy_text(text);
                    self.toaster.info("copied");
                }
                if let Some(target) = action.regenerate {
                    self.regenerate(target);
                }
                if action.toggle_voice {
                    self.toggle_voice();
                }
                if action.toggle_tts {
                    let new_state = !self.chat_view.tts_enabled;
                    self.chat_view.tts_enabled = new_state;
                    let updated = self.settings.update(|s| s.tts_enabled = new_state);
                    if let Err(e) = self.store.save_settings(&updated) {
                        self.toaster.error(format!("settings: {e}"));
                    }
                }
            });

        // Settings window
        let mut current = (*self.settings.get()).clone();
        let action = crate::ui::settings_view::render(
            &ctx,
            &mut self.settings_view,
            &mut current,
            &self.stats,
            &mut self.toaster,
        );
        if action.save_settings {
            theme::apply(&ctx, current.theme, current.font_size);
            self.chat_view.tts_enabled = current.tts_enabled;
            self.settings.set(current.clone());
            if let Err(e) = self.store.save_settings(&current) {
                self.toaster.error(format!("settings save: {e}"));
            }
        }
        if action.save_api_key {
            let provider = self.settings.get().default_provider;
            if let Err(e) =
                secrets::set_api_key(provider.id(), self.settings_view.api_key_buffer.trim())
            {
                self.toaster.error(format!("keyring: {e}"));
            } else {
                self.toaster.info("api key saved");
            }
        }
        if action.clear_api_key {
            let provider = self.settings.get().default_provider;
            if let Err(e) = secrets::delete_api_key(provider.id()) {
                self.toaster.error(format!("keyring: {e}"));
            } else {
                self.toaster.info("api key cleared");
            }
        }
        if action.rebuild_index {
            match self.store.rebuild_index() {
                Ok(n) => self
                    .toaster
                    .info(format!("rebuilt search index — {n} messages")),
                Err(e) => self.toaster.error(format!("index: {e}")),
            }
        }
        // Scrub the API-key buffer when the dialog closes OR when it
        // was saved. Best-effort — see `SettingsState::clear_securely`
        // for the threat-model caveats. The buffer typically holds
        // ~50-100 bytes of key, easy to zeroize.
        if action.close || action.save_api_key || action.clear_api_key {
            self.settings_view.clear_securely();
        }

        // Command palette overlay (Cmd/Ctrl+K). Renders on top of everything.
        let palette_action =
            crate::ui::palette::render(&ctx, &mut self.palette, &self.chats, self.active_chat);
        self.handle_palette_action(&ctx, palette_action);

        self.toaster.render(&ctx);

        if self.streaming_message_id.is_some() || self.chat_view.voice_active {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::error::ApiError;
    use crate::services::providers::ChatEvent;
    use futures_util::stream;
    use tokio::sync::mpsc;

    fn fake_stream(
        events: Vec<Result<ChatEvent, ApiError>>,
    ) -> crate::services::providers::EventStream {
        Box::pin(stream::iter(events))
    }

    /// Cancel-flag set BEFORE the loop starts → first poll honours it
    /// and exits without forwarding any deltas.
    #[tokio::test(flavor = "current_thread")]
    async fn consume_chat_stream_honours_cancel_set_before_loop() {
        let cancel = Arc::new(AtomicBool::new(true)); // pre-set
        let (tx, mut rx) = mpsc::unbounded_channel();
        let assistant = Uuid::new_v4();
        let s = fake_stream(vec![
            Ok(ChatEvent::Delta("a".into())),
            Ok(ChatEvent::Delta("b".into())),
            Ok(ChatEvent::Done),
        ]);
        consume_chat_stream(s, cancel, tx, assistant).await;
        // Exactly one terminal Done with the right id; no deltas leaked.
        let mut deltas = 0;
        let mut dones = 0;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                StreamMsg::Delta(id, _) => {
                    assert_eq!(id, assistant);
                    deltas += 1;
                }
                StreamMsg::Done(id) => {
                    assert_eq!(id, assistant);
                    dones += 1;
                }
                other => panic!("unexpected msg: {other:?}"),
            }
        }
        assert_eq!(deltas, 0);
        assert_eq!(dones, 1);
    }

    /// Cancel flipped MID-stream is honoured at the very next loop iter.
    #[tokio::test(flavor = "current_thread")]
    async fn consume_chat_stream_honours_cancel_mid_stream() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let assistant = Uuid::new_v4();

        // Construct a stream whose second item flips the cancel flag
        // BEFORE yielding (simulating an external cancel arriving
        // between yields).
        let cancel_inner = cancel.clone();
        let s = stream::unfold(0u32, move |i| {
            let cancel_inner = cancel_inner.clone();
            async move {
                match i {
                    0 => Some((Ok(ChatEvent::Delta("good".into())), 1)),
                    1 => {
                        cancel_inner.store(true, Ordering::SeqCst);
                        // Yield another delta — but the cancel check
                        // at the TOP of the next iteration must drop it.
                        Some((Ok(ChatEvent::Delta("poison".into())), 2))
                    }
                    _ => None,
                }
            }
        });
        consume_chat_stream(Box::pin(s), cancel, tx, assistant).await;
        let mut deltas: Vec<String> = Vec::new();
        let mut dones = 0;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                StreamMsg::Delta(_, body) => deltas.push(body),
                StreamMsg::Done(_) => dones += 1,
                other => panic!("unexpected msg: {other:?}"),
            }
        }
        // We see at most "good"; "poison" might or might not be visible
        // depending on iteration order, but at least the cancel must
        // produce a terminal Done.
        assert!(!deltas.contains(&"poison".to_owned()), "deltas={deltas:?}");
        assert_eq!(dones, 1);
    }

    /// `Err` from the stream surfaces as exactly one `StreamMsg::Error`
    /// with the carrier id; no extra Done after.
    #[tokio::test(flavor = "current_thread")]
    async fn consume_chat_stream_surfaces_provider_error() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let assistant = Uuid::new_v4();
        let s = fake_stream(vec![
            Ok(ChatEvent::Delta("partial".into())),
            Err(ApiError::ProviderStream {
                provider: "test",
                message: "boom".into(),
                request_id: String::new(),
            }),
            // Trailing events MUST be ignored: the function returns on
            // the first Err.
            Ok(ChatEvent::Delta("ignored".into())),
        ]);
        consume_chat_stream(s, cancel, tx, assistant).await;
        let mut got_error = false;
        let mut got_done = false;
        let mut deltas: Vec<String> = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                StreamMsg::Delta(_, body) => deltas.push(body),
                StreamMsg::Error(_, _) => got_error = true,
                StreamMsg::Done(_) => got_done = true,
                StreamMsg::Usage(_, _, _) => {}
            }
        }
        assert_eq!(deltas, vec!["partial"]);
        assert!(got_error, "Error not surfaced");
        assert!(
            !got_done,
            "Done emitted after Error — would overwrite UI error state"
        );
    }

    /// `Usage` is forwarded; `Done` follows; no extra messages after.
    #[tokio::test(flavor = "current_thread")]
    async fn consume_chat_stream_forwards_usage_then_done() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let assistant = Uuid::new_v4();
        let s = fake_stream(vec![
            Ok(ChatEvent::Delta("hi".into())),
            Ok(ChatEvent::Usage {
                input: 3,
                output: 1,
            }),
            Ok(ChatEvent::Done),
        ]);
        consume_chat_stream(s, cancel, tx, assistant).await;
        let mut seq: Vec<&'static str> = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            seq.push(match msg {
                StreamMsg::Delta(_, _) => "delta",
                StreamMsg::Usage(_, _, _) => "usage",
                StreamMsg::Done(_) => "done",
                StreamMsg::Error(_, _) => "error",
            });
        }
        assert_eq!(seq, vec!["delta", "usage", "done"]);
    }

    /// Stream that ends without a terminator still produces ONE Done.
    /// (Production decoders convert this to `StreamTruncated`; this is
    /// the belt-and-braces path.)
    #[tokio::test(flavor = "current_thread")]
    async fn consume_chat_stream_synthesises_done_on_silent_eof() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let assistant = Uuid::new_v4();
        let s = fake_stream(vec![Ok(ChatEvent::Delta("only".into()))]);
        consume_chat_stream(s, cancel, tx, assistant).await;
        let mut dones = 0;
        while let Ok(msg) = rx.try_recv() {
            if matches!(msg, StreamMsg::Done(_)) {
                dones += 1;
            }
        }
        assert_eq!(dones, 1);
    }

    /// Adversarial: a model returning a tool_use with attacker-controlled
    /// fences in the input JSON, plus newline/backtick/tilde in the id
    /// and name fields, must NOT break out of the rendered markdown
    /// fence. The sanitiser in the ToolUse arm escapes ``` and ~~~ and
    /// scrubs control chars from id/name.
    #[tokio::test(flavor = "current_thread")]
    async fn consume_chat_stream_sanitises_hostile_tool_use_fence_injection() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let assistant = Uuid::new_v4();
        // id with newline, name with backtick + tilde, input with both
        // fence styles inside a JSON string.
        let s = fake_stream(vec![
            Ok(ChatEvent::ToolUse {
                id: "toolu_x\n!!! injected".into(),
                name: "ev`il~~~name".into(),
                input: serde_json::json!({
                    "cmd": "ls; ```bash\nrm -rf /\n``` and ~~~ blah ~~~"
                }),
            }),
            Ok(ChatEvent::Done),
        ]);
        consume_chat_stream(s, cancel, tx, assistant).await;
        let mut rendered = String::new();
        while let Ok(msg) = rx.try_recv() {
            if let StreamMsg::Delta(_, d) = msg {
                rendered.push_str(&d);
            }
        }
        // Fence integrity: the tilde-fence we emit must be at the start
        // and end ONLY. Counting raw "```" and "~~~" in the output:
        let raw_backticks = rendered.matches("```").count();
        let raw_tildes_fence = rendered.matches("~~~").count();
        // We open with `~~~tool-use` and close with `~~~`. The model's
        // backticks and tildes are escaped to `` ` ` ` `` and `~ ~ ~`.
        // Net: 0 `` ``` `` in output, exactly 2 `~~~` (open + close).
        assert_eq!(
            raw_backticks, 0,
            "model backticks leaked into output — fence can be broken: {rendered}"
        );
        assert_eq!(
            raw_tildes_fence, 2,
            "expected exactly 2 ~~~ (open+close), got {raw_tildes_fence}: {rendered}"
        );
        // Newline injection in id is scrubbed.
        assert!(
            !rendered.contains("toolu_x\n!!!"),
            "newline in id leaked through: {rendered}"
        );
        // The injection payload should still be visible in some form
        // (we're sanitising, not censoring) but unable to break out.
        assert!(rendered.contains("injected") || rendered.contains("inject"));
    }
}
