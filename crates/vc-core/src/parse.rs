//! Content handlers per media type (spec §5.7 step 1). Implemented by `vc-parse`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::media::{MediaType, MediaTypeMatch};
use crate::store::CaptureMeta;
use crate::url::CanonicalUrl;
use crate::usepolicy::UseSignals;

/// Safety limits for parsing untrusted content (spec §5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Max DOM depth (deeper subtrees are dropped).
    pub max_dom_depth: u32,
    /// Max DOM nodes.
    pub max_nodes: u32,
    /// Max bytes of main text kept.
    pub max_text_bytes: usize,
    /// Max links kept.
    pub max_links: usize,
    /// Max entries read from a sitemap (protocol cap 50,000).
    pub max_sitemap_entries: usize,
    /// CPU time budget per capture.
    pub cpu_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_dom_depth: 256,
            max_nodes: 200_000,
            max_text_bytes: 1024 * 1024,
            max_links: 5_000,
            max_sitemap_entries: 50_000,
            cpu_time: Duration::from_secs(5),
        }
    }
}

/// What a link points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// An ordinary page link (`<a href>`, sitemap `<url>`).
    Page,
    /// A sitemap (`<sitemap>` in an index, robots `Sitemap:`).
    Sitemap,
    /// A feed (`<link rel=alternate type=application/rss+xml>`).
    Feed,
    /// An item of a feed.
    FeedItem,
    /// `rel=canonical`.
    Canonical,
    /// `rel=alternate` (hreflang etc.).
    Alternate,
}

/// An outgoing link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// The canonical target.
    pub url: CanonicalUrl,
    /// Anchor text or item title (trimmed, ≤ 200 chars).
    pub anchor: String,
    /// Kind.
    pub kind: LinkKind,
    /// `rel` tokens (e.g. `nofollow`).
    #[serde(default)]
    pub rel: Vec<String>,
    /// Sitemap `<lastmod>` or feed item date.
    pub lastmod: Option<DateTime<Utc>>,
}

/// The output of a content handler.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Parsed {
    /// Document title.
    pub title: Option<String>,
    /// Main content as plain text (dom_smoothie for HTML), capped.
    pub main_text: Option<String>,
    /// Detected language (BCP 47 primary tag, e.g. `en`).
    pub lang: Option<String>,
    /// Links.
    pub links: Vec<Link>,
    /// Structured data (JSON-LD objects with `@graph` flattened; microdata as JSON-LD-like).
    pub structured: Vec<serde_json::Value>,
    /// 64-bit simhash of the main text.
    pub simhash: Option<u64>,
    /// Body signals (meta robots, TDMRep `<link>`).
    pub body_signals: UseSignals,
    /// Whether the page looks like an SPA shell (little text, SPA markers): render it.
    pub needs_render: bool,
}

/// Parses one media type.
pub trait ContentHandler: Send + Sync {
    /// The media types this handler accepts.
    fn media(&self) -> &'static [MediaTypeMatch];

    /// Parses a decoded body. Must respect `lim` and never fetch anything.
    fn handle(&self, meta: &CaptureMeta, body: &[u8], lim: &Limits) -> Result<Parsed>;
}

/// The registry of content handlers. First registered match wins.
#[derive(Default, Clone)]
pub struct HandlerRegistry {
    handlers: Vec<Arc<dyn ContentHandler>>,
}

impl HandlerRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        HandlerRegistry::default()
    }

    /// Registers a handler.
    pub fn register(&mut self, h: Arc<dyn ContentHandler>) -> &mut Self {
        self.handlers.push(h);
        self
    }

    /// The handler for a media type, if any.
    pub fn for_media(&self, m: &MediaType) -> Option<Arc<dyn ContentHandler>> {
        self.handlers
            .iter()
            .find(|h| h.media().iter().any(|mm| mm.matches(m)))
            .cloned()
    }

    /// Number of registered handlers.
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Whether no handler is registered.
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

impl std::fmt::Debug for HandlerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandlerRegistry")
            .field("handlers", &self.handlers.len())
            .finish()
    }
}
