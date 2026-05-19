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

pub fn http_client() -> Client {
    Client::builder()
        .user_agent(concat!("grok-insane/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .pool_idle_timeout(Some(Duration::from_secs(60)))
        .build()
        .expect("reqwest client")
}

/// xAI Grok chat completions client. Endpoint is OpenAI-compatible.
pub struct XaiClient {
    http: Client,
    base: String,
    api_key: String,
}

impl XaiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base(api_key, "https://api.x.ai/v1")
    }

    pub fn with_base(api_key: impl Into<String>, base: impl Into<String>) -> Self {
        Self {
            http: http_client(),
            base: base.into(),
            api_key: api_key.into(),
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
            HeaderValue::from_str(&format!("Bearer {}", self.api_key))
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
