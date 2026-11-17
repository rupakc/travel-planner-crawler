//! `[fetch]`: permit-gated HTTP (owner: vc-fetch).

use serde::{Deserialize, Serialize};

/// Fetch settings. The User-Agent is a constant (`vc_core::USER_AGENT`), not a setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FetchConfig {
    /// Own request timeout, seconds (always clamped to the lease deadline).
    pub request_timeout_s: u64,
    /// Connect timeout, seconds.
    pub connect_timeout_s: u64,
    /// Max redirects (5).
    pub max_redirects: u8,
    /// Decoded byte cap for HTML (5 MB).
    pub html_max_bytes: u64,
    /// Decoded byte cap for other types (10 MB).
    pub other_max_bytes: u64,
    /// Max nested gzip layers decoded.
    pub max_nested_gzip: u8,
    /// Allowed `Content-Type` essences.
    pub allowed_content_types: Vec<String>,
    /// Extra `Content-Type` essences allowed for render sub-requests only (scripts, styles).
    pub subresource_content_types: Vec<String>,
    /// Extra `Content-Type` essences allowed for dump downloads only (zip archives such as
    /// GTFS feeds; the dump adapter checks the archive's own magic bytes).
    pub dump_content_types: Vec<String>,
    /// Daily raw-bytes cap across all keys (3 GB).
    pub daily_raw_bytes_cap: u64,
}

impl Default for FetchConfig {
    fn default() -> Self {
        FetchConfig {
            request_timeout_s: 30,
            connect_timeout_s: 10,
            max_redirects: 5,
            html_max_bytes: 5 * 1024 * 1024,
            other_max_bytes: 10 * 1024 * 1024,
            max_nested_gzip: 2,
            allowed_content_types: [
                "text/html",
                "application/xhtml+xml",
                "application/rss+xml",
                "application/atom+xml",
                "application/rdf+xml",
                "application/xml",
                "text/xml",
                "application/json",
                "application/ld+json",
                "text/csv",
                "text/plain",
                "application/gzip",
                "application/x-gzip",
                "application/x-bzip2",
            ]
            .map(String::from)
            .to_vec(),
            subresource_content_types: [
                "text/css",
                "text/javascript",
                "application/javascript",
                "application/x-javascript",
                "application/ecmascript",
            ]
            .map(String::from)
            .to_vec(),
            dump_content_types: [
                "application/zip",
                "application/x-zip-compressed",
                "application/octet-stream",
            ]
            .map(String::from)
            .to_vec(),
            daily_raw_bytes_cap: 3 * 1024 * 1024 * 1024,
        }
    }
}
