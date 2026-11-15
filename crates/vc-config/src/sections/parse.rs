//! `[parse]`: content handler limits (owner: vc-parse).

use serde::{Deserialize, Serialize};

/// Parser limits (mirrors `vc_core::parse::Limits`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParseConfig {
    /// Max DOM depth.
    pub max_dom_depth: u32,
    /// Max DOM nodes.
    pub max_nodes: u32,
    /// Max main-text bytes.
    pub max_text_bytes: usize,
    /// Max links per page.
    pub max_links: usize,
    /// Max sitemap entries per file.
    pub max_sitemap_entries: usize,
    /// CPU time per capture, ms.
    pub cpu_time_ms: u64,
    /// Below this many characters of main text an HTML page is flagged for rendering (auto).
    pub render_min_text_chars: usize,
}

impl Default for ParseConfig {
    fn default() -> Self {
        ParseConfig {
            max_dom_depth: 256,
            max_nodes: 200_000,
            max_text_bytes: 1024 * 1024,
            max_links: 5_000,
            max_sitemap_entries: 50_000,
            cpu_time_ms: 5_000,
            render_min_text_chars: 200,
        }
    }
}
