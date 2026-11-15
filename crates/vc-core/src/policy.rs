//! Politeness and compliance decisions, **pure** (no I/O; r5 Q4). Implemented by `vc-policy`.
//!
//! The fetcher fetches robots.txt itself (expired origins come back in
//! [`crate::frontier::Claim::robots_due`]), spools it raw-first and stores the result with
//! [`crate::frontier::Frontier::store_robots`]; everything here only *decides*.

use std::net::IpAddr;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::fetch::Exchange;
use crate::ids::OriginKey;
use crate::url::CanonicalUrl;
use crate::usepolicy::{UsePolicy, UseSignals};

/// The outcome of the last robots.txt fetch for an origin (RFC 9309 §2.3.1 + our 401/403 rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RobotsStatus {
    /// 2xx: parse the body.
    Ok,
    /// 404/410 (and other 4xx except 401/403/429): allow all.
    NotFound,
    /// 401/403: the whole key is do-not-crawl (our stricter rule).
    Unauthorized,
    /// The robots.txt response was a challenge page: the whole key is do-not-crawl.
    Challenge,
    /// 5xx, 429 or a network error: disallow all; retry with back-off; after 30 days fall back
    /// to the last good copy.
    Unavailable,
}

/// A cached robots.txt entry for an origin (`origins` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotsEntry {
    /// The origin.
    pub origin: OriginKey,
    /// Outcome of the last fetch.
    pub status: RobotsStatus,
    /// HTTP status of the last fetch, if any.
    pub http_status: Option<u16>,
    /// The first 500 KiB of the body (when `status == Ok`).
    pub body: Option<Bytes>,
    /// The last successfully parsed body and when it was fetched (for the 30-day 5xx rule).
    pub last_good: Option<(Bytes, DateTime<Utc>)>,
    /// When it was fetched.
    pub fetched_at: DateTime<Utc>,
    /// When it must be refreshed (fetched_at + 24 h by default).
    pub expires_at: DateTime<Utc>,
}

impl RobotsEntry {
    /// Whether the entry is still fresh at `now`.
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        now < self.expires_at
    }
}

/// The verdict for one URL against an origin's robots.txt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotsVerdict {
    /// Crawling allowed.
    Allowed {
        /// `Crawl-delay` of the matched group, if any.
        crawl_delay: Option<Duration>,
        /// The AI-bot block applies (AI reservation, see [`UseSignals::ai_bot_blocked`]).
        ai_reserved: bool,
    },
    /// This path is disallowed.
    Disallowed,
    /// The whole politeness key is do-not-crawl (401/403 or challenge on robots.txt): park it.
    KeyBlocked,
    /// robots.txt is temporarily unavailable (5xx/network) and there is no usable last good
    /// copy: treat as disallowed, retry later.
    Unavailable,
}

impl RobotsVerdict {
    /// Whether a request may be made.
    pub fn allows(&self) -> bool {
        matches!(self, RobotsVerdict::Allowed { .. })
    }
}

/// Origin-level facts parsed from robots.txt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RobotsSummary {
    /// `Crawl-delay` for our group (or `*`).
    pub crawl_delay: Option<Duration>,
    /// `Sitemap:` lines.
    pub sitemaps: Vec<Url>,
    /// The site blocks the major AI crawlers but allows `*` (AI reservation).
    pub ai_reserved: bool,
    /// The whole key must be blocked (401/403/challenge).
    pub key_blocked: bool,
}

/// Why a URL is on the denylist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenyReason {
    /// The matching denylist host (registrable domain or host) and optional path prefix.
    pub rule: String,
    /// The reason recorded in `policy/denylist.yaml`.
    pub reason: String,
}

/// Why a host/IP/port was blocked by the SSRF guard (spec §5.2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("ssrf blocked {host} -> {ip}:{port}: {rule}")]
pub struct SsrfBlock {
    /// The requested host.
    pub host: String,
    /// The resolved (pinned) IP.
    pub ip: IpAddr,
    /// The port.
    pub port: u16,
    /// The matching rule, e.g. `loopback`, `rfc1918`, `link_local`, `cgnat`, `ula`,
    /// `ipv4_mapped`, `metadata`, `port`.
    pub rule: String,
}

/// The pure politeness/compliance policy. One instance is built from config + policy files at
/// startup and shared (`Arc<dyn Policy>`).
pub trait Policy: Send + Sync {
    /// The robots verdict for `url` (whose origin must equal `r.origin`), for the
    /// `VoyagerBot` group, else `*`.
    fn robots_verdict(&self, r: &RobotsEntry, url: &CanonicalUrl) -> RobotsVerdict;

    /// Origin-level facts from robots.txt (crawl delay, sitemaps, AI reservation, key block).
    fn robots_summary(&self, r: &RobotsEntry) -> RobotsSummary;

    /// Of `urls`, those allowed under `old` but disallowed under `new` (robots refresh →
    /// tombstone derived records; Review Focus 1).
    fn newly_disallowed(
        &self,
        old: &RobotsEntry,
        new: &RobotsEntry,
        urls: &[CanonicalUrl],
    ) -> Vec<CanonicalUrl>;

    /// The use policy for a set of signals. Implementations must return
    /// [`UsePolicy::from_signals`] or something below it (only ever restrict).
    fn use_policy(&self, s: &UseSignals) -> UsePolicy;

    /// Whether `url` is denylisted (registrable domain + optional path prefix, incl. brand
    /// domains).
    fn denied(&self, url: &CanonicalUrl) -> Option<DenyReason>;

    /// Whether a third-party host is on `policy/subresource_allowlist.yaml` (render
    /// sub-resources only).
    fn subresource_host_allowed(&self, host: &str) -> bool;

    /// The SSRF guard for one resolved, pinned hop (checked once per hop after DNS).
    fn ssrf_check(&self, host: &str, ip: IpAddr, port: u16) -> Result<(), SsrfBlock>;

    /// Whether a response is a bot challenge / interstitial (status + `cf-mitigated`, known
    /// provider markers, captcha forms). A challenge means **detect → park**, never retry.
    fn is_challenge(&self, ex: &Exchange) -> bool;
}

/// One entry of `policy/denylist.yaml` (`{host, reason, source, path_prefix?, except_hosts?}`).
///
/// `host` matches that host and every subdomain. `<brand>.*` matches the brand label directly
/// under **any** public suffix (`expedia.*` matches `expedia.com`, `www.expedia.co.uk`).
/// `except_hosts` (and their subdomains) are not covered by the entry (partner APIs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DenyEntry {
    /// Host or `<brand>.*`.
    pub host: String,
    /// Why it is denied.
    pub reason: String,
    /// Where the decision comes from (`seed:<id>`, `sources.md §N`, `brand`).
    pub source: String,
    /// Only URLs whose path starts with this prefix are denied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Hosts excluded from this entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub except_hosts: Vec<String>,
}

impl DenyEntry {
    /// Whether the entry is a brand wildcard (`<brand>.*`); returns the brand label.
    pub fn brand(&self) -> Option<&str> {
        self.host.strip_suffix(".*")
    }
}

/// The whole `policy/denylist.yaml` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Denylist {
    /// File format version (1).
    pub version: u32,
    /// Entries.
    pub entries: Vec<DenyEntry>,
}
