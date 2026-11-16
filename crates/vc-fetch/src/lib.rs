//! # vc-fetch (Task 4)
//!
//! [`ReqwestPermitClient`]: the only path for crawl-target bytes (spec §5.2, §17.2).
//!
//! For every hop (the requested URL and each inline redirect), in this order:
//!
//! 1. **Scope**: the URL must be covered by the permit (its key or the source's extra hosts),
//!    or, for render sub-requests, be on the sub-resource allowlist. The first URL out of scope
//!    is [`FetchError::OutOfScope`]; a redirect out of scope is
//!    [`FetchError::CrossKeyRedirect`] and is **never followed** (the caller enqueues it).
//!    Denylisted hosts and private IP literals are refused before that, so they are never
//!    handed back for enqueueing.
//! 2. **robots.txt** of the hop's origin, from the frontier's cache; missing or stale fails
//!    closed with [`FetchError::RobotsUnavailable`] (robots.txt requests themselves skip this).
//! 3. **DNS once per hop**: every resolved address goes through `Policy::ssrf_check`; the first
//!    one is pinned for the connection (reqwest `resolve` override, proxies disabled).
//! 4. **Pacing**: wait until `last request end + permit delay`.
//! 5. **Lease check + global rate token** (`LeaseCheck::check`, `Frontier::take_rate_token`).
//! 6. The request, with timeouts `min(own, deadline − now)`, the one [`vc_core::USER_AGENT`],
//!    no cookies, no automatic redirects, retries or decompression.
//!
//! Responses: 401/403 and anything `Policy::is_challenge` flags are
//! [`FetchError::Challenge`] (detect → park; never retried). Bodies are content-decoded here
//! (gzip, deflate) under a decoded-byte cap, and nested `.gz` payloads are probed against the
//! same cap (up to `max_nested_gzip` layers). The content-type allowlist applies to 2xx
//! responses. robots.txt bodies are truncated at 500 KiB instead of failing.
//!
//! Streams ([`PermitClient::stream`]) check the permit's current lease deadline before every
//! chunk (fail closed) through a [`vc_core::frontier::DeadlineHandle`], so a `Frontier::extend`
//! made while a long dump streams is seen; the end of the body is recorded on the permit for
//! pacing.

mod decode;
mod resolve;
mod retry;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use vc_config::FetchConfig;
use vc_core::error::LeaseLostReason;
use vc_core::fetch::{
    Exchange, FetchError, FetchRequest, LimitedBodyStream, PermitClient, RedirectHop,
    RequestPurpose, ResponseHead,
};
use vc_core::frontier::{ClaimPermit, DeadlineHandle, Frontier};
use vc_core::policy::{Policy, RobotsVerdict};
use vc_core::{CanonicalUrl, Clock, LeaseLost, SystemClock, USER_AGENT};

use crate::decode::{BodyDecoder, DecodeError, supported_encoding};
pub use crate::resolve::{HickoryResolver, Resolve, StaticResolver};
pub use crate::retry::{parse_retry_after, retry_after};

/// robots.txt bodies are kept up to this many bytes (RFC 9309 asks for at least 500 KiB).
pub const ROBOTS_MAX_BYTES: u64 = 500 * 1024;

/// The `Accept-Encoding` we send: only what [`ReqwestPermitClient`] decodes itself.
const ACCEPT_ENCODING: &str = "gzip, deflate";

/// Request headers callers may not set (we set or forbid them).
const RESERVED_HEADERS: &[HeaderName] = &[
    header::USER_AGENT,
    header::COOKIE,
    header::HOST,
    header::ACCEPT_ENCODING,
    header::IF_NONE_MATCH,
    header::IF_MODIFIED_SINCE,
    header::REFERER,
    header::CONNECTION,
    header::PROXY_AUTHORIZATION,
];

/// The production permit client.
#[derive(Clone)]
pub struct ReqwestPermitClient {
    /// Settings.
    pub config: FetchConfig,
    /// Lease checks, rate tokens, robots cache.
    pub frontier: Arc<dyn Frontier>,
    /// Policy decisions.
    pub policy: Arc<dyn Policy>,
    clock: Arc<dyn Clock>,
    resolver: Arc<dyn Resolve>,
}

