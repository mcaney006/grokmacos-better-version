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
    /// HTTP 429 from the provider. Distinct from `BadStatus` so callers
    /// can apply backoff instead of surfacing a generic "server returned
    /// 429" error. `retry_after` is parsed from the `Retry-After` header
    /// when present (seconds-form only — HTTP-date form is ignored as
    /// providers don't use it for rate limiting in practice).
    #[error("rate limited{retry_hint}")]
    RateLimited {
        retry_after: Option<std::time::Duration>,
        /// Pre-formatted `" (retry after Ns)"` or empty. Display-only.
        retry_hint: String,
    },
}

impl ApiError {
    /// Parse the `Retry-After` header value (RFC 7231 seconds-form only)
    /// into a `Duration`. HTTP-date form returns `None` because providers
    /// don't use it for rate limiting in practice.
    pub fn parse_retry_after(value: &str) -> Option<std::time::Duration> {
        value
            .trim()
            .parse::<u64>()
            .ok()
            .map(std::time::Duration::from_secs)
    }

    /// Build the rendered `retry_hint` string for the `RateLimited` display.
    pub fn fmt_retry_hint(retry_after: Option<std::time::Duration>) -> String {
        retry_after.map_or_else(String::new, |d| {
            let secs = d.as_secs();
            format!(" (retry after {secs}s)")
        })
    }

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_retry_after_seconds_form() {
        assert_eq!(
            ApiError::parse_retry_after("5"),
            Some(std::time::Duration::from_secs(5))
        );
        assert_eq!(
            ApiError::parse_retry_after("  12  "),
            Some(std::time::Duration::from_secs(12))
        );
    }

    #[test]
    fn parse_retry_after_rejects_http_date() {
        // HTTP-date form: must not be misinterpreted as 0 seconds.
        assert_eq!(
            ApiError::parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"),
            None
        );
        assert_eq!(ApiError::parse_retry_after(""), None);
        assert_eq!(ApiError::parse_retry_after("-1"), None);
    }

    #[test]
    fn rate_limited_display_includes_retry_hint() {
        let err = ApiError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(3)),
            retry_hint: ApiError::fmt_retry_hint(Some(std::time::Duration::from_secs(3))),
        };
        let s = err.to_string();
        assert!(s.contains("rate limited"), "{s}");
        assert!(s.contains("retry after 3s"), "{s}");
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
