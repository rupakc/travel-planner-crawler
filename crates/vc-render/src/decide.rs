//! The pure part of the per-request decision (spec §17.1 step 1): method, SSRF on IP literals,
//! blocked resource types, known trackers, the denylist and the permit scope. robots.txt (which
//! needs the frontier's `origins` cache) and the budgets are checked by the engine afterwards.

use std::net::IpAddr;

use vc_core::CanonicalUrl;
use vc_core::fetch::FetchError;
use vc_core::frontier::PermitScope;
use vc_core::policy::Policy;
use vc_core::render::BlockReason;

use crate::cdp::{PausedRequest, ResourceKind};

/// Per-type check for third-party (allowlisted) hosts: `(host, kind)` with `kind` the
/// lower-case CDP resource type ([`ResourceKind::cdp_name`]). Production passes
/// `VoyagerPolicy::subresource_allowed`, which reads the allowlist's `types`.
pub type SubresourceFilter = std::sync::Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// Known tracker / ad hosts (and their subdomains). Blocked even if a source allowlists them.
pub const TRACKER_HOSTS: &[&str] = &[
    "google-analytics.com",
    "googletagmanager.com",
    "googletagservices.com",
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "facebook.net",
    "hotjar.com",
    "hotjar.io",
    "cdn.segment.com",
    "api.segment.io",
    "scorecardresearch.com",
    "quantserve.com",
    "criteo.com",
    "criteo.net",
    "taboola.com",
    "outbrain.com",
    "adnxs.com",
    "amazon-adsystem.com",
    "clarity.ms",
    "mixpanel.com",
    "nr-data.net",
    "bat.bing.com",
    "analytics.tiktok.com",
    "snap.licdn.com",
    "px.ads.linkedin.com",
    "static.ads-twitter.com",
    "chartbeat.com",
    "chartbeat.net",
    "fullstory.com",
    "mouseflow.com",
    "crazyegg.com",
    "optimizely.com",
    "matomo.cloud",
];

/// Whether `host` is a known tracker (exact or subdomain match).
pub fn is_tracker(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    TRACKER_HOSTS
        .iter()
        .any(|t| h == *t || h.strip_suffix(t).is_some_and(|p| p.ends_with('.')))
}

/// A request the renderer refuses, with the matching fetch error when there is one (used when
/// the refused request is the page's own document, which turns into an error of the render).
#[derive(Debug, Clone)]
pub struct Blocked {
    /// Why.
    pub reason: BlockReason,
    /// The equivalent `FetchError`, if the reason has one.
    pub error: Option<FetchError>,
}

impl Blocked {
    fn new(reason: BlockReason) -> Self {
        Blocked {
            reason,
            error: None,
        }
    }

    fn with(reason: BlockReason, error: FetchError) -> Self {
        Blocked {
            reason,
            error: Some(error),
        }
    }
}

/// The IP of an IP-literal host (`10.0.0.5`, `[::1]`), if it is one.
fn ip_literal(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// [`precheck_with`] without a per-type filter for third-party hosts.
pub fn precheck(
    policy: &dyn Policy,
    scope: &PermitScope,
    req: &PausedRequest,
    url: &CanonicalUrl,
    is_document: bool,
) -> Result<(), Blocked> {
    precheck_with(policy, scope, req, url, is_document, None)
}

/// The synchronous checks, in order: method/scheme, SSRF (IP literals; hostnames are checked on
/// the pinned IP by the permit client), resource type (any document other than the page's own
/// is refused: iframes and JS/meta-refresh navigations are not fetched), trackers, denylist,
/// scope (the page's own key for the document; the key, or a sub-resource allowlist host whose
/// `types` include this request's type per `filter`, for everything else).
pub fn precheck_with(
    policy: &dyn Policy,
    scope: &PermitScope,
    req: &PausedRequest,
    url: &CanonicalUrl,
    is_document: bool,
    filter: Option<&SubresourceFilter>,
) -> Result<(), Blocked> {
    if !req.method.eq_ignore_ascii_case("GET") {
        return Err(Blocked::new(BlockReason::Method));
    }
    if let Some(ip) = ip_literal(url.host()) {
        policy
            .ssrf_check(url.host(), ip, url.port())
            .map_err(|e| Blocked::with(BlockReason::Ssrf, FetchError::Ssrf(e)))?;
    }
    if !is_document && (!req.kind.allowed() || req.kind == ResourceKind::Document) {
        return Err(Blocked::new(BlockReason::ResourceType));
    }
    if is_tracker(url.host()) {
        return Err(Blocked::new(BlockReason::Tracker));
    }
    if let Some(d) = policy.denied(url) {
        return Err(Blocked::with(
            BlockReason::Denylisted,
            FetchError::Denied(d),
        ));
    }
    let in_scope = scope.covers(url)
        || (!is_document
            && policy.subresource_host_allowed(url.host())
            && filter.is_none_or(|f| f(url.host(), req.kind.cdp_name())));
    if !in_scope {
        return Err(Blocked::with(
            BlockReason::OutOfScope,
            FetchError::OutOfScope(url.clone()),
        ));
    }
    Ok(())
}

/// The block reason for a sub-request whose permit-gated fetch failed.
pub fn reason_for(e: &FetchError) -> BlockReason {
    match e {
        FetchError::Ssrf(_) => BlockReason::Ssrf,
        FetchError::Denied(_) => BlockReason::Denylisted,
        FetchError::RobotsDisallowed(_) => BlockReason::RobotsDisallowed,
        FetchError::RobotsUnavailable(_) => BlockReason::RobotsUnavailable,
        FetchError::OutOfScope(_) => BlockReason::OutOfScope,
        FetchError::BodyTooLarge { .. } | FetchError::DeadlineExceeded => BlockReason::Budget,
        _ => BlockReason::FetchFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trackers_match_subdomains_only() {
        assert!(is_tracker("www.google-analytics.com"));
        assert!(is_tracker("google-analytics.com"));
        assert!(is_tracker("connect.facebook.net"));
        assert!(is_tracker("WWW.GOOGLETAGMANAGER.COM."));
        assert!(!is_tracker("notgoogle-analytics.com"));
        assert!(!is_tracker("facebook.com"));
        assert!(!is_tracker("segment.com"));
    }

    #[test]
    fn ip_literals() {
        assert_eq!(ip_literal("10.0.0.5"), Some("10.0.0.5".parse().unwrap()));
        assert_eq!(ip_literal("[::1]"), Some("::1".parse().unwrap()));
        assert_eq!(ip_literal("example.com"), None);
    }

    #[test]
    fn fetch_errors_map_to_reasons() {
        let u = CanonicalUrl::parse("https://a.test/").unwrap();
        assert_eq!(
            reason_for(&FetchError::RobotsDisallowed(u.clone())),
            BlockReason::RobotsDisallowed
        );
        assert_eq!(
            reason_for(&FetchError::BodyTooLarge { limit: 1 }),
            BlockReason::Budget
        );
        assert_eq!(reason_for(&FetchError::Timeout), BlockReason::FetchFailed);
        assert_eq!(
            reason_for(&FetchError::CrossKeyRedirect(u)),
            BlockReason::FetchFailed
        );
    }
}