impl std::fmt::Debug for ReqwestPermitClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestPermitClient")
            .field("config", &self.config)
            .field("resolver", &self.resolver)
            .finish_non_exhaustive()
    }
}

/// The final (non-redirect) response of a hop chain, body not yet read.
struct Opened {
    url: CanonicalUrl,
    redirects: Vec<RedirectHop>,
    request_headers: HeaderMap,
    response: reqwest::Response,
    headers: HeaderMap,
    status: StatusCode,
    encoding: Option<String>,
    peer_ip: IpAddr,
    started_at: DateTime<Utc>,
    /// When this hop's own timeout ends (monotonic).
    hop_deadline: tokio::time::Instant,
    /// Decoded-byte cap for the body.
    limit: u64,
    /// Whether exceeding `limit` truncates (robots.txt) rather than fails.
    truncate: bool,
}

/// What one response looked like, for building exchanges.
struct Seen<'a> {
    url: &'a CanonicalUrl,
    redirects: &'a [RedirectHop],
    request_headers: &'a HeaderMap,
    status: StatusCode,
    headers: &'a HeaderMap,
    peer_ip: IpAddr,
    started_at: DateTime<Utc>,
    purpose: RequestPurpose,
}

impl ReqwestPermitClient {
    /// Creates the client (system clock, system DNS resolver).
    pub fn new(config: FetchConfig, frontier: Arc<dyn Frontier>, policy: Arc<dyn Policy>) -> Self {
        ReqwestPermitClient {
            config,
            frontier,
            policy,
            clock: Arc::new(SystemClock),
            resolver: Arc::new(HickoryResolver::new()),
        }
    }

    /// Uses `clock` for deadlines and timestamps (tests: `testkit::FakeClock`).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Uses `resolver` for DNS (tests and the E2E fake web: [`StaticResolver`]).
    pub fn with_resolver(mut self, resolver: Arc<dyn Resolve>) -> Self {
        self.resolver = resolver;
        self
    }

    fn in_scope(&self, p: &ClaimPermit, url: &CanonicalUrl, purpose: RequestPurpose) -> bool {
        p.scope().covers(url)
            || (purpose == RequestPurpose::SubResource
                && self.policy.subresource_host_allowed(url.host()))
    }

    /// The SSRF check for an IP-literal host (no DNS involved), if the host is one.
    fn check_ip_literal(&self, url: &CanonicalUrl) -> Result<Option<IpAddr>, FetchError> {
        let Some(ip) = ip_literal(url) else {
            return Ok(None);
        };
        self.policy.ssrf_check(url.host(), ip, url.port())?;
        Ok(Some(ip))
    }

    /// Checks a redirect target before anything else: denylist, private IP literals, scope.
    fn check_redirect(
        &self,
        p: &ClaimPermit,
        url: &CanonicalUrl,
        purpose: RequestPurpose,
    ) -> Result<(), FetchError> {
        if let Some(reason) = self.policy.denied(url) {
            return Err(FetchError::Denied(reason));
        }
        self.check_ip_literal(url)?;
        if !self.in_scope(p, url, purpose) {
            return Err(FetchError::CrossKeyRedirect(url.clone()));
        }
        Ok(())
    }

    /// robots.txt for one hop, from the cache (fail closed when missing or stale).
    async fn check_robots(&self, url: &CanonicalUrl) -> Result<(), FetchError> {
        let origin = url.origin();
        let entry = match self.frontier.robots(&origin).await {
            Ok(Some(e)) if e.is_fresh(self.clock.now()) => e,
            _ => return Err(FetchError::RobotsUnavailable(origin)),
        };
        match self.policy.robots_verdict(&entry, url) {
            RobotsVerdict::Allowed { .. } => Ok(()),
            RobotsVerdict::Disallowed | RobotsVerdict::KeyBlocked => {
                Err(FetchError::RobotsDisallowed(url.clone()))
            }
            RobotsVerdict::Unavailable => Err(FetchError::RobotsUnavailable(origin)),
        }
    }

