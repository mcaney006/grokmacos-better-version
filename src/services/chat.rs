//! Chat completion client. Today this only implements xAI (OpenAI-compatible);
//! the architecture allows adding OpenAI/Anthropic/local providers behind the
//! same `ChatProvider` trait without touching the UI.

use crate::error::ApiError;
use crate::models::{Provider, WireMessage};
use crate::services::providers::{ChatEvent, ChatProvider, ChatRequest, EventStream};
use async_trait::async_trait;
use futures_util::stream::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use zeroize::Zeroizing;

/// Shared HTTP client with hardened defaults:
/// * TLS 1.2+ enforced (TLS 1.0/1.1 are deprecated and have known weaknesses).
/// * `https_only(true)` blocks accidental plain-HTTP downgrades.
/// * Tight `connect_timeout` so DNS or TCP stalls fail fast instead of
///   keeping a half-open socket around.
/// * Overall `timeout` caps any single request — guards against a malicious
///   or buggy server holding the stream open forever.
pub fn http_client() -> Client {
    #[allow(clippy::expect_used)] // Builder failure is a process-wide misconfig at startup
    Client::builder()
        .user_agent(concat!("grok-insane/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Some(Duration::from_secs(60)))
        .https_only(true)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .build()
        .expect("reqwest client")
}

/// Hard cap on the in-memory buffer the SSE decoder will hold between
/// newlines. A server that streams a single line without ever terminating
/// would otherwise blow up RAM. 4 MiB is far more than any well-formed
/// streaming chunk should ever reach.
const SSE_LINE_BUDGET_BYTES: usize = 4 * 1024 * 1024;

/// xAI Grok chat completions client. Endpoint is OpenAI-compatible.
///
/// The `api_key` is wrapped in [`Zeroizing`] so that when the client is
/// dropped the secret bytes are overwritten before the allocator can reuse
/// them, even if a later allocation lands on the same address. This is
/// belt-and-braces — we already store keys in the OS keyring; this layer
/// guards process-memory dumps and core files.
pub struct XaiClient {
    http: Client,
    base: String,
    api_key: Zeroizing<String>,
}

impl XaiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base(api_key, "https://api.x.ai/v1")
    }

    pub fn with_base(api_key: impl Into<String>, base: impl Into<String>) -> Self {
        Self {
            http: http_client(),
            base: base.into(),
            api_key: Zeroizing::new(api_key.into()),
        }
    }
}

#[async_trait]
impl ChatProvider for XaiClient {
    fn id(&self) -> Provider {
        Provider::Xai
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, ApiError> {
        if self.api_key.trim().is_empty() {
            return Err(ApiError::MissingKey);
        }

        let mut messages = Vec::with_capacity(req.messages.len() + 1);
        if let Some(sys) = req.system_prompt.as_ref() {
            if !sys.trim().is_empty() {
                messages.push(WireMessage {
                    role: "system".into(),
                    content: sys.clone(),
                });
            }
        }
        messages.extend(req.messages);

        let body = ChatBody {
            model: &req.model,
            messages: &messages,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
        };

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key.as_str()))
                .map_err(|e| ApiError::InvalidResponse(e.to_string()))?,
        );

        let url = format!("{}/chat/completions", self.base.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::BadStatus {
                status: status.as_u16(),
                body,
            });
        }

        let stream = resp.bytes_stream();
        Ok(Box::pin(sse_to_events(stream)))
    }
}

fn sse_to_events<S>(input: S) -> impl futures_util::Stream<Item = Result<ChatEvent, ApiError>>
where
    S: futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
{
    use futures_util::stream;

    let state = SseDecoder::default();
    stream::unfold(
        (Box::pin(input), state, false),
        |(mut input, mut state, done)| async move {
            if done {
                return None;
            }
            loop {
                // Emit any events buffered from a previous chunk.
                if let Some(event) = state.next_event() {
                    return Some((Ok(event), (input, state, false)));
                }
                match input.next().await {
                    Some(Ok(chunk)) => {
                        state.feed(&chunk);
                    }
                    Some(Err(e)) => return Some((Err(ApiError::Http(e)), (input, state, true))),
                    None => {
                        state.eof();
                        if let Some(event) = state.next_event() {
                            return Some((Ok(event), (input, state, true)));
                        }
                        return None;
                    }
                }
            }
        },
    )
}

#[derive(Default)]
struct SseDecoder {
    buf: String,
    pending: std::collections::VecDeque<ChatEvent>,
    last_was_done: bool,
}

impl SseDecoder {
    fn feed(&mut self, bytes: &[u8]) {
        // SSE is text/event-stream; provider chunks are UTF-8.
        if let Ok(s) = std::str::from_utf8(bytes) {
            self.buf.push_str(s);
        } else {
            // Should be unreachable for compliant providers. Drop invalid bytes.
            self.buf.push_str(&String::from_utf8_lossy(bytes));
        }
        // DoS guard: if the server never sends a newline we'd grow this
        // buffer indefinitely. Trip the kill switch instead.
        if self.buf.len() > SSE_LINE_BUDGET_BYTES {
            tracing::warn!(
                buf_bytes = self.buf.len(),
                "SSE line buffer exceeded budget; ending stream"
            );
            self.buf.clear();
            self.pending.push_back(ChatEvent::Done);
            self.last_was_done = true;
            return;
        }
        // Process every complete line.
        while let Some(idx) = self.buf.find('\n') {
            let mut line = self.buf[..idx].to_string();
            self.buf.drain(..=idx);
            if line.ends_with('\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.trim_start();
            if payload == "[DONE]" {
                self.last_was_done = true;
                self.pending.push_back(ChatEvent::Done);
                continue;
            }
            match serde_json::from_str::<StreamChunk>(payload) {
                Ok(chunk) => {
                    for choice in chunk.choices {
                        if let Some(content) = choice.delta.content {
                            if !content.is_empty() {
                                self.pending.push_back(ChatEvent::Delta(content));
                            }
                        }
                    }
                    if let Some(usage) = chunk.usage {
                        self.pending.push_back(ChatEvent::Usage {
                            input: usage.prompt_tokens,
                            output: usage.completion_tokens,
                        });
                    }
                }
                Err(e) => {
                    self.pending.push_back(ChatEvent::Delta(String::new())); // keep stream alive
                    tracing::debug!(error = %e, payload = %payload, "sse parse fail");
                }
            }
        }
    }

    fn eof(&mut self) {
        if !self.last_was_done {
            self.pending.push_back(ChatEvent::Done);
            self.last_was_done = true;
        }
    }

    fn next_event(&mut self) -> Option<ChatEvent> {
        self.pending.pop_front()
    }
}

// --- wire types -------------------------------------------------------------

#[derive(Serialize)]
struct ChatBody<'a> {
    model: &'a str,
    messages: &'a [WireMessage],
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sse_decoder_extracts_deltas_and_done() {
        let mut d = SseDecoder::default();
        d.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi \"}}]}\n\n");
        d.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"there\"}}]}\n\n");
        d.feed(b"data: [DONE]\n\n");
        let mut got = Vec::new();
        while let Some(e) = d.next_event() {
            got.push(format!("{e:?}"));
        }
        assert!(got.iter().any(|s| s.contains("hi ")));
        assert!(got.iter().any(|s| s.contains("there")));
        assert!(got.iter().any(|s| s.contains("Done")));
    }
}
