//! Anthropic Claude streaming client.
//!
//! Uses Anthropic's Messages API (`/v1/messages?stream=true`) which emits a
//! server-sent-event stream with typed events: `content_block_delta`,
//! `message_delta`, `message_stop`. We translate those into our shared
//! `ChatEvent` enum so the UI doesn't need to know which provider replied.

use crate::error::ApiError;
use crate::models::{Provider, WireMessage};
use crate::services::chat::{http_client, HttpPolicy};
use crate::services::providers::{ChatEvent, ChatProvider, ChatRequest, EventStream};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const DEFAULT_BASE: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    http: Client,
    base: String,
    api_key: Zeroizing<String>,
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: http_client(HttpPolicy::STRICT),
            base: DEFAULT_BASE.to_string(),
            api_key: Zeroizing::new(api_key.into()),
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
        let resp =
            crate::services::chat::send_with_rate_limit_retry(&self.http, &url, &headers, &body)
                .await?;

        let request_id = crate::services::chat::extract_request_id(resp.headers());
        let stream = resp.bytes_stream();
        Ok(Box::pin(sse_to_events(stream, request_id)))
    }
}

fn sse_to_events<S>(
    input: S,
    request_id: Option<String>,
) -> impl futures_util::Stream<Item = Result<ChatEvent, ApiError>>
where
    S: futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
{
    use futures_util::stream;
    let state = AnthropicDecoder::new(request_id);
    stream::unfold(
        (Box::pin(input), state, false),
        |(mut input, mut state, done)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(event) = state.next_event() {
                    let terminal = event.is_err() || matches!(event, Ok(ChatEvent::Done));
                    return Some((event, (input, state, terminal)));
                }
                match input.next().await {
                    Some(Ok(chunk)) => state.feed(&chunk),
                    Some(Err(e)) => return Some((Err(ApiError::Http(e)), (input, state, true))),
                    None => {
                        state.eof();
                        if let Some(event) = state.next_event() {
                            return Some((event, (input, state, true)));
                        }
                        return None;
                    }
                }
            }
        },
    )
}

/// After this many JSON parse failures in a single stream we stop trying.
/// See the same constant in `chat.rs` for rationale.
const ANTHROPIC_PARSE_FAILURE_LIMIT: u32 = 3;

/// Per-content-block accumulator for a streaming tool call. Anthropic
/// sends `tool_use` blocks as: one `content_block_start` (with id+name),
/// N `content_block_delta` frames each carrying an `input_json_delta`
/// fragment of the input JSON, and one `content_block_stop`. We assemble
/// the fragments here and emit a single `ChatEvent::ToolUse` at stop time.
#[derive(Debug)]
struct ToolUseAccum {
    id: String,
    name: String,
    /// Concatenated `partial_json` fragments. May be `""` if the tool's
    /// input schema has no required fields and the model emits nothing
    /// — that's a valid case, parsed below as `{}`.
    partial: String,
}

/// Anthropic SSE decoder.
///
/// Failure model — what changed from the original version:
///
/// - The queue is `VecDeque<Result<ChatEvent, ApiError>>`, not just
///   `VecDeque<ChatEvent>`. This is what lets us surface provider
///   `error` events as actual errors instead of pretending they were
///   empty content deltas.
/// - `eof()` without a `message_stop` is a `StreamTruncated` error,
///   not a synthetic `Done`. A dropped connection no longer looks like
///   a clean completion.
/// - Repeated JSON parse failures escalate to a `ProviderStream` error
///   after `ANTHROPIC_PARSE_FAILURE_LIMIT` strikes. Either the
///   protocol drifted or the upstream is malicious; either way silent
///   loss is the wrong answer.
/// - `request_id` from the response headers is captured at
///   construction and embedded in every error variant — production
///   support is unworkable without it.
struct AnthropicDecoder {
    buf: crate::services::sse::LineByteBuffer,
    pending: std::collections::VecDeque<Result<ChatEvent, ApiError>>,
    saw_stop: bool,
    input_tokens: u32,
    output_tokens: u32,
    parse_failures: u32,
    request_id: Option<String>,
    /// In-flight tool-use blocks keyed by `content_block_delta.index`.
    /// Indexes are u32 — Anthropic uses contiguous small integers.
    tool_blocks: std::collections::HashMap<u32, ToolUseAccum>,
}

