//! The permit client, the renderer and the policy.
//!
//! [`SubresourcePacing`] wraps the permit client the renderer uses: `ReqwestPermitClient`
//! paces **every** request by the permit's page delay (≥ 5 s), which would put 5 s between a
//! page's scripts and stylesheets. Spec §5.4/§17.1 asks for the per-key sub-resource gap
//! (250 ms) between sub-requests while the page-level delay still applies between pages. The
//! wrapper only shortens the wait of `RequestPurpose::SubResource` requests to the gap; the
//! lease check, the global rate token, robots, SSRF and scope checks are untouched (they all
//! stay inside the wrapped client).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use vc_config::Config;
use vc_core::Clock;
use vc_core::fetch::{
    Exchange, FetchError, FetchRequest, LimitedBodyStream, PermitClient, RequestPurpose,
};
use vc_core::frontier::{ClaimPermit, Frontier};
use vc_core::policy::Policy;
use vc_core::render::Renderer;
use vc_core::{Error, Result};
use vc_fetch::{ReqwestPermitClient, Resolve};
use vc_policy::VoyagerPolicy;
use vc_render::ChromiumRenderer;

/// The production policy (loads `policy/denylist.yaml` and the sub-resource allowlist; a
/// missing file is fatal).
pub fn policy(cfg: &Config) -> Result<Arc<dyn Policy>> {
    Ok(Arc::new(VoyagerPolicy::new(cfg.policy.clone())?))
}

/// The permit client (system DNS unless `resolver` is given).
pub fn permit_client(
    cfg: &Config,
    frontier: Arc<dyn Frontier>,
    policy: Arc<dyn Policy>,
    clock: Arc<dyn Clock>,
    resolver: Option<Arc<dyn Resolve>>,
) -> Arc<dyn PermitClient> {
    let mut c = ReqwestPermitClient::new(cfg.fetch.clone(), frontier, policy).with_clock(clock);
    if let Some(r) = resolver {
        c = c.with_resolver(r);
    }
    Arc::new(c)
}

/// The renderer, when `[render] enabled` (otherwise `None`: render rows fall back to HTTP).
pub fn renderer(
    cfg: &Config,
    client: Arc<dyn PermitClient>,
    frontier: Arc<dyn Frontier>,
    policy: Arc<dyn Policy>,
    clock: Arc<dyn Clock>,
) -> Option<Arc<dyn Renderer>> {
    if !cfg.render.enabled {
        return None;
    }
    let paced = Arc::new(SubresourcePacing::new(
        client,
        Duration::from_millis(cfg.render.subresource_gap_ms),
    ));
    Some(Arc::new(ChromiumRenderer::new(
        cfg.render.clone(),
        paced,
        frontier,
        policy,
        clock,
    )))
}

/// Paces render sub-requests by the sub-resource gap instead of the page delay (module docs).
pub struct SubresourcePacing {
    inner: Arc<dyn PermitClient>,
    gap: Duration,
}

impl std::fmt::Debug for SubresourcePacing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubresourcePacing")
            .field("gap", &self.gap)
            .finish_non_exhaustive()
    }
}

impl SubresourcePacing {
    /// Wraps `inner` with a sub-resource gap.
    pub fn new(inner: Arc<dyn PermitClient>, gap: Duration) -> Self {
        SubresourcePacing { inner, gap }
    }

    /// Moves the permit's "last request end" back so the inner client's wait
    /// (`delay − elapsed`) equals `gap − elapsed` (never negative, never longer than the
    /// inner client would wait anyway).
    fn shorten_wait(&self, p: &ClaimPermit, purpose: RequestPurpose) {
        if purpose != RequestPurpose::SubResource || self.gap >= p.delay() {
            return;
        }
        let Some(end) = p.last_request_end() else {
            return;
        };
        let since = end.elapsed();
        if since >= p.delay() {
            return;
        }
        let want = self.gap.saturating_sub(since);
        if let Some(t) = Instant::now().checked_sub(p.delay().saturating_sub(want)) {
            p.record_request_end(t);
        }
    }
}

#[async_trait]
impl PermitClient for SubresourcePacing {
    async fn fetch(
        &self,
        p: &ClaimPermit,
        req: FetchRequest,
    ) -> std::result::Result<Exchange, FetchError> {
        self.shorten_wait(p, req.purpose);
        self.inner.fetch(p, req).await
    }

    async fn stream(
        &self,
        p: &ClaimPermit,
        req: FetchRequest,
    ) -> std::result::Result<LimitedBodyStream, FetchError> {
        self.shorten_wait(p, req.purpose);
        self.inner.stream(p, req).await
    }
}

/// Maps a render error to the fetch error inside it, if any.
pub fn fetch_error_of(e: &Error) -> Option<&FetchError> {
    match e {
        Error::Fetch(f) => Some(f),
        _ => None,
    }
}
