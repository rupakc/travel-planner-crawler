//! # vc-render (Task 5, stub)
//!
//! `ChromiumRenderer`: chromiumoxide + CDP `Fetch` interception. Chromium has no network; every
//! paused request is policy-checked and fulfilled through the `PermitClient`.

use std::sync::Arc;

use async_trait::async_trait;
use vc_config::RenderConfig;
use vc_core::fetch::PermitClient;
use vc_core::frontier::{ClaimPermit, Frontier};
use vc_core::policy::Policy;
use vc_core::render::{RenderBudget, RenderOutcome, Renderer};
use vc_core::{CanonicalUrl, Result};

/// The production renderer.
#[derive(Clone)]
pub struct ChromiumRenderer {
    /// Settings.
    pub config: RenderConfig,
    /// Fetches every paused request.
    pub client: Arc<dyn PermitClient>,
    /// Robots cache.
    pub frontier: Arc<dyn Frontier>,
    /// Policy decisions.
    pub policy: Arc<dyn Policy>,
}

#[async_trait]
impl Renderer for ChromiumRenderer {
    async fn render(
        &self,
        _p: &ClaimPermit,
        _url: &CanonicalUrl,
        _budget: RenderBudget,
    ) -> Result<RenderOutcome> {
        todo!("Task 5")
    }
}
