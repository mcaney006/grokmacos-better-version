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
/// * **No** overall `.timeout()`. The original config set 120s, which
///   reqwest applies to the *entire* request including streaming body
///   read. A long Claude / Grok answer can legitimately stream for
///   several minutes; we'd kill it mid-token. Instead we enforce
///   `connect_timeout` (so a dead server is rejected fast) plus a
///   per-attempt cap inside `send_with_rate_limit_retry` (which only
///   covers pre-first-byte). The stream itself is allowed to run for
///   as long as the provider keeps emitting bytes.
///
/// Builds the hardened client. On the rare path where reqwest's builder
/// fails (e.g., the platform's TLS backend can't initialise), we degrade
/// in a precise order:
///
/// 1. **Drop `min_tls_version`** — some embedded targets ship a rustls
///    build that doesn't expose `min_tls_version`; the cipher list is
///    still TLS 1.2+ by default. We keep `https_only`.
/// 2. **Last-resort: panic.** If even the bare `Client::builder().build()`
///    fails AND `policy.https_only` is set, refusing to run is the only
///    safe outcome — falling back to a plain `Client::new()` would
///    SILENTLY downgrade a cloud client to http:// + no TLS hardening,
///    which is a way more dangerous failure mode than a startup panic.
///    The previous version did exactly that.
///
/// The `LOOPBACK` policy explicitly opts in to plain HTTP for local
/// development; it's the only case where the bare `Client::new()`
/// fallback is acceptable.
pub fn http_client(policy: HttpPolicy) -> Client {
    fn build_hardened(policy: HttpPolicy, with_min_tls: bool) -> Result<Client, reqwest::Error> {
        let mut b = Client::builder()
            .user_agent(concat!("grok-insane/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .https_only(policy.https_only);
        if with_min_tls {
            b = b.min_tls_version(reqwest::tls::Version::TLS_1_2);
        }
        b.build()
    }

    match build_hardened(policy, true) {
        Ok(c) => c,
        Err(e1) => {
            tracing::warn!(error = %e1, "hardened reqwest builder failed; retrying without min_tls_version");
            match build_hardened(policy, false) {
                Ok(c) => c,
                Err(e2) => {
                    tracing::error!(
                        error_first = %e1,
                        error_retry = %e2,
                        https_only = policy.https_only,
                        "reqwest builder failed twice"
                    );
                    if policy.https_only {
                        // STRICT clients NEVER fall back to Client::new(),
                        // which would drop https_only and ship credentials
                        // over plaintext. A panic here is loud and visible;
                        // a silent downgrade is the dangerous failure mode.
                        panic!(
                            "Cannot build a TLS-hardened HTTP client on this platform: \
                             {e1}. Refusing to fall back to an unhardened client because \
                             the policy requires https_only."
                        );
                    }
                    // LOOPBACK policy already permits plaintext locally;
                    // this matches what the user asked for.
                    Client::new()
                }
            }
        }
    }
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
        let resp = send_with_rate_limit_retry(&self.http, &url, &headers, &body).await?;

        let request_id = extract_request_id(resp.headers());
        let stream = resp.bytes_stream();
        Ok(Box::pin(sse_to_events(stream, request_id)))
    }
}

/// Default per-stream retry budget for HTTP 429. Bounded so a sustained
/// rate-limit cycle surfaces to the user instead of hanging indefinitely.
pub(crate) const RATE_LIMIT_RETRY_ATTEMPTS: usize = 3;
/// Cap on the wait we'll honour from a server-provided `Retry-After`.
/// Anthropic and xAI typically send small values (<=10s); a malicious or
/// confused proxy that sends `Retry-After: 86400` should not freeze our
/// app — we cap, retry once, then surface the error.
pub(crate) const RATE_LIMIT_MAX_WAIT_SECS: u64 = 30;

/// Per-attempt cap for the pre-first-byte send. Bounds DNS + TCP + TLS +
/// HTTP-headers wait, but NOT the stream body (the stream itself can run
/// for as long as the provider keeps emitting tokens). 60s is generous
/// enough that providers under modest load still complete; a hanging
/// transport that gets nowhere in 60s is broken, not slow.
pub(crate) const PRE_FIRST_BYTE_TIMEOUT_SECS: u64 = 60;

/// Cap on the number of error-body bytes we read into a `BadStatus` for
/// the user-facing diagnostic. A malicious / broken upstream that
/// returns a multi-gigabyte 500 body would otherwise OOM the process
/// via `resp.text()`. 16 KiB is plenty to fit the JSON error envelopes
/// every real provider returns; any genuine truncation is logged.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

/// Read at most `MAX_ERROR_BODY_BYTES` of the response body via the
/// streaming API. `Response::text()` has no built-in cap; iterating the
/// `bytes_stream` ourselves keeps memory bounded regardless of what the
/// peer sends. Decoding is `from_utf8_lossy` so a binary error body
/// (e.g., a misbehaving CDN serving HTML or gzip) still surfaces
/// something human-readable.
async fn read_capped_body(resp: reqwest::Response) -> String {
    use futures_util::stream::StreamExt as _;
    let mut buf = Vec::with_capacity(1024);
    let mut stream = resp.bytes_stream();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                if buf.len() + bytes.len() > MAX_ERROR_BODY_BYTES {
                    let room = MAX_ERROR_BODY_BYTES.saturating_sub(buf.len());
                    buf.extend_from_slice(&bytes[..room]);
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(&bytes);
            }
            Err(e) => {
                tracing::warn!(error = %e, "error reading body for non-success response");
                break;
            }
        }
    }
    let mut out = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        out.push_str(&format!("\n[…truncated at {MAX_ERROR_BODY_BYTES} bytes]"));
    }
    out
}

