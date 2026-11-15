//! Permit-gated HTTP (spec §5.2, r5 Q2/Q8). Implemented by `vc-fetch`
//! (`ReqwestPermitClient`).
//!
//! Every request: lease check + global rate token + pacing by the key's delay since the last
//! request end; SSRF check on the pinned IP of every hop; robots check of every hop from the
//! `origins` cache; ≤ 5 redirects, and a **cross-key redirect is never followed** (it returns
//! [`FetchError::CrossKeyRedirect`] and the caller enqueues the target); streaming byte caps
//! after decompression (incl. nested gzip); the content-type allowlist; timeouts clamped to the
//! lease deadline; the one [`crate::USER_AGENT`]. A challenge returns
//! [`FetchError::Challenge`]: **detect → park**, never retry with another identity.

use std::net::IpAddr;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::Stream;
use http::{HeaderMap, Method, StatusCode};
use serde::{Deserialize, Serialize};

use crate::error::{ErrorClass, LeaseLost};
use crate::frontier::{ClaimPermit, Validators};
use crate::ids::OriginKey;
use crate::policy::{DenyReason, SsrfBlock};
use crate::url::CanonicalUrl;

/// Why a request is made (drives caps, accept headers and metrics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    /// A page (HTML or other document).
    Page,
    /// robots.txt (first 500 KiB kept).
    Robots,
    /// `/.well-known/tdmrep.json`.
    TdmRep,
    /// A sitemap or sitemap index.
    Sitemap,
    /// An RSS/Atom feed.
    Feed,
    /// An API call by an adapter.
    Api,
    /// A dump download (streamed).
    Dump,
    /// A render sub-request (script, style, XHR).
    SubResource,
}

/// One GET request.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// The URL (must be covered by the permit's scope, or be an allowlisted sub-resource).
    pub url: CanonicalUrl,
    /// Purpose.
    pub purpose: RequestPurpose,
    /// Validators for a conditional GET (from the last committed capture).
    pub validators: Validators,
    /// Cap on decoded body bytes (0 = the configured default for the purpose).
    pub max_body_bytes: u64,
    /// `Accept` header override.
    pub accept: Option<String>,
    /// Extra request headers (e.g. API keys). `User-Agent` is always overwritten with
    /// [`crate::USER_AGENT`]; `Cookie` is never sent.
    pub headers: HeaderMap,
    /// Own timeout (clamped to the lease; `None` = configured default, 30 s).
    pub timeout: Option<Duration>,
}

impl FetchRequest {
    /// A plain GET for `url` with defaults.
    pub fn get(url: CanonicalUrl, purpose: RequestPurpose) -> Self {
        FetchRequest {
            url,
            purpose,
            validators: Validators::default(),
            max_body_bytes: 0,
            accept: None,
            headers: HeaderMap::new(),
            timeout: None,
        }
    }
}

/// A redirect hop that was followed inline (same key, robots-allowed, SSRF-checked).
#[derive(Debug, Clone)]
pub struct RedirectHop {
    /// The requested URL of this hop.
    pub url: CanonicalUrl,
    /// Its 3xx status.
    pub status: StatusCode,
    /// The pinned peer IP.
    pub peer_ip: Option<IpAddr>,
}

/// One HTTP exchange. Bodies are **content-decoded** (r5 Q2): `Content-Encoding` is dropped,
/// the original encoding kept in `original_encoding`, and the payload digest is SHA-256 of the
/// decoded body. `vc-store::warc` alone serialises exchanges into WARC records.
#[derive(Debug, Clone)]
pub struct Exchange {
    /// The URL finally requested (after inline same-key redirects).
    pub url: CanonicalUrl,
    /// Redirect hops followed before `url`.
    pub redirects: Vec<RedirectHop>,
    /// Request method (GET).
    pub method: Method,
    /// Request headers as sent (incl. `User-Agent`).
    pub request_headers: HeaderMap,
    /// Response status.
    pub status: StatusCode,
    /// Response headers as received (`Content-Encoding` removed after decoding).
    pub response_headers: HeaderMap,
    /// The decoded body (possibly truncated, see `truncated`).
    pub body: Bytes,
    /// The `Content-Encoding` the server used, if any.
    pub original_encoding: Option<String>,
    /// Whether the body was truncated at a purpose cap that allows truncation (robots.txt).
    pub truncated: bool,
    /// The pinned peer IP.
    pub peer_ip: Option<IpAddr>,
    /// When the request started (local clock).
    pub started_at: DateTime<Utc>,
    /// When the response finished (local clock).
    pub ended_at: DateTime<Utc>,
    /// Purpose.
    pub purpose: RequestPurpose,
}

impl Exchange {
    /// The `Content-Type` header, if any.
    pub fn content_type(&self) -> Option<&str> {
        self.response_headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
    }

