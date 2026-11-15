//! # vc-fetch (Task 4, stub)
//!
//! `ReqwestPermitClient`: the only path for crawl-target bytes. Lease check + rate token per
//! request, SSRF-pinned DNS per hop, robots per hop, cross-key redirects returned (never
//! followed), decoded-byte caps, the one `vc_core::USER_AGENT`.

use std::sync::Arc;

use async_trait::async_trait;
use vc_config::FetchConfig;
use vc_core::fetch::{Exchange, FetchError, FetchRequest, LimitedBodyStream, PermitClient};
use vc_core::frontier::{ClaimPermit, Frontier};
use vc_core::policy::Policy;

/// The production permit client.
#[derive(Clone)]
pub struct ReqwestPermitClient {
    /// Settings.
    pub config: FetchConfig,
    /// Lease checks, rate tokens, robots cache.
    pub frontier: Arc<dyn Frontier>,
    /// Policy decisions.
    pub policy: Arc<dyn Policy>,
}

impl ReqwestPermitClient {
    /// Creates the client.
    pub fn new(config: FetchConfig, frontier: Arc<dyn Frontier>, policy: Arc<dyn Policy>) -> Self {
        ReqwestPermitClient {
            config,
            frontier,
            policy,
        }
    }
}

#[async_trait]
impl PermitClient for ReqwestPermitClient {
    async fn fetch(&self, _p: &ClaimPermit, _req: FetchRequest) -> Result<Exchange, FetchError> {
        todo!("Task 4")
    }
    async fn stream(
        &self,
        _p: &ClaimPermit,
        _req: FetchRequest,
    ) -> Result<LimitedBodyStream, FetchError> {
        todo!("Task 4")
    }
}
