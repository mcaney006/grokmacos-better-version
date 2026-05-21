//! Tiny shared building block for the two SSE-driven providers
//! (OpenAI-compatible + Anthropic). Specifically: **byte-buffered**
//! line splitter that hands one decoded `String` to the caller per
//! `\n`-terminated input line.
//!
//! Why this exists instead of an off-the-shelf `tokio-stream`-based SSE
//! crate: our needs are narrow (we don't use named events, retry, or
//! reconnect IDs), and we want to keep the decode boundary at lines so
//! a chunk that lands mid-codepoint can never corrupt a streamed token.
//! That single guarantee is the entire point of this module.
//!
//! Usage:
//! ```ignore
//! let mut split = LineByteBuffer::default();
//! split.extend(&network_chunk);
//! while let Some(line) = split.take_line() {
//!     // `line` is owned, UTF-8-safe per the contract below.
//! }
//! ```

/// Maximum amount of *unterminated* line data we'll buffer before
/// declaring the upstream malformed. A well-formed SSE stream sends
/// `\n` at least once per event; if we go this far without one, something
/// is wrong and we'd rather fail loudly than grow RAM forever.
pub const LINE_BUDGET_BYTES: usize = 4 * 1024 * 1024;

/// Outcome of `LineByteBuffer::extend` when the budget is exceeded.
/// Callers should treat this as a hard error — there's nothing useful
/// left to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferStatus {
    Ok,
    Overflow,
}

/// Byte-level line buffer. Holds bytes between calls to `extend`, hands
/// out complete `String` lines via `take_line`. Lines are split on `\n`
/// (0x0A), with a trailing `\r` (CRLF) stripped automatically. UTF-8
/// decoding happens **per line** via `String::from_utf8_lossy`, which
/// is safe because `\n` is an ASCII byte that cannot appear inside a
/// multi-byte UTF-8 codepoint — splitting on it never bisects a
/// character.
#[derive(Default)]
pub struct LineByteBuffer {
    buf: Vec<u8>,
}

impl LineByteBuffer {
    /// Append bytes to the buffer. Returns `BufferStatus::Overflow` if
    /// the buffer exceeded `LINE_BUDGET_BYTES` after the append. The
    /// caller decides what to do — typically: stop reading the stream,
    /// surface an error to the user.
    pub fn extend(&mut self, bytes: &[u8]) -> BufferStatus {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > LINE_BUDGET_BYTES {
            self.buf.clear();
            BufferStatus::Overflow
        } else {
            BufferStatus::Ok
        }
    }

    /// Pop the next complete line. Returns `None` if no `\n` is in the
    /// buffer yet (caller should `extend` more bytes and try again).
    /// The returned string never includes the terminating `\n` or the
    /// optional preceding `\r`.
    pub fn take_line(&mut self) -> Option<String> {
        let idx = self.buf.iter().position(|&b| b == b'\n')?;
        let line_bytes: Vec<u8> = self.buf.drain(..=idx).collect();
        // Strip trailing \n (and \r if present) before decoding.
        let end = line_bytes
            .len()
            .saturating_sub(if line_bytes.ends_with(b"\r\n") { 2 } else { 1 });
        Some(String::from_utf8_lossy(&line_bytes[..end]).into_owned())
    }

    /// True iff there are pending bytes that haven't formed a complete
    /// line yet. Useful for "did the stream end cleanly" checks at EOF.
    /// `pub` because it's exposed across the SSE primitive API surface,
    /// `#[allow(dead_code)]` because the only current caller is the
    /// internal test suite — that's fine, we don't want to fight clippy
    /// over an obviously-useful invariant probe.
    #[allow(dead_code)]
    pub fn has_pending(&self) -> bool {
        !self.buf.is_empty()
    }
}

