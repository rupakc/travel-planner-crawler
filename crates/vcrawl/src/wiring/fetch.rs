//! The permit client, the renderer and the policy.
//!
//! The permit client paces render sub-requests by `[render] subresource_gap_ms` (spec §17.1)
//! and pages by the permit's delay; both live in `vc_fetch::ReqwestPermitClient::admit`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use vc_config::Config;
use vc_core::Clock;
use vc_core::PolitenessKey;
use vc_core::fetch::{
    Exchange, FetchError, FetchRequest, LimitedBodyStream, PermitClient, RequestPurpose,
};
use vc_core::frontier::{ClaimPermit, Frontier};
use vc_core::policy::Policy;
use vc_core::render::Renderer;
use vc_core::{Error, Result};
use vc_fetch::{ReqwestPermitClient, Resolve};
use vc_frontier::PgFrontier;
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
    let mut c = ReqwestPermitClient::new(cfg.fetch.clone(), frontier, policy)
        .with_clock(clock)
        .with_subresource_gap(Duration::from_millis(cfg.render.subresource_gap_ms));
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
    // The allowlist's per-host `types` (the `Policy` trait only knows hosts). If the policy
    // files cannot be read here, no third-party host is allowed (fail closed).
    let types = VoyagerPolicy::new(cfg.policy.clone()).ok();
    let filter: vc_render::SubresourceFilter = Arc::new(move |host: &str, kind: &str| {
        types
            .as_ref()
            .is_some_and(|p| p.subresource_allowed(host, kind))
    });
    Some(Arc::new(
        ChromiumRenderer::new(cfg.render.clone(), client, frontier, policy, clock)
            .with_subresource_filter(filter),
    ))
}

/// Maps a render error to the fetch error inside it, if any.
pub fn fetch_error_of(e: &Error) -> Option<&FetchError> {
    match e {
        Error::Fetch(f) => Some(f),
        _ => None,
    }
}

/// Borrowing a third-party key for one render sub-request (see [`third_party_gate`]).
#[async_trait]
pub trait KeyBorrow: Send + Sync {
    /// Whether `key` may take one request now (and is then held for its delay).
    async fn borrow(&self, key: &PolitenessKey) -> Result<bool>;

    /// The borrowed request ended: hold `key` for its delay from now.
    async fn done(&self, key: &PolitenessKey) -> Result<()>;
}

#[async_trait]
impl KeyBorrow for PgFrontier {
    async fn borrow(&self, key: &PolitenessKey) -> Result<bool> {
        self.borrow_subresource(key).await
    }

    async fn done(&self, key: &PolitenessKey) -> Result<()> {
        self.subresource_done(key).await
    }
}

/// The renderer's permit client: a render sub-request to **another** politeness key (an
/// allowlisted CDN) first borrows that key through the frontier, so it honours the key's own
/// `next_allowed_at`, parking and in-flight claim across every process, and moves its
/// `next_allowed_at` on. A refused borrow fails the sub-request at once (the renderer skips
/// it); nothing waits for the other key.
pub fn third_party_gate(
    inner: Arc<dyn PermitClient>,
    borrow: Arc<dyn KeyBorrow>,
) -> Arc<dyn PermitClient> {
    Arc::new(ThirdPartyGate { inner, borrow })
}

struct ThirdPartyGate {
    inner: Arc<dyn PermitClient>,
    borrow: Arc<dyn KeyBorrow>,
}

impl ThirdPartyGate {
    /// `Some(key)`: a third-party key was borrowed (call [`ThirdPartyGate::done`] after).
    async fn admit(
        &self,
        p: &ClaimPermit,
        req: &FetchRequest,
    ) -> Result<Option<PolitenessKey>, FetchError> {
        let key = req.url.politeness_key();
        if req.purpose != RequestPurpose::SubResource || key == *p.key() {
            return Ok(None);
        }
        match self.borrow.borrow(&key).await {
            Ok(true) => Ok(Some(key)),
            Ok(false) => Err(FetchError::Transport(format!(
                "third-party key {key} not ready: sub-resource skipped"
            ))),
            Err(e) => {
                tracing::info!(key = %key, error_class = ?e.class(), "sub-resource borrow failed");
                Err(FetchError::Transport(format!(
                    "third-party key {key} unverifiable: sub-resource skipped"
                )))
            }
        }
    }

    async fn done(&self, key: Option<PolitenessKey>) {
        if let Some(k) = key
            && let Err(e) = self.borrow.done(&k).await
        {
            tracing::info!(key = %k, error_class = ?e.class(), "sub-resource release failed");
        }
    }
}

#[async_trait]
impl PermitClient for ThirdPartyGate {
    async fn fetch(&self, p: &ClaimPermit, req: FetchRequest) -> Result<Exchange, FetchError> {
        let borrowed = self.admit(p, &req).await?;
        let r = self.inner.fetch(p, req).await;
        self.done(borrowed).await;
        r
    }

    async fn stream(
        &self,
        p: &ClaimPermit,
        req: FetchRequest,
    ) -> Result<LimitedBodyStream, FetchError> {
        let borrowed = self.admit(p, &req).await?;
        let r = self.inner.stream(p, req).await;
        self.done(borrowed).await;
        r
    }
}
