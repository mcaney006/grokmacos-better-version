//! Local OpenAI-compatible endpoint (Ollama, LM Studio, llama.cpp server, etc.).
//!
//! Most local runners expose an `/v1/chat/completions` endpoint compatible
//! with OpenAI's wire format. Default base is `http://127.0.0.1:11434/v1`
//! (Ollama).
//!
//! ## Security boundary
//!
//! The "Local" provider is the only one in this crate that:
//!   1. Is allowed to talk plain HTTP (via `HttpPolicy::LOOPBACK`).
//!   2. Accepts an arbitrary user-supplied URL as the endpoint.
//!
//! Those two properties combined are an exfiltration footgun: a user
//! (or social-engineering attacker who convinces a user) who pastes
//! `https://attacker.example/v1` into the "Local endpoint" field will
//! ship every prompt and response over the wire to that host. The
//! field is a free-text URL, not a dropdown, so paste-mistake +
//! malicious-config attacks are real.
//!
//! We enforce a host allowlist instead of relying on a doc comment:
//!
//!   * **Default**: only loopback (127.0.0.0/8, ::1, `localhost`) is
//!     accepted. Anything else returns `ApiError::InvalidResponse`
//!     at first request with an explanatory message and the prompt
//!     never leaves the process.
//!   * **Opt-in for LAN runs**: setting `GROK_INSANE_ALLOW_LAN_LOCAL=1`
//!     in the environment also accepts RFC 1918 / RFC 4193 private
//!     addresses (10/8, 172.16/12, 192.168/16, fc00::/7) and
//!     `*.local` mDNS hostnames. Still no public DNS. Set this
//!     yourself, deliberately, and only if your local model is on
//!     another machine on your LAN.
//!   * **Public hosts**: never accepted from this provider. Use the
//!     OpenAI provider (which expects a key + HTTPS) for cloud
//!     endpoints. There is no escape hatch — pasting `api.openai.com`
//!     here is always a misconfiguration.
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
use std::net::IpAddr;

const DEFAULT_BASE: &str = "http://127.0.0.1:11434/v1";

/// Env var that opts the user in to RFC 1918 / link-local / `*.local`
/// targets in addition to loopback. Public-internet hosts are still
/// rejected.
const LAN_OPT_IN_ENV: &str = "GROK_INSANE_ALLOW_LAN_LOCAL";

pub struct LocalClient {
    inner: XaiClient,
    /// Pre-validated base URL. None if the input failed the host
    /// allowlist — in that case `stream()` returns an explanatory
    /// `InvalidResponse` before any network I/O.
    validation_error: Option<String>,
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
        // Validate the resolved base against the host allowlist
        // BEFORE handing it to reqwest. We don't refuse to construct
        // — that would silently break the UI — instead we stash the
        // validation error and surface it on the first `stream()`
        // call so the user sees a toast.
        let lan_opt_in = std::env::var(LAN_OPT_IN_ENV)
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        let validation_error = validate_local_base(&base, lan_opt_in).err();
        Self {
            // `LOOPBACK` is what un-breaks `http://127.0.0.1` reqwest calls.
            // Cloud clients keep `STRICT`; this one and only this one is
            // allowed to send plain HTTP.
            inner: XaiClient::with_base_and_policy(key, base, HttpPolicy::LOOPBACK),
            validation_error,
        }
    }
}

/// Reject any local-provider URL whose host is not loopback (or, with
/// opt-in, an RFC 1918 / link-local / `*.local` address). The check
/// has two pieces:
///   1. Parse the URL — malformed URLs are rejected up front.
///   2. Classify the host. IP literals are checked numerically;
///      DNS names are checked against a tiny allowlist (`localhost`,
///      `*.local`) because we can't safely DNS-resolve here without
///      introducing a TOCTOU between resolve and connect.
fn validate_local_base(base: &str, lan_opt_in: bool) -> Result<(), String> {
    let url =
        reqwest::Url::parse(base).map_err(|e| format!("local endpoint URL is malformed: {e}"))?;
    let Some(host) = url.host() else {
        return Err("local endpoint URL has no host component".to_owned());
    };
    match host {
        url::Host::Ipv4(ip) => classify_ipv4(ip, lan_opt_in),
        url::Host::Ipv6(ip) => classify_ipv6(ip, lan_opt_in),
        url::Host::Domain(name) => classify_domain(name, lan_opt_in),
    }
}

fn classify_ipv4(ip: std::net::Ipv4Addr, lan_opt_in: bool) -> Result<(), String> {
    if ip.is_loopback() {
        return Ok(());
    }
    if lan_opt_in && (ip.is_private() || ip.is_link_local()) {
        return Ok(());
    }
    Err(reject_msg(IpAddr::V4(ip).to_string(), lan_opt_in))
}

fn classify_ipv6(ip: std::net::Ipv6Addr, lan_opt_in: bool) -> Result<(), String> {
    if ip.is_loopback() {
        return Ok(());
    }
    // ULA = fc00::/7. Stable across Rust versions without unstable APIs:
    // the high octet has its top two bits as 0b1111110.
    let ula = (ip.octets()[0] & 0xfe) == 0xfc;
    // Link-local fe80::/10. is_unicast_link_local is stable in 1.84+.
    let link_local = (ip.octets()[0] == 0xfe) && (ip.octets()[1] & 0xc0 == 0x80);
    if lan_opt_in && (ula || link_local) {
        return Ok(());
    }
    Err(reject_msg(IpAddr::V6(ip).to_string(), lan_opt_in))
}

