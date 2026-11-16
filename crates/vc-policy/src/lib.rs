//! # vc-policy (Task 1)
//!
//! Pure politeness/compliance decisions: robots (RFC 9309 + our 401/403 rule), the UsePolicy
//! matrix, denylist, SSRF guard, challenge detector. Implements [`vc_core::policy::Policy`].
//! Nothing here does I/O except [`VoyagerPolicy::new`], which reads the two policy files once.

pub mod challenge;
pub mod denylist;
pub mod robots;
pub mod ssrf;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::Deserialize;
use url::Url;
use vc_config::PolicyConfig;
use vc_core::CanonicalUrl;
use vc_core::fetch::Exchange;
use vc_core::policy::{
    DenyReason, Denylist, Policy, RobotsEntry, RobotsStatus, RobotsSummary, RobotsVerdict,
    SsrfBlock,
};
use vc_core::usepolicy::{UsePolicy, UseSignals};

use crate::denylist::{DenyMatcher, norm_host};
use crate::robots::ParsedRobots;

/// Our robots.txt product token.
pub const ROBOTS_TOKEN: &str = "VoyagerBot";

/// Parsed robots bodies kept in memory (keyed by body bytes).
const ROBOTS_MEMO_CAP: usize = 512;

/// One host of `policy/subresource_allowlist.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubresourceHost {
    /// The exact host.
    pub host: String,
    /// CDP resource types allowed from it.
    pub types: Vec<String>,
    /// Why it is allowed.
    pub reason: String,
}

/// The whole `policy/subresource_allowlist.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubresourceAllowlist {
    /// Format version (1).
    pub version: u32,
    /// Hosts.
    pub hosts: Vec<SubresourceHost>,
}

type Memo = Arc<Mutex<HashMap<Bytes, Arc<ParsedRobots>>>>;

/// The production [`Policy`].
#[derive(Debug, Clone)]
pub struct VoyagerPolicy {
    /// Settings.
    pub config: PolicyConfig,
    deny: DenyMatcher,
    subresource_hosts: Vec<String>,
    memo: Memo,
}

fn invalid(what: &str, e: impl std::fmt::Display) -> vc_core::Error {
    vc_core::Error::Invalid(format!("{what}: {e}"))
}

impl VoyagerPolicy {
    /// Builds the policy (loads the denylist and allowlist files). A missing or invalid file is
    /// an error: the crawler must not start without its denylist.
    pub fn new(config: PolicyConfig) -> vc_core::Result<Self> {
        let deny = std::fs::read_to_string(&config.denylist_path)?;
        let deny: Denylist =
            serde_norway::from_str(&deny).map_err(|e| invalid(&config.denylist_path, e))?;
        let allow = std::fs::read_to_string(&config.subresource_allowlist_path)?;
        let allow: SubresourceAllowlist = serde_norway::from_str(&allow)
            .map_err(|e| invalid(&config.subresource_allowlist_path, e))?;
        Self::from_parts(config, deny, allow)
    }

    /// Builds the policy from already-parsed files.
    pub fn from_parts(
        config: PolicyConfig,
        deny: Denylist,
        allow: SubresourceAllowlist,
    ) -> vc_core::Result<Self> {
        let deny = DenyMatcher::new(deny).map_err(|e| invalid("denylist", e))?;
        if allow.version != 1 {
            return Err(invalid("subresource allowlist", "version must be 1"));
        }
        let subresource_hosts = allow.hosts.iter().map(|h| norm_host(&h.host)).collect();
        Ok(VoyagerPolicy {
            config,
            deny,
            subresource_hosts,
            memo: Memo::default(),
        })
    }

    /// Parses (memoised by body) a robots.txt body, cut at `robots_max_bytes`.
    fn parsed(&self, body: &Bytes) -> Arc<ParsedRobots> {
        let max = usize::try_from(self.config.robots_max_bytes).unwrap_or(usize::MAX);
        let body = body.slice(..body.len().min(max));
        let mut memo = self.memo.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(p) = memo.get(&body) {
            return Arc::clone(p);
        }
        let parsed = Arc::new(ParsedRobots::parse(&body));
        if memo.len() >= ROBOTS_MEMO_CAP {
            memo.clear();
        }
        memo.insert(body, Arc::clone(&parsed));
        parsed
    }

