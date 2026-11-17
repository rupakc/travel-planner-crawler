//! Fixtures for `vc-fetch` tests (owned by the vc-fetch task; add items here, never edit lib.rs).
//!
//! [`StubPolicy`] is a minimal, deterministic [`Policy`] so the permit client can be tested
//! without depending on `vc-policy`'s implementation. It is **not** a reference
//! implementation: robots matching is a plain prefix match on the `Disallow:` lines of every
//! group.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Mutex;

use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use vc_core::fetch::Exchange;
use vc_core::policy::{
    DenyReason, Policy, RobotsEntry, RobotsStatus, RobotsSummary, RobotsVerdict, SsrfBlock,
};
use vc_core::usepolicy::{UsePolicy, UseSignals};
use vc_core::{CanonicalUrl, OriginKey};

/// A minimal policy for fetch tests.
///
/// * SSRF: IPs added with [`StubPolicy::allow_ip`] pass (the wiremock loopback); otherwise
///   loopback, RFC 1918, link-local (incl. the metadata server), CGNAT, ULA, unspecified and
///   IPv4-mapped IPv6 are blocked. Ports are not checked (wiremock uses random ports).
/// * Denylist: exact hosts added with [`StubPolicy::deny_host`].
/// * Challenge: a `cf-mitigated: challenge` header or a body containing `cf-challenge`.
/// * Robots: prefix match of `Disallow:` lines; `Unauthorized` → `Disallowed` (that origin
///   only); `Challenge` → `KeyBlocked`;
///   `Unavailable` → `Unavailable`; `NotFound` → allowed.
#[derive(Debug, Default)]
pub struct StubPolicy {
    allowed_ips: Mutex<HashSet<IpAddr>>,
    denied_hosts: Mutex<HashSet<String>>,
    subresource_hosts: Mutex<HashSet<String>>,
    ssrf_checks: Mutex<Vec<(String, IpAddr, u16)>>,
}

impl StubPolicy {
    /// A policy that lets `127.0.0.1` through the SSRF guard (for wiremock).
    pub fn loopback_ok() -> Self {
        let p = StubPolicy::default();
        p.allow_ip(IpAddr::from([127, 0, 0, 1]));
        p
    }

    /// Lets `ip` pass the SSRF guard.
    pub fn allow_ip(&self, ip: IpAddr) {
        self.allowed_ips.lock().expect("lock").insert(ip);
    }

    /// Denylists an exact host.
    pub fn deny_host(&self, host: &str) {
        self.denied_hosts
            .lock()
            .expect("lock")
            .insert(host.to_ascii_lowercase());
    }

    /// Allowlists a third-party sub-resource host.
    pub fn allow_subresource_host(&self, host: &str) {
        self.subresource_hosts
            .lock()
            .expect("lock")
            .insert(host.to_ascii_lowercase());
    }

    /// Every `(host, ip, port)` passed to [`Policy::ssrf_check`], in order.
    pub fn ssrf_checks(&self) -> Vec<(String, IpAddr, u16)> {
        self.ssrf_checks.lock().expect("lock").clone()
    }
}

fn blocked_rule(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() {
                Some("loopback")
            } else if o == [169, 254, 169, 254] {
                Some("metadata")
            } else if v4.is_link_local() {
                Some("link_local")
            } else if v4.is_private() {
                Some("rfc1918")
            } else if o[0] == 100 && (64..128).contains(&o[1]) {
                Some("cgnat")
            } else if o[0] == 0 {
                Some("unspecified")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            if v6.to_ipv4_mapped().is_some() {
                Some("ipv4_mapped")
            } else if v6.is_loopback() {
                Some("loopback")
            } else if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                Some("ula")
            } else if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                Some("link_local")
            } else if v6.is_unspecified() {
                Some("unspecified")
            } else {
                None
            }
        }
    }
}

impl Policy for StubPolicy {
    fn robots_verdict(&self, r: &RobotsEntry, url: &CanonicalUrl) -> RobotsVerdict {
        match r.status {
            RobotsStatus::NotFound => RobotsVerdict::Allowed {
                crawl_delay: None,
                ai_reserved: false,
            },
            RobotsStatus::Unauthorized => RobotsVerdict::Disallowed,
            RobotsStatus::Challenge => RobotsVerdict::KeyBlocked,
            RobotsStatus::Unavailable => RobotsVerdict::Unavailable,
            RobotsStatus::Ok => {
                let body = r.body.as_deref().unwrap_or_default();
                let text = String::from_utf8_lossy(body);
                let path = url.path_and_query();
                let disallowed = text.lines().any(|l| {
                    let l = l.trim();
                    let Some(rest) = l
                        .strip_prefix("Disallow:")
                        .or_else(|| l.strip_prefix("disallow:"))
                    else {
                        return false;
                    };
                    let rule = rest.trim();
                    !rule.is_empty() && path.starts_with(rule)
                });
                if disallowed {
                    RobotsVerdict::Disallowed
                } else {
                    RobotsVerdict::Allowed {
                        crawl_delay: None,
                        ai_reserved: false,
                    }
                }
            }
        }
    }

    fn robots_summary(&self, r: &RobotsEntry) -> RobotsSummary {
        RobotsSummary {
            key_blocked: r.status == RobotsStatus::Challenge,
            ..RobotsSummary::default()
        }
    }

    fn newly_disallowed(
        &self,
        old: &RobotsEntry,
        new: &RobotsEntry,
        urls: &[CanonicalUrl],
    ) -> Vec<CanonicalUrl> {
        urls.iter()
            .filter(|u| {
                self.robots_verdict(old, u).allows() && !self.robots_verdict(new, u).allows()
            })
            .cloned()
            .collect()
    }

    fn use_policy(&self, s: &UseSignals) -> UsePolicy {
        UsePolicy::from_signals(s)
    }

    fn denied(&self, url: &CanonicalUrl) -> Option<DenyReason> {
        let host = url.host().to_ascii_lowercase();
        let hit = self.denied_hosts.lock().expect("lock").contains(&host);
        hit.then(|| DenyReason {
            rule: host,
            reason: "test denylist".into(),
        })
    }

    fn subresource_host_allowed(&self, host: &str) -> bool {
        self.subresource_hosts
            .lock()
            .expect("lock")
            .contains(&host.to_ascii_lowercase())
    }

    fn ssrf_check(&self, host: &str, ip: IpAddr, port: u16) -> Result<(), SsrfBlock> {
        self.ssrf_checks
            .lock()
            .expect("lock")
            .push((host.to_string(), ip, port));
        if self.allowed_ips.lock().expect("lock").contains(&ip) {
            return Ok(());
        }
        match blocked_rule(ip) {
            Some(rule) => Err(SsrfBlock {
                host: host.to_string(),
                ip,
                port,
                rule: rule.to_string(),
            }),
            None => Ok(()),
        }
    }

    fn is_challenge(&self, ex: &Exchange) -> bool {
        let header = ex
            .response_headers
            .get("cf-mitigated")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("challenge"));
        header || ex.body.windows(12).any(|w| w == b"cf-challenge")
    }
}

/// A fresh robots.txt entry for `origin` (`Ok` with `body`), fetched at `now`, valid 24 h.
pub fn robots_ok(origin: OriginKey, body: &str, now: DateTime<Utc>) -> RobotsEntry {
    RobotsEntry {
        origin,
        status: RobotsStatus::Ok,
        http_status: Some(200),
        body: Some(Bytes::from(body.to_string())),
        last_good: None,
        fetched_at: now,
        expires_at: now + Duration::hours(24),
    }
}
