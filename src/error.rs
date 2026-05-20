//! Strongly-typed errors used throughout the application.
//!
//! Surface-area rules:
//! * Anything that escapes a thread/task boundary is converted into one of
//!   the `*Error` enums below so callers never see a raw `anyhow::Error`
//!   upstream.
//! * UI code shows the `Display` impl directly — keep messages short,
//!   lowercase, imperative.
//!
//! Variants are added when a call site actually constructs them, not
//! before. Dead variants used to live here on the theory "we might need
//! `Cancelled` / `NotFound` / `Resample` someday"; they were just text
//! attached to a derive macro that confused readers. Gone.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Db(#[from] redb::Error),
    #[error("transaction error: {0}")]
    Txn(#[from] redb::TransactionError),
    #[error("table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("encode error: {0}")]
    Encode(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("search index error: {0}")]
    Index(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<tantivy::TantivyError> for StorageError {
    fn from(value: tantivy::TantivyError) -> Self {
        StorageError::Index(value.to_string())
    }
}

impl From<tantivy::directory::error::OpenDirectoryError> for StorageError {
    fn from(value: tantivy::directory::error::OpenDirectoryError) -> Self {
        StorageError::Index(value.to_string())
    }
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("missing api key — set one in settings")]
    MissingKey,
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("server returned {status}: {body}")]
    BadStatus { status: u16, body: String },
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    // --- Streaming-specific failure modes ---------------------------------
    // The original API conflated "the provider sent us garbage" with "the
    // stream ended before its terminator" with "the server reported an
    // error event mid-stream". Each is a different fix-it instruction and
    // each deserves its own variant so the UI / logs can be precise.
    #[error("{provider} stream error{request_id}: {message}")]
    ProviderStream {
        provider: &'static str,
        message: String,
        /// `request-id` header from the response, when we captured one.
        /// Pre-formatted as `" (request-id …)"` so the Display impl above
        /// doesn't have to know whether it's present.
        request_id: String,
    },
    /// Stream closed before the provider sent its terminating event
    /// (`message_stop` for Anthropic, `[DONE]` for OpenAI-compatible).
    /// Distinguishes "everything completed cleanly" from "connection
    /// dropped and the UI just got a polite EOF".
    #[error("{provider} stream truncated{request_id}: {message}")]
    StreamTruncated {
        provider: &'static str,
        message: String,
        request_id: String,
    },
}

impl ApiError {
    /// Render an `Option<String>` request-id into the bracketed form
    /// expected by the Display strings above. Keeps the formatting in one
    /// place so it stays consistent across variants.
    pub fn fmt_request_id(id: Option<&str>) -> String {
        match id {
            Some(rid) if !rid.is_empty() => format!(" (request-id {rid})"),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no input device available")]
    NoInput,
    #[error("no output device available")]
    NoOutput,
    #[error("supported config error: {0}")]
    Config(String),
    #[error("build stream error: {0}")]
    BuildStream(String),
    #[error("play error: {0}")]
    Play(String),
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("keyring error: {0}")]
    Keyring(#[from] keyring::Error),
}