    /// Resolves the hop's host once, SSRF-checks every address, and returns the pinned one.
    async fn pin(&self, url: &CanonicalUrl) -> Result<IpAddr, FetchError> {
        if let Some(ip) = self.check_ip_literal(url)? {
            return Ok(ip);
        }
        let host = url.host();
        let ips = self.resolver.resolve(host).await?;
        let Some(first) = ips.first().copied() else {
            return Err(FetchError::Dns(format!("no address for {host}")));
        };
        for ip in &ips {
            self.policy.ssrf_check(host, *ip, url.port())?;
        }
        Ok(first)
    }

    /// Pacing, then lease check + rate token. Returns the hop timeout
    /// `min(own, deadline − now)`.
    async fn admit(&self, p: &ClaimPermit, own: Duration) -> Result<Duration, FetchError> {
        if let Some(end) = p.last_request_end() {
            let wait = p.delay().saturating_sub(end.elapsed());
            if !wait.is_zero() {
                if wait >= p.remaining(self.clock.now()) {
                    return Err(FetchError::DeadlineExceeded);
                }
                tokio::time::sleep(wait).await;
            }
        }
        self.frontier.check(p).await?;
        self.frontier.take_rate_token(p).await?;
        let t = p.clamp_timeout(own, self.clock.now());
        if t.is_zero() {
            return Err(FetchError::DeadlineExceeded);
        }
        Ok(t)
    }

