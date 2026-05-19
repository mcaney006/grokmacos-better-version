#![allow(dead_code)]
//! Trait describing a chat-completion provider (xAI, OpenAI, Anthropic, local).
//!
//! Each provider knows how to:
//! 1. Build a streaming HTTP request from a list of `WireMessage`s.
//! 2. Decode the provider-specific SSE payload into token deltas.
//!
//! The xAI provider lives in `services::chat::xai`. Others are stubbed and
//! return `ApiError::InvalidResponse("unimplemented")` until wired up, but the
//! trait + factory let the UI route messages without changes.

use crate::error::ApiError;
use crate::models::{Provider, WireMessage};
use async_trait::async_trait;
use futures_util::Stream;
use std::pin::Pin;

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<WireMessage>,
    pub temperature: f32,
    pub max_tokens: Option<u32>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// A delta of generated text. Concatenate to form the full response.
    Delta(String),
    /// Server reported token usage. Emitted at most once, typically last.
    Usage { input: u32, output: u32 },
    /// Stream completed cleanly.
    Done,
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<ChatEvent, ApiError>> + Send + 'static>>;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    fn id(&self) -> Provider;
    async fn stream(&self, req: ChatRequest) -> Result<EventStream, ApiError>;
}
