//! Chromium rendering through CDP `Fetch` interception (spec §17.1; there is no egress proxy).
//! Implemented by `vc-render` (`ChromiumRenderer`).
//!
//! Chromium has no network. Every request it makes is paused; the renderer applies the policy
//! checks (SSRF, denylist, robots from the `origins` cache — stale/missing fails the request,
//! permit scope or sub-resource allowlist, blocked resource types), fetches through
//! [`crate::fetch::PermitClient`] (lease check + rate token per request) and answers with
//! `Fetch.fulfillRequest` or `Fetch.failRequest`.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::fetch::Exchange;
use crate::frontier::ClaimPermit;
use crate::url::CanonicalUrl;

/// Limits for one render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderBudget {
    /// Total wall time (default 90 s); also bounded by the lease deadline.
    pub total: Duration,
    /// Max sub-requests (default 60).
    pub max_subrequests: u32,
    /// Max total decoded bytes across sub-requests (default 20 MB).
    pub max_bytes: u64,
    /// Network idle = no paused request outstanding for this long (default 1 s).
    pub network_idle: Duration,
    /// Per-key gap between sub-requests (default 250 ms).
    pub subresource_gap: Duration,
    /// Allowlisted CSS selectors to click once to expand content (from the SourceSpec).
    pub expand_selectors: Vec<String>,
    /// Scroll to the bottom to trigger lazy loading.
    pub auto_scroll: bool,
}

impl Default for RenderBudget {
    fn default() -> Self {
        RenderBudget {
            total: Duration::from_secs(90),
            max_subrequests: 60,
            max_bytes: 20 * 1024 * 1024,
            network_idle: Duration::from_secs(1),
            subresource_gap: Duration::from_millis(250),
            expand_selectors: Vec::new(),
            auto_scroll: true,
        }
    }
}

/// Why a paused request was failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// SSRF guard.
    Ssrf,
    /// Denylisted host/path.
    Denylisted,
    /// robots.txt disallows it.
    RobotsDisallowed,
    /// robots.txt for its origin is missing or stale (a refresh was enqueued).
    RobotsUnavailable,
    /// Neither the page's key nor an allowlisted sub-resource host.
    OutOfScope,
    /// A blocked resource type (image, media, font).
    ResourceType,
    /// A known tracker.
    Tracker,
    /// Render budget (time, count or bytes) exhausted.
    Budget,
    /// Not a GET, or a non-http(s) scheme.
    Method,
    /// The permit-gated fetch failed.
    FetchFailed,
}

/// The result of a render.
#[derive(Debug, Clone)]
pub struct RenderOutcome {
    /// The final serialised DOM (stored as a WARC `resource` record).
    pub dom_html: Bytes,
    /// Every request that was fulfilled, incl. the document itself (WARC request/response).
    pub exchanges: Vec<Exchange>,
    /// Every request that was failed, with the reason.
    pub blocked: Vec<(CanonicalUrl, BlockReason)>,
}

/// Renders pages under a permit.
#[async_trait]
pub trait Renderer: Send + Sync {
    /// Renders `url` (covered by the permit) within `budget` and the lease deadline.
    async fn render(
        &self,
        p: &ClaimPermit,
        url: &CanonicalUrl,
        budget: RenderBudget,
    ) -> Result<RenderOutcome>;
}
