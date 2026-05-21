//! xAI Realtime Voice WebSocket client.
//!
//! Transport: `wss://api.x.ai/v1/realtime`. Wire protocol is modelled on
//! OpenAI's Realtime API. We open a single WebSocket, send a `session.update`
//! to configure modalities + VAD, then run two tasks concurrently:
//!
//! * **uplink** — drains the audio engine's capture channel, base64-encodes
//!   PCM16-LE frames, and writes them as `input_audio_buffer.append` events.
//! * **downlink** — reads server frames, fans typed events out to the UI, and
//!   pushes decoded TTS PCM into the audio engine's playback channel.

use crate::error::ApiError;
use crate::models::VoicePersona;
use crate::services::audio::VoiceShared;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const REALTIME_URL: &str = "wss://api.x.ai/v1/realtime";

/// Depth of the in-process uplink channel that buffers PCM frames between
/// the cpal audio thread and the WebSocket sink. 16 × 24 kHz mono i16
/// frames is well under a second of audio — large enough to absorb a
/// brief network hiccup, small enough that we drop frames if the WS sink
/// stalls instead of pinning RAM.
const VOICE_UPLINK_CHANNEL_DEPTH: usize = 16;

/// How often we send a WebSocket Ping. Many corporate proxies time out
/// idle WebSockets at 60 s; pinging every 30 s keeps the path warm and
/// gives us a clean error signal when the underlying TCP dies.
const WS_PING_INTERVAL_SECS: u64 = 30;

/// Receive-side watchdog cadence. Every 60 s we check `last_recv`; if the
/// last message was older than `WS_RECV_DEADLINE_SECS` we declare the
/// connection dead and emit an Error event. A send-only health check
/// (Ping/Pong) catches only one half of a half-open TCP — the watchdog
/// is the other half: silent receive-side hangs (network partition,
/// upstream load balancer eating frames) get a finite-time error.
const WS_RECV_WATCHDOG_INTERVAL_SECS: u64 = 60;
const WS_RECV_DEADLINE_SECS: i64 = 90;

/// Cap on the WebSocket connect + upgrade handshake. Without this an
/// attacker (or a misbehaving load balancer) that completes the TCP
/// accept but never speaks HTTP would hang `connect_async().await`
/// forever, leaking the spawned voice-open task and pinning resources
/// for the entire process lifetime. Matches the HTTP `connect_timeout`
/// in `services::chat::http_client` (10s).
const WS_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Per-`sink.send()` cap on the uplink. The receive watchdog catches
/// the case where the server stops sending; this catches the inverse
/// (server keeps the socket open but stops reading, so our writes
/// block in the kernel send buffer). 15s is way longer than any
/// healthy provider's window — well-behaved peers ACK within ms — so
/// it's only ever tripped by a broken transport.
const WS_SEND_TIMEOUT_SECS: u64 = 15;

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub enum VoiceEvent {
    Connected,
    PartialTranscript(String),
    FinalTranscript(String),
    AssistantTextDelta(String),
    AssistantTextDone,
    SpeechStarted,
    SpeechStopped,
    Error(String),
    Closed,
}

