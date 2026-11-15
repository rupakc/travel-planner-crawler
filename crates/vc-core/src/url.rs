//! Canonical URLs.
//!
//! Canonicalisation is **idempotent** (`canon(canon(u)) == canon(u)`, a proptest in
//! `tests/contracts.rs`) and conservative: it never changes which resource a well-behaved server
//! returns, except for dropping well-known tracking parameters. The rules, in order:
//!
//! 1. Only `http` and `https` are accepted; a host is required. Scheme and host are lowercased,
//!    IDNs become punycode, IPv4 forms (decimal, octal, hex) are normalised by the WHATWG parser,
//!    a trailing dot on the host is removed, and the default port is dropped.
//! 2. User info (`user:pass@`) and the fragment are removed.
//! 3. Path: dot segments are resolved, an empty path becomes `/`, percent-escapes of unreserved
//!    characters (`A–Z a–z 0–9 - . _ ~`) are decoded and all other escapes are uppercased.
//! 4. Query: empty pairs and tracking parameters ([`TRACKING_PARAMS`], and any `utm_*`) are
//!    removed, escapes are normalised as for the path, and the pairs are stably sorted by name.
//!    An empty query is removed entirely.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

use crate::ids::{OriginKey, PolitenessKey, UrlKey};

/// Query parameters dropped during canonicalisation (compared case-insensitively), in addition
/// to every parameter starting with `utm_`.
pub const TRACKING_PARAMS: &[&str] = &[
    "gclid",
    "dclid",
    "gbraid",
    "wbraid",
    "fbclid",
    "msclkid",
    "yclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "_gl",
    "_hsenc",
    "_hsmi",
    "mkt_tok",
    "oly_anon_id",
    "oly_enc_id",
    "vero_id",
];

/// Why a string is not an acceptable crawl URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    /// Not parseable as a URL.
    #[error("unparseable url: {0}")]
    Parse(String),
    /// A scheme other than http/https.
    #[error("unsupported scheme: {0}")]
    Scheme(String),
    /// No host.
    #[error("url has no host")]
    NoHost,
}

/// A URL in canonical form (see the module docs). The only way to build one is through
/// [`CanonicalUrl::parse`], [`CanonicalUrl::from_url`] or [`CanonicalUrl::join`].
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CanonicalUrl(Url);

impl CanonicalUrl {
    /// Parses and canonicalises an absolute URL.
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let u = Url::parse(s.trim()).map_err(|e| UrlError::Parse(e.to_string()))?;
        Self::from_url(u)
    }

    /// Canonicalises an already-parsed URL.
    pub fn from_url(mut u: Url) -> Result<Self, UrlError> {
        match u.scheme() {
            "http" | "https" => {}
            other => return Err(UrlError::Scheme(other.to_string())),
        }
        let host = u.host_str().ok_or(UrlError::NoHost)?.to_string();
        if host.is_empty() {
            return Err(UrlError::NoHost);
        }
        if host.ends_with('.') && host.len() > 1 {
            let trimmed = host.trim_end_matches('.').to_string();
            if trimmed.is_empty() {
                return Err(UrlError::NoHost);
            }
            u.set_host(Some(&trimmed))
                .map_err(|e| UrlError::Parse(e.to_string()))?;
        }
        // These cannot fail for http(s) URLs with a host.
        let _ = u.set_username("");
        let _ = u.set_password(None);
        u.set_fragment(None);

        let path = normalize_escapes(u.path());
        let path = if path.is_empty() {
            "/".to_string()
        } else {
            path
        };
        u.set_path(&path);

        let query = u.query().map(canonical_query);
        match query {
            Some(q) if !q.is_empty() => u.set_query(Some(&q)),
            _ => u.set_query(None),
        }
        Ok(CanonicalUrl(u))
    }

    /// Resolves `href` against this URL and canonicalises the result.
    pub fn join(&self, href: &str) -> Result<Self, UrlError> {
        let u = self
            .0
            .join(href.trim())
            .map_err(|e| UrlError::Parse(e.to_string()))?;
        Self::from_url(u)
    }

    /// The canonical string.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The parsed form.
    pub fn as_url(&self) -> &Url {
        &self.0
    }

    /// The host (lowercase ASCII; IPv6 in brackets).
    pub fn host(&self) -> &str {
        self.0.host_str().unwrap_or_default()
    }

    /// The port, or the scheme's default.
    pub fn port(&self) -> u16 {
        self.0.port_or_known_default().unwrap_or(80)
    }

    /// The path plus query (what robots.txt rules match against).
    pub fn path_and_query(&self) -> String {
        match self.0.query() {
            Some(q) => format!("{}?{}", self.0.path(), q),
            None => self.0.path().to_string(),
        }
    }

    /// The url key (`blake3` of the canonical string, 16 bytes).
    pub fn key(&self) -> UrlKey {
        UrlKey::of(self.as_str())
    }

    /// The origin (`scheme://host[:port]`), which owns robots.txt.
    pub fn origin(&self) -> OriginKey {
        OriginKey(self.0.origin().ascii_serialization())
    }

    /// The URL of this origin's robots.txt.
    pub fn robots_url(&self) -> CanonicalUrl {
        let mut u = self.0.clone();
        u.set_path("/robots.txt");
        u.set_query(None);
        CanonicalUrl(u)
    }

    /// The default politeness key (registrable domain of the host).
    pub fn politeness_key(&self) -> PolitenessKey {
        PolitenessKey::for_host(self.host())
    }
}

