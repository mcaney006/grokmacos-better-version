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
    // Custom Display because reqwest's own error wall-of-text is a
    // production support nightmare: "error sending request for url
    // (http://127.0.0.1:11434/v1/chat/completions): error trying to
    // connect: tcp connect error: Connection refused (os error 111)".
    // We render a concise message keyed off the failure mode (connect
    // failure → "could not reach <host>", timeout → "request timed
    // out", everything else → "http error: <e>") and keep the original
    // accessible via the source chain for the developer log.
    #[error("{}", Self::render_http_error(.0))]
    Http(#[from] reqwest::Error),
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("server returned {status}: {body}")]
    BadStatus { status: u16, body: String },
    /// HTTP 401 / 403: the provider rejected our credentials. Distinct
    /// from BadStatus so the UI can surface a "check your API key in
    /// Settings" toast instead of a generic "server returned 401" wall
    /// of text. Never retried — auth failure isn't transient.
    #[error("authentication failed — check your API key in Settings")]
    AuthFailed { provider: &'static str, status: u16 },
    /// HTTP 5xx: the provider is having an incident. Conceptually
    /// retryable but our retry middleware only handles 429; surfaced
    /// here so the UI can say "Anthropic is down" instead of "server
    /// returned 503".
    #[error("{provider} is unavailable (status {status}) — try again shortly")]
    ProviderUnavailable { provider: &'static str, status: u16 },
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
    /// Render a `reqwest::Error` into a concise, user-facing string.
    /// Connect-refused, DNS, and timeout errors get crisp messages
    /// instead of the multi-clause wall of text reqwest produces by
    /// default. The developer log still has the full chain via the
    /// `#[source]` chain that `thiserror` wires up.
    fn render_http_error(e: &reqwest::Error) -> String {
        if e.is_connect() {
            let host = e
                .url()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_else(|| "endpoint".to_owned());
            format!("could not reach {host} — is it running?")
        } else if e.is_timeout() {
            "request timed out".to_owned()
        } else if e.is_request() {
            format!("request build error: {e}")
        } else if e.is_decode() {
            format!("response decode error: {e}")
        } else {
            format!("http error: {e}")
        }
    }

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

    /// keyring::Error::NoStorageAccess maps to our `AccessDenied`
    /// variant — callers shouldn't need to know about keyring's
    /// internal taxonomy. Guards against a keyring-crate variant
    /// rename silently turning AccessDenied into a generic Backend
    /// failure, which would lose UX information (user can be told
    /// "unlock your keychain" vs. "your platform is broken").
    #[test]
    fn secret_error_maps_keyring_variants_to_stable_surface() {
        // PlatformFailure with an arbitrary wrapped error must land in
        // Backend(details=...).
        let pf = keyring::Error::PlatformFailure(Box::new(std::io::Error::other(
            "synthetic test failure",
        )));
        match SecretError::from(pf) {
            SecretError::Backend { details } => {
                assert!(details.contains("synthetic test failure"), "{details}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
        // BadEncoding similarly lands in Backend.
        match SecretError::from(keyring::Error::BadEncoding(vec![0xFFu8, 0xFE])) {
            SecretError::Backend { .. } => {}
            other => panic!("expected Backend, got {other:?}"),
        }
        // Display strings are stable and don't accidentally include
        // "keyring::" — that would leak the crate name into the UI.
        assert!(!format!("{}", SecretError::NoBackend).contains("keyring::"));
        assert!(!format!("{}", SecretError::AccessDenied).contains("keyring::"));
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

/// Failure modes for the OS keyring integration. The variants are
/// intentionally crate-defined rather than `#[from] keyring::Error` so
/// the keyring crate's internal taxonomy doesn't leak into our public
/// API surface — pattern matching on `keyring::Error::PlatformFailure(_)`
/// from a downstream caller would break every time we bump the keyring
/// crate. The `Backend` variant carries the original error's display
/// string for debugging.
#[derive(Debug, Error)]
pub enum SecretError {
    /// Keyring backend not available (Linux: no Secret Service running;
    /// embedded / headless distributions). Distinct from `Backend(_)`
    /// so callers can degrade gracefully (e.g., prompt the user to
    /// re-enter the key per launch).
    #[error("no usable keyring backend on this platform")]
    NoBackend,
    /// Entry locked / access denied by the OS (macOS Keychain still
    /// locked, Windows user cancelled the prompt). Retryable in
    /// principle — the user can unlock and try again.
    #[error("keyring access denied or locked")]
    AccessDenied,
    /// Any other keyring failure. The backend error is rendered into
    /// `details` for logs; we deliberately do NOT expose the
    /// `keyring::Error` enum so its variants are free to change.
    #[error("keyring backend error: {details}")]
    Backend { details: String },
}

impl From<keyring::Error> for SecretError {
    fn from(value: keyring::Error) -> Self {
        // Map the keyring crate's variants into our smaller surface.
        // Anything we don't recognise lands in `Backend` with the
        // original Display for debugging. `NoEntry` is NOT a
        // SecretError — it's `Ok(None)` at the call site.
        match value {
            keyring::Error::PlatformFailure(ref e) => SecretError::Backend {
                details: format!("platform failure: {e}"),
            },
            keyring::Error::NoStorageAccess(_) => SecretError::AccessDenied,
            keyring::Error::Invalid(_, _) | keyring::Error::BadEncoding(_) => {
                SecretError::Backend {
                    details: value.to_string(),
                }
            }
            other => SecretError::Backend {
                details: other.to_string(),
            },
        }
    }
}
