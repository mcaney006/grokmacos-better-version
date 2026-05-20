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
        if api_key.trim().is_empty() {
            return Err(ApiError::MissingKey);
        }
        let mut request = REALTIME_URL
            .into_client_request()
            .map_err(|e| ApiError::WebSocket(e.to_string()))?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {api_key}").parse().map_err(
                |e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                    ApiError::WebSocket(e.to_string())
                },
            )?,
        );

        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ApiError::WebSocket(e.to_string()))?;
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
        std::thread::Builder::new()
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
            })
            .ok();

        let uplink_events = events_tx.clone();
        let uplink = tokio::spawn(async move {
            // Heartbeat ticker — keeps the WS alive across stateful NATs
            // and surfaces dead connections (the Ping send fails on a
            // broken TCP socket, so the loop exits with an error event
            // instead of silently sitting forever).
            let mut heartbeat =
                tokio::time::interval(std::time::Duration::from_secs(WS_PING_INTERVAL_SECS));
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                        if let Err(e) = sink.send(WsMessage::Text(event.to_string().into())).await {
                            let _ = uplink_events.send(VoiceEvent::Error(format!("uplink: {e}")));
                            break;
                        }
                    }
                    _ = heartbeat.tick() => {
                        if let Err(e) = sink.send(WsMessage::Ping(Default::default())).await {
                            let _ = uplink_events.send(VoiceEvent::Error(
                                format!("ws keepalive: {e}")
                            ));
                            break;
                        }
                    }
                }
            }
            let _ = sink.close().await;
        });

        // Downlink: decode JSON frames + ferry events to UI / audio.
        let playback_tx = shared.playback_tx.clone();
        let downlink_events = events_tx.clone();
        tokio::spawn(async move {
            let _ = downlink_events.send(VoiceEvent::Connected);
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    msg = stream.next() => {
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
