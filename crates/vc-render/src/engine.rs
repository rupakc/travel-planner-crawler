//! The render loop (spec §17.1): open an intercepted tab, navigate, answer every paused request
//! (policy checks → `PermitClient` → `Fetch.fulfillRequest` / `Fetch.failRequest`), wait for
//! network idle, auto-scroll, click the allowlisted expand selectors, then serialise the DOM.
//!
//! Requests are answered **one at a time**: there is never more than one request in flight
//! under the permit, and same-key sub-requests are spaced by the sub-resource gap.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, LazyLock};

use bytes::Bytes;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Duration, Instant, sleep_until};
use tracing::debug;
use vc_core::fetch::{Exchange, FetchError, FetchRequest, PermitClient, RequestPurpose};
use vc_core::frontier::{ClaimPermit, Frontier, NewUrl};
use vc_core::media::MediaType;
use vc_core::policy::{Policy, RobotsVerdict};
use vc_core::render::{BlockReason, RenderBudget, RenderOutcome};
use vc_core::{CanonicalUrl, Clock, Error, OriginKey, PolitenessKey, Result, USER_AGENT};

use crate::ThirdPartyBorrow;
use crate::cache::SubresourceCache;
use crate::cdp::{CdpBrowser, CdpError, CdpTab, Fulfilment, PausedRequest, ResourceKind};
use crate::decide::{self, Blocked, SubresourceFilter};

/// The last request end per third-party host, one lock per host, shared by every render in the
/// process (both tabs of a fetcher, and every renderer instance): third-party CDNs are not
/// covered by any permit, so this is what keeps concurrent renders from hitting one at once.
/// Only allowlisted hosts ever get here, so the map stays small.
type HostSlot = Arc<tokio::sync::Mutex<Option<Instant>>>;
static THIRD_PARTY: LazyLock<std::sync::Mutex<HashMap<String, HostSlot>>> =
    LazyLock::new(Default::default);

/// `Retry-After` (RFC 9110 §10.2.3): delay-seconds, or an HTTP-date relative to `now` (a past
/// date gives zero). Anything else is `None`.
pub(crate) fn parse_retry_after(
    value: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<std::time::Duration> {
    let v = value.trim();
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        return v.parse::<u64>().ok().map(std::time::Duration::from_secs);
    }
    let at = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    Some(
        (at.with_timezone(&chrono::Utc) - now)
            .to_std()
            .unwrap_or(std::time::Duration::ZERO),
    )
}

fn third_party_slot(host: &str) -> HostSlot {
    let mut m = THIRD_PARTY.lock().unwrap_or_else(|p| p.into_inner());
    m.entry(host.to_ascii_lowercase()).or_default().clone()
}

/// At most this many scrolls to the bottom (stops early once the page stops growing).
const MAX_SCROLLS: u32 = 5;
/// Time allowed to serialise the DOM after the network phase ended.
const CONTENT_GRACE: Duration = Duration::from_secs(5);

/// Response headers never handed back to Chromium: bodies are already decoded, lengths are
/// recomputed, and the renderer is stateless (no cookies).
const DROP_HEADERS: &[&str] = &[
    "content-encoding",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "set-cookie",
    "set-cookie2",
];

/// What the render loop needs.
#[derive(Clone)]
pub(crate) struct Deps {
    pub client: Arc<dyn PermitClient>,
    pub frontier: Arc<dyn Frontier>,
    pub policy: Arc<dyn Policy>,
    pub clock: Arc<dyn Clock>,
    pub cache: Arc<SubresourceCache>,
    pub filter: Option<SubresourceFilter>,
    pub borrow: Option<Arc<dyn ThirdPartyBorrow>>,
}

pub(crate) fn cdp_error(e: CdpError) -> Error {
    Error::other(e)
}

