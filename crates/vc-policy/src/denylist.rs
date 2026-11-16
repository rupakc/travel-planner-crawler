//! `policy/denylist.yaml` matching (spec §2 anti-goals, §5.2; review r3 #8).
//!
//! - `host` matches that host and every subdomain (`booking.com` → `m.booking.com`).
//! - `<brand>.*` matches when the URL host's registrable domain (PSL) is `<brand>.<suffix>` for
//!   **any** public suffix (`expedia.*` → `expedia.com`, `www.expedia.co.uk`, `expedia.de`).
//! - `path_prefix` restricts the entry to paths starting with it (case-sensitive, canonical
//!   path).
//! - `except_hosts` (and their subdomains) are not covered by that entry.

use vc_core::CanonicalUrl;
use vc_core::policy::{DenyEntry, DenyReason, Denylist};

/// Lowercases and strips a trailing dot.
pub(crate) fn norm_host(h: &str) -> String {
    h.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// `host` equals `base` or is a subdomain of it.
pub(crate) fn host_within(host: &str, base: &str) -> bool {
    host == base
        || (host.len() > base.len()
            && host.ends_with(base)
            && host.as_bytes()[host.len() - base.len() - 1] == b'.')
}

/// The registrable domain's first label and the public suffix, when the host has one.
fn brand_of(host: &str) -> Option<(&str, &str)> {
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return None;
    }
    let d = psl::domain_str(host)?;
    let suffix = psl::suffix_str(host)?;
    let label = d.strip_suffix(suffix)?.strip_suffix('.')?;
    Some((label, suffix))
}

/// A validated, matchable denylist.
#[derive(Debug, Clone)]
pub struct DenyMatcher {
    entries: Vec<DenyEntry>,
}

impl DenyMatcher {
    /// Validates and normalises the entries.
    pub fn new(list: Denylist) -> Result<Self, String> {
        if list.version != 1 {
            return Err(format!("denylist version {} unsupported", list.version));
        }
        let mut entries = Vec::with_capacity(list.entries.len());
        for mut e in list.entries {
            e.host = norm_host(&e.host);
            let bare = e.brand().unwrap_or(&e.host);
            if bare.is_empty()
                || bare.contains(['/', ':', '*', ' ', '@'])
                || (e.brand().is_some() && bare.contains('.'))
            {
                return Err(format!("invalid denylist host {:?}", e.host));
            }
            if let Some(p) = &e.path_prefix
                && !p.starts_with('/')
            {
                return Err(format!(
                    "path_prefix {p:?} of {} must start with '/'",
                    e.host
                ));
            }
            e.except_hosts = e.except_hosts.iter().map(|h| norm_host(h)).collect();
            entries.push(e);
        }
        Ok(DenyMatcher { entries })
    }

    /// The first entry covering `url`.
    pub fn check(&self, url: &CanonicalUrl) -> Option<DenyReason> {
        let host = norm_host(url.host());
        let path = url.as_url().path();
        let brand = brand_of(&host);
        self.entries
            .iter()
            .find(|e| entry_matches(e, &host, brand, path))
            .map(|e| DenyReason {
                rule: match &e.path_prefix {
                    Some(p) => format!("{}{}", e.host, p),
                    None => e.host.clone(),
                },
                reason: e.reason.clone(),
            })
    }

    /// Whether a bare host (any path) is covered by an entry without a path prefix.
    pub fn host_denied(&self, host: &str) -> bool {
        let host = norm_host(host);
        let brand = brand_of(&host);
        self.entries
            .iter()
            .filter(|e| e.path_prefix.is_none())
            .any(|e| entry_matches(e, &host, brand, "/"))
    }
}

fn entry_matches(e: &DenyEntry, host: &str, brand: Option<(&str, &str)>, path: &str) -> bool {
    let host_hit = match e.brand() {
        Some(b) => brand.is_some_and(|(label, _)| label == b),
        None => host_within(host, &e.host),
    };
    host_hit
        && !e.except_hosts.iter().any(|x| host_within(host, x))
        && e.path_prefix.as_deref().is_none_or(|p| path.starts_with(p))
}
