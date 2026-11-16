//! # vc-render
//!
//! [`ChromiumRenderer`] implements [`vc_core::render::Renderer`] with chromiumoxide and CDP
//! `Fetch` interception (spec §5.4, **§17.1**). Chromium has **no network**: it runs on a Docker
//! network with no route out, and every request it makes (document, script, style, XHR, fetch)
//! is paused. For each paused request the renderer:
//!
//! 1. applies the policy checks: method/scheme, SSRF (IP literals here; every hostname on its
//!    pinned IP inside the permit client), blocked resource types (images, media, fonts and
//!    everything else that does not build the DOM), known trackers, the denylist, the permit
//!    scope (the page's key, or a `policy/subresource_allowlist.yaml` host for sub-resources),
//!    robots.txt from the frontier's `origins` cache (missing or stale → fail and enqueue a
//!    refresh; the renderer never fetches robots.txt), and the render budget;
//! 2. fetches through the [`vc_core::fetch::PermitClient`] (lease check + global rate token per
//!    request, the one [`vc_core::USER_AGENT`]), one request at a time, same-key sub-requests at
//!    least `subresource_gap` apart;
//! 3. answers with `Fetch.fulfillRequest`, or `Fetch.failRequest` with a [`BlockReason`].
//!
//! Every network request becomes an [`vc_core::fetch::Exchange`] in the [`RenderOutcome`]
//! (WARC `request`/`response` records next to the DOM `resource` record); refused requests are
//! listed in `blocked`. Scripts and stylesheets are served from a shared, content-addressed
//! [`SubresourceCache`] (TTL 24 h) after the policy checks.
//!
//! **Detect → park:** a challenge, 401 or 403 on the page's own key aborts the render with
//! [`FetchError::Challenge`] (the caller parks the key; nothing is retried). A lost lease aborts
//! with a lease error. A refused or failed document is an error of the render (e.g.
//! [`FetchError::CrossKeyRedirect`], which the caller enqueues).
//!
//! The browser side sits behind the [`cdp`] traits so the decision logic is tested with a mock;
//! production uses [`ChromiumBrowser`].

pub mod cache;
pub mod cdp;
pub mod chromium;
pub mod decide;

mod b64;
mod engine;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use vc_config::RenderConfig;
use vc_core::fetch::{FetchError, PermitClient};
use vc_core::frontier::{ClaimPermit, Frontier};
use vc_core::policy::Policy;
use vc_core::render::{RenderBudget, RenderOutcome, Renderer};
use vc_core::{CanonicalUrl, Clock, Error, Result};

pub use cache::SubresourceCache;
pub use chromium::ChromiumBrowser;
pub use vc_core::render::BlockReason;

/// The production renderer.
#[derive(Clone)]
pub struct ChromiumRenderer {
    config: RenderConfig,
    deps: engine::Deps,
    browser: Arc<dyn cdp::CdpBrowser>,
    tabs: Arc<Semaphore>,
}

impl std::fmt::Debug for ChromiumRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromiumRenderer")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ChromiumRenderer {
    /// A renderer connected (lazily) to the headless-shell container at `config.cdp_url`.
    pub fn new(
        config: RenderConfig,
        client: Arc<dyn PermitClient>,
        frontier: Arc<dyn Frontier>,
        policy: Arc<dyn Policy>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let cache = Arc::new(SubresourceCache::new(
            clock.clone(),
            Duration::from_secs(config.subresource_cache_ttl_h.saturating_mul(3600)),
            config.subresource_cache_max_bytes,
        ));
        let browser = Arc::new(ChromiumBrowser::new(
            config.cdp_url.clone(),
            config.restart_after_renders,
            // CDP commands (navigate waits for the intercepted document) get the whole budget.
            Duration::from_secs(config.budget_s.saturating_add(30)),
        ));
        Self::with_browser(config, client, frontier, policy, clock, cache, browser)
    }

    /// A renderer over any [`cdp::CdpBrowser`] with a given cache (tests, or a shared cache).
    pub fn with_browser(
        config: RenderConfig,
        client: Arc<dyn PermitClient>,
        frontier: Arc<dyn Frontier>,
        policy: Arc<dyn Policy>,
        clock: Arc<dyn Clock>,
        cache: Arc<SubresourceCache>,
        browser: Arc<dyn cdp::CdpBrowser>,
    ) -> Self {
        let tabs = Arc::new(Semaphore::new(config.max_tabs.max(1) as usize));
        ChromiumRenderer {
            config,
            deps: engine::Deps {
                client,
                frontier,
                policy,
                clock,
                cache,
            },
            browser,
            tabs,
        }
    }

    /// The default budget from the config (the caller adds the source's expand selectors).
    pub fn default_budget(&self) -> RenderBudget {
        budget_from(&self.config)
    }

    /// The shared sub-resource cache.
    pub fn cache(&self) -> &Arc<SubresourceCache> {
        &self.deps.cache
    }

    /// The budget for one render: the caller's, never above the configured limits.
    fn clamp(&self, b: RenderBudget) -> RenderBudget {
        let c = budget_from(&self.config);
        RenderBudget {
            total: b.total.min(c.total),
            max_subrequests: b.max_subrequests.min(c.max_subrequests),
            max_bytes: b.max_bytes.min(c.max_bytes),
            ..b
        }
    }
}

/// The [`RenderBudget`] the config describes.
pub fn budget_from(c: &RenderConfig) -> RenderBudget {
    RenderBudget {
        total: Duration::from_secs(c.budget_s),
        max_subrequests: c.max_subrequests,
        max_bytes: c.max_bytes,
        network_idle: Duration::from_millis(c.network_idle_ms),
        subresource_gap: Duration::from_millis(c.subresource_gap_ms),
        ..RenderBudget::default()
    }
}

#[async_trait]
impl Renderer for ChromiumRenderer {
    async fn render(
        &self,
        p: &ClaimPermit,
        url: &CanonicalUrl,
        budget: RenderBudget,
    ) -> Result<RenderOutcome> {
        if !self.config.enabled {
            return Err(Error::Policy(
                "rendering is disabled ([render] enabled = false)".into(),
            ));
        }
        let budget = self.clamp(budget);
        // Waiting for a tab counts against the render's own time.
        let wait = budget.total.min(p.remaining(self.deps.clock.now()));
        let _tab = tokio::time::timeout(wait, self.tabs.clone().acquire_owned())
            .await
            .map_err(|_| Error::Fetch(FetchError::DeadlineExceeded))?
            .map_err(Error::other)?;
        engine::render(&self.deps, &*self.browser, p, url, budget).await
    }
}
