#![allow(dead_code)]
//! Core domain types persisted to redb and rendered by the UI.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub chat_id: Uuid,
    pub role: Role,
    pub content: String,
    pub created_at: DateTime<Utc>,
    /// Token count, populated post-stream where available.
    #[serde(default)]
    pub tokens: Option<u32>,
    /// Provider that authored this assistant message (None for user/system).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model that authored this assistant message.
    #[serde(default)]
    pub model: Option<String>,
}

impl Message {
    pub fn new(chat_id: Uuid, role: Role, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            chat_id,
            role,
            content: content.into(),
            created_at: Utc::now(),
            tokens: None,
            provider: None,
            model: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: Uuid,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Provider key (e.g. "xai", "openai", "anthropic", "local").
    #[serde(default = "Chat::default_provider")]
    pub provider: String,
    /// Model id within that provider.
    #[serde(default = "Chat::default_model")]
    pub model: String,
    #[serde(default)]
    pub temperature: f32,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    /// Optional system prompt scoped to this chat.
    #[serde(default)]
    pub system_prompt: Option<String>,
}

impl Chat {
    pub fn default_provider() -> String {
        "xai".into()
    }
    pub fn default_model() -> String {
        "grok-beta".into()
    }

    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            title: "New Chat".into(),
            created_at: now,
            updated_at: now,
            provider: provider.into(),
            model: model.into(),
            temperature: 0.7,
            pinned: false,
            archived: false,
            system_prompt: None,
        }
    }

    /// Derive a sensible title from the first user message.
    pub fn derive_title(message: &str) -> String {
        let trimmed: String = message
            .lines()
            .next()
            .unwrap_or(message)
            .chars()
            .take(64)
            .collect();
        if trimmed.is_empty() {
            "New Chat".into()
        } else {
            trimmed
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Xai,
    OpenAi,
    Anthropic,
    Local,
}

impl Provider {
    pub fn id(&self) -> &'static str {
        match self {
            Provider::Xai => "xai",
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Local => "local",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Provider::Xai => "xAI Grok",
            Provider::OpenAi => "OpenAI",
            Provider::Anthropic => "Anthropic",
            Provider::Local => "Local",
        }
    }

    pub fn default_model(&self) -> &'static str {
        match self {
            Provider::Xai => "grok-beta",
            Provider::OpenAi => "gpt-4o-mini",
            Provider::Anthropic => "claude-3-5-sonnet-latest",
            Provider::Local => "llama-3.1-8b",
        }
    }

    pub fn all() -> &'static [Provider] {
        &[
            Provider::Xai,
            Provider::OpenAi,
            Provider::Anthropic,
            Provider::Local,
        ]
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VoicePersona {
    Ara,
    Rex,
    Sal,
    Eve,
    Leo,
}

impl VoicePersona {
    pub fn id(&self) -> &'static str {
        match self {
            VoicePersona::Ara => "ara",
            VoicePersona::Rex => "rex",
            VoicePersona::Sal => "sal",
            VoicePersona::Eve => "eve",
            VoicePersona::Leo => "leo",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            VoicePersona::Ara => "warm, friendly female (default)",
            VoicePersona::Rex => "confident, clear male",
            VoicePersona::Sal => "smooth neutral",
            VoicePersona::Eve => "energetic, upbeat female",
            VoicePersona::Leo => "authoritative male",
        }
    }

    pub fn all() -> &'static [VoicePersona] {
        &[
            VoicePersona::Ara,
            VoicePersona::Rex,
            VoicePersona::Sal,
            VoicePersona::Eve,
            VoicePersona::Leo,
        ]
    }
}

/// User-facing settings, persisted to redb under a single key. Secrets are
/// stored separately in the OS keyring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub default_provider: Provider,
    pub xai_model: String,
    pub openai_model: String,
    pub anthropic_model: String,
    pub local_model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub font_size: f32,
    pub theme: ThemeMode,
    pub tts_enabled: bool,
    pub voice_persona: VoicePersona,
    pub system_prompt: Option<String>,
    /// Toggle semantic retrieval over chat history (requires `rag` feature).
    pub rag_enabled: bool,
    /// How many past messages to retrieve as RAG context.
    pub rag_top_k: u32,
    /// Show the in-app performance dashboard.
    pub perf_dashboard: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_provider: Provider::Xai,
            xai_model: "grok-beta".into(),
            openai_model: "gpt-4o-mini".into(),
            anthropic_model: "claude-3-5-sonnet-latest".into(),
            local_model: "llama-3.1-8b".into(),
            temperature: 0.7,
            max_tokens: 4096,
            font_size: 14.0,
            theme: ThemeMode::Cosmic,
            tts_enabled: true,
            voice_persona: VoicePersona::Ara,
            system_prompt: None,
            rag_enabled: false,
            rag_top_k: 4,
            perf_dashboard: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    Cosmic,
    Dark,
    Light,
}

/// A wire-format message used by OpenAI-compatible providers (xAI, OpenAI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMessage {
    pub role: String,
    pub content: String,
}

impl From<&Message> for WireMessage {
    fn from(m: &Message) -> Self {
        Self {
            role: m.role.as_str().to_string(),
            content: m.content.clone(),
        }
    }
}

/// Aggregated lightweight stats used by the perf dashboard.
#[derive(Debug, Default, Clone, Copy)]
pub struct PerfStats {
    pub frame_ms: f32,
    pub fps: f32,
    pub tokens_per_sec: f32,
    pub last_request_ms: u32,
    pub messages_indexed: u64,
    pub mem_bytes: u64,
}
