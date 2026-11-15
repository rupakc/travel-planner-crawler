//! Media types and fetch modes.
//!
//! The media type of a capture is fixed **at fetch time** (r5 Q5): the discovery hint
//! (`url_state.expect`) first, then the `Content-Type`, then a sniff of the first 512 bytes; on a
//! mismatch the sniff wins (and the caller logs it). See [`MediaType::detect`]. It is stored in
//! `captures.media_type` and the processor dispatches on it through
//! [`crate::parse::HandlerRegistry::for_media`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::Error;

/// The media type of a capture, as far as the crawler cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    /// HTML or XHTML.
    Html,
    /// RSS 0.9x/1.0/2.0.
    Rss,
    /// Atom.
    Atom,
    /// An XML sitemap (`<urlset>`) or sitemap index (`<sitemapindex>`).
    Sitemap,
    /// A plain-text sitemap (one URL per line).
    SitemapText,
    /// JSON (including JSON-LD documents and API responses).
    Json,
    /// CSV.
    Csv,
    /// Other XML.
    Xml,
    /// `text/plain` (also robots.txt).
    Text,
    /// PDF (no handler in v1).
    Pdf,
    /// Anything else (not processed).
    Other,
}

impl MediaType {
    /// The stable snake_case name stored in Postgres.
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaType::Html => "html",
            MediaType::Rss => "rss",
            MediaType::Atom => "atom",
            MediaType::Sitemap => "sitemap",
            MediaType::SitemapText => "sitemap_text",
            MediaType::Json => "json",
            MediaType::Csv => "csv",
            MediaType::Xml => "xml",
            MediaType::Text => "text",
            MediaType::Pdf => "pdf",
            MediaType::Other => "other",
        }
    }

    /// Maps a `Content-Type` header value (parameters ignored) to a media type.
    pub fn from_content_type(ct: &str) -> Option<MediaType> {
        let essence = ct
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        Some(match essence.as_str() {
            "text/html" | "application/xhtml+xml" => MediaType::Html,
            "application/rss+xml" | "application/rdf+xml" => MediaType::Rss,
            "application/atom+xml" => MediaType::Atom,
            "application/json" | "application/ld+json" | "text/json" => MediaType::Json,
            "text/csv" | "application/csv" => MediaType::Csv,
            "application/xml" | "text/xml" => MediaType::Xml,
            "text/plain" => MediaType::Text,
            "application/pdf" => MediaType::Pdf,
            e if e.ends_with("+json") => MediaType::Json,
            e if e.ends_with("+xml") => MediaType::Xml,
            "" => return None,
            _ => MediaType::Other,
        })
    }

    /// Sniffs the first bytes of a (decoded) body. Returns `None` when inconclusive.
    pub fn sniff(prefix: &[u8]) -> Option<MediaType> {
        let p = &prefix[..prefix.len().min(512)];
        let text = String::from_utf8_lossy(p);
        let t = text.trim_start_matches('\u{feff}').trim_start();
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("%pdf-") {
            return Some(MediaType::Pdf);
        }
        if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
            return Some(MediaType::Html);
        }
        if lower.starts_with('{') || lower.starts_with('[') {
            return Some(MediaType::Json);
        }
        if lower.starts_with('<') {
            if lower.contains("<urlset") || lower.contains("<sitemapindex") {
                return Some(MediaType::Sitemap);
            }
            if lower.contains("<rss") || lower.contains("<rdf:rdf") {
                return Some(MediaType::Rss);
            }
            if lower.contains("<feed") {
                return Some(MediaType::Atom);
            }
            if lower.contains("<html") || lower.contains("<head") || lower.contains("<body") {
                return Some(MediaType::Html);
            }
            if lower.starts_with("<?xml") {
                return Some(MediaType::Xml);
            }
        }
        None
    }

    /// Fixes the media type at fetch time: hint, then `Content-Type`, then sniff; when the
    /// sniff is conclusive and disagrees, the sniff wins. Returns the type and whether there
    /// was a mismatch (to be logged by the caller). A plain-text sitemap can only come from the
    /// hint.
    pub fn detect(
        hint: Option<MediaType>,
        content_type: Option<&str>,
        prefix: &[u8],
    ) -> (MediaType, bool) {
        let declared = hint.or_else(|| content_type.and_then(MediaType::from_content_type));
        let sniffed = MediaType::sniff(prefix);
        match (declared, sniffed) {
            (Some(MediaType::SitemapText), _) => (MediaType::SitemapText, false),
            // A generic XML declaration is refined by the sniff without counting as a mismatch.
            (Some(MediaType::Xml), Some(s)) => (s, false),
            (Some(d), Some(s)) if d != s => (s, true),
            (Some(d), _) => (d, false),
            (None, Some(s)) => (s, false),
            (None, None) => (MediaType::Other, false),
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MediaType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        serde_json::from_value(serde_json::Value::String(s.to_string()))
            .map_err(|_| Error::Invalid(format!("unknown media type {s:?}")))
    }
}