/// Renders `url` on a fresh tab of `browser`. The tab is always closed afterwards.
pub(crate) async fn render(
    deps: &Deps,
    browser: &dyn CdpBrowser,
    p: &ClaimPermit,
    url: &CanonicalUrl,
    budget: RenderBudget,
) -> Result<RenderOutcome> {
    deps.frontier.check(p).await?;
    let total = budget.total.min(p.remaining(deps.clock.now()));
    if total.is_zero() {
        return Err(Error::Fetch(FetchError::DeadlineExceeded));
    }
    let deadline = Instant::now() + total;
    let (tab, events) = browser.open_tab().await.map_err(cdp_error)?;
    let mut run = Run {
        deps,
        tab: &*tab,
        events,
        permit: p,
        budget,
        deadline,
        expect_doc: url.clone(),
        doc_done: false,
        exchanges: Vec::new(),
        blocked: Vec::new(),
        subrequests: 0,
        bytes: 0,
        last_end: HashMap::new(),
        prefetched: HashMap::new(),
        refresh_enqueued: HashSet::new(),
    };
    let out = run.go(url).await;
    tab.close().await;
    out
}

enum Driven<T> {
    Done(T),
    TimedOut,
}

struct Run<'a> {
    deps: &'a Deps,
    tab: &'a dyn CdpTab,
    events: UnboundedReceiver<PausedRequest>,
    permit: &'a ClaimPermit,
    budget: RenderBudget,
    deadline: Instant,
    /// The URL the main document is expected at (moves along same-key redirects).
    expect_doc: CanonicalUrl,
    doc_done: bool,
    exchanges: Vec<Exchange>,
    blocked: Vec<(CanonicalUrl, BlockReason)>,
    subrequests: u32,
    bytes: u64,
    last_end: HashMap<PolitenessKey, Instant>,
    /// Final URLs of redirects the permit client already followed: served from memory when
    /// Chromium follows the redirect itself (no second request).
    prefetched: HashMap<CanonicalUrl, Exchange>,
    refresh_enqueued: HashSet<OriginKey>,
}

fn tab_gone() -> Error {
    cdp_error(CdpError::Protocol("tab closed".into()))
}