impl AnthropicDecoder {
    fn new(request_id: Option<String>) -> Self {
        Self {
            buf: Default::default(),
            pending: Default::default(),
            saw_stop: false,
            input_tokens: 0,
            output_tokens: 0,
            parse_failures: 0,
            request_id,
            tool_blocks: std::collections::HashMap::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        use crate::services::sse::BufferStatus;
        // Mirror chat.rs: drop bytes once the stream has reached terminal
        // state (clean stop, provider error, truncation, parse overflow).
        if self.saw_stop {
            return;
        }
        if self.buf.extend(bytes) == BufferStatus::Overflow {
            self.push_truncation(format!(
                "SSE line buffer exceeded {} bytes",
                crate::services::sse::LINE_BUDGET_BYTES
            ));
            return;
        }
        while let Some(line) = self.buf.take_line() {
            if line.is_empty() {
                continue;
            }
            // SSE comments start with `:`. We also pass through `event:`
            // lines without dispatching on them — Anthropic's current API
            // emits redundant `event:` for every `data:`, so the data line
            // alone is enough.
            if line.starts_with(':') || line.starts_with("event:") {
                continue;
            }
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.trim_start();
            match serde_json::from_str::<AnthropicEvent>(payload) {
                Ok(AnthropicEvent::ContentBlockStart {
                    index,
                    content_block,
                }) => {
                    if let AnthropicContentBlock::ToolUse { id, name } = content_block {
                        // Begin accumulating partial_json for this block.
                        // We don't emit anything yet — the input isn't
                        // ready until content_block_stop.
                        self.tool_blocks.insert(
                            index,
                            ToolUseAccum {
                                id,
                                name,
                                partial: String::new(),
                            },
                        );
                    }
                }
                Ok(AnthropicEvent::ContentBlockDelta { index, delta }) => match delta {
                    AnthropicDelta::TextDelta { text } if !text.is_empty() => {
                        self.pending.push_back(Ok(ChatEvent::Delta(text)));
                    }
                    AnthropicDelta::InputJsonDelta { partial_json } => {
                        if let Some(accum) = self.tool_blocks.get_mut(&index) {
                            accum.partial.push_str(&partial_json);
                        }
                        // If no accumulator exists for this index, the
                        // server emitted an input_json_delta without a
                        // content_block_start of type tool_use. That's a
                        // wire-protocol violation we silently ignore
                        // rather than crash — same posture as other
                        // unknown shapes (AnthropicDelta::Other).
                    }
                    _ => {}
                },
                Ok(AnthropicEvent::ContentBlockStop { index }) => {
                    if let Some(accum) = self.tool_blocks.remove(&index) {
                        // Empty partial means "no fields" — that's a valid
                        // call with an empty input object, e.g. a tool
                        // that takes no arguments.
                        let raw = if accum.partial.is_empty() {
                            "{}".to_string()
                        } else {
                            accum.partial
                        };
                        match serde_json::from_str::<serde_json::Value>(&raw) {
                            Ok(input) => {
                                self.pending.push_back(Ok(ChatEvent::ToolUse {
                                    id: accum.id,
                                    name: accum.name,
                                    input,
                                }));
                            }
                            Err(e) => {
                                // Partial JSON didn't form a valid object
                                // — surface a typed error rather than
                                // silently dropping the tool call.
                                self.push_provider_error(format!(
                                    "tool_use input_json_delta produced invalid JSON \
                                     (id={}, name={}): {e}",
                                    accum.id, accum.name
                                ));
                            }
                        }
                    }
                }
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
                    self.pending.push_back(Ok(ChatEvent::Usage {
                        input: self.input_tokens,
                        output: self.output_tokens,
                    }));
                    self.pending.push_back(Ok(ChatEvent::Done));
                    self.saw_stop = true;
                }
                Ok(AnthropicEvent::Error { error }) => {
                    // Provider said something is wrong. Surface it as a
                    // typed stream error so the UI can show it, log
                    // pipelines pick it up, and incident response has
                    // the request-id to lean on.
                    self.push_provider_error(format!(
                        "{}: {}",
                        error.kind.as_deref().unwrap_or("error"),
                        error.message
                    ));
                }
                Ok(AnthropicEvent::Ping) => {}
                Err(e) => {
                    self.parse_failures += 1;
                    tracing::warn!(
                        error = %e,
                        payload = %payload,
                        failures = self.parse_failures,
                        request_id = ?self.request_id,
                        "anthropic sse parse fail"
                    );
                    if self.parse_failures >= ANTHROPIC_PARSE_FAILURE_LIMIT {
                        self.push_provider_error(format!(
                            "too many malformed Anthropic SSE events ({})",
                            self.parse_failures
                        ));
                    }
                }
            }
        }
    }

