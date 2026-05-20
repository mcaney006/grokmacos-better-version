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

/// HTTP client policy. Lets each provider relax exactly the constraint it
/// needs. We default to the strictest setting and explicit opt-outs are
/// required at the call site — there's no "permissive default" anywhere.
#[derive(Debug, Clone, Copy)]
pub struct HttpPolicy {
    /// Refuse plain `http://` URLs. The remote providers (xAI / OpenAI /
    /// Anthropic) are HTTPS-only and we want a hard failure on anyone
    /// trying to downgrade. The local provider talks to
    /// `http://127.0.0.1` by default and explicitly relaxes this.
    pub https_only: bool,
}

impl HttpPolicy {
    /// Strictest stance: HTTPS-only, TLS 1.2+. Used by every cloud
    /// provider client.
    pub const STRICT: Self = HttpPolicy { https_only: true };
    /// Loopback-friendly: still TLS 1.2+ if HTTPS is used, but plain HTTP
    /// is also accepted. The only legitimate use is the LocalClient
    /// talking to Ollama / LM Studio / llama.cpp-server on 127.0.0.1.
    pub const LOOPBACK: Self = HttpPolicy { https_only: false };
}

/// Build a hardened HTTP client tuned by `policy`:
/// * TLS 1.2+ enforced (TLS 1.0/1.1 are deprecated and have known
///   weaknesses) regardless of policy.
/// * `https_only` per the policy. Most providers want it true.
/// * Tight `connect_timeout` so DNS or TCP stalls fail fast instead of
///   keeping a half-open socket around.
/// * Overall `timeout` caps any single request — guards against a malicious
///   or buggy server holding the stream open forever.
pub fn http_client(policy: HttpPolicy) -> Client {
    #[allow(clippy::expect_used)] // Builder failure is a process-wide misconfig at startup
    Client::builder()
        .user_agent(concat!("grok-insane/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Some(Duration::from_secs(60)))
        .https_only(policy.https_only)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .build()
        .expect("reqwest client")
}

// The line-budget guard moved into `services::sse::LINE_BUDGET_BYTES`
// when the byte-line buffer was extracted into a shared primitive.

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
        Self::with_base_and_policy(api_key, base, HttpPolicy::STRICT)
    }

    /// Lower-level constructor that lets a non-cloud caller (notably
    /// LocalClient) relax the HTTPS-only constraint without weakening
    /// the cloud-facing defaults.
    pub fn with_base_and_policy(
        api_key: impl Into<String>,
        base: impl Into<String>,
        policy: HttpPolicy,
    ) -> Self {
        Self {
            http: http_client(policy),
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

        let request_id = extract_request_id(resp.headers());
        let stream = resp.bytes_stream();
        Ok(Box::pin(sse_to_events(stream, request_id)))
    }
}

/// Extract the provider's request-id header for observability. xAI + OpenAI
/// both use `x-request-id`; Anthropic uses `request-id`. We probe both
/// because the same helper is used by both providers via the shared SSE
/// decoder pipeline. Exposed `pub(crate)` so the Anthropic adapter can
/// call it without duplicating the lookup table.
pub(crate) fn extract_request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    for name in ["request-id", "x-request-id"] {
        if let Some(v) = headers.get(name) {
            if let Ok(s) = v.to_str() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn sse_to_events<S>(
    input: S,
    request_id: Option<String>,
) -> impl futures_util::Stream<Item = Result<ChatEvent, ApiError>>
where
    S: futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
{
    use futures_util::stream;

    let state = SseDecoder::new(request_id);
    stream::unfold(
        (Box::pin(input), state, false),
        |(mut input, mut state, done)| async move {
            if done {
                return None;
            }
            loop {
                // Emit any events (or stream errors) buffered from a previous chunk.
                if let Some(event) = state.next_event() {
                    let terminal = event.is_err() || matches!(event, Ok(ChatEvent::Done));
                    return Some((event, (input, state, terminal)));
                }
                match input.next().await {
                    Some(Ok(chunk)) => {
                        state.feed(&chunk);
                    }
                    Some(Err(e)) => {
                        return Some((Err(ApiError::Http(e)), (input, state, true)));
                    }
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

/// OpenAI-compatible SSE decoder.
///
/// Failure model: the queue holds `Result<ChatEvent, ApiError>`, not just
/// `ChatEvent`. That lets us emit truncation / overflow / parse-failure as
/// real errors instead of pretending the stream finished cleanly. The UI
/// already treats `Err` items from the stream as a `StreamMsg::Error` —
/// the only thing we needed to do was actually emit them.
struct SseDecoder {
    buf: crate::services::sse::LineByteBuffer,
    pending: std::collections::VecDeque<Result<ChatEvent, ApiError>>,
    saw_done: bool,
    parse_failures: u32,
    /// Provider request-id captured from the response headers, propagated
    /// into structured errors for support handoffs.
    request_id: Option<String>,
}

/// After this many JSON parse failures in a single stream we stop trying.
/// Persistent JSON failures usually mean either a wire-protocol drift or
/// a malicious upstream; either way, "keep streaming nothing" is worse
/// than "fail with a clear error".
const SSE_PARSE_FAILURE_LIMIT: u32 = 3;

impl SseDecoder {
    fn new(request_id: Option<String>) -> Self {
        Self {
            buf: Default::default(),
            pending: Default::default(),
            saw_done: false,
            parse_failures: 0,
            request_id,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        use crate::services::sse::BufferStatus;
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
            // SSE comments start with `:` per the spec. We don't use them
            // but the parser must skip them rather than treat them as data.
            if line.starts_with(':') {
                continue;
            }
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.trim_start();
            if payload == "[DONE]" {
                self.saw_done = true;
                self.pending.push_back(Ok(ChatEvent::Done));
                continue;
            }
            match serde_json::from_str::<StreamChunk>(payload) {
                Ok(chunk) => {
                    for choice in chunk.choices {
                        if let Some(content) = choice.delta.content {
                            if !content.is_empty() {
                                self.pending.push_back(Ok(ChatEvent::Delta(content)));
                            }
                        }
                    }
                    if let Some(usage) = chunk.usage {
                        self.pending.push_back(Ok(ChatEvent::Usage {
                            input: usage.prompt_tokens,
                            output: usage.completion_tokens,
                        }));
                    }
                }
                Err(e) => {
                    self.parse_failures += 1;
                    tracing::warn!(
                        error = %e,
                        payload = %payload,
                        failures = self.parse_failures,
                        request_id = ?self.request_id,
                        "sse parse fail"
                    );
                    if self.parse_failures >= SSE_PARSE_FAILURE_LIMIT {
                        self.push_provider_error(format!(
                            "too many malformed SSE events ({})",
                            self.parse_failures
                        ));
                    }
                }
            }
        }
    }

    fn eof(&mut self) {
        if !self.saw_done {
            self.push_truncation(
                "stream ended before [DONE] terminator (connection dropped or proxy timeout)"
                    .to_string(),
            );
        }
    }

    fn next_event(&mut self) -> Option<Result<ChatEvent, ApiError>> {
        self.pending.pop_front()
    }

    fn push_truncation(&mut self, msg: String) {
        self.saw_done = true; // stop further EOF handling
        self.pending.push_back(Err(ApiError::StreamTruncated {
            provider: "openai-compatible",
            message: msg,
            request_id: ApiError::fmt_request_id(self.request_id.as_deref()),
        }));
    }

    fn push_provider_error(&mut self, msg: String) {
        self.saw_done = true;
        self.pending.push_back(Err(ApiError::ProviderStream {
            provider: "openai-compatible",
            message: msg,
            request_id: ApiError::fmt_request_id(self.request_id.as_deref()),
        }));
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

    fn collect_ok(d: &mut SseDecoder) -> Vec<ChatEvent> {
        let mut out = Vec::new();
        while let Some(item) = d.next_event() {
            if let Ok(ev) = item {
                out.push(ev);
            }
        }
        out
    }

    #[test]
    fn sse_decoder_extracts_deltas_and_done() {
        let mut d = SseDecoder::new(None);
        d.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi \"}}]}\n\n");
        d.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"there\"}}]}\n\n");
        d.feed(b"data: [DONE]\n\n");
        let got: Vec<String> = collect_ok(&mut d)
            .iter()
            .map(|e| format!("{e:?}"))
            .collect();
        assert!(got.iter().any(|s| s.contains("hi ")));
        assert!(got.iter().any(|s| s.contains("there")));
        assert!(got.iter().any(|s| s.contains("Done")));
    }

    /// Regression: a network chunk that splits a multi-byte UTF-8 codepoint
    /// must not corrupt the rebuilt token.
    #[test]
    fn sse_decoder_handles_utf8_split_across_chunks() {
        let mut d = SseDecoder::new(None);
        let full = b"data: {\"choices\":[{\"delta\":{\"content\":\"\xF0\x9F\x8E\x99\"}}]}\n\n";
        let split_at = full.iter().position(|&b| b == 0xF0).map(|i| i + 2).unwrap();
        d.feed(&full[..split_at]);
        d.feed(&full[split_at..]);
        let deltas: Vec<String> = collect_ok(&mut d)
            .into_iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(s) => Some(s),
                _ => None,
            })
            .collect();
        let joined = deltas.concat();
        assert!(
            joined.contains('\u{1F399}'),
            "expected the emoji codepoint U+1F399, got {joined:?}"
        );
        assert!(
            !joined.contains('\u{FFFD}'),
            "decoder produced a replacement character: {joined:?}"
        );
    }

    /// Regression: CRLF line endings must keep working after the byte-buffer
    /// refactor.
    #[test]
    fn sse_decoder_handles_crlf_line_endings() {
        let mut d = SseDecoder::new(None);
        d.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"crlf\"}}]}\r\n\r\n");
        let deltas: Vec<String> = collect_ok(&mut d)
            .into_iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["crlf"]);
    }

    /// Regression: EOF before `[DONE]` is treated as truncation, NOT as a
    /// clean stop. Previously the decoder synthesised `ChatEvent::Done` at
    /// EOF, which meant a connection drop, proxy timeout, or upstream
    /// crash all looked like normal completion to the UI.
    #[test]
    fn sse_decoder_treats_eof_before_done_as_truncation() {
        let mut d = SseDecoder::new(Some("req-123".into()));
        d.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n");
        // No [DONE] line. Pretend the connection died.
        d.eof();

        // First event: the partial delta we did receive.
        let first = d.next_event().expect("delta");
        assert!(matches!(first, Ok(ChatEvent::Delta(ref s)) if s == "partial"));

        // Second event: a structured truncation error carrying our request-id.
        let second = d.next_event().expect("truncation error");
        let err = second.expect_err("expected Err");
        let rendered = err.to_string();
        assert!(rendered.contains("truncated"), "got {rendered}");
        assert!(rendered.contains("req-123"), "got {rendered}");
    }

    /// Regression: an adversarial server that interleaves empty `Bytes`
    /// chunks must not confuse the decoder. reqwest's `bytes_stream()` can
    /// surface zero-length chunks legitimately (heartbeat slices, TLS
    /// record boundaries) and a naive decoder that special-cased "len > 0"
    /// would drop subsequent real data.
    #[test]
    fn sse_decoder_handles_zero_byte_chunks() {
        let mut d = SseDecoder::new(None);
        // Real payload chopped fine + zero-byte chunks scattered throughout.
        let pieces: &[&[u8]] = &[
            b"",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a",
            b"",
            b"",
            b"lpha\"}}]}\n",
            b"",
            b"\n",
            b"",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"beta\"}}]}\n\n",
            b"",
            b"data: [DONE]\n\n",
            b"",
        ];
        for p in pieces {
            d.feed(p);
        }
        let deltas: Vec<String> = collect_ok(&mut d)
            .into_iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.concat(), "alphabeta");
    }

    /// Regression: persistent JSON parse failures surface as a
    /// `ProviderStream` error after `SSE_PARSE_FAILURE_LIMIT` strikes
    /// instead of silently swallowing forever.
    #[test]
    fn sse_decoder_surfaces_repeated_parse_failures() {
        let mut d = SseDecoder::new(None);
        for _ in 0..SSE_PARSE_FAILURE_LIMIT {
            d.feed(b"data: {not-json\n\n");
        }
        let err = d
            .next_event()
            .expect("error after repeated parse failures")
            .expect_err("expected Err");
        assert!(err.to_string().contains("malformed"), "got {err}");
    }
}
