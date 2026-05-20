//! Local OpenAI-compatible endpoint (Ollama, LM Studio, llama.cpp server, etc.).
//!
//! Most local runners expose an `/v1/chat/completions` endpoint compatible
//! with OpenAI's wire format. Default base is `http://127.0.0.1:11434/v1`
//! (Ollama).
//!
//! Unlike the cloud providers, this one explicitly opts out of the
//! workspace-wide `https_only(true)` reqwest setting — Ollama and friends
//! talk plain HTTP on loopback. Without this opt-out, every request fails
//! before it leaves the process with reqwest's "URL scheme is not allowed"
//! error.

use crate::error::ApiError;
use crate::models::Provider;
use crate::services::chat::{HttpPolicy, XaiClient};
use crate::services::providers::{ChatProvider, ChatRequest, EventStream};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "http://127.0.0.1:11434/v1";

pub struct LocalClient {
    inner: XaiClient,
}

impl LocalClient {
    pub fn new(base_or_key: impl Into<String>) -> Self {
        // The settings UI reuses the "API key" field for the local
        // endpoint URL because local runners don't typically authenticate.
        // Parse what we got:
        //   * starts with http:// or https://  -> treat as base URL
        //   * empty                            -> default Ollama URL
        //   * looks like a host:port           -> prepend http:// and use as base
        //   * anything else                    -> treat as auth token against default base
        let raw = base_or_key.into();
        let (base, key) = if raw.starts_with("http://") || raw.starts_with("https://") {
            (raw, "local".to_string())
        } else if raw.is_empty() {
            (DEFAULT_BASE.to_string(), "local".to_string())
        } else if looks_like_authority(&raw) {
            (format!("http://{raw}/v1"), "local".to_string())
        } else {
            (DEFAULT_BASE.to_string(), raw)
        };
        Self {
            // `LOOPBACK` is what un-breaks `http://127.0.0.1` reqwest calls.
            // Cloud clients keep `STRICT`; this one and only this one is
            // allowed to send plain HTTP.
            inner: XaiClient::with_base_and_policy(key, base, HttpPolicy::LOOPBACK),
        }
    }
}

/// Cheap heuristic: does `s` look like a `host:port` or `host` we should
/// treat as a URL authority rather than an auth token? We accept anything
/// that contains `:` or starts with a digit, IPv4-looking, or `localhost`.
/// Bare hostnames without ports also count.
fn looks_like_authority(s: &str) -> bool {
    if s.contains(':') {
        return true;
    }
    if s == "localhost" {
        return true;
    }
    // IPv4-looking: starts with a digit and contains dots.
    s.chars().next().is_some_and(|c| c.is_ascii_digit()) && s.contains('.')
}

#[async_trait]
impl ChatProvider for LocalClient {
    fn id(&self) -> Provider {
        Provider::Local
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, ApiError> {
        self.inner.stream(req).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_authority_classifier() {
        assert!(looks_like_authority("127.0.0.1:11434"));
        assert!(looks_like_authority("localhost"));
        assert!(looks_like_authority("localhost:8080"));
        assert!(looks_like_authority("192.168.1.10"));
        assert!(!looks_like_authority("sk-xxxxxx")); // looks like an OpenAI token
        assert!(!looks_like_authority("just-a-key"));
    }

    /// Construction smoke test — proves the constructor doesn't panic on
    /// any of the realistic input shapes.
    #[test]
    fn local_client_builds_for_every_input_shape() {
        let _ = LocalClient::new("");
        let _ = LocalClient::new("http://localhost:11434/v1");
        let _ = LocalClient::new("https://example.com/v1");
        let _ = LocalClient::new("127.0.0.1:11434");
        let _ = LocalClient::new("localhost");
        let _ = LocalClient::new("some-random-token");
    }
}