/// Privacy-preserving payload fingerprint for log correlation.
///
/// SSE event payloads are content-laden — the model's response tokens
/// land verbatim inside `data: {...}` lines. Logging the raw payload
/// on a parse failure (or any other log event) sends user content
/// into whatever log pipeline aggregates this process's stdout. We
/// log this fingerprint instead: a short hex string derived from a
/// keyed hash of the payload bytes. Two log lines with the same
/// fingerprint were caused by the same payload; the fingerprint
/// reveals nothing about the contents.
///
/// Uses `std::hash::DefaultHasher` (SipHash) keyed by a randomised
/// process-startup seed so fingerprints aren't comparable across
/// processes — a leaked log can't be cross-referenced with a known
/// payload. Returns a 16-hex-char string.
pub fn payload_fingerprint(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    PROCESS_SALT.with(|salt| salt.hash(&mut h));
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

thread_local! {
    /// Per-process random salt used as a SipHash key for
    /// `payload_fingerprint`. Initialised once per thread on first
    /// access from `std::time::SystemTime` nanoseconds + thread id —
    /// not cryptographically strong against an active attacker, but
    /// enough to prevent log-fingerprint reuse across deployments.
    static PROCESS_SALT: u64 = {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        std::process::id().hash(&mut h);
        if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            d.as_nanos().hash(&mut h);
        }
        std::thread::current().id().hash(&mut h);
        h.finish()
    };
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_lf() {
        let mut b = LineByteBuffer::default();
        b.extend(b"alpha\nbeta\n");
        assert_eq!(b.take_line().unwrap(), "alpha");
        assert_eq!(b.take_line().unwrap(), "beta");
        assert!(b.take_line().is_none());
    }

    #[test]
    fn handles_crlf() {
        let mut b = LineByteBuffer::default();
        b.extend(b"alpha\r\nbeta\r\n");
        assert_eq!(b.take_line().unwrap(), "alpha");
        assert_eq!(b.take_line().unwrap(), "beta");
    }

    #[test]
    fn buffers_partial_line_until_terminator() {
        let mut b = LineByteBuffer::default();
        b.extend(b"par");
        assert!(b.take_line().is_none());
        b.extend(b"tial\n");
        assert_eq!(b.take_line().unwrap(), "partial");
    }

    #[test]
    fn handles_utf8_split_across_extend_calls() {
        // 🦀 = F0 9F A6 80. Split inside the codepoint.
        let mut b = LineByteBuffer::default();
        b.extend(&[0xF0, 0x9F]);
        b.extend(&[0xA6, 0x80, b'\n']);
        let line = b.take_line().unwrap();
        assert!(line.contains('🦀'));
        assert!(!line.contains('\u{FFFD}'));
    }

    #[test]
    fn empty_line_is_returned() {
        let mut b = LineByteBuffer::default();
        b.extend(b"\n");
        assert_eq!(b.take_line().unwrap(), "");
    }

    #[test]
    fn budget_overflow_clears_buffer() {
        let mut b = LineByteBuffer::default();
        let huge = vec![b'x'; LINE_BUDGET_BYTES + 1];
        assert_eq!(b.extend(&huge), BufferStatus::Overflow);
        assert!(!b.has_pending());
    }

    #[test]
    fn budget_ok_below_limit() {
        let mut b = LineByteBuffer::default();
        let payload = vec![b'x'; 1024];
        assert_eq!(b.extend(&payload), BufferStatus::Ok);
        assert!(b.has_pending());
    }

    /// Fingerprint property 1: deterministic within a single process
    /// (same thread). Used by log readers to group repeated parse
    /// failures of the same payload.
    #[test]
    fn payload_fingerprint_is_deterministic_within_process() {
        let body = b"data: {\"hello\": \"world\"}\n";
        let fp1 = payload_fingerprint(body);
        let fp2 = payload_fingerprint(body);
        assert_eq!(fp1, fp2, "same input must hash to same fingerprint");
        assert_eq!(fp1.len(), 16, "fingerprint should be 16 hex chars");
    }

    /// Fingerprint property 2: different inputs hash to different
    /// outputs (with overwhelming probability). One-bit difference is
    /// the most aggressive check we can do without statistical tests.
    #[test]
    fn payload_fingerprint_differs_for_different_inputs() {
        let fp_a = payload_fingerprint(b"alpha");
        let fp_b = payload_fingerprint(b"beta");
        let fp_a2 = payload_fingerprint(b"alphb"); // single-byte change
        assert_ne!(fp_a, fp_b);
        assert_ne!(fp_a, fp_a2);
    }

    /// Fingerprint property 3: the output is hex digits only — no
    /// payload bytes ever appear in the fingerprint. Defence against
    /// a future bug that pastes the input back into the output.
    #[test]
    fn payload_fingerprint_emits_only_hex_digits() {
        let secret = b"the user typed their password here: hunter2";
        let fp = payload_fingerprint(secret);
        assert!(
            fp.chars().all(|c| c.is_ascii_hexdigit()),
            "fingerprint must be pure hex, got: {fp}"
        );
        assert!(
            !fp.as_bytes().windows(7).any(|w| w == b"hunter2"),
            "fingerprint must not contain the literal input"
        );
    }
}