fn classify_domain(name: &str, lan_opt_in: bool) -> Result<(), String> {
    let lower = name.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Ok(());
    }
    if lan_opt_in && lower.ends_with(".local") {
        // mDNS link-local domain. Distinct from .localhost — used by
        // Bonjour / Avahi for LAN devices.
        return Ok(());
    }
    Err(reject_msg(name.to_owned(), lan_opt_in))
}

fn reject_msg(host: String, lan_opt_in: bool) -> String {
    if lan_opt_in {
        format!(
            "local endpoint host {host:?} is not loopback or RFC1918 / link-local; \
             the Local provider refuses to send prompts to public hosts. \
             Use the OpenAI provider with an HTTPS endpoint instead."
        )
    } else {
        format!(
            "local endpoint host {host:?} is not loopback. The Local provider only \
             talks to 127.0.0.0/8, ::1, or localhost by default; \
             set {LAN_OPT_IN_ENV}=1 to also allow RFC1918 / mDNS *.local hosts."
        )
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
        // SSRF guard: if construction-time validation rejected the host,
        // return the error here instead of letting reqwest fire the
        // request. This is the actual security control — the comment
        // above used to assert "loopback only" without enforcing it.
        if let Some(msg) = &self.validation_error {
            return Err(ApiError::InvalidResponse(msg.clone()));
        }
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

    /// Loopback hosts (the realistic case) are always accepted.
    #[test]
    fn validate_local_base_accepts_loopback() {
        assert!(validate_local_base("http://127.0.0.1:11434/v1", false).is_ok());
        assert!(validate_local_base("http://[::1]:11434/v1", false).is_ok());
        assert!(validate_local_base("http://localhost:11434/v1", false).is_ok());
        assert!(validate_local_base("http://foo.localhost/", false).is_ok());
        // Loopback works with HTTPS too (some local runners present a
        // self-signed cert on loopback).
        assert!(validate_local_base("https://127.0.0.1:8443/v1", false).is_ok());
    }

    /// Public-internet hosts NEVER pass. This is the SSRF/exfiltration
    /// guard. Defense in depth against a user pasting a malicious URL
    /// into the Local field.
    #[test]
    fn validate_local_base_rejects_public_hosts_unconditionally() {
        for base in [
            "https://api.openai.com/v1",
            "https://attacker.example/v1",
            "http://8.8.8.8/v1",
            "http://1.1.1.1:80/",
            "https://example.com:443/v1",
        ] {
            // Even with LAN opt-in: public hosts must be rejected.
            assert!(
                validate_local_base(base, true).is_err(),
                "{base} should be rejected even with LAN opt-in"
            );
            assert!(
                validate_local_base(base, false).is_err(),
                "{base} should be rejected without LAN opt-in"
            );
        }
    }

    /// RFC 1918 / link-local require explicit opt-in.
    #[test]
    fn validate_local_base_rfc1918_requires_opt_in() {
        for base in [
            "http://192.168.1.10:11434/v1",
            "http://10.0.0.5/v1",
            "http://172.16.0.1:8080/v1",
            "http://workstation.local:11434/v1",
        ] {
            assert!(
                validate_local_base(base, false).is_err(),
                "{base} must be rejected without opt-in"
            );
            assert!(
                validate_local_base(base, true).is_ok(),
                "{base} must be allowed with LAN opt-in"
            );
        }
    }

    /// IPv6 link-local + ULA also require opt-in.
    #[test]
    fn validate_local_base_ipv6_lan_requires_opt_in() {
        // fd00::/8 ULA, fe80::/10 link-local. Bracket-form per RFC 3986.
        for base in ["http://[fd00::1]:11434/", "http://[fe80::1]:11434/"] {
            assert!(validate_local_base(base, false).is_err());
            assert!(validate_local_base(base, true).is_ok());
        }
    }

    /// Malformed URLs surface as Err, not as a panic, and not as a
    /// silent network request to wherever reqwest happens to interpret
    /// the garbage.
    #[test]
    fn validate_local_base_rejects_malformed_urls() {
        assert!(validate_local_base("not a url", false).is_err());
        assert!(validate_local_base("http://", false).is_err());
        assert!(validate_local_base("://noscheme", false).is_err());
    }

    /// Reject error message includes the explanation of how to opt in
    /// (without it the user has no path forward).
    #[test]
    fn reject_message_explains_opt_in_when_locked_down() {
        let err = validate_local_base("http://192.168.1.10/v1", false).unwrap_err();
        assert!(err.contains(LAN_OPT_IN_ENV), "{err}");
        assert!(err.contains("loopback"), "{err}");
    }

    /// `stream()` must refuse to fire when validation rejected the
    /// host, returning an `ApiError::InvalidResponse` (which the UI
    /// renders as a toast) instead of letting reqwest dispatch a
    /// real request to the external host.
    #[tokio::test]
    async fn stream_refuses_to_send_when_host_is_external() {
        let client = LocalClient::new("https://attacker.example/v1");
        let req = ChatRequest {
            model: "doesnt-matter".to_owned(),
            messages: vec![],
            temperature: 0.0,
            max_tokens: None,
            system_prompt: None,
        };
        let err = client.stream(req).await.err().expect("must reject");
        match err {
            ApiError::InvalidResponse(msg) => {
                assert!(msg.contains("attacker.example"), "{msg}");
            }
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }
}