    fn request_headers(
        &self,
        req: &FetchRequest,
        url: &CanonicalUrl,
        first: bool,
    ) -> Result<HeaderMap, FetchError> {
        let mut h = req.headers.clone();
        for name in RESERVED_HEADERS {
            h.remove(name);
        }
        if url.host() != req.url.host() {
            // Credentials are for the requested host only, even within one key.
            h.remove(header::AUTHORIZATION);
        }
        h.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        h.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static(ACCEPT_ENCODING),
        );
        let accept = match &req.accept {
            Some(a) => HeaderValue::from_str(a)
                .map_err(|_| FetchError::Transport("invalid Accept header".into()))?,
            None => HeaderValue::from_static(default_accept(req.purpose)),
        };
        h.insert(header::ACCEPT, accept);
        if first {
            let v = &req.validators;
            if let Some(etag) = v
                .etag
                .as_deref()
                .and_then(|e| HeaderValue::from_str(e).ok())
            {
                h.insert(header::IF_NONE_MATCH, etag);
            }
            if let Some(lm) = v
                .last_modified
                .as_deref()
                .and_then(|e| HeaderValue::from_str(e).ok())
            {
                h.insert(header::IF_MODIFIED_SINCE, lm);
            }
        }
        Ok(h)
    }

    fn http_client(
        &self,
        url: &CanonicalUrl,
        ip: IpAddr,
        timeout: Duration,
        streaming: bool,
    ) -> Result<reqwest::Client, FetchError> {
        let connect = Duration::from_secs(self.config.connect_timeout_s).min(timeout);
        let mut b = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .referer(false)
            .user_agent(USER_AGENT)
            .connect_timeout(connect)
            .read_timeout(timeout);
        if !streaming {
            b = b.timeout(timeout);
        }
        if ip_literal(url).is_none() {
            b = b.resolve(url.host(), SocketAddr::new(ip, url.port()));
        }
        b.build().map_err(|e| FetchError::Transport(err_text(&e)))
    }

    fn body_limit(&self, req: &FetchRequest, headers: &HeaderMap) -> (u64, bool) {
        if req.purpose == RequestPurpose::Robots {
            let cap = if req.max_body_bytes > 0 {
                req.max_body_bytes.min(ROBOTS_MAX_BYTES)
            } else {
                ROBOTS_MAX_BYTES
            };
            return (cap, true);
        }
        if req.max_body_bytes > 0 {
            return (req.max_body_bytes, false);
        }
        let html = content_essence(headers)
            .is_some_and(|e| e == "text/html" || e == "application/xhtml+xml");
        if html {
            (self.config.html_max_bytes, false)
        } else {
            (self.config.other_max_bytes, false)
        }
    }

    fn check_content_type(
        &self,
        purpose: RequestPurpose,
        status: StatusCode,
        headers: &HeaderMap,
    ) -> Result<(), FetchError> {
        if purpose == RequestPurpose::Robots || !status.is_success() {
            return Ok(());
        }
        let essence = content_essence(headers).unwrap_or_default();
        let allowed = |list: &[String]| list.iter().any(|t| t.eq_ignore_ascii_case(&essence));
        if allowed(&self.config.allowed_content_types)
            || (purpose == RequestPurpose::SubResource
                && allowed(&self.config.subresource_content_types))
        {
            Ok(())
        } else {
            Err(FetchError::ContentTypeNotAllowed(essence))
        }
    }

    fn exchange(&self, s: &Seen<'_>, body: Bytes) -> Exchange {
        Exchange {
            url: s.url.clone(),
            redirects: s.redirects.to_vec(),
            method: Method::GET,
            request_headers: s.request_headers.clone(),
            status: s.status,
            response_headers: s.headers.clone(),
            body,
            original_encoding: None,
            truncated: false,
            peer_ip: Some(s.peer_ip),
            started_at: s.started_at,
            ended_at: self.clock.now(),
            purpose: s.purpose,
        }
    }

    /// Runs the hop chain up to the final response (body unread).
    async fn open(
        &self,
        p: &ClaimPermit,
        req: &FetchRequest,
        streaming: bool,
    ) -> Result<Opened, FetchError> {
        let purpose = req.purpose;
        if let Some(reason) = self.policy.denied(&req.url) {
            return Err(FetchError::Denied(reason));
        }
        if !self.in_scope(p, &req.url, purpose) {
            return Err(FetchError::OutOfScope(req.url.clone()));
        }
        let own = req
            .timeout
            .unwrap_or(Duration::from_secs(self.config.request_timeout_s));
        let mut url = req.url.clone();
        let mut redirects: Vec<RedirectHop> = Vec::new();
        loop {
            let first = redirects.is_empty();
            if purpose != RequestPurpose::Robots {
                self.check_robots(&url).await?;
            }
            let ip = self.pin(&url).await?;
            let headers = self.request_headers(req, &url, first)?;
            let timeout = self.admit(p, own).await?;
            let hop_deadline = tokio::time::Instant::now() + timeout;
            let client = self.http_client(&url, ip, timeout, streaming)?;
            let started_at = self.clock.now();
            let sent = tokio::time::timeout_at(
                hop_deadline,
                client
                    .get(url.as_url().clone())
                    .headers(headers.clone())
                    .send(),
            )
            .await;
            let response = match sent {
                Err(_) => {
                    p.record_request_end(Instant::now());
                    return Err(FetchError::Timeout);
                }
                Ok(Err(e)) => {
                    p.record_request_end(Instant::now());
                    return Err(map_reqwest(&e));
                }
                Ok(Ok(r)) => r,
            };
            let status = response.status();
            let resp_headers = response.headers().clone();
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                p.record_request_end(Instant::now());
                return Err(FetchError::Challenge {
                    status: status.as_u16(),
                });
            }
            let seen = Seen {
                url: &url,
                redirects: &redirects,
                request_headers: &headers,
                status,
                headers: &resp_headers,
                peer_ip: ip,
                started_at,
                purpose,
            };
            if self
                .policy
                .is_challenge(&self.exchange(&seen, Bytes::new()))
            {
                p.record_request_end(Instant::now());
                return Err(FetchError::Challenge {
                    status: status.as_u16(),
                });
            }
            let location = resp_headers
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .filter(|_| is_redirect(status))
                .map(str::to_string);
            if let Some(location) = location {
                drop(response);
                p.record_request_end(Instant::now());
                let next = url
                    .join(&location)
                    .map_err(|e| FetchError::Transport(format!("bad redirect location: {e}")))?;
                if redirects.len() >= usize::from(self.config.max_redirects) {
                    return Err(FetchError::TooManyRedirects);
                }
                self.check_redirect(p, &next, purpose)?;
                redirects.push(RedirectHop {
                    url: url.clone(),
                    status,
                    peer_ip: Some(ip),
                });
                url = next;
                continue;
            }
            if let Err(e) = self.check_content_type(purpose, status, &resp_headers) {
                p.record_request_end(Instant::now());
                return Err(e);
            }
            let encoding = resp_headers
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty() && s != "identity");
            if let Some(enc) = &encoding
                && !supported_encoding(enc)
            {
                p.record_request_end(Instant::now());
                return Err(FetchError::Transport(format!(
                    "unsupported content-encoding {enc}"
                )));
            }
            let mut out_headers = resp_headers;
            if encoding.is_some() {
                out_headers.remove(header::CONTENT_ENCODING);
                out_headers.remove(header::CONTENT_LENGTH);
            }
            let (limit, truncate) = self.body_limit(req, &out_headers);
            return Ok(Opened {
                url,
                redirects,
                request_headers: headers,
                response,
                headers: out_headers,
                status,
                encoding,
                peer_ip: ip,
                started_at,
                hop_deadline,
                limit,
                truncate,
            });
        }
    }

    fn decoder(&self, o: &Opened, purpose: RequestPurpose) -> Result<BodyDecoder, FetchError> {
        let nested = (purpose != RequestPurpose::Robots).then_some(self.config.max_nested_gzip);
        BodyDecoder::new(o.encoding.as_deref(), o.limit, nested)
            .ok_or_else(|| FetchError::Transport("unsupported content-encoding".into()))
    }
}

