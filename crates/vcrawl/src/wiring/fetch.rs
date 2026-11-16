//! The permit client, the renderer and the policy.
//!
//! The permit client paces render sub-requests by `[render] subresource_gap_ms` (spec §17.1)
//! and pages by the permit's delay; both live in `vc_fetch::ReqwestPermitClient::admit`.

use std::sync::Arc;
use std::time::Duration;

use vc_config::Config;
use vc_core::Clock;
use vc_core::fetch::{FetchError, PermitClient};
use vc_core::frontier::Frontier;
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
    Some(Arc::new(ChromiumRenderer::new(
        cfg.render.clone(),
        client,
        frontier,
        policy,
        clock,
    )))
}

/// Maps a render error to the fetch error inside it, if any.
pub fn fetch_error_of(e: &Error) -> Option<&FetchError> {
    match e {
        Error::Fetch(f) => Some(f),
        _ => None,
    }
}