/// Send a request with bounded pre-first-byte retry on HTTP 429. The
/// `build`-style alternative (closure that rebuilds RequestBuilder each
/// attempt) was simpler in shape but harder to use because
/// `RequestBuilder` isn't `Clone`. Taking the raw pieces and assembling
/// a fresh request inside the loop keeps the call sites simple and
/// retries truly fresh (new TCP, new TLS handshake if needed).
///
/// "Pre-first-byte only" means: we retry the entire send, but we never
/// touch a stream once `bytes_stream()` has been called on it. Mid-stream
/// failures propagate as-is.
pub(crate) async fn send_with_rate_limit_retry<B: serde::Serialize>(
    http: &Client,
    url: &str,
    headers: &HeaderMap,
    body: &B,
) -> Result<reqwest::Response, ApiError> {
    let mut attempt = 0usize;
    loop {
        // Bound the pre-first-byte send via `tokio::time::timeout`, NOT
        // reqwest's RequestBuilder::timeout — the latter applies to the
        // entire request including the body, which would kill our
        // streaming response. `Client::send().await` resolves as soon as
        // the response status + headers arrive (per reqwest's docs), so
        // wrapping the future itself bounds only the pre-body wait.
        let send_fut = http.post(url).headers(headers.clone()).json(body).send();
        let resp = match tokio::time::timeout(
            std::time::Duration::from_secs(PRE_FIRST_BYTE_TIMEOUT_SECS),
            send_fut,
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(ApiError::Http(e)),
            Err(_elapsed) => {
                return Err(ApiError::InvalidResponse(format!(
                    "request timed out before response headers ({PRE_FIRST_BYTE_TIMEOUT_SECS}s)"
                )));
            }
        };
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status.as_u16() == 429 && attempt + 1 < RATE_LIMIT_RETRY_ATTEMPTS {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(ApiError::parse_retry_after);
            let wait = retry_after
                .map(|d| {
                    if d.as_secs() > RATE_LIMIT_MAX_WAIT_SECS {
                        std::time::Duration::from_secs(RATE_LIMIT_MAX_WAIT_SECS)
                    } else {
                        d
                    }
                })
                .unwrap_or_else(|| std::time::Duration::from_millis(500 << attempt));
            tracing::warn!(?wait, attempt, url = %url, "rate-limited; backing off");
            tokio::time::sleep(wait).await;
            attempt += 1;
            continue;
        }
        if status.as_u16() == 429 {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(ApiError::parse_retry_after);
            return Err(ApiError::RateLimited {
                retry_hint: ApiError::fmt_retry_hint(retry_after),
                retry_after,
            });
        }
        let body = read_capped_body(resp).await;
        return Err(ApiError::BadStatus {
            status: status.as_u16(),
            body,
        });
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
///
/// `cargo fuzz` harnesses reach this type via the `__fuzz` feature
/// (`crate::services::chat::__fuzz_drive`). Not public API.
#[cfg(feature = "__fuzz")]
#[doc(hidden)]
pub fn __fuzz_drive(bytes: &[u8]) {
    let mut d = SseDecoder::new(None);
    d.feed(bytes);
    d.eof();
    // Drain; a fuzz iteration must terminate.
    while let Some(_e) = d.next_event() {}
}

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
        // Once we've decided the stream is over (clean DONE, provider
        // error, truncation, or parse-failure overflow), drop subsequent
        // bytes on the floor. Callers SHOULD stop feeding after they see
        // an Err / Done, but a defensive guard here prevents a buggy
        // caller from re-driving the parser past terminal state.
        if self.saw_done {
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
            // saw_done may be flipped on by [DONE] or by the parse-failure
            // / overflow paths below. Re-check on every iteration so a
            // single feed call carrying data BOTH before AND after the
            // terminator can't keep pumping events out the back of the
            // decoder.
            if self.saw_done {
                break;
            }
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

    /// One scripted reply from `spawn_mock_http`. Wrapped in a struct so
    /// the test signature stays under clippy's `type_complexity` lint.
    #[derive(Clone)]
    struct MockReply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// Mock HTTP server that returns one or more pre-canned responses
    /// to POST requests. Used by the rate-limit + bad-status tests to
    /// drive `send_with_rate_limit_retry` against a real network stack
    /// without depending on the public internet.
    ///
    /// On each inbound request the server pops the front of `responses`
    /// and serves it. When only one entry remains it serves that one
    /// indefinitely (useful for the "always 429" / "always 500" cases).
    async fn spawn_mock_http(responses: Vec<MockReply>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let responses = std::sync::Arc::new(parking_lot::Mutex::new(responses));
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let responses = responses.clone();
                tokio::spawn(async move {
                    // Drain request bytes until we see "\r\n\r\n" — naive
                    // but fine for our short JSON POSTs.
                    let mut buf = [0u8; 8192];
                    let mut total = Vec::new();
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        total.extend_from_slice(&buf[..n]);
                        if total.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let next = {
                        let mut q = responses.lock();
                        if q.len() > 1 {
                            q.remove(0)
                        } else if let Some(last) = q.first() {
                            last.clone()
                        } else {
                            MockReply {
                                status: 500,
                                headers: vec![],
                                body: "no canned response".to_string(),
                            }
                        }
                    };
                    let MockReply {
                        status,
                        headers,
                        body,
                    } = next;
                    let status_text = match status {
                        200 => "OK",
                        429 => "Too Many Requests",
                        500 => "Internal Server Error",
                        _ => "Error",
                    };
                    let mut resp = format!("HTTP/1.1 {status} {status_text}\r\n");
                    resp.push_str(&format!("Content-Length: {}\r\n", body.len()));
                    resp.push_str("Connection: close\r\n");
                    for (k, v) in &headers {
                        resp.push_str(&format!("{k}: {v}\r\n"));
                    }
                    resp.push_str("\r\n");
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.write_all(body.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        // Let the listener register.
        tokio::time::sleep(Duration::from_millis(20)).await;
        format!("http://127.0.0.1:{port}")
    }

    /// Adversarial: a hostile / broken upstream that returns a non-success
    /// status with a multi-megabyte body must not let us pull the whole
    /// thing into memory. The error path uses `resp.text()` to extract a
    /// diagnostic snippet for the user; without an upstream cap, an
    /// attacker who returns a 10 GB 500 OOMs the process.
    ///
    /// This test plants an 8 MB body behind a 500 status; the surfaced
    /// `BadStatus.body` must be capped at `MAX_ERROR_BODY_BYTES`.
    #[tokio::test(flavor = "current_thread")]
    async fn bad_status_body_is_capped_against_oom() {
        let huge = "x".repeat(8 * 1024 * 1024);
        let url = spawn_mock_http(vec![MockReply {
            status: 500,
            headers: vec![],
            body: huge,
        }])
        .await;
        let http = http_client(HttpPolicy::LOOPBACK);
        let err =
            send_with_rate_limit_retry(&http, &url, &HeaderMap::new(), &serde_json::json!({}))
                .await
                .expect_err("500 must surface");
        match err {
            ApiError::BadStatus { status, body } => {
                assert_eq!(status, 500);
                assert!(
                    body.len() <= MAX_ERROR_BODY_BYTES + 64,
                    "body not capped (len {}, expected <= {})",
                    body.len(),
                    MAX_ERROR_BODY_BYTES + 64
                );
            }
            other => panic!("expected BadStatus, got {other:?}"),
        }
    }

    /// 429 + small Retry-After → first retry succeeds with 200. Proves
    /// the retry loop both honours the header and exits the loop once
    /// the server starts cooperating.
    #[tokio::test(flavor = "current_thread")]
    async fn rate_limit_retry_honours_retry_after_then_succeeds() {
        let url = spawn_mock_http(vec![
            MockReply {
                status: 429,
                headers: vec![("Retry-After".into(), "1".into())],
                body: "slow down".into(),
            },
            MockReply {
                status: 200,
                headers: vec![],
                body: "ok".into(),
            },
        ])
        .await;
        let http = http_client(HttpPolicy::LOOPBACK);
        let body = serde_json::json!({"hello": "world"});
        let started = std::time::Instant::now();
        let resp = send_with_rate_limit_retry(&http, &url, &HeaderMap::new(), &body)
            .await
            .expect("retry should succeed on the second attempt");
        let elapsed = started.elapsed();
        assert_eq!(resp.status().as_u16(), 200);
        assert!(
            elapsed >= Duration::from_millis(900),
            "expected ~1s Retry-After honoured, got {elapsed:?}"
        );
    }

    /// Sustained 429s burn through RATE_LIMIT_RETRY_ATTEMPTS and then
    /// surface as `ApiError::RateLimited` with the parsed Retry-After.
    #[tokio::test(flavor = "current_thread")]
    async fn rate_limit_retry_gives_up_after_budget_and_classifies_correctly() {
        // Tiny Retry-After so the test doesn't take forever.
        let responses = vec![
            MockReply {
                status: 429,
                headers: vec![("Retry-After".into(), "0".into())],
                body: "limit".into(),
            };
            RATE_LIMIT_RETRY_ATTEMPTS + 1
        ];
        let url = spawn_mock_http(responses).await;
        let http = http_client(HttpPolicy::LOOPBACK);
        let err =
            send_with_rate_limit_retry(&http, &url, &HeaderMap::new(), &serde_json::json!({}))
                .await
                .expect_err("should have surrendered after retry budget");
        match err {
            ApiError::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(0)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    /// Non-429 errors do NOT enter the retry loop — they short-circuit
    /// as `BadStatus`. Guards against accidentally retrying a 500 (which
    /// is usually NOT idempotent on the server's side) just because the
    /// loop body was copy-pasted carelessly.
    #[tokio::test(flavor = "current_thread")]
    async fn rate_limit_retry_does_not_retry_500() {
        let url = spawn_mock_http(vec![
            MockReply {
                status: 500,
                headers: vec![],
                body: "boom".into(),
            };
            5
        ])
        .await;
        let http = http_client(HttpPolicy::LOOPBACK);
        let started = std::time::Instant::now();
        let err =
            send_with_rate_limit_retry(&http, &url, &HeaderMap::new(), &serde_json::json!({}))
                .await
                .expect_err("500 must surface");
        let elapsed = started.elapsed();
        // First-attempt failure: well under one Retry-After cycle.
        assert!(
            elapsed < Duration::from_millis(500),
            "500 was retried (took {elapsed:?})"
        );
        match err {
            ApiError::BadStatus { status, .. } => assert_eq!(status, 500),
            other => panic!("expected BadStatus, got {other:?}"),
        }
    }

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

    /// Adversarial #2: a SINGLE feed call whose buffer contains data
    /// AFTER [DONE] must NOT emit events for that trailing data. The
    /// previous fix only short-circuited the next feed() call; the
    /// while-let line-pop loop kept iterating past the terminator
    /// within the same call.
    #[test]
    fn sse_decoder_stops_iterating_lines_after_done_within_one_feed() {
        let mut d = SseDecoder::new(None);
        // One feed, three records: a delta, the terminator, and a
        // trailing record that must be silently dropped.
        d.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"good\"}}]}\n\n\
              data: [DONE]\n\n\
              data: {\"choices\":[{\"delta\":{\"content\":\"poison\"}}]}\n\n",
        );
        let events: Vec<String> = collect_ok(&mut d)
            .into_iter()
            .map(|e| format!("{e:?}"))
            .collect();
        // First the legitimate delta, then Done. Nothing after.
        let joined = events.join(",");
        assert!(joined.contains("good"), "missing pre-DONE delta: {joined}");
        assert!(joined.contains("Done"), "missing Done: {joined}");
        assert!(
            !joined.contains("poison"),
            "post-DONE delta leaked through: {joined}"
        );
    }

    /// Adversarial: once the decoder has reached terminal state, any
    /// further bytes fed to it must be dropped — not parsed, not
    /// allocated through the line buffer, not surfaced as new events.
    /// Protects against a buggy caller that keeps draining the upstream
    /// stream past the [DONE] terminator.
    #[test]
    fn sse_decoder_drops_bytes_fed_after_done() {
        let mut d = SseDecoder::new(None);
        d.feed(b"data: [DONE]\n\n");
        // Drain the terminal Done.
        let _ = d.next_event();
        // Now slam 1 MB of garbage at it.
        let garbage = vec![b'x'; 1_000_000];
        d.feed(&garbage);
        d.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"poison\"}}]}\n\n");
        // No further events must materialise.
        assert!(d.next_event().is_none(), "post-Done bytes leaked an event");
    }

    /// Adversarial: oversized line (no `\n` within LINE_BUDGET_BYTES)
    /// must surface as a `StreamTruncated` error instead of growing the
    /// buffer without bound. Tests the byte-line buffer's overflow
    /// guard end-to-end through the decoder.
    #[test]
    fn sse_decoder_oversize_line_surfaces_truncation() {
        let mut d = SseDecoder::new(Some("req-overflow".into()));
        // Half the budget, no newline.
        let huge = vec![b'x'; crate::services::sse::LINE_BUDGET_BYTES / 2];
        d.feed(&huge);
        // One more half + 1 byte tips us over the budget.
        let more = vec![b'x'; crate::services::sse::LINE_BUDGET_BYTES / 2 + 1];
        d.feed(&more);
        let err = d
            .next_event()
            .expect("expected truncation after overflow")
            .expect_err("expected Err");
        let rendered = err.to_string();
        assert!(rendered.contains("truncated"), "got {rendered}");
        assert!(
            rendered.contains("LINE_BUDGET")
                || rendered.contains("buffer exceeded")
                || rendered.contains(&format!("{}", crate::services::sse::LINE_BUDGET_BYTES)),
            "expected byte-count in error: {rendered}"
        );
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

    /// Fixture-driven invariance check: a single golden SSE byte-buffer
    /// replayed at chunk sizes 1..=128 must produce identical event
    /// streams. Catches off-by-one bugs in the line buffer that only
    /// surface under specific chunk boundaries (e.g. one byte short of a
    /// `\n`, or the `\n` arriving alone in its own chunk).
    #[test]
    fn sse_decoder_fixture_replay_chunk_sizes_1_through_128() {
        // Realistic xAI-shaped SSE: 3 deltas (one with multi-byte UTF-8),
        // one usage event, terminator. CRLF + LF mixed to stress both.
        let fixture: &[u8] = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"world \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\u{1F600}\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}], \"usage\":{\"prompt_tokens\":4,\"completion_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        ).as_bytes();

        let mut canonical: Option<Vec<String>> = None;
        for chunk_size in 1..=128usize {
            let mut d = SseDecoder::new(None);
            let mut i = 0;
            while i < fixture.len() {
                let end = (i + chunk_size).min(fixture.len());
                d.feed(&fixture[i..end]);
                i = end;
            }
            let events: Vec<String> = collect_ok(&mut d)
                .into_iter()
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
        assert!(canonical.iter().any(|s| s.contains("hello ")));
        assert!(canonical.iter().any(|s| s.contains("world ")));
        assert!(canonical.iter().any(|s| s.contains("\u{1F600}")));
        assert!(canonical.iter().any(|s| s.contains("Done")));
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

    /// Property-tested arbitrary-byte invariants for the OpenAI / xAI
    /// SSE decoder. Same shape as the Anthropic counterpart: any byte
    /// sequence, any chunking, must never panic / hang / over-enqueue,
    /// and post-terminal state stays terminal.
    fn property_sse_decoder_never_panics(input: Vec<u8>, chunk_sz: usize) {
        let chunk = chunk_sz.max(1).min(input.len().max(1));
        let mut d = SseDecoder::new(None);
        for chunk_bytes in input.chunks(chunk) {
            d.feed(chunk_bytes);
        }
        d.eof();
        let mut drained = 0usize;
        while let Some(_e) = d.next_event() {
            drained += 1;
            assert!(drained < 100_000, "decoder over-enqueued events");
        }
        d.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n");
        assert!(d.next_event().is_none(), "post-terminal event leaked");
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 256,
            .. proptest::test_runner::Config::default()
        })]

        #[test]
        fn sse_decoder_never_panics_proptest(
            input in proptest::collection::vec(proptest::num::u8::ANY, 0..2048),
            chunk_sz in 1usize..256,
        ) {
            property_sse_decoder_never_panics(input, chunk_sz);
        }
    }
}
