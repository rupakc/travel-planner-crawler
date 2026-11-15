//! RSS 0.9x/1.0/2.0 and Atom feeds (feed-rs), after the DTD guard.

use vc_core::media::MediaTypeMatch;
use vc_core::parse::{ContentHandler, Limits, Link, LinkKind, Parsed};
use vc_core::store::CaptureMeta;
use vc_core::{Error, Result};

use crate::lang::primary_subtag;
use crate::text::{maybe_gunzip, short_text};
use crate::xml::reject_dtd;

/// RSS and Atom. Produces the feed title, its language and one [`LinkKind::FeedItem`] link per
/// entry (anchor = entry title, `lastmod` = updated, else published), capped at
/// `Limits::max_links`.
#[derive(Debug, Default, Clone, Copy)]
pub struct FeedHandler;

impl ContentHandler for FeedHandler {
    fn media(&self) -> &'static [MediaTypeMatch] {
        &[MediaTypeMatch::AnyFeed]
    }

    fn handle(&self, meta: &CaptureMeta, body: &[u8], lim: &Limits) -> Result<Parsed> {
        let body = maybe_gunzip(body)?;
        reject_dtd(&body)?;
        let feed =
            feed_rs::parser::parse(&body[..]).map_err(|e| Error::Invalid(format!("feed: {e}")))?;
        // Relative item links resolve against the feed's own URL.
        let mut links = Vec::new();
        for entry in &feed.entries {
            if links.len() >= lim.max_links {
                break;
            }
            let href = entry
                .links
                .iter()
                .find(|l| l.rel.as_deref().is_none_or(|r| r == "alternate"))
                .or_else(|| entry.links.first())
                .map(|l| l.href.as_str())
                // RSS items without <link> often use a permalink <guid>.
                .or_else(|| {
                    (entry.id.starts_with("http://") || entry.id.starts_with("https://"))
                        .then_some(entry.id.as_str())
                });
            let Some(url) = href.and_then(|h| meta.url.join(h).ok()) else {
                continue;
            };
            links.push(Link {
                url,
                anchor: entry
                    .title
                    .as_ref()
                    .map(|t| short_text(&t.content))
                    .unwrap_or_default(),
                kind: LinkKind::FeedItem,
                rel: Vec::new(),
                lastmod: entry.updated.or(entry.published),
            });
        }
        Ok(Parsed {
            title: feed
                .title
                .as_ref()
                .map(|t| short_text(&t.content))
                .filter(|t| !t.is_empty()),
            lang: feed.language.as_deref().and_then(primary_subtag),
            links,
            ..Parsed::default()
        })
    }
}
