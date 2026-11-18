//! A small pure `Policy` for tests: prefix robots rules, a host denylist, a sub-resource
//! allowlist and an SSRF guard for private/loopback/link-local IPs and non-80/443 ports.

use std::net::IpAddr;

use vc_core::CanonicalUrl;
use vc_core::fetch::Exchange;
use vc_core::policy::{
    DenyReason, Policy, RobotsEntry, RobotsStatus, RobotsSummary, RobotsVerdict, SsrfBlock,
};
use vc_core::usepolicy::{UsePolicy, UseSignals};

#[derive(Debug, Default, Clone)]
pub struct FakePolicy {
    pub denied_hosts: Vec<String>,
    pub allowlisted: Vec<String>,
}

impl FakePolicy {
    pub fn new() -> Self {
        FakePolicy {
            denied_hosts: vec!["denied.example.org".into()],
            allowlisted: vec!["cdn.allowed.test".into()],
        }
    }
}

fn private(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v) if v.is_loopback() => Some("loopback"),
        IpAddr::V4(v) if v.is_private() => Some("rfc1918"),
        IpAddr::V4(v) if v.is_link_local() => Some("link_local"),
        IpAddr::V4(v) if v.is_unspecified() => Some("unspecified"),
        IpAddr::V6(v) if v.is_loopback() || v.is_unspecified() => Some("loopback"),
        _ => None,
    }
}

impl Policy for FakePolicy {
    fn robots_verdict(&self, r: &RobotsEntry, url: &CanonicalUrl) -> RobotsVerdict {
        match r.status {
            RobotsStatus::Ok => {
                let body = r.body.clone().unwrap_or_default();
                let text = String::from_utf8_lossy(&body);
                let path = url.path_and_query();
                for line in text.lines() {
                    if let Some(prefix) = line.trim().strip_prefix("Disallow:") {
                        let prefix = prefix.trim();
                        if !prefix.is_empty() && path.starts_with(prefix) {
                            return RobotsVerdict::Disallowed;
                        }
                    }
                }
                RobotsVerdict::Allowed {
                    crawl_delay: None,
                    ai_reserved: false,
                }
            }
            RobotsStatus::NotFound | RobotsStatus::NoResponse => RobotsVerdict::Allowed {
                crawl_delay: None,
                ai_reserved: false,
            },
            RobotsStatus::Unauthorized => RobotsVerdict::Disallowed,
            RobotsStatus::Challenge => RobotsVerdict::KeyBlocked,
            RobotsStatus::Unavailable => RobotsVerdict::Unavailable,
        }
    }

    fn robots_summary(&self, _r: &RobotsEntry) -> RobotsSummary {
        RobotsSummary::default()
    }

    fn newly_disallowed(
        &self,
        _old: &RobotsEntry,
        _new: &RobotsEntry,
        _urls: &[CanonicalUrl],
    ) -> Vec<CanonicalUrl> {
        Vec::new()
    }

    fn use_policy(&self, s: &UseSignals) -> UsePolicy {
        UsePolicy::from_signals(s)
    }

    fn denied(&self, url: &CanonicalUrl) -> Option<DenyReason> {
        let h = url.host();
        self.denied_hosts
            .iter()
            .find(|d| h == d.as_str() || h.ends_with(&format!(".{d}")))
            .map(|d| DenyReason {
                rule: d.clone(),
                reason: "test".into(),
            })
    }

    fn subresource_host_allowed(&self, host: &str) -> bool {
        self.allowlisted.iter().any(|h| h == host)
    }

    fn ssrf_check(&self, host: &str, ip: IpAddr, port: u16) -> Result<(), SsrfBlock> {
        let rule = private(ip).or(if port == 80 || port == 443 {
            None
        } else {
            Some("port")
        });
        match rule {
            Some(rule) => Err(SsrfBlock {
                host: host.to_string(),
                ip,
                port,
                rule: rule.into(),
            }),
            None => Ok(()),
        }
    }

    fn is_challenge(&self, ex: &Exchange) -> bool {
        ex.response_headers
            .get("cf-mitigated")
            .is_some_and(|v| v == "challenge")
    }
}
