//! DNS resolution for the SSRF guard: resolved once per hop, every address checked, the first
//! one pinned for the connection (spec §5.2).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::OnceLock;

use async_trait::async_trait;
use hickory_resolver::TokioResolver;
use vc_core::fetch::FetchError;

/// Resolves a host name to IP addresses. The permit client checks **every** returned address
/// with `Policy::ssrf_check` and connects only to the first (pinned).
#[async_trait]
pub trait Resolve: Send + Sync + std::fmt::Debug {
    /// The addresses of `host` (a DNS name, never an IP literal).
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, FetchError>;
}

/// The system resolver (hickory, `/etc/resolv.conf`), built on first use.
#[derive(Debug, Default)]
pub struct HickoryResolver {
    inner: OnceLock<Result<TokioResolver, String>>,
}

impl HickoryResolver {
    /// A resolver using the system configuration.
    pub fn new() -> Self {
        HickoryResolver::default()
    }

    fn resolver(&self) -> Result<&TokioResolver, FetchError> {
        self.inner
            .get_or_init(|| {
                TokioResolver::builder_tokio()
                    .and_then(|b| b.build())
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| FetchError::Dns(e.clone()))
    }
}

#[async_trait]
impl Resolve for HickoryResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, FetchError> {
        let r = self.resolver()?;
        let lookup = r
            .lookup_ip(host)
            .await
            .map_err(|e| FetchError::Dns(e.to_string()))?;
        Ok(lookup.iter().collect())
    }
}

/// A fixed host → addresses map (tests, the E2E fake web). Unknown hosts fail with
/// [`FetchError::Dns`].
#[derive(Debug, Clone, Default)]
pub struct StaticResolver {
    hosts: HashMap<String, Vec<IpAddr>>,
}

impl StaticResolver {
    /// An empty map.
    pub fn new() -> Self {
        StaticResolver::default()
    }

    /// Adds one address for `host` (lowercased).
    pub fn with(mut self, host: &str, ip: IpAddr) -> Self {
        self.hosts
            .entry(host.to_ascii_lowercase())
            .or_default()
            .push(ip);
        self
    }
}

#[async_trait]
impl Resolve for StaticResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, FetchError> {
        self.hosts
            .get(&host.to_ascii_lowercase())
            .cloned()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| FetchError::Dns(format!("no address for {host}")))
    }
}