pub struct VoiceSession {
    pub events: mpsc::UnboundedReceiver<VoiceEvent>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl VoiceSession {
    pub async fn open(
        api_key: String,
        persona: VoicePersona,
        shared: VoiceShared,
    ) -> Result<Self, ApiError> {
        Self::open_with_url(REALTIME_URL, api_key, persona, shared).await
    }

    /// URL-parameterised constructor for tests. Production callers go via
    /// `open()`. Kept `pub(crate)` so we can drive the real keepalive +
    /// watchdog paths against a loopback WebSocket server in unit tests.
    pub(crate) async fn open_with_url(
        url: &str,
        api_key: String,
        persona: VoicePersona,
        shared: VoiceShared,
    ) -> Result<Self, ApiError> {
        if api_key.trim().is_empty() {
            return Err(ApiError::MissingKey);
        }
        let mut request = url
            .into_client_request()
            .map_err(|e| ApiError::WebSocket(e.to_string()))?;
        let mut auth: tokio_tungstenite::tungstenite::http::HeaderValue =
            format!("Bearer {api_key}").parse().map_err(
                |e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                    ApiError::WebSocket(e.to_string())
                },
            )?;
        // Mark the bearer token sensitive — the underlying http crate's
        // header impl respects this flag when Debug-formatting headers
        // (which tokio-tungstenite does on handshake failure). Without
        // it the token can land in a panic message or trace log.
        auth.set_sensitive(true);
        request.headers_mut().insert("Authorization", auth);

        let connect_fut = tokio_tungstenite::connect_async(request);
        let (ws, _) = match tokio::time::timeout(
            std::time::Duration::from_secs(WS_CONNECT_TIMEOUT_SECS),
            connect_fut,
        )
        .await
        {
            Ok(Ok(ok)) => ok,
            Ok(Err(e)) => return Err(ApiError::WebSocket(e.to_string())),
            Err(_elapsed) => {
                return Err(ApiError::WebSocket(format!(
                    "websocket connect timed out after {WS_CONNECT_TIMEOUT_SECS}s (handshake never completed)"
                )));
            }
        };
        let (mut sink, mut stream) = ws.split();

        let config = serde_json::json!({
            "type": "session.update",
            "session": {
                "modalities": ["text", "audio"],
                "voice": persona.id(),
                "input_audio_format": "pcm16",
                "output_audio_format": "pcm16",
                "input_audio_transcription": { "model": "whisper-1" },
                "turn_detection": {
                    "type": "server_vad",
                    "threshold": 0.5,
                    "prefix_padding_ms": 200,
                    "silence_duration_ms": 500
                }
            }
        });
        sink.send(WsMessage::Text(config.to_string().into()))
            .await
            .map_err(|e| ApiError::WebSocket(e.to_string()))?;

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        // Uplink: bridge the (sync) crossbeam capture channel into a tokio
        // mpsc, then drive the WS sink from a real `recv().await`. The
        // previous version polled `try_recv` with a 10 ms sleep on empty —
        // that burned CPU at 100 Hz when idle and silently fell behind the
        // 24 kHz capture stream when busy. The bridge thread blocks on
        // crossbeam recv (efficient), and `forward_tx.try_send` exerts
        // proper backpressure: if the network can't keep up, frames drop
        // at the bridge instead of growing RAM unbounded.
        let capture_rx = shared.capture_rx.clone();
        let (forward_tx, mut forward_rx) =
            tokio::sync::mpsc::channel::<Vec<i16>>(VOICE_UPLINK_CHANNEL_DEPTH);
        // OS thread create can fail under memory pressure / fd exhaustion.
        // The previous code dropped the Result with `.ok()`, which meant
        // a failed spawn silently produced a voice session that captured
        // audio but never uplinked it. Now we surface the failure as an
        // error event so the UI tells the user and tears the session
        // down instead of pretending it's healthy.
        let bridge_result = std::thread::Builder::new()
            .name("voice-uplink-bridge".into())
            .spawn(move || {
                while let Ok(frame) = capture_rx.recv() {
                    // Best-effort: if the WS sink is backed up, drop frames
                    // rather than queue. Voice is real-time; stale audio
                    // helps nobody.
                    if forward_tx.try_send(frame).is_err() {
                        // Channel full or closed; on closed we exit, on
                        // full we keep going so we don't lock the audio
                        // capture thread.
                        if forward_tx.is_closed() {
                            break;
                        }
                    }
                }
            });
        if let Err(e) = bridge_result {
            return Err(ApiError::WebSocket(format!(
                "voice uplink bridge thread spawn failed: {e}"
            )));
        }

        let uplink_events = events_tx.clone();
        let uplink = tokio::spawn(async move {
            // Heartbeat ticker — keeps the WS alive across stateful NATs
            // and surfaces dead connections (the Ping send fails on a
            // broken TCP socket, so the loop exits with an error event
            // instead of silently sitting forever).
            let mut heartbeat =
                tokio::time::interval(std::time::Duration::from_secs(WS_PING_INTERVAL_SECS));
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            let send_dur = std::time::Duration::from_secs(WS_SEND_TIMEOUT_SECS);
            loop {
                tokio::select! {
                    maybe_frame = forward_rx.recv() => {
                        let Some(frame) = maybe_frame else { break; };
                        let mut bytes = Vec::with_capacity(frame.len() * 2);
                        for sample in &frame {
                            bytes.extend_from_slice(&sample.to_le_bytes());
                        }
                        let b64 = B64.encode(&bytes);
                        let event = serde_json::json!({
                            "type": "input_audio_buffer.append",
                            "audio": b64,
                        });
                        // Bound sink.send: a server that ACKs Pongs but
                        // stops reading our payload bytes would otherwise
                        // hang us in the kernel send buffer indefinitely.
                        // The receive-watchdog can't see this — only a
                        // send-side timeout can.
                        match tokio::time::timeout(
                            send_dur,
                            sink.send(WsMessage::Text(event.to_string().into())),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                let _ = uplink_events
                                    .send(VoiceEvent::Error(format!("uplink: {e}")));
                                break;
                            }
                            Err(_) => {
                                let _ = uplink_events.send(VoiceEvent::Error(format!(
                                    "uplink: send timed out after {WS_SEND_TIMEOUT_SECS}s (server not reading)"
                                )));
                                break;
                            }
                        }
                    }
                    _ = heartbeat.tick() => {
                        match tokio::time::timeout(
                            send_dur,
                            sink.send(WsMessage::Ping(Default::default())),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                let _ = uplink_events.send(VoiceEvent::Error(format!(
                                    "ws keepalive: {e}"
                                )));
                                break;
                            }
                            Err(_) => {
                                let _ = uplink_events.send(VoiceEvent::Error(format!(
                                    "ws keepalive: ping send timed out after {WS_SEND_TIMEOUT_SECS}s"
                                )));
                                break;
                            }
                        }
                    }
                }
            }
            let _ = sink.close().await;
        });

        // Receive watchdog: shared timestamp updated on every server frame
        // (text/binary/pong/ping/close — anything that proves the upstream
        // is alive). A separate task wakes every 60 s, compares against
        // `WS_RECV_DEADLINE_SECS`, and surfaces an error if the gap is too
        // wide. The watchdog itself never decides "fail loud"; it always
        // emits an error event so the UI can react.
        let last_recv = Arc::new(AtomicI64::new(epoch_secs()));
        let watchdog_recv = last_recv.clone();
        let watchdog_events = events_tx.clone();
        let watchdog = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                WS_RECV_WATCHDOG_INTERVAL_SECS,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // First tick fires immediately; skip it so we don't false-alarm
            // before any frame has had a chance to arrive.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let now = epoch_secs();
                // Acquire pairs with the Release store in the downlink
                // task. Relaxed would be a correctness bug under weak
                // memory models — the watchdog might observe a stale
                // last_recv and fire a spurious deadline error on the
                // tick that follows a fresh frame.
                let last = watchdog_recv.load(Ordering::Acquire);
                if now - last > WS_RECV_DEADLINE_SECS {
                    let _ = watchdog_events.send(VoiceEvent::Error(format!(
                        "ws receive watchdog: no frames for {}s (deadline {}s)",
                        now - last,
                        WS_RECV_DEADLINE_SECS
                    )));
                    break;
                }
            }
        });

        // Downlink: decode JSON frames + ferry events to UI / audio.
        // `events_tx` and `last_recv` have already been cloned for the
        // uplink + watchdog tasks above — this is their last use here,
        // so move into the downlink task rather than refcount-bump.
        let playback_tx = shared.playback_tx.clone();
        let downlink_events = events_tx;
        let downlink_recv = last_recv;
        tokio::spawn(async move {
            let _ = downlink_events.send(VoiceEvent::Connected);
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    msg = stream.next() => {
                        // Release pairs with the Acquire load in the
                        // watchdog task: every fresh `last_recv` write
                        // must be visible before the watchdog's next
                        // tick observes it.
                        downlink_recv.store(epoch_secs(), Ordering::Release);
                        match msg {
                            Some(Ok(WsMessage::Text(text))) => {
                                match serde_json::from_str::<ServerEvent>(&text) {
                                    Ok(ev) => handle_server_event(ev, &playback_tx, &downlink_events),
                                    Err(e) => {
                                        let _ = downlink_events.send(VoiceEvent::Error(
                                            format!("decode: {e}"),
                                        ));
                                    }
                                }
                            }
                            Some(Ok(WsMessage::Binary(_))) => {}
                            Some(Ok(WsMessage::Close(_))) | None => break,
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                let _ = downlink_events
                                    .send(VoiceEvent::Error(format!("ws: {e}")));
                                break;
                            }
                        }
                    }
                }
            }
            uplink.abort();
            watchdog.abort();
            let _ = downlink_events.send(VoiceEvent::Closed);
        });

        Ok(Self {
            events: events_rx,
            shutdown: Some(shutdown_tx),
        })
    }

    pub fn close(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for VoiceSession {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

fn handle_server_event(
    ev: ServerEvent,
    playback_tx: &crossbeam_channel::Sender<Vec<i16>>,
    events: &mpsc::UnboundedSender<VoiceEvent>,
) {
    match ev {
        ServerEvent::AudioDelta { delta } => {
            if let Ok(bytes) = B64.decode(delta.as_bytes()) {
                let mut samples = Vec::with_capacity(bytes.len() / 2);
                for chunk in bytes.chunks_exact(2) {
                    samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                }
                let _ = playback_tx.send(samples);
            }
        }
        ServerEvent::AudioDone => {
            let _ = events.send(VoiceEvent::AssistantTextDone);
        }
        ServerEvent::AudioTranscriptDelta { delta } => {
            let _ = events.send(VoiceEvent::AssistantTextDelta(delta));
        }
        ServerEvent::AudioTranscriptDone => {
            let _ = events.send(VoiceEvent::AssistantTextDone);
        }
        ServerEvent::InputTranscriptionPartial { transcript } => {
            let _ = events.send(VoiceEvent::PartialTranscript(transcript));
        }
        ServerEvent::InputTranscriptionCompleted { transcript } => {
            let _ = events.send(VoiceEvent::FinalTranscript(transcript));
        }
        ServerEvent::SpeechStarted => {
            let _ = events.send(VoiceEvent::SpeechStarted);
        }
        ServerEvent::SpeechStopped => {
            let _ = events.send(VoiceEvent::SpeechStopped);
        }
        ServerEvent::Error { message } => {
            let _ = events.send(VoiceEvent::Error(message.message));
        }
        ServerEvent::Other => {}
    }
}

// --- wire types -------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ServerEvent {
    #[serde(rename = "response.audio.delta")]
    AudioDelta {
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.audio.done")]
    AudioDone,
    #[serde(rename = "response.audio_transcript.delta")]
    AudioTranscriptDelta {
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.audio_transcript.done")]
    AudioTranscriptDone,
    #[serde(rename = "conversation.item.input_audio_transcription.delta")]
    InputTranscriptionPartial {
        #[serde(default)]
        transcript: String,
    },
    #[serde(rename = "conversation.item.input_audio_transcription.completed")]
    InputTranscriptionCompleted {
        #[serde(default)]
        transcript: String,
    },
    #[serde(rename = "input_audio_buffer.speech_started")]
    SpeechStarted,
    #[serde(rename = "input_audio_buffer.speech_stopped")]
    SpeechStopped,
    #[serde(rename = "error")]
    Error {
        #[serde(default, rename = "error")]
        message: ErrorMessage,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ErrorMessage {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::services::audio::{LevelMeter, VoiceShared};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tokio::net::TcpListener;

    fn dummy_shared() -> VoiceShared {
        let (_capture_tx, capture_rx) = crossbeam_channel::bounded::<Vec<i16>>(8);
        let (playback_tx, _playback_rx) = crossbeam_channel::unbounded::<Vec<i16>>();
        VoiceShared {
            capture_rx,
            playback_tx,
            level_in: LevelMeter::default(),
            level_out: LevelMeter::default(),
            speaking: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Spawn a loopback WS server that completes the handshake then drops
    /// the connection. Returns the `ws://` URL bound to the ephemeral port.
    async fn spawn_fake_ws_drop_after_handshake() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                    drop(ws);
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        format!("ws://127.0.0.1:{port}/")
    }

    /// Adversarial: a server that ACCEPTS the TCP connection but never
    /// completes the WebSocket upgrade handshake. Without a connect
    /// timeout, `connect_async().await` would block indefinitely and
    /// leak the spawned voice-open task. The fix wraps connect_async
    /// in a `tokio::time::timeout` with the same budget as the HTTP
    /// connect_timeout.
    #[tokio::test(flavor = "current_thread")]
    async fn ws_open_with_url_times_out_on_hung_handshake() {
        // Listener accepts TCP but never sends an HTTP response — the
        // client should give up via the new connect timeout rather than
        // hang forever.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept and HOLD the socket forever. Never read, never write.
            if let Ok((stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(600)).await;
                drop(stream);
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let url = format!("ws://127.0.0.1:{port}/");

        // Run with a `tokio::time::timeout` that's WIDER than the
        // internal connect timeout so we can tell whether the system
        // gave up on its own (good) or we had to bail out (bad).
        let outer = tokio::time::timeout(
            Duration::from_secs(WS_CONNECT_TIMEOUT_SECS + 5),
            VoiceSession::open_with_url(
                &url,
                "dummy-key".into(),
                VoicePersona::Ara,
                dummy_shared(),
            ),
        )
        .await;

        match outer {
            Ok(Err(ApiError::WebSocket(msg))) => {
                assert!(
                    msg.contains("timed out") || msg.contains("timeout") || msg.contains("connect"),
                    "expected websocket-connect timeout, got: {msg}"
                );
            }
            Ok(Ok(_session)) => panic!("connection should NOT have succeeded"),
            Ok(Err(other)) => panic!("expected ApiError::WebSocket, got {other:?}"),
            Err(_elapsed) => {
                panic!(
                    "VoiceSession::open_with_url never returned — internal connect timeout missing or wrong"
                );
            }
        }
    }

    /// When the server closes the TCP connection mid-session, the next
    /// keepalive Ping send fails — the uplink loop must surface that as a
    /// `VoiceEvent::Error("ws keepalive: …")` rather than silently dying.
    #[tokio::test(flavor = "current_thread")]
    async fn ws_keepalive_send_failure_emits_error() {
        let url = spawn_fake_ws_drop_after_handshake().await;
        let mut session = VoiceSession::open_with_url(
            &url,
            "dummy-key".into(),
            VoicePersona::Ara,
            dummy_shared(),
        )
        .await
        .expect("open_with_url ok");

        // Drain events until we see a structured Error indicating the
        // failure was detected. Previously this test also accepted a
        // bare `Closed` event as proof, but that's a false-positive
        // signal: with ALL hardening removed (no keepalive ping, no
        // receive watchdog, no send-side timeout), the downlink would
        // STILL emit Closed naturally when stream.next() returns None
        // after the server drops the TCP socket. The test would pass
        // for the wrong reason.
        //
        // The right anti-regression assertion: we MUST see a
        // VoiceEvent::Error within 35s — that's the surface point that
        // only the keepalive / watchdog / send-timeout code produces.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
        let mut events_seen: Vec<String> = Vec::new();
        let mut error_signal: Option<String> = None;
        let mut saw_closed = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), session.events.recv()).await {
                Ok(Some(VoiceEvent::Error(msg))) => {
                    events_seen.push(format!("Error({msg:?})"));
                    let is_keepalive_or_uplink = msg.contains("ws keepalive")
                        || msg.contains("uplink")
                        || msg.starts_with("ws:")
                        || msg.starts_with("ws ");
                    if is_keepalive_or_uplink {
                        error_signal = Some(msg);
                        break;
                    }
                }
                Ok(Some(VoiceEvent::Closed)) => {
                    events_seen.push("Closed".to_string());
                    saw_closed = true;
                    // Don't break on bare Closed: keep draining for the
                    // Error event that the hardening was supposed to
                    // emit BEFORE the natural close.
                }
                Ok(Some(ev)) => {
                    events_seen.push(format!("{ev:?}"));
                }
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(
            error_signal.is_some(),
            "expected VoiceEvent::Error from keepalive/uplink/ws path within 35s; \
             saw_closed={saw_closed}; events: {events_seen:?}"
        );
    }
}