    /// The robots file that currently governs the origin, or the verdict to use instead.
    fn effective(&self, r: &RobotsEntry) -> Result<Arc<ParsedRobots>, RobotsVerdict> {
        match r.status {
            RobotsStatus::Unauthorized | RobotsStatus::Challenge => Err(RobotsVerdict::KeyBlocked),
            RobotsStatus::NotFound => Ok(Arc::new(ParsedRobots::default())),
            RobotsStatus::Ok => Ok(match &r.body {
                Some(b) => self.parsed(b),
                None => Arc::new(ParsedRobots::default()),
            }),
            RobotsStatus::Unavailable => {
                // RFC 9309 §2.3.1.4: unreachable → disallow everything; once it has been
                // unreachable for 30 days the last good copy governs again.
                let days = chrono::Duration::days(i64::from(self.config.robots_last_good_days));
                match &r.last_good {
                    Some((body, at)) if r.fetched_at - *at >= days => Ok(self.parsed(body)),
                    _ => Err(RobotsVerdict::Unavailable),
                }
            }
        }
    }

    /// The AI reservation: some configured AI crawler token is named explicitly and blocked
    /// from `/`, while our own group (else `*`) allows `/`.
    fn ai_reserved(&self, p: &ParsedRobots) -> bool {
        if p.rules_for(ROBOTS_TOKEN).blocks_root() {
            return false;
        }
        self.config
            .ai_bot_tokens
            .iter()
            .any(|t| p.names_agent(t) && p.rules_for(t).blocks_root())
    }

    /// Whether a bare host is covered by a host-wide (no `path_prefix`) denylist entry.
    pub fn host_denied(&self, host: &str) -> bool {
        self.deny.host_denied(host)
    }
}

impl Policy for VoyagerPolicy {
    fn robots_verdict(&self, r: &RobotsEntry, url: &CanonicalUrl) -> RobotsVerdict {
        if url.origin() != r.origin {
            // Fail closed: a robots entry never speaks for another origin.
            return RobotsVerdict::Disallowed;
        }
        let parsed = match self.effective(r) {
            Ok(p) => p,
            Err(v) => return v,
        };
        let rules = parsed.rules_for(ROBOTS_TOKEN);
        if rules.allowed(&url.path_and_query()) {
            RobotsVerdict::Allowed {
                crawl_delay: rules.crawl_delay(),
                ai_reserved: self.ai_reserved(&parsed),
            }
        } else {
            RobotsVerdict::Disallowed
        }
    }

    fn robots_summary(&self, r: &RobotsEntry) -> RobotsSummary {
        match self.effective(r) {
            Err(RobotsVerdict::KeyBlocked) => RobotsSummary {
                key_blocked: true,
                ..RobotsSummary::default()
            },
            Err(_) => RobotsSummary::default(),
            Ok(p) => RobotsSummary {
                crawl_delay: p.rules_for(ROBOTS_TOKEN).crawl_delay(),
                sitemaps: p
                    .sitemaps()
                    .iter()
                    .filter_map(|s| Url::parse(s).ok())
                    .filter(|u| matches!(u.scheme(), "http" | "https"))
                    .collect(),
                ai_reserved: self.ai_reserved(&p),
                key_blocked: false,
            },
        }
    }

    /// URLs allowed under `old` that are `Disallowed` or `KeyBlocked` under `new`. A transient
    /// `Unavailable` (5xx) never tombstones anything.
    fn newly_disallowed(
        &self,
        old: &RobotsEntry,
        new: &RobotsEntry,
        urls: &[CanonicalUrl],
    ) -> Vec<CanonicalUrl> {
        urls.iter()
            .filter(|u| {
                self.robots_verdict(old, u).allows()
                    && matches!(
                        self.robots_verdict(new, u),
                        RobotsVerdict::Disallowed | RobotsVerdict::KeyBlocked
                    )
            })
            .cloned()
            .collect()
    }

    fn use_policy(&self, s: &UseSignals) -> UsePolicy {
        UsePolicy::from_signals(s)
    }

    fn denied(&self, url: &CanonicalUrl) -> Option<DenyReason> {
        self.deny.check(url)
    }

    fn subresource_host_allowed(&self, host: &str) -> bool {
        let h = norm_host(host);
        self.subresource_hosts.contains(&h) && !self.deny.host_denied(&h)
    }

    fn ssrf_check(&self, host: &str, ip: IpAddr, port: u16) -> Result<(), SsrfBlock> {
        match ssrf::check(host, ip, port, &self.config.allowed_ports) {
            None => Ok(()),
            Some(rule) => Err(SsrfBlock {
                host: host.to_string(),
                ip,
                port,
                rule: rule.to_string(),
            }),
        }
    }

    fn is_challenge(&self, ex: &Exchange) -> bool {
        challenge::detect(ex).is_some()
    }
}