    /// The validators the server returned.
    pub fn validators(&self) -> Validators {
        let h = |n: http::header::HeaderName| {
            self.response_headers
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        Validators {
            etag: h(http::header::ETAG),
            last_modified: h(http::header::LAST_MODIFIED),
        }
    }

    /// Wall time of the exchange.
    pub fn latency(&self) -> Duration {
        (self.ended_at - self.started_at)
            .to_std()
            .unwrap_or_default()
    }
}

/// The response head of a streamed fetch.
#[derive(Debug, Clone)]
pub struct ResponseHead {
    /// The URL finally requested.
    pub url: CanonicalUrl,
    /// Redirect hops followed.
    pub redirects: Vec<RedirectHop>,
    /// Request headers as sent.
    pub request_headers: HeaderMap,
    /// Status.
    pub status: StatusCode,
    /// Headers (`Content-Encoding` removed; the stream yields decoded bytes).
    pub headers: HeaderMap,
    /// The original `Content-Encoding`.
    pub original_encoding: Option<String>,
    /// Pinned peer IP.
    pub peer_ip: Option<IpAddr>,
    /// Start time.
    pub started_at: DateTime<Utc>,
}

/// A streamed, decoded, byte-capped body (dumps, large API pages). The stream yields
/// [`FetchError::BodyTooLarge`] when the cap is exceeded and [`FetchError::Lease`] when the lease
/// is lost mid-stream (adapters must call `Frontier::extend` between chunks).
pub struct LimitedBodyStream {
    /// The response head.
    pub head: ResponseHead,
    /// Decoded body chunks.
    pub body: Pin<Box<dyn Stream<Item = Result<Bytes, FetchError>> + Send>>,
}

impl std::fmt::Debug for LimitedBodyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LimitedBodyStream")
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

/// Why a permit-gated fetch failed.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum FetchError {
    /// The lease is gone.
    #[error(transparent)]
    Lease(#[from] LeaseLost),
    /// The lease deadline left no time for the request.
    #[error("deadline exceeded")]
    DeadlineExceeded,
    /// A redirect to another politeness key: **not followed**; the caller enqueues the target.
    #[error("cross-key redirect to {0}")]
    CrossKeyRedirect(CanonicalUrl),
    /// More than 5 redirects.
    #[error("too many redirects")]
    TooManyRedirects,
    /// The URL (or a redirect hop) is outside the permit's scope.
    #[error("out of permit scope: {0}")]
    OutOfScope(CanonicalUrl),
    /// robots.txt disallows this URL (or a hop).
    #[error("robots disallowed: {0}")]
    RobotsDisallowed(CanonicalUrl),
    /// The origin's robots.txt is missing or stale in the cache: fail closed, enqueue a
    /// refresh.
    #[error("robots unavailable for {0:?}")]
    RobotsUnavailable(OriginKey),
    /// Denylisted.
    #[error("denylisted: {0:?}")]
    Denied(DenyReason),
    /// Blocked by the SSRF guard.
    #[error(transparent)]
    Ssrf(#[from] SsrfBlock),
    /// A challenge / interstitial / 401 / 403 was detected: park the key.
    #[error("challenge detected (status {status})")]
    Challenge {
        /// The HTTP status of the challenge response.
        status: u16,
    },
    /// The decoded body exceeded its cap.
    #[error("body exceeds {limit} bytes")]
    BodyTooLarge {
        /// The cap.
        limit: u64,
    },
    /// The `Content-Type` is not on the allowlist.
    #[error("content type not allowed: {0}")]
    ContentTypeNotAllowed(String),
    /// A timeout (connect, read or total).
    #[error("timeout")]
    Timeout,
    /// DNS resolution failed.
    #[error("dns: {0}")]
    Dns(String),
    /// Connection / TLS / protocol failure.
    #[error("transport: {0}")]
    Transport(String),
}

impl FetchError {
    /// The coarse class.
    pub fn class(&self) -> ErrorClass {
        match self {
            FetchError::Lease(_) | FetchError::DeadlineExceeded => ErrorClass::Lease,
            FetchError::CrossKeyRedirect(_)
            | FetchError::TooManyRedirects
            | FetchError::BodyTooLarge { .. }
            | FetchError::ContentTypeNotAllowed(_) => ErrorClass::Permanent,
            FetchError::OutOfScope(_)
            | FetchError::RobotsDisallowed(_)
            | FetchError::RobotsUnavailable(_)
            | FetchError::Denied(_)
            | FetchError::Ssrf(_)
            | FetchError::Challenge { .. } => ErrorClass::Policy,
            FetchError::Timeout | FetchError::Dns(_) | FetchError::Transport(_) => {
                ErrorClass::Transient
            }
        }
    }
}

/// The only way crawl-target bytes enter the system. Every method takes the permit.
#[async_trait]
pub trait PermitClient: Send + Sync {
    /// Fetches into memory (decoded, capped). HTTP error statuses (404, 429, 5xx) are returned
    /// as `Ok(Exchange)`; only policy, lease and transport failures are errors.
    async fn fetch(&self, p: &ClaimPermit, req: FetchRequest) -> Result<Exchange, FetchError>;

    /// Streams the decoded body (dumps, large API responses) under the same rules.
    async fn stream(
        &self,
        p: &ClaimPermit,
        req: FetchRequest,
    ) -> Result<LimitedBodyStream, FetchError>;
}