impl fmt::Display for CanonicalUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for CanonicalUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CanonicalUrl({})", self.as_str())
    }
}

impl std::str::FromStr for CanonicalUrl {
    type Err = UrlError;
    fn from_str(s: &str) -> Result<Self, UrlError> {
        CanonicalUrl::parse(s)
    }
}

impl AsRef<str> for CanonicalUrl {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Serialize for CanonicalUrl {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CanonicalUrl {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        CanonicalUrl::parse(&s).map_err(serde::de::Error::custom)
    }
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes escapes of unreserved characters and uppercases every other escape.
fn normalize_escapes(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2]))
        {
            let v = h * 16 + l;
            if is_unreserved(v) {
                out.push(v as char);
            } else {
                out.push('%');
                out.push(b[i + 1].to_ascii_uppercase() as char);
                out.push(b[i + 2].to_ascii_uppercase() as char);
            }
            i += 3;
            continue;
        }
        // Safe: we only split at ASCII bytes, so pushing the char at i keeps UTF-8 valid.
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_tracking(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("utm_") || TRACKING_PARAMS.contains(&n.as_str())
}

fn canonical_query(q: &str) -> String {
    let mut pairs: Vec<(String, String)> = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(normalize_escapes)
        .filter_map(|p| {
            let name = p.split('=').next().unwrap_or_default().to_string();
            if name.is_empty() && !p.contains('=') {
                return None;
            }
            if is_tracking(&name) {
                None
            } else {
                Some((name, p))
            }
        })
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
        .into_iter()
        .map(|(_, p)| p)
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> String {
        CanonicalUrl::parse(s).unwrap().to_string()
    }

    #[test]
    fn canonicalises_examples() {
        assert_eq!(c("HTTP://Example.COM"), "http://example.com/");
        assert_eq!(
            c("https://example.com:443/a/./b/../c#frag"),
            "https://example.com/a/c"
        );
        assert_eq!(c("https://user:pw@example.com./x"), "https://example.com/x");
        assert_eq!(
            c("https://example.com/%7euser/%3a"),
            "https://example.com/~user/%3A"
        );
        assert_eq!(
            c("https://example.com/?b=2&utm_source=x&a=1&&fbclid=y"),
            "https://example.com/?a=1&b=2"
        );
        assert_eq!(
            c("https://example.com/?utm_medium=x"),
            "https://example.com/"
        );
        assert_eq!(c("http://0x7f.1/"), "http://127.0.0.1/");
    }

    #[test]
    fn rejects_non_http() {
        assert!(matches!(
            CanonicalUrl::parse("ftp://x/"),
            Err(UrlError::Scheme(_))
        ));
        assert!(CanonicalUrl::parse("not a url").is_err());
    }

    #[test]
    fn origin_and_robots() {
        let u = CanonicalUrl::parse("https://en.wikivoyage.org:8443/wiki/Rome?x=1").unwrap();
        assert_eq!(u.origin().0, "https://en.wikivoyage.org:8443");
        assert_eq!(
            u.robots_url().as_str(),
            "https://en.wikivoyage.org:8443/robots.txt"
        );
        assert_eq!(u.politeness_key().0, "wikivoyage.org");
        assert_eq!(u.path_and_query(), "/wiki/Rome?x=1");
    }
}
