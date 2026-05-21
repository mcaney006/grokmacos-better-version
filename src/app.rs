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
use crate::services::anthropic::AnthropicClient;
use crate::services::audio::VoiceAudio;
use crate::services::chat::XaiClient;
use crate::services::embeddings::Retriever;
use crate::services::local::LocalClient;
use crate::services::openai::OpenAiClient;
use crate::services::providers::{ChatEvent, ChatProvider, ChatRequest};
use crate::services::voice::{VoiceEvent, VoiceSession};
use crate::storage::Store;
use crate::theme;
use crate::ui::chat_view::ChatViewState;
use crate::ui::settings_view::SettingsState;
use crate::ui::sidebar::{SidebarAction, SidebarState};
use crate::ui::toast::Toaster;
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
        self.messages.push(user_msg.clone());

        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            if c.title == "New Chat" {
                c.title = Chat::derive_title(&body);
            }
            c.updated_at = chrono::Utc::now();
            let _ = self.store.upsert_chat(c);
        }

        let s = self.settings.get();
        let provider = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .map(|c| c.provider.clone())
            .unwrap_or_else(|| "xai".to_string());
        let model = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .map(|c| c.model.clone())
            .unwrap_or_else(|| provider_model(&s, s.default_provider).to_string());

        let messages: Vec<WireMessage> = self.messages.iter().map(WireMessage::from).collect();
        // RAG augmentation was previously a synchronous call here on the UI
        // thread. With `--features rag` that's hundreds of milliseconds of
        // ONNX inference; eframe stalls for the duration. Now we capture
        // the inputs and run augmentation inside the spawned tokio task
        // (via spawn_blocking, since fastembed isn't async-aware).
        let rag_enabled = s.rag_enabled;
        let rag_top_k = s.rag_top_k.max(1) as usize;
        let rag_query = body.clone();
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

            let result = run_completion(
                provider_id,
                api_key,
                request,
                cancel,
                tx.clone(),
                assistant_id,
            )
            .await;
            if let Err(e) = result {
                let _ = tx.send(StreamMsg::Error(assistant_id, e.to_string()));
            }
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
        let mut buffered: Vec<StreamMsg> = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            buffered.push(msg);
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
                        let elapsed_ms = start.elapsed().as_millis().min(u32::MAX as u128) as u32;
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
                            m.content
                                .push_str(&format!("\n\n⚠ stream ended early: {err}"));
                        }
                        let _ = self.store.update_message(m);
                    }
                    self.toaster.error(err);
                    dirty_id = None;
                    should_clear = true;
                }
            }
        }
        // Debounced persistence of streamed deltas.
        if let Some(id) = dirty_id {
            let now = Instant::now();
            let should_flush = self
                .last_stream_persist
                .map(|t| now.duration_since(t) >= STREAM_PERSIST_DEBOUNCE)
                .unwrap_or(true);
            if should_flush {
                if let Some(m) = self.messages.iter().find(|m| m.id == id) {
                    let _ = self.store.update_message(m);
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
                VoiceEvent::Error(e) => self.toaster.error(format!("voice: {e}")),
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
            self.chat_view.voice_active = false;
            self.voice_audio = None;
            return;
        }
        // If a previous toggle is mid-connect, cancel it by dropping the
        // receiver — the spawned task's send-half will fail silently when
        // it eventually tries to deliver.
        if self.voice_open_rx.is_some() {
            self.voice_open_rx = None;
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
        handle.spawn(async move {
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
                self.toaster.info(format!("provider: {}", p.label()));
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
            Ok(()) => self.toaster.info(format!("exported to {}", dest.display())),
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
    for h in hits {
        context.push_str(&format!("- {}\n", h.snippet.replace('\n', " ")));
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

fn load_existing_key(provider: Provider) -> String {
    match secrets::get_api_key(provider.id()) {
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
    let client: Box<dyn ChatProvider + Send + Sync> = match provider {
        Provider::Xai => Box::new(XaiClient::new(api_key)),
        Provider::OpenAi => Box::new(OpenAiClient::new(api_key)),
        Provider::Anthropic => Box::new(AnthropicClient::new(api_key)),
        Provider::Local => Box::new(LocalClient::new(api_key)),
    };
    let mut stream = client.stream(request).await?;
    use futures_util::StreamExt;
    while let Some(item) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            let _ = tx.send(StreamMsg::Done(assistant_id));
            return Ok(());
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
                tracing::info!(%id, %name, ?input, "anthropic tool_use");
                let rendered = format!("\n```tool-use\n{name} ({id}): {input}\n```\n");
                let _ = tx.send(StreamMsg::Delta(assistant_id, rendered));
            }
            Ok(ChatEvent::Usage { input, output }) => {
                let _ = tx.send(StreamMsg::Usage(assistant_id, input, output));
            }
            Ok(ChatEvent::Done) => {
                let _ = tx.send(StreamMsg::Done(assistant_id));
                return Ok(());
            }
            Err(e) => {
                let _ = tx.send(StreamMsg::Error(assistant_id, e.to_string()));
                return Ok(());
            }
        }
    }
    let _ = tx.send(StreamMsg::Done(assistant_id));
    Ok(())
}

impl eframe::App for GrokApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.tick_perf();
        self.drain_stream();
        self.drain_voice();

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
                let chats: Vec<Chat> = if self.sidebar.search_text.trim().is_empty() {
                    self.chats.clone()
                } else {
                    let needle = self.sidebar.search_text.to_lowercase();
                    let mut from_titles: Vec<Chat> = self
                        .chats
                        .iter()
                        .filter(|c| c.title.to_lowercase().contains(&needle))
                        .cloned()
                        .collect();
                    let referenced: std::collections::HashSet<Uuid> =
                        self.search_hits.iter().map(|h| h.chat_id).collect();
                    for c in &self.chats {
                        if referenced.contains(&c.id) && !from_titles.iter().any(|t| t.id == c.id) {
                            from_titles.push(c.clone());
                        }
                    }
                    from_titles
                };
                let action = crate::ui::sidebar::render(
                    ui,
                    &mut self.sidebar,
                    &chats,
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
                        self.search_hits = self.store.search(&q, 40).unwrap_or_default();
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