/// Reads the whole body of `o` into `dec`; returns whether it was truncated.
async fn read_body(o: &mut Opened, dec: &mut BodyDecoder) -> Result<bool, FetchError> {
    while let Some(chunk) = o.response.chunk().await.map_err(|e| map_reqwest(&e))? {
        match dec.push(&chunk) {
            Ok(()) => {}
            Err(DecodeError::Over) if o.truncate => return Ok(true),
            Err(e) => return Err(decode_error(e, o.limit)),
        }
    }
    match dec.finish() {
        Ok(()) => Ok(false),
        Err(DecodeError::Over) if o.truncate => Ok(true),
        Err(e) => Err(decode_error(e, o.limit)),
    }
}

#[async_trait]
impl PermitClient for ReqwestPermitClient {
    async fn fetch(&self, p: &ClaimPermit, req: FetchRequest) -> Result<Exchange, FetchError> {
        let mut o = self.open(p, &req, false).await?;
        let mut dec = self.decoder(&o, req.purpose)?;
        let read = tokio::time::timeout_at(o.hop_deadline, read_body(&mut o, &mut dec)).await;
        p.record_request_end(Instant::now());
        let truncated = match read {
            Err(_) => return Err(FetchError::Timeout),
            Ok(r) => r?,
        };
        let seen = Seen {
            url: &o.url,
            redirects: &o.redirects,
            request_headers: &o.request_headers,
            status: o.status,
            headers: &o.headers,
            peer_ip: o.peer_ip,
            started_at: o.started_at,
            purpose: req.purpose,
        };
        let mut ex = self.exchange(&seen, Bytes::from(dec.take()));
        ex.original_encoding = o.encoding.clone();
        ex.truncated = truncated;
        if self.policy.is_challenge(&ex) {
            return Err(FetchError::Challenge {
                status: o.status.as_u16(),
            });
        }
        Ok(ex)
    }

    async fn stream(
        &self,
        p: &ClaimPermit,
        req: FetchRequest,
    ) -> Result<LimitedBodyStream, FetchError> {
        let o = self.open(p, &req, true).await?;
        p.record_request_end(Instant::now());
        let dec = self.decoder(&o, req.purpose)?;
        let head = ResponseHead {
            url: o.url.clone(),
            redirects: o.redirects.clone(),
            request_headers: o.request_headers.clone(),
            status: o.status,
            headers: o.headers.clone(),
            original_encoding: o.encoding.clone(),
            peer_ip: Some(o.peer_ip),
            started_at: o.started_at,
        };
        let own = req
            .timeout
            .unwrap_or(Duration::from_secs(self.config.request_timeout_s));
        let state = StreamState {
            response: o.response,
            dec,
            limit: o.limit,
            truncate: o.truncate,
            done: false,
            pending: None,
            clock: self.clock.clone(),
            deadline: p.deadline_handle(),
            lease_lost: p.lease_lost(LeaseLostReason::Expired),
            read_timeout: own,
        };
        let body = futures::stream::unfold(state, next_chunk).boxed();
        Ok(LimitedBodyStream { head, body })
    }
}

