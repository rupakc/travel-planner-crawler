//! Sitemaps: `<urlset>`, `<sitemapindex>`, gzip'd variants and plain-text sitemaps.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use quick_xml::Reader;
use quick_xml::events::Event;
use vc_core::media::{MediaType, MediaTypeMatch};
use vc_core::parse::{ContentHandler, Limits, Link, LinkKind, Parsed};
use vc_core::store::CaptureMeta;
use vc_core::{Error, Result};

use crate::text::{Deadline, decode, maybe_gunzip};
use crate::xml::{push_ref, reject_dtd};

/// XML and plain-text sitemaps (and sitemap indexes), gzip'd or not. Entries are capped at
/// `Limits::max_sitemap_entries`; unparseable `<loc>`s are skipped.
#[derive(Debug, Default, Clone, Copy)]
pub struct SitemapHandler;

impl ContentHandler for SitemapHandler {
    fn media(&self) -> &'static [MediaTypeMatch] {
        &[MediaTypeMatch::AnySitemap]
    }

    fn handle(&self, meta: &CaptureMeta, body: &[u8], lim: &Limits) -> Result<Parsed> {
        let deadline = Deadline::new(lim.cpu_time);
        let body = maybe_gunzip(body)?;
        let links = if meta.media_type == MediaType::SitemapText {
            text_sitemap(meta, &body, lim)
        } else {
            reject_dtd(&body)?;
            xml_sitemap(meta, &body, lim, &deadline)?
        };
        Ok(Parsed {
            links,
            ..Parsed::default()
        })
    }
}

fn text_sitemap(meta: &CaptureMeta, body: &[u8], lim: &Limits) -> Vec<Link> {
    decode(body, None)
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
        .filter_map(|l| meta.url.join(l).ok())
        .take(lim.max_sitemap_entries)
        .map(|url| Link {
            url,
            anchor: String::new(),
            kind: LinkKind::Page,
            rel: Vec::new(),
            lastmod: None,
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    None,
    Loc,
    Lastmod,
}

fn xml_sitemap(
    meta: &CaptureMeta,
    body: &[u8],
    lim: &Limits,
    deadline: &Deadline,
) -> Result<Vec<Link>> {
    let mut r = Reader::from_reader(body);
    r.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    let mut links = Vec::new();
    // The kind of the entry being read (`<url>` → page, `<sitemap>` → sitemap).
    let mut entry: Option<LinkKind> = None;
    let mut field = Field::None;
    let (mut loc, mut lastmod) = (String::new(), String::new());
    loop {
        if links.len() >= lim.max_sitemap_entries {
            break;
        }
        let ev = r
            .read_event_into(&mut buf)
            .map_err(|e| Error::Invalid(format!("sitemap xml: {e}")))?;
        match ev {
            Event::Start(e) => match e.local_name().as_ref() {
                "url" => entry = Some(LinkKind::Page),
                "sitemap" => entry = Some(LinkKind::Sitemap),
                "loc" if entry.is_some() => {
                    field = Field::Loc;
                    loc.clear();
                }
                "lastmod" if entry.is_some() => {
                    field = Field::Lastmod;
                    lastmod.clear();
                }
                _ => {}
            },
            Event::Text(t) => match field {
                Field::Loc => loc.push_str(&t.xml10_content()),
                Field::Lastmod => lastmod.push_str(&t.xml10_content()),
                Field::None => {}
            },
            Event::CData(t) => {
                let s = t.xml10_content();
                match field {
                    Field::Loc => loc.push_str(&s),
                    Field::Lastmod => lastmod.push_str(&s),
                    Field::None => {}
                }
            }
            Event::GeneralRef(g) => match field {
                Field::Loc => push_ref(&mut loc, &g),
                Field::Lastmod => push_ref(&mut lastmod, &g),
                Field::None => {}
            },
            Event::End(e) => match e.local_name().as_ref() {
                "loc" | "lastmod" => field = Field::None,
                "url" | "sitemap" => {
                    if let Some(kind) = entry.take()
                        && let Ok(url) = meta.url.join(loc.trim())
                    {
                        links.push(Link {
                            url,
                            anchor: String::new(),
                            kind,
                            rel: Vec::new(),
                            lastmod: parse_w3c_datetime(lastmod.trim()),
                        });
                    }
                    loc.clear();
                    lastmod.clear();
                    if links.len() % 1024 == 0 {
                        deadline.check("sitemap")?;
                    }
                }
                _ => {}
            },
            Event::DocType(_) => {
                return Err(Error::Policy("xml dtd not allowed".into()));
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(links)
}

/// Parses a W3C datetime (the sitemap `lastmod` profile of ISO 8601): `YYYY`, `YYYY-MM`,
/// `YYYY-MM-DD`, or a date-time with minutes or seconds and a zone. Zone-less times are UTC.
pub(crate) fn parse_w3c_datetime(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc));
    }
    for f in ["%Y-%m-%dT%H:%M%:z", "%Y-%m-%dT%H:%M%z"] {
        if let Ok(d) = DateTime::parse_from_str(s, f) {
            return Some(d.with_timezone(&Utc));
        }
    }
    let zless = s.strip_suffix('Z').unwrap_or(s);
    for f in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(zless, f) {
            return Some(d.and_utc());
        }
    }
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d"))
        .or_else(|_| NaiveDate::parse_from_str(&format!("{s}-01-01"), "%Y-%m-%d"))
        .ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w3c_datetimes() {
        let f = |s| parse_w3c_datetime(s).map(|d| d.to_rfc3339());
        assert_eq!(f("2026").as_deref(), Some("2026-01-01T00:00:00+00:00"));
        assert_eq!(f("2026-03").as_deref(), Some("2026-03-01T00:00:00+00:00"));
        assert_eq!(
            f("2026-03-04").as_deref(),
            Some("2026-03-04T00:00:00+00:00")
        );
        assert_eq!(
            f("2026-03-04T05:06+02:00").as_deref(),
            Some("2026-03-04T03:06:00+00:00")
        );
        assert_eq!(
            f("2026-03-04T05:06:07.5Z").as_deref(),
            Some("2026-03-04T05:06:07.500+00:00")
        );
        assert_eq!(
            f("2026-03-04T05:06:07").as_deref(),
            Some("2026-03-04T05:06:07+00:00")
        );
        assert_eq!(f("yesterday"), None);
        assert_eq!(f(""), None);
        assert_eq!(f("2026-13-01"), None);
    }
}
