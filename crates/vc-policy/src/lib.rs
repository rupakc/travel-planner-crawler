//! # vc-policy (Task 1, stub)
//!
//! Pure politeness/compliance decisions: robots (RFC 9309 + our 401/403 rule), the UsePolicy
//! matrix, denylist, SSRF guard, challenge detector. Implements [`vc_core::policy::Policy`].

use std::net::IpAddr;

use vc_config::PolicyConfig;
use vc_core::CanonicalUrl;
use vc_core::fetch::Exchange;
use vc_core::policy::{DenyReason, Policy, RobotsEntry, RobotsSummary, RobotsVerdict, SsrfBlock};
use vc_core::usepolicy::{UsePolicy, UseSignals};

/// The production [`Policy`].
#[derive(Debug, Clone)]
pub struct VoyagerPolicy {
    /// Settings.
    pub config: PolicyConfig,
}

impl VoyagerPolicy {
    /// Builds the policy (loads the denylist and allowlist files).
    pub fn new(config: PolicyConfig) -> vc_core::Result<Self> {
        Ok(VoyagerPolicy { config })
    }
}

impl Policy for VoyagerPolicy {
    fn robots_verdict(&self, _r: &RobotsEntry, _url: &CanonicalUrl) -> RobotsVerdict {
        todo!("Task 1")
    }
    fn robots_summary(&self, _r: &RobotsEntry) -> RobotsSummary {
        todo!("Task 1")
    }
    fn newly_disallowed(
        &self,
        _old: &RobotsEntry,
        _new: &RobotsEntry,
        _urls: &[CanonicalUrl],
    ) -> Vec<CanonicalUrl> {
        todo!("Task 1")
    }
    fn use_policy(&self, s: &UseSignals) -> UsePolicy {
        UsePolicy::from_signals(s)
    }
    fn denied(&self, _url: &CanonicalUrl) -> Option<DenyReason> {
        todo!("Task 1")
    }
    fn subresource_host_allowed(&self, _host: &str) -> bool {
        todo!("Task 1")
    }
    fn ssrf_check(&self, _host: &str, _ip: IpAddr, _port: u16) -> Result<(), SsrfBlock> {
        todo!("Task 1")
    }
    fn is_challenge(&self, _ex: &Exchange) -> bool {
        todo!("Task 1")
    }
}