struct StreamState {
    response: reqwest::Response,
    dec: BodyDecoder,
    limit: u64,
    truncate: bool,
    done: bool,
    /// An error to yield after the bytes decoded before it.
    pending: Option<FetchError>,
    clock: Arc<dyn Clock>,
    /// The permit's shared deadline (sees later extensions) and pacing state (body end).
    deadline: DeadlineHandle,
    lease_lost: LeaseLost,
    read_timeout: Duration,
}

type Item = Result<Bytes, FetchError>;

fn end(mut s: StreamState, e: FetchError) -> Option<(Item, StreamState)> {
    s.done = true;
    s.deadline.record_request_end(Instant::now());
    Some((Err(e), s))
}

async fn next_chunk(mut s: StreamState) -> Option<(Item, StreamState)> {
    loop {
        if let Some(e) = s.pending.take() {
            return end(s, e);
        }
        if s.done {
            s.deadline.record_request_end(Instant::now());
            return None;
        }
        let remaining = s.deadline.remaining(s.clock.now());
        if remaining.is_zero() {
            let e = FetchError::Lease(s.lease_lost.clone());
            return end(s, e);
        }
        let wait = s.read_timeout.min(remaining);
        let chunk = match tokio::time::timeout(wait, s.response.chunk()).await {
            Err(_) => return end(s, FetchError::Timeout),
            Ok(Err(e)) => return end(s, map_reqwest(&e)),
            Ok(Ok(c)) => c,
        };
        let r = match &chunk {
            Some(c) => s.dec.push(c),
            None => {
                s.done = true;
                s.deadline.record_request_end(Instant::now());
                s.dec.finish()
            }
        };
        let out = s.dec.take();
        match r {
            Ok(()) => {}
            Err(DecodeError::Over) if s.truncate => s.done = true,
            Err(e) => s.pending = Some(decode_error(e, s.limit)),
        }
        if !out.is_empty() {
            return Some((Ok(Bytes::from(out)), s));
        }
    }
}

fn ip_literal(url: &CanonicalUrl) -> Option<IpAddr> {
    match url.as_url().host() {
        Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    }
}

fn is_redirect(s: StatusCode) -> bool {
    matches!(s.as_u16(), 301 | 302 | 303 | 307 | 308)
}

fn default_accept(purpose: RequestPurpose) -> &'static str {
    match purpose {
        RequestPurpose::Page => "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5",
        RequestPurpose::Robots => "text/plain,*/*;q=0.5",
        RequestPurpose::TdmRep | RequestPurpose::Api => "application/json,*/*;q=0.5",
        RequestPurpose::Sitemap | RequestPurpose::Feed => {
            "application/xml,text/xml,application/rss+xml,application/atom+xml,*/*;q=0.5"
        }
        RequestPurpose::Dump | RequestPurpose::SubResource => "*/*",
    }
}

/// The lowercase `Content-Type` essence (`type/subtype`, parameters dropped).
fn content_essence(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    let e = v.split(';').next().unwrap_or_default().trim();
    (!e.is_empty()).then(|| e.to_ascii_lowercase())
}

fn decode_error(e: DecodeError, limit: u64) -> FetchError {
    match e {
        DecodeError::Over => FetchError::BodyTooLarge { limit },
        DecodeError::Corrupt(m) => FetchError::Transport(format!("content decoding failed: {m}")),
    }
}

/// A reqwest error's cause chain without the URL (logs never carry query tokens).
fn err_text(e: &reqwest::Error) -> String {
    let mut s = String::new();
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(x) = src {
        if !s.is_empty() {
            s.push_str(": ");
        }
        s.push_str(&x.to_string());
        src = x.source();
    }
    if s.is_empty() {
        "request failed".into()
    } else {
        s
    }
}

fn map_reqwest(e: &reqwest::Error) -> FetchError {
    if e.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Transport(err_text(e))
    }
}