impl Run<'_> {
    fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    async fn go(&mut self, url: &CanonicalUrl) -> Result<RenderOutcome> {
        let tab = self.tab;
        tab.set_user_agent(USER_AGENT).await.map_err(cdp_error)?;
        if let Driven::Done(Err(e)) = self.drive(tab.navigate(url.as_str())).await? {
            return Err(cdp_error(e));
        }
        self.wait_idle().await?;
        if !self.doc_done {
            self.drain_over_budget().await;
            return Err(Error::Fetch(FetchError::DeadlineExceeded));
        }
        if self.budget.auto_scroll {
            let mut last = None;
            for _ in 0..MAX_SCROLLS {
                if self.expired() {
                    break;
                }
                let h = match self.drive(tab.scroll_to_bottom()).await? {
                    Driven::Done(Ok(h)) => h,
                    Driven::Done(Err(e)) => {
                        debug!(error = %e, "render: scroll failed");
                        break;
                    }
                    Driven::TimedOut => break,
                };
                self.wait_idle().await?;
                if last == Some(h) {
                    break;
                }
                last = Some(h);
            }
        }
        for sel in self.budget.expand_selectors.clone() {
            if self.expired() {
                break;
            }
            match self.drive(tab.click_all(&sel)).await? {
                Driven::Done(Ok(_)) => self.wait_idle().await?,
                Driven::Done(Err(e)) => debug!(error = %e, "render: expand click failed"),
                Driven::TimedOut => break,
            }
        }
        self.drain_over_budget().await;
        let dom = match tokio::time::timeout(CONTENT_GRACE, tab.content()).await {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => return Err(cdp_error(e)),
            Err(_) => {
                return Err(cdp_error(CdpError::Protocol(
                    "DOM serialisation timed out".into(),
                )));
            }
        };
        Ok(RenderOutcome {
            dom_html: Bytes::from(dom),
            exchanges: std::mem::take(&mut self.exchanges),
            blocked: std::mem::take(&mut self.blocked),
        })
    }

    /// Runs `fut` (a CDP command) while answering paused requests, until it completes or the
    /// render deadline passes.
    async fn drive<T>(&mut self, fut: impl Future<Output = T>) -> Result<Driven<T>> {
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                out = &mut fut => return Ok(Driven::Done(out)),
                ev = self.events.recv() => match ev {
                    Some(r) => self.handle(r).await?,
                    None => return Err(tab_gone()),
                },
                () = sleep_until(self.deadline) => return Ok(Driven::TimedOut),
            }
        }
    }

    /// Answers paused requests until none arrived for `network_idle`, or the deadline.
    async fn wait_idle(&mut self) -> Result<()> {
        loop {
            let now = Instant::now();
            if now >= self.deadline {
                return Ok(());
            }
            let quiet_until = (now + self.budget.network_idle).min(self.deadline);
            tokio::select! {
                ev = self.events.recv() => match ev {
                    Some(r) => self.handle(r).await?,
                    None => return Err(tab_gone()),
                },
                () = sleep_until(quiet_until) => return Ok(()),
            }
        }
    }

    /// Fails whatever is still paused once the network phase is over.
    async fn drain_over_budget(&mut self) {
        while let Ok(r) = self.events.try_recv() {
            self.fail(&r.id, &BlockReason::Budget).await;
            if let Ok(u) = CanonicalUrl::parse(&r.url) {
                self.blocked.push((u, BlockReason::Budget));
            }
        }
    }

    async fn fail(&self, id: &str, reason: &BlockReason) {
        if let Err(e) = self.tab.fail(id, reason).await {
            debug!(error = %e, "render: failRequest");
        }
    }

    async fn fulfill(&self, id: &str, ex: &Exchange) {
        let headers = ex
            .response_headers
            .iter()
            .filter(|(k, _)| !DROP_HEADERS.contains(&k.as_str()))
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let resp = Fulfilment {
            status: ex.status.as_u16(),
            headers,
            body: ex.body.clone(),
        };
        if let Err(e) = self.tab.fulfill(id, resp).await {
            debug!(error = %e, "render: fulfillRequest");
        }
    }

    /// Refuses one request. For the page's own document that ends the render with an error.
    async fn refuse(
        &mut self,
        req: &PausedRequest,
        url: CanonicalUrl,
        is_doc: bool,
        b: Blocked,
    ) -> Result<()> {
        self.fail(&req.id, &b.reason).await;
        debug!(host = url.host(), reason = ?b.reason, "render: request refused");
        if is_doc {
            return Err(b.error.map(Error::Fetch).unwrap_or_else(|| {
                Error::Policy(format!("render document refused: {:?}", b.reason))
            }));
        }
        self.blocked.push((url, b.reason));
        Ok(())
    }

    /// robots.txt from the `origins` cache; missing or stale fails closed and enqueues a
    /// refresh (the renderer never fetches robots.txt itself).
    async fn robots(&mut self, url: &CanonicalUrl) -> std::result::Result<(), Blocked> {
        let origin = url.origin();
        let entry = self.deps.frontier.robots(&origin).await.ok().flatten();
        let unavailable = |origin: OriginKey| Blocked {
            reason: BlockReason::RobotsUnavailable,
            error: Some(FetchError::RobotsUnavailable(origin)),
        };
        match entry {
            Some(e) if e.is_fresh(self.deps.clock.now()) => {
                match self.deps.policy.robots_verdict(&e, url) {
                    RobotsVerdict::Allowed { .. } => Ok(()),
                    RobotsVerdict::Disallowed | RobotsVerdict::KeyBlocked => Err(Blocked {
                        reason: BlockReason::RobotsDisallowed,
                        error: Some(FetchError::RobotsDisallowed(url.clone())),
                    }),
                    RobotsVerdict::Unavailable => Err(unavailable(origin)),
                }
            }
            _ => {
                if self.refresh_enqueued.insert(origin.clone()) {
                    let mut nu = NewUrl::new(url.robots_url());
                    nu.expect = Some(MediaType::Text);
                    nu.priority = 1.0;
                    if let Err(e) = self.deps.frontier.enqueue(&[nu]).await {
                        debug!(error = %e, "render: robots refresh enqueue failed");
                    }
                }
                Err(unavailable(origin))
            }
        }
    }

    async fn handle(&mut self, req: PausedRequest) -> Result<()> {
        let Ok(url) = CanonicalUrl::parse(&req.url) else {
            // data:, blob:, chrome-extension: and other non-http(s) schemes.
            self.fail(&req.id, &BlockReason::Method).await;
            return Ok(());
        };
        let is_doc = !self.doc_done && req.kind == ResourceKind::Document && url == self.expect_doc;

        if let Some(ex) = self.prefetched.remove(&url) {
            self.fulfill(&req.id, &ex).await;
            self.doc_done |= is_doc;
            return Ok(());
        }
        if let Err(b) = decide::precheck_with(
            &*self.deps.policy,
            self.permit.scope(),
            &req,
            &url,
            is_doc,
            self.deps.filter.as_ref(),
        ) {
            return self.refuse(&req, url, is_doc, b).await;
        }
        if let Err(b) = self.robots(&url).await {
            return self.refuse(&req, url, is_doc, b).await;
        }
        if !is_doc
            && req.kind.cacheable()
            && let Some(ex) = self.deps.cache.get(url.as_str())
        {
            self.fulfill(&req.id, &ex).await;
            self.exchanges.push(ex);
            return Ok(());
        }

        // Budgets: time, count, bytes, then the per-key gap.
        let budget_block = Blocked {
            reason: BlockReason::Budget,
            error: Some(FetchError::DeadlineExceeded),
        };
        if self.expired() {
            return self.refuse(&req, url, is_doc, budget_block).await;
        }
        let key = url.politeness_key();
        if !is_doc {
            if self.subrequests >= self.budget.max_subrequests
                || self.bytes >= self.budget.max_bytes
            {
                return self.refuse(&req, url, is_doc, budget_block).await;
            }
            if let Some(last) = self.last_end.get(&key) {
                let at = *last + self.budget.subresource_gap;
                if at >= self.deadline {
                    return self.refuse(&req, url, is_doc, budget_block).await;
                }
                sleep_until(at).await;
            }
        }

        let mut fr = FetchRequest::get(
            url.clone(),
            if is_doc {
                RequestPurpose::Page
            } else {
                RequestPurpose::SubResource
            },
        );
        fr.accept = req.accept.clone();
        fr.timeout = Some(self.deadline.saturating_duration_since(Instant::now()));
        if !is_doc {
            fr.max_body_bytes = self.budget.max_bytes - self.bytes;
            self.subrequests += 1;
        }
        // Third-party host: one request at a time per host in this process, `subresource_gap`
        // apart, then the optional cross-process borrow.
        let third_party = !is_doc && !self.permit.scope().covers(&url);
        let mut host_slot = None;
        if third_party {
            let slot = third_party_slot(url.host());
            let Ok(g) = tokio::time::timeout_at(self.deadline, slot.lock_owned()).await else {
                self.subrequests -= 1;
                return self.refuse(&req, url, is_doc, budget_block).await;
            };
            if let Some(last) = *g {
                let at = last + self.budget.subresource_gap;
                if at >= self.deadline {
                    self.subrequests -= 1;
                    return self.refuse(&req, url, is_doc, budget_block).await;
                }
                sleep_until(at).await;
            }
            host_slot = Some(g);
        }
        let borrowed = match (&self.deps.borrow, third_party) {
            (Some(b), true) => b.borrow(self.permit, &url, self.deadline).await.map(Some),
            _ => Ok(None),
        };
        let res = match borrowed {
            Ok(guard) => {
                let r = self.deps.client.fetch(self.permit, fr).await;
                drop(guard);
                r
            }
            Err(e) => Err(e),
        };
        let ended = Instant::now();
        if let Some(mut g) = host_slot {
            *g = Some(ended);
        }
        self.last_end.insert(key, ended);

        let ex = match res {
            Ok(ex) => ex,
            Err(e) => {
                let in_scope = self.permit.scope().covers(&url);
                let fatal = is_doc
                    || matches!(e, FetchError::Lease(_) | FetchError::DeadlineExceeded)
                    || (in_scope && matches!(e, FetchError::Challenge { .. }));
                if fatal {
                    self.fail(&req.id, &decide::reason_for(&e)).await;
                    return Err(Error::Fetch(e));
                }
                let reason = decide::reason_for(&e);
                return self
                    .refuse(
                        &req,
                        url,
                        false,
                        Blocked {
                            reason,
                            error: Some(e),
                        },
                    )
                    .await;
            }
        };

        // A challenge, 401 or 403 under the page's key: detect → park (never retried); a 429 or
        // 503 there: stop and let the caller back the key off. From an allowlisted third party
        // either only fails that sub-request.
        let status = ex.status.as_u16();
        if status == 401 || status == 403 || self.deps.policy.is_challenge(&ex) {
            let in_scope = self.permit.scope().covers(&ex.url);
            self.exchanges.push(ex);
            if in_scope {
                self.fail(&req.id, &BlockReason::FetchFailed).await;
                return Err(Error::Fetch(FetchError::Challenge { status }));
            }
            return self
                .refuse(
                    &req,
                    url,
                    false,
                    Blocked {
                        reason: BlockReason::FetchFailed,
                        error: None,
                    },
                )
                .await;
        }
        if status == 429 || status == 503 {
            if self.permit.scope().covers(&ex.url) {
                self.fail(&req.id, &BlockReason::FetchFailed).await;
                let retry_after = ex
                    .response_headers
                    .get(http::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| parse_retry_after(v, self.deps.clock.now()));
                return Err(Error::Fetch(FetchError::RateLimited {
                    status,
                    retry_after,
                }));
            }
            self.exchanges.push(ex);
            return self
                .refuse(
                    &req,
                    url,
                    false,
                    Blocked {
                        reason: BlockReason::FetchFailed,
                        error: None,
                    },
                )
                .await;
        }

        if !is_doc {
            self.bytes += ex.body.len() as u64;
        }
        if ex.url != url {
            // The permit client followed a same-key redirect inline. Let Chromium follow it
            // too (so relative URLs resolve against the final URL), served from memory.
            let redirect = Fulfilment {
                status: 302,
                headers: vec![("location".into(), ex.url.as_str().to_string())],
                body: Bytes::new(),
            };
            if is_doc {
                self.expect_doc = ex.url.clone();
            }
            self.prefetched.insert(ex.url.clone(), ex.clone());
            self.exchanges.push(ex);
            if let Err(e) = self.tab.fulfill(&req.id, redirect).await {
                debug!(error = %e, "render: fulfillRequest (redirect)");
            }
            return Ok(());
        }
        if !is_doc && req.kind.cacheable() {
            self.deps.cache.put(url.as_str(), &ex);
        }
        self.fulfill(&req.id, &ex).await;
        self.exchanges.push(ex);
        self.doc_done |= is_doc;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn retry_after_seconds_and_dates() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 11, 16, 12, 0, 0)
            .unwrap();
        assert_eq!(
            parse_retry_after("120", now),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Mon, 16 Nov 2026 12:01:00 GMT", now),
            Some(std::time::Duration::from_secs(60))
        );
        assert_eq!(
            parse_retry_after("Mon, 16 Nov 2026 11:00:00 GMT", now),
            Some(std::time::Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
    }
}
