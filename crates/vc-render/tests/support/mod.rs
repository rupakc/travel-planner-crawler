//! Shared test support: fake web (PermitClient), fake policy, frontier overlay, mock CDP.
#![allow(dead_code, missing_docs, clippy::unwrap_used)]

pub mod frontier;
pub mod mock_cdp;
pub mod policy;
pub mod web;

use std::sync::Arc;
use std::time::Duration;

use testkit::FakeClock;
use vc_config::RenderConfig;
use vc_core::render::RenderBudget;
use vc_render::cdp::CdpBrowser;
use vc_render::{ChromiumRenderer, SubresourceCache};

#[allow(unused_imports)]
pub use frontier::TestFrontier;
#[allow(unused_imports)]
pub use mock_cdp::{MockBrowser, Script, req};
pub use policy::FakePolicy;
pub use web::FakeWeb;

/// Everything a render test needs.
pub struct Env {
    pub clock: FakeClock,
    pub frontier: Arc<TestFrontier>,
    pub web: Arc<FakeWeb>,
    pub policy: Arc<FakePolicy>,
    pub cache: Arc<SubresourceCache>,
}

impl Env {
    pub fn new() -> Self {
        let clock = FakeClock::default();
        let frontier = TestFrontier::new(clock.clone());
        let web = Arc::new(FakeWeb::new(frontier.clone()));
        let cache = Arc::new(SubresourceCache::new(
            Arc::new(clock.clone()),
            Duration::from_secs(24 * 3600),
            64 * 1024 * 1024,
        ));
        Env {
            clock,
            frontier,
            web,
            policy: Arc::new(FakePolicy::new()),
            cache,
        }
    }

    /// A renderer over `browser` (the mock, or real Chromium).
    pub fn renderer(&self, config: RenderConfig, browser: Arc<dyn CdpBrowser>) -> ChromiumRenderer {
        let config = RenderConfig {
            enabled: true,
            ..config
        };
        ChromiumRenderer::with_browser(
            config,
            self.web.clone(),
            self.frontier.clone(),
            self.policy.clone(),
            Arc::new(self.clock.clone()),
            self.cache.clone(),
            browser,
        )
    }
}

/// A budget with short idle/gap windows so tests are fast.
pub fn fast_budget() -> RenderBudget {
    RenderBudget {
        total: Duration::from_secs(10),
        network_idle: Duration::from_millis(100),
        subresource_gap: Duration::from_millis(1),
        ..RenderBudget::default()
    }
}
