//! Local OpenAI-compatible endpoint (Ollama, LM Studio, llama.cpp server, etc.).
//!
//! Most local runners expose an `/v1/chat/completions` endpoint compatible
//! with OpenAI's wire format. Default base is `http://127.0.0.1:11434/v1`
//! (Ollama).

use crate::error::ApiError;
use crate::models::Provider;
use crate::services::chat::XaiClient;
use crate::services::providers::{ChatProvider, ChatRequest, EventStream};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "http://127.0.0.1:11434/v1";

pub struct LocalClient {
    inner: XaiClient,
}

impl LocalClient {
    pub fn new(base_or_key: impl Into<String>) -> Self {
        // For local endpoints the "API key" field doubles as the base URL.
        // If the user typed a URL we use it as the base; otherwise we treat
        // the input as an auth token against the default Ollama URL.
        let raw = base_or_key.into();
        let (base, key) = if raw.starts_with("http://") || raw.starts_with("https://") {
            (raw, "local".to_string())
        } else if raw.is_empty() {
            (DEFAULT_BASE.to_string(), "local".to_string())
        } else {
            (DEFAULT_BASE.to_string(), raw)
        };
        Self {
            inner: XaiClient::with_base(key, base),
        }
    }
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
