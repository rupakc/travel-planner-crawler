//! `[policy]`: robots, denylist, SSRF, challenge detection (owner: vc-policy).

use serde::{Deserialize, Serialize};

/// Politeness and compliance policy settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    /// Path of the denylist.
    pub denylist_path: String,
    /// Path of the render sub-resource allowlist.
    pub subresource_allowlist_path: String,
    /// robots.txt cache TTL, seconds (24 h).
    pub robots_ttl_s: u64,
    /// Max robots.txt bytes parsed (500 KiB).
    pub robots_max_bytes: u64,
    /// Keep using the last good robots.txt this many days of 5xx (30).
    pub robots_last_good_days: u32,
    /// AI crawler tokens whose block (while `*` is allowed) means an AI reservation.
    pub ai_bot_tokens: Vec<String>,
    /// Allowed destination ports (80, 443).
    pub allowed_ports: Vec<u16>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            denylist_path: "policy/denylist.yaml".into(),
            subresource_allowlist_path: "policy/subresource_allowlist.yaml".into(),
            robots_ttl_s: 24 * 3600,
            robots_max_bytes: 500 * 1024,
            robots_last_good_days: 30,
            ai_bot_tokens: ["GPTBot", "ClaudeBot", "CCBot", "Google-Extended"]
                .map(String::from)
                .to_vec(),
            allowed_ports: vec![80, 443],
        }
    }
}
