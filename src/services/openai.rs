//! OpenAI chat completions client.
//!
//! The wire format is identical to xAI's (it's the source format), so we
//! delegate to `XaiClient::with_base` configured against `api.openai.com/v1`.

use crate::error::ApiError;
use crate::models::Provider;
use crate::services::chat::XaiClient;
use crate::services::providers::{ChatProvider, ChatRequest, EventStream};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://api.openai.com/v1";

pub struct OpenAiClient {
    inner: XaiClient,
}

impl OpenAiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            inner: XaiClient::with_base(api_key, DEFAULT_BASE),
        }
    }

    #[allow(dead_code)] // public escape hatch for custom OpenAI-compatible endpoints
    pub fn with_base(api_key: impl Into<String>, base: impl Into<String>) -> Self {
        Self {
            inner: XaiClient::with_base(api_key, base),
        }
    }
}

#[async_trait]
impl ChatProvider for OpenAiClient {
    fn id(&self) -> Provider {
        Provider::OpenAi
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, ApiError> {
        self.inner.stream(req).await
    }
}
