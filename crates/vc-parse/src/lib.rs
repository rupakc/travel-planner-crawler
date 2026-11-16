//! # vc-parse
//!
//! Content handlers behind [`vc_core::parse::ContentHandler`] (spec §5.7 step 1), one per media
//! type, and [`registry`] building the [`vc_core::parse::HandlerRegistry`]:
//!
//! | Handler | Media | Output |
//! |---|---|---|
//! | [`HtmlHandler`] | `html` | title, main text (dom_smoothie), language, links with anchor text, JSON-LD + microdata, meta robots / TDMRep signals, simhash, `needs_render` |
//! | [`FeedHandler`] | `rss`, `atom` | title, language, one `feed_item` link per entry |
//! | [`SitemapHandler`] | `sitemap`, `sitemap_text` | `page` links (`<urlset>`) or `sitemap` links (`<sitemapindex>`), with `lastmod`; gzip accepted |
//! | [`JsonHandler`] | `json` | JSON-LD flattened, else the document as one value |
//! | [`CsvHandler`] | `csv` | rows as objects keyed by the header |
//!
//! Safety (spec §5.2): handlers never fetch; XML with a DTD is refused with
//! [`vc_core::Error::Policy`] (no entity expansion, no external entities); the HTML DOM is
//! pruned to [`Limits::max_dom_depth`] and [`Limits::max_nodes`] before anything walks it;
//! links, text, sitemap entries, structured objects and gzip output are capped; the CPU budget
//! [`Limits::cpu_time`] is checked between stages and exceeding it is a permanent error.
//! Handlers never log page content.

mod csv_handler;
mod depth;
mod feed;
mod html;
mod json;
mod lang;
mod simhash;
mod sitemap;
mod structured;
mod text;
mod xml;

use std::sync::Arc;
use std::time::Duration;

use vc_config::ParseConfig;
use vc_core::Result;
use vc_core::media::MediaType;
use vc_core::parse::{HandlerRegistry, Limits, Parsed};
use vc_core::store::CaptureMeta;

pub use crate::csv_handler::{CsvHandler, MAX_CSV_ROWS};
pub use crate::feed::FeedHandler;
pub use crate::html::{DEFAULT_RENDER_MIN_TEXT_CHARS, HtmlHandler};
pub use crate::json::JsonHandler;
pub use crate::simhash::{hamming, simhash64};
pub use crate::sitemap::SitemapHandler;

/// The registry of all built-in handlers with default settings.
pub fn registry() -> HandlerRegistry {
    registry_with(&ParseConfig::default())
}

/// The registry of all built-in handlers, configured from `[parse]`.
pub fn registry_with(cfg: &ParseConfig) -> HandlerRegistry {
    let mut r = HandlerRegistry::new();
    r.register(Arc::new(HtmlHandler {
        render_min_text_chars: cfg.render_min_text_chars,
    }))
    .register(Arc::new(FeedHandler))
    .register(Arc::new(SitemapHandler))
    .register(Arc::new(JsonHandler))
    .register(Arc::new(CsvHandler));
    r
}

/// The parser [`Limits`] from `[parse]`.
pub fn limits_from(cfg: &ParseConfig) -> Limits {
    Limits {
        max_dom_depth: cfg.max_dom_depth,
        max_nodes: cfg.max_nodes,
        max_text_bytes: cfg.max_text_bytes,
        max_links: cfg.max_links,
        max_sitemap_entries: cfg.max_sitemap_entries,
        cpu_time: Duration::from_millis(cfg.cpu_time_ms),
    }
}

/// Sniffs a body like [`MediaType::sniff`], looking through one gzip layer.
pub fn sniff(body: &[u8]) -> Option<MediaType> {
    if text::is_gzip(body) {
        // 4 KiB of output is plenty for a 512-byte sniff window, and bounds the work.
        let mut prefix = Vec::with_capacity(4096);
        let d = flate2::read::MultiGzDecoder::new(body);
        let _ = std::io::Read::read_to_end(&mut std::io::Read::take(d, 4096), &mut prefix);
        return MediaType::sniff(&prefix);
    }
    MediaType::sniff(body)
}

/// Parses a capture with the handler for its stored media type (fixed at fetch time, r5 Q5).
/// When there is none and the stored type is generic (`xml`, `text`, `other`), the body is
/// sniffed (through gzip) and the sniffed type's handler is used. `Ok(None)`: no handler.
pub fn parse(
    reg: &HandlerRegistry,
    meta: &CaptureMeta,
    body: &[u8],
    lim: &Limits,
) -> Result<Option<Parsed>> {
    if let Some(h) = reg.for_media(&meta.media_type) {
        return h.handle(meta, body, lim).map(Some);
    }
    if !matches!(
        meta.media_type,
        MediaType::Xml | MediaType::Text | MediaType::Other
    ) {
        return Ok(None);
    }
    let Some(sniffed) = sniff(body) else {
        return Ok(None);
    };
    match reg.for_media(&sniffed) {
        Some(h) => {
            let mut m = meta.clone();
            m.media_type = sniffed;
            h.handle(&m, body, lim).map(Some)
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_mirror_config() {
        assert_eq!(limits_from(&ParseConfig::default()), Limits::default());
    }
}
