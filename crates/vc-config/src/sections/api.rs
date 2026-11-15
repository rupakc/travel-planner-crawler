//! `[api]`: the internal `/v1` read API (owner: vc-api).

use serde::{Deserialize, Serialize};

/// API settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// TLS listen address (internal IP only in production).
    pub bind: String,
    /// Metrics listen address (localhost only).
    pub metrics_bind: String,
    /// Secret holding the TLS certificate (PEM).
    pub tls_cert_secret: String,
    /// Secret holding the TLS private key (PEM).
    pub tls_key_secret: String,
    /// Default per-client rate limit, requests/minute.
    pub rate_per_min: u32,
    /// Max page size.
    pub max_page: u32,
    /// Excerpt length for `facts_only` items (≤ 300).
    pub excerpt_chars: usize,
    /// Request timeout, seconds.
    pub request_timeout_s: u64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig {
            bind: "0.0.0.0:8443".into(),
            metrics_bind: "127.0.0.1:9100".into(),
            tls_cert_secret: "crawler-api-tls-cert".into(),
            tls_key_secret: "crawler-api-tls-key".into(),
            rate_per_min: 600,
            max_page: 100,
            excerpt_chars: 300,
            request_timeout_s: 10,
        }
    }
}
