//! `[render]`: Chromium via CDP interception (owner: vc-render).

use serde::{Deserialize, Serialize};

/// Render settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderConfig {
    /// Rendering on/off.
    pub enabled: bool,
    /// CDP endpoint of the headless-shell container (internal `render` network only).
    pub cdp_url: String,
    /// Max concurrent tabs (1: the 4 GB VM renders one page at a time).
    pub max_tabs: u32,
    /// Render budget, seconds (90).
    pub budget_s: u64,
    /// Max sub-requests per render (60).
    pub max_subrequests: u32,
    /// Max decoded bytes per render (20 MB).
    pub max_bytes: u64,
    /// Network idle window, ms (1000).
    pub network_idle_ms: u64,
    /// Per-key sub-resource gap, ms (250).
    pub subresource_gap_ms: u64,
    /// Sub-resource cache TTL, hours (24).
    pub subresource_cache_ttl_h: u64,
    /// Sub-resource cache directory (reserved: the v1 cache is in memory, see
    /// `subresource_cache_max_bytes`).
    pub cache_dir: String,
    /// Max body bytes held by the in-memory, content-addressed sub-resource cache (64 MiB).
    pub subresource_cache_max_bytes: u64,
    /// Re-establish the CDP session after this many renders (leak containment, 50; each render
    /// already gets its own browser context, disposed afterwards).
    pub restart_after_renders: u32,
}

impl Default for RenderConfig {
    fn default() -> Self {
        RenderConfig {
            enabled: false,
            cdp_url: "http://chromium:9222".into(),
            max_tabs: 1,
            budget_s: 90,
            max_subrequests: 60,
            max_bytes: 20 * 1024 * 1024,
            network_idle_ms: 1000,
            subresource_gap_ms: 250,
            subresource_cache_ttl_h: 24,
            cache_dir: "/var/lib/voyager-crawler/render-cache".into(),
            subresource_cache_max_bytes: 64 * 1024 * 1024,
            restart_after_renders: 50,
        }
    }
}
