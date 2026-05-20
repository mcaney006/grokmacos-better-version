//! Anthropic Claude streaming client.
//!
//! Uses Anthropic's Messages API (`/v1/messages?stream=true`) which emits a
//! server-sent-event stream with typed events: `content_block_delta`,
//! `message_delta`, `message_stop`. We translate those into our shared
//! `ChatEvent` enum so the UI doesn't need to know which provider replied.

use crate::error::ApiError;
use crate::models::{Provider, WireMessage};
use crate::services::chat::http_client;
use crate::services::providers::{ChatEvent, ChatProvider, ChatRequest, EventStream};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Client;
use serde::{Deserialize, Serialize};

const DEFAULT_BASE: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    http: Client,
    base: String,
    api_key: String,
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: http_client(),
            base: DEFAULT_BASE.to_string(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl ChatProvider for AnthropicClient {
    fn id(&self) -> Provider {
        Provider::Anthropic
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, ApiError> {
        if self.api_key.trim().is_empty() {
            return Err(ApiError::MissingKey);
        }

        // Anthropic separates the system prompt from the message list and
        // never uses a "system" role inside `messages`. We strip + lift it.
        let mut system: Option<String> = req.system_prompt.clone();
        let mut messages: Vec<WireMessage> = Vec::with_capacity(req.messages.len());
        for m in req.messages {
            if m.role == "system" {
                system = Some(match system {
                    Some(prev) if !prev.is_empty() => format!("{prev}\n\n{}", m.content),
                    _ => m.content,
                });
            } else {
                messages.push(m);
            }
        }

        let body = MessagesBody {
            model: &req.model,
            messages: &messages,
            system: system.as_deref(),
            max_tokens: req.max_tokens.unwrap_or(4096),
            temperature: req.temperature,
            stream: true,
        };

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&self.api_key)
                .map_err(|e| ApiError::InvalidResponse(e.to_string()))?,
        );
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );

        let url = format!("{}/messages", self.base.trim_end_matches('/'));
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
    let state = AnthropicDecoder::default();
    stream::unfold(
        (Box::pin(input), state, false),
        |(mut input, mut state, done)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(event) = state.next_event() {
                    return Some((Ok(event), (input, state, false)));
                }
                match input.next().await {
                    Some(Ok(chunk)) => state.feed(&chunk),
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
struct AnthropicDecoder {
    buf: String,
    pending: std::collections::VecDeque<ChatEvent>,
    saw_stop: bool,
    input_tokens: u32,
    output_tokens: u32,
}

impl AnthropicDecoder {
    fn feed(&mut self, bytes: &[u8]) {
        if let Ok(s) = std::str::from_utf8(bytes) {
            self.buf.push_str(s);
        } else {
            self.buf.push_str(&String::from_utf8_lossy(bytes));
        }
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
            match serde_json::from_str::<AnthropicEvent>(payload) {
                Ok(AnthropicEvent::ContentBlockDelta { delta }) => match delta {
                    AnthropicDelta::TextDelta { text } if !text.is_empty() => {
                        self.pending.push_back(ChatEvent::Delta(text));
                    }
                    _ => {}
                },
                Ok(AnthropicEvent::MessageStart { message }) => {
                    if let Some(usage) = message.usage {
                        self.input_tokens = usage.input_tokens;
                    }
                }
                Ok(AnthropicEvent::MessageDelta { usage, .. }) => {
                    if let Some(u) = usage {
                        self.output_tokens = u.output_tokens.max(self.output_tokens);
                    }
                }
                Ok(AnthropicEvent::MessageStop) => {
                    self.pending.push_back(ChatEvent::Usage {
                        input: self.input_tokens,
                        output: self.output_tokens,
                    });
                    self.pending.push_back(ChatEvent::Done);
                    self.saw_stop = true;
                }
                Ok(AnthropicEvent::Error { error }) => {
                    self.pending.push_back(ChatEvent::Delta(String::new()));
                    tracing::warn!(error = %error.message, "anthropic error event");
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(error = %e, payload = %payload, "anthropic sse parse fail");
                }
            }
        }
    }

    fn eof(&mut self) {
        if !self.saw_stop {
            self.pending.push_back(ChatEvent::Done);
            self.saw_stop = true;
        }
    }

    fn next_event(&mut self) -> Option<ChatEvent> {
        self.pending.pop_front()
    }
}

// --- wire types -------------------------------------------------------------

#[derive(Serialize)]
struct MessagesBody<'a> {
    model: &'a str,
    messages: &'a [WireMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    max_tokens: u32,
    temperature: f32,
    stream: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
#[allow(clippy::enum_variant_names)]
enum AnthropicEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicMessage },
    #[serde(rename = "content_block_start")]
    ContentBlockStart,
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { delta: AnthropicDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop,
    #[serde(rename = "message_delta")]
    MessageDelta {
        #[serde(default)]
        usage: Option<MessageDeltaUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: AnthropicErrorBody },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct AnthropicMessage {
    #[serde(default)]
    usage: Option<MessageStartUsage>,
}

#[derive(Deserialize)]
struct MessageStartUsage {
    #[serde(default)]
    input_tokens: u32,
}

#[derive(Deserialize)]
struct MessageDeltaUsage {
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Deserialize)]
struct AnthropicErrorBody {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_decoder_parses_text_delta_and_stop() {
        let mut d = AnthropicDecoder::default();
        d.feed(
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n",
        );
        d.feed(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n");
        d.feed(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}\n\n");
        d.feed(
            b"data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":7}}\n\n",
        );
        d.feed(b"data: {\"type\":\"message_stop\"}\n\n");
        let mut deltas = Vec::new();
        let mut saw_usage = false;
        let mut saw_done = false;
        while let Some(e) = d.next_event() {
            match e {
                ChatEvent::Delta(s) => deltas.push(s),
                ChatEvent::Usage { input, output } => {
                    saw_usage = true;
                    assert_eq!(input, 3);
                    assert_eq!(output, 7);
                }
                ChatEvent::Done => saw_done = true,
            }
        }
        assert_eq!(deltas.join(""), "Hello, world");
        assert!(saw_usage);
        assert!(saw_done);
    }
}
