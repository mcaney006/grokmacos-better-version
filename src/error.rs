#![allow(dead_code)]
//! Strongly-typed errors used throughout the application.
//!
//! Surface-area rules:
//! * Anything that escapes a thread/task boundary is converted into one of the
//!   `*Error` enums below so callers never see a raw `anyhow::Error` upstream.
//! * UI code shows the `Display` impl directly — keep messages short, lowercase,
//!   imperative.

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
    #[error("not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<bincode::Error> for StorageError {
    fn from(value: bincode::Error) -> Self {
        StorageError::Decode(value.to_string())
    }
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
    #[error("decoding stream: {0}")]
    StreamDecode(String),
    #[error("request was cancelled")]
    Cancelled,
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no input device available")]
    NoInput,
    #[error("no output device available")]
    NoOutput,
    #[error("device error: {0}")]
    Device(String),
    #[error("supported config error: {0}")]
    Config(String),
    #[error("build stream error: {0}")]
    BuildStream(String),
    #[error("play error: {0}")]
    Play(String),
    #[error("resample error: {0}")]
    Resample(String),
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("keyring error: {0}")]
    Keyring(#[from] keyring::Error),
}