/// What a [`crate::parse::ContentHandler`] accepts. Const-constructible so handlers can return
/// a `&'static [MediaTypeMatch]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaTypeMatch {
    /// Exactly this type.
    Is(MediaType),
    /// RSS or Atom.
    AnyFeed,
    /// XML sitemap or plain-text sitemap.
    AnySitemap,
}

impl MediaTypeMatch {
    /// Whether `m` matches.
    pub fn matches(&self, m: &MediaType) -> bool {
        match self {
            MediaTypeMatch::Is(t) => t == m,
            MediaTypeMatch::AnyFeed => matches!(m, MediaType::Rss | MediaType::Atom),
            MediaTypeMatch::AnySitemap => matches!(m, MediaType::Sitemap | MediaType::SitemapText),
        }
    }
}

/// How a URL is fetched. Stored in `url_state.fetch_mode` as `http`, `render` or
/// `adapter:<id>` (r5 Q6: adapter work is a URL row whose fetch mode names the adapter).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum FetchMode {
    /// A plain HTTP GET through `PermitClient`.
    #[default]
    Http,
    /// Render with Chromium (`Renderer`).
    Render,
    /// Run the named `SourceAdapter`.
    Adapter(String),
}

impl fmt::Display for FetchMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchMode::Http => f.write_str("http"),
            FetchMode::Render => f.write_str("render"),
            FetchMode::Adapter(id) => write!(f, "adapter:{id}"),
        }
    }
}

impl FromStr for FetchMode {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "http" => Ok(FetchMode::Http),
            "render" => Ok(FetchMode::Render),
            _ => match s.strip_prefix("adapter:") {
                Some(id) if !id.is_empty() => Ok(FetchMode::Adapter(id.to_string())),
                _ => Err(Error::Invalid(format!("unknown fetch mode {s:?}"))),
            },
        }
    }
}

impl Serialize for FetchMode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for FetchMode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_prefers_sniff_on_mismatch() {
        let (m, mismatch) =
            MediaType::detect(None, Some("text/html"), b"<?xml version='1.0'?><rss>");
        assert_eq!(m, MediaType::Rss);
        assert!(mismatch);
        let (m, mismatch) = MediaType::detect(None, Some("application/xml"), b"<urlset>");
        assert_eq!((m, mismatch), (MediaType::Sitemap, false));
        let (m, _) = MediaType::detect(Some(MediaType::SitemapText), None, b"https://a/\n");
        assert_eq!(m, MediaType::SitemapText);
        let (m, _) = MediaType::detect(None, Some("text/html; charset=utf-8"), b"   ");
        assert_eq!(m, MediaType::Html);
    }

    #[test]
    fn fetch_mode_roundtrip() {
        for s in ["http", "render", "adapter:ecb_fx"] {
            assert_eq!(s.parse::<FetchMode>().unwrap().to_string(), s);
        }
        assert!("adapter:".parse::<FetchMode>().is_err());
        for m in [MediaType::Html, MediaType::SitemapText, MediaType::Other] {
            assert_eq!(m.as_str().parse::<MediaType>().unwrap(), m);
        }
    }
}