    fn eof(&mut self) {
        if !self.saw_stop {
            self.push_truncation(
                "stream ended before message_stop (connection dropped, proxy timeout, \
                 or provider terminated abnormally)"
                    .to_string(),
            );
        }
    }

    fn next_event(&mut self) -> Option<Result<ChatEvent, ApiError>> {
        self.pending.pop_front()
    }

    fn push_truncation(&mut self, msg: String) {
        self.saw_stop = true;
        self.pending.push_back(Err(ApiError::StreamTruncated {
            provider: "anthropic",
            message: msg,
            request_id: ApiError::fmt_request_id(self.request_id.as_deref()),
        }));
    }

    fn push_provider_error(&mut self, msg: String) {
        self.saw_stop = true;
        self.pending.push_back(Err(ApiError::ProviderStream {
            provider: "anthropic",
            message: msg,
            request_id: ApiError::fmt_request_id(self.request_id.as_deref()),
        }));
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
    ContentBlockStart {
        #[serde(default)]
        index: u32,
        content_block: AnthropicContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        #[serde(default)]
        index: u32,
        delta: AnthropicDelta,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        #[serde(default)]
        index: u32,
    },
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

/// The `content_block` payload inside a `content_block_start`. We only
/// care about `tool_use` here; text blocks don't need any per-block state
/// because their deltas carry the text directly. `Other` swallows any
/// future block type (image, search_result, …) so a protocol bump doesn't
/// break parsing.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "tool_use")]
    ToolUse {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    /// Streamed JSON fragment for a `tool_use` content block. Each fragment
    /// is appended verbatim to the per-index buffer; the concatenation is
    /// parsed as JSON at `content_block_stop` time.
    #[serde(rename = "input_json_delta")]
    InputJsonDelta {
        #[serde(default)]
        partial_json: String,
    },
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
    /// Anthropic includes a `type` discriminator on error bodies
    /// (`overloaded_error`, `invalid_request_error`, etc.). We propagate
    /// it into the structured error so incident response can route by
    /// category, not just by message text.
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn collect(d: &mut AnthropicDecoder) -> Vec<Result<ChatEvent, ApiError>> {
        let mut out = Vec::new();
        while let Some(e) = d.next_event() {
            out.push(e);
        }
        out
    }

    #[test]
    fn anthropic_decoder_parses_text_delta_and_stop() {
        let mut d = AnthropicDecoder::new(None);
        d.feed(
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n",
        );
        d.feed(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n");
        d.feed(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}\n\n");
        d.feed(
            b"data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":7}}\n\n",
        );
        d.feed(b"data: {\"type\":\"message_stop\"}\n\n");

        let events: Vec<ChatEvent> = collect(&mut d).into_iter().filter_map(Result::ok).collect();
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.join(""), "Hello, world");
        let saw_usage = events.iter().any(|e| {
            matches!(
                e,
                ChatEvent::Usage {
                    input: 3,
                    output: 7
                }
            )
        });
        assert!(saw_usage);
        assert!(events.iter().any(|e| matches!(e, ChatEvent::Done)));
    }

    /// Regression: an Anthropic `error` event must become an actual stream
    /// error, not a polite empty delta. The old code emitted an empty
    /// `ChatEvent::Delta(String::new())` and let the stream continue —
    /// the UI then displayed "completed successfully" with no indication
    /// that the provider had said anything at all.
    #[test]
    fn anthropic_decoder_turns_error_event_into_stream_error() {
        let mut d = AnthropicDecoder::new(Some("req-xyz".into()));
        d.feed(b"data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"the model is overloaded\"}}\n\n");

        let events = collect(&mut d);
        let err = events
            .into_iter()
            .find_map(|e| e.err())
            .expect("expected a stream error");
        match err {
            ApiError::ProviderStream {
                provider,
                message,
                request_id,
            } => {
                assert_eq!(provider, "anthropic");
                assert!(message.contains("overloaded"), "msg: {message}");
                assert!(request_id.contains("req-xyz"), "request-id: {request_id}");
            }
            other => panic!("expected ProviderStream, got {other:?}"),
        }
    }

    /// Regression: EOF before `message_stop` is truncation, not success.
    /// Previously the decoder synthesised `Done` at EOF — connection drops,
    /// proxy timeouts, and provider crashes all looked identical to clean
    /// completion.
    #[test]
    fn anthropic_decoder_errors_on_eof_before_message_stop() {
        let mut d = AnthropicDecoder::new(Some("req-trunc".into()));
        d.feed(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n");
        d.eof();

        let events = collect(&mut d);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Ok(ChatEvent::Delta(s)) if s == "partial")),
            "expected the partial delta we did receive"
        );
        let err = events
            .into_iter()
            .find_map(|e| e.err())
            .expect("expected a truncation error");
        match err {
            ApiError::StreamTruncated {
                provider,
                request_id,
                ..
            } => {
                assert_eq!(provider, "anthropic");
                assert!(request_id.contains("req-trunc"));
            }
            other => panic!("expected StreamTruncated, got {other:?}"),
        }
    }

    /// Regression: a multi-byte codepoint split across two `feed()` calls
    /// reconstructs cleanly. Previously the decoder ran `from_utf8` per
    /// chunk and lossy-replaced orphan continuation bytes with U+FFFD.
    #[test]
    fn anthropic_decoder_handles_utf8_split_across_chunks() {
        let mut d = AnthropicDecoder::new(None);
        let full = "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello 🦀\"}}\n\n".as_bytes();
        // Split inside the emoji's UTF-8 bytes.
        let split = full.len() - 5;
        d.feed(&full[..split]);
        d.feed(&full[split..]);
        d.feed(b"data: {\"type\":\"message_stop\"}\n\n");

        let events: Vec<ChatEvent> = collect(&mut d).into_iter().filter_map(Result::ok).collect();
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains('🦀'), "got {text:?}");
        assert!(!text.contains('\u{FFFD}'), "got {text:?}");
    }

    /// Regression: persistent JSON parse failures should surface as a
    /// stream error after `ANTHROPIC_PARSE_FAILURE_LIMIT` strikes, not
    /// pretend everything is fine forever.
    #[test]
    fn anthropic_decoder_surfaces_repeated_parse_failures() {
        let mut d = AnthropicDecoder::new(None);
        for _ in 0..ANTHROPIC_PARSE_FAILURE_LIMIT {
            d.feed(b"data: {not-valid-json\n\n");
        }
        let err = collect(&mut d)
            .into_iter()
            .find_map(|e| e.err())
            .expect("expected error after repeated parse failures");
        assert!(
            matches!(err, ApiError::ProviderStream { .. }),
            "expected ProviderStream, got {err:?}"
        );
    }

    /// Property test: the decoder must never panic regardless of how
    /// adversarial the input is. We feed arbitrary byte blobs in
    /// arbitrary-sized chunks and assert that `feed` + `eof` + `next_event`
    /// always return cleanly.
    ///
    /// Adversarial: feeding bytes to a decoder that's already seen
    /// `message_stop` must not produce any new events.
    #[test]
    fn anthropic_decoder_drops_bytes_fed_after_stop() {
        let mut d = AnthropicDecoder::new(None);
        d.feed(b"data: {\"type\":\"message_stop\"}\n\n");
        // Drain Usage + Done.
        while let Some(e) = d.next_event() {
            assert!(e.is_ok(), "unexpected error during clean drain: {e:?}");
        }
        d.feed(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"poison\"}}\n\n");
        assert!(d.next_event().is_none(), "post-stop bytes leaked an event");
    }

    /// Adversarial: an oversized line (no `\n` within budget) must
    /// surface as a `StreamTruncated`, not OOM the process.
    #[test]
    fn anthropic_decoder_oversize_line_surfaces_truncation() {
        let mut d = AnthropicDecoder::new(Some("req-anthropic-overflow".into()));
        let huge = vec![b'x'; crate::services::sse::LINE_BUDGET_BYTES + 1];
        d.feed(&huge);
        let err = d
            .next_event()
            .expect("expected truncation after overflow")
            .expect_err("expected Err");
        let rendered = err.to_string();
        assert!(rendered.contains("truncated"), "got {rendered}");
        assert!(
            rendered.contains("req-anthropic-overflow"),
            "got {rendered}"
        );
    }

    /// Streaming tool_use: `content_block_start` carries the id+name,
    /// `input_json_delta` fragments are concatenated, and a single
    /// `ChatEvent::ToolUse` with the parsed input is emitted at
    /// `content_block_stop`. Replays the same fixture across chunk sizes
    /// 1..=64 to catch fragment-boundary bugs in the accumulator.
    #[test]
    fn anthropic_decoder_assembles_streaming_tool_use_call() {
        let fixture: &[u8] = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":\
                {\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":\
                {\"type\":\"text_delta\",\"text\":\"Looking up... \"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":\
                {\"type\":\"tool_use\",\"id\":\"toolu_abc\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":\
                {\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"location\\\":\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":\
                {\"type\":\"input_json_delta\",\"partial_json\":\" \\\"SF\\\",\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":\
                {\"type\":\"input_json_delta\",\"partial_json\":\" \\\"unit\\\": \\\"C\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":42}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .as_bytes();

        for chunk_size in [1usize, 7, 16, 64] {
            let mut d = AnthropicDecoder::new(None);
            let mut i = 0;
            while i < fixture.len() {
                let end = (i + chunk_size).min(fixture.len());
                d.feed(&fixture[i..end]);
                i = end;
            }
            let events: Vec<ChatEvent> =
                collect(&mut d).into_iter().filter_map(Result::ok).collect();

            // We should see the text delta, then exactly one ToolUse with
            // the assembled input.
            let deltas: Vec<&str> = events
                .iter()
                .filter_map(|e| match e {
                    ChatEvent::Delta(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(deltas.join(""), "Looking up... ", "chunk_size={chunk_size}");

            let tool_calls: Vec<(&str, &str, &serde_json::Value)> = events
                .iter()
                .filter_map(|e| match e {
                    ChatEvent::ToolUse { id, name, input } => {
                        Some((id.as_str(), name.as_str(), input))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(tool_calls.len(), 1, "chunk_size={chunk_size}");
            let (id, name, input) = tool_calls[0];
            assert_eq!(id, "toolu_abc");
            assert_eq!(name, "get_weather");
            assert_eq!(
                input,
                &serde_json::json!({"location": "SF", "unit": "C"}),
                "chunk_size={chunk_size}"
            );
        }
    }

    /// A tool_use call with no fields (empty `partial_json` stream)
    /// must still emit a `ChatEvent::ToolUse` with `input = {}` — that's
    /// a valid call shape, not an error.
    #[test]
    fn anthropic_decoder_tool_use_with_empty_input() {
        let mut d = AnthropicDecoder::new(None);
        d.feed(b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_empty\",\"name\":\"ping\",\"input\":{}}}\n\n");
        d.feed(b"data: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
        let events: Vec<ChatEvent> = collect(&mut d).into_iter().filter_map(Result::ok).collect();
        match events.first() {
            Some(ChatEvent::ToolUse { id, name, input }) => {
                assert_eq!(id, "toolu_empty");
                assert_eq!(name, "ping");
                assert_eq!(input, &serde_json::json!({}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    /// Malformed `partial_json` fragments must surface as a typed
    /// `ProviderStream` error, not be silently dropped.
    #[test]
    fn anthropic_decoder_tool_use_with_malformed_json_surfaces_error() {
        let mut d = AnthropicDecoder::new(Some("req-bad-tool".into()));
        d.feed(b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_x\",\"name\":\"f\",\"input\":{}}}\n\n");
        d.feed(b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{not-json\"}}\n\n");
        d.feed(b"data: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
        let err = collect(&mut d)
            .into_iter()
            .find_map(|e| e.err())
            .expect("expected error from malformed tool_use input");
        let s = err.to_string();
        assert!(s.contains("tool_use"), "{s}");
        assert!(s.contains("req-bad-tool"), "{s}");
    }

    /// Fixture-driven invariance check: replay a captured Anthropic SSE
    /// stream at every chunk size from 1 to 128 bytes; the emitted event
    /// stream must be identical across all chunkings. Catches subtle
    /// boundary bugs that only fire under specific chunk sizes (e.g. one
    /// byte short of a CRLF, or `\n` arriving alone).
    #[test]
    fn anthropic_decoder_fixture_replay_chunk_sizes_1_through_128() {
        let fixture: &[u8] = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\r\n\r\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"world \"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"\u{1F600}\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":9}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .as_bytes();

        let mut canonical: Option<Vec<String>> = None;
        for chunk_size in 1..=128usize {
            let mut d = AnthropicDecoder::new(None);
            let mut i = 0;
            while i < fixture.len() {
                let end = (i + chunk_size).min(fixture.len());
                d.feed(&fixture[i..end]);
                i = end;
            }
            let events: Vec<String> = collect(&mut d)
                .into_iter()
                .filter_map(Result::ok)
                .map(|e| format!("{e:?}"))
                .collect();
            match &canonical {
                None => canonical = Some(events),
                Some(c) => assert_eq!(
                    c, &events,
                    "fixture replay diverged at chunk_size={chunk_size}"
                ),
            }
        }
        let canonical = canonical.expect("ran at least once");
        let joined: String = canonical.join(",");
        assert!(joined.contains("hello "), "{joined}");
        assert!(joined.contains("world "), "{joined}");
        assert!(joined.contains("\u{1F600}"), "{joined}");
        assert!(joined.contains("Done"), "{joined}");
    }

    /// Uses a simple LCG so this stays a unit test (no extra dep) and
    /// rotates through 1000 deterministic seeds — equivalent in coverage
    /// to a small proptest run.
    #[test]
    fn anthropic_decoder_never_panics_on_arbitrary_bytes() {
        fn rng_byte(state: &mut u64) -> u8 {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (*state >> 33) as u8
        }
        for seed in 0..1000u64 {
            let mut state = seed.wrapping_add(0xDEAD_BEEF);
            let mut d = AnthropicDecoder::new(None);
            // Up to 8 chunks, each up to 256 random bytes.
            let chunks = (rng_byte(&mut state) % 8) as usize + 1;
            for _ in 0..chunks {
                let n = (rng_byte(&mut state) as usize) + 1;
                let mut buf = Vec::with_capacity(n);
                for _ in 0..n {
                    buf.push(rng_byte(&mut state));
                }
                d.feed(&buf);
            }
            d.eof();
            // Drain — should never panic.
            while d.next_event().is_some() {}
        }
    }
}
