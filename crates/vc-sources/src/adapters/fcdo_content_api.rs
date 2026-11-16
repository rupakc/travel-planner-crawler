//! `fcdo_content_api`: UK FCDO foreign travel advice through the GOV.UK Content API.
//!
//! Three kinds of leased row, told apart by path:
//! - the index `/api/content/foreign-travel-advice` → spool, then enqueue one row per country
//!   (`/api/content/foreign-travel-advice/<slug>`) and the Atom change feed;
//! - a country `/api/content/foreign-travel-advice/<slug>` → spool (the processor extracts
//!   entry requirements, safety, emergency numbers from the parts);
//! - the change feed `/foreign-travel-advice.atom` → spool, then enqueue the changed
//!   countries with a high priority.
//!
//! Follow-ups are always resolved on the leased URL's own origin.

use async_trait::async_trait;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{
    Fetch, check_status, enqueue_adapter_rows, fetch_raw, json, on_origin, xml_attr,
};

/// The index path.
pub const INDEX_PATH: &str = "/api/content/foreign-travel-advice";
/// The Atom change feed path.
pub const FEED_PATH: &str = "/foreign-travel-advice.atom";

/// One country listed by the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FcdoCountry {
    /// URL slug (`france`).
    pub slug: String,
    /// Display name.
    pub name: String,
    /// The Content API path of the country page.
    pub api_path: String,
    /// The latest change description.
    pub change_description: Option<String>,
}

#[derive(Deserialize)]
struct Index {
    links: IndexLinks,
}
#[derive(Deserialize)]
struct IndexLinks {
    #[serde(default)]
    children: Vec<Child>,
}
#[derive(Deserialize)]
struct Child {
    api_path: String,
    #[serde(default)]
    details: ChildDetails,
}
#[derive(Deserialize, Default)]
struct ChildDetails {
    change_description: Option<String>,
    country: Option<Country>,
}
#[derive(Deserialize)]
struct Country {
    name: String,
    slug: String,
}

/// Parses the index (`links.children`).
pub fn parse_index(body: &[u8]) -> Result<Vec<FcdoCountry>> {
    let idx: Index = json(body)?;
    let out: Vec<FcdoCountry> = idx
        .links
        .children
        .into_iter()
        .filter(|c| c.api_path.starts_with(&format!("{INDEX_PATH}/")))
        .filter_map(|c| {
            let country = c.details.country?;
            Some(FcdoCountry {
                slug: country.slug,
                name: country.name,
                api_path: c.api_path,
                change_description: c.details.change_description,
            })
        })
        .collect();
    if out.is_empty() {
        return Err(Error::Invalid("fcdo: index lists no countries".into()));
    }
    Ok(out)
}

/// The advice for one country (headline fields; the processor reads the parts).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FcdoAdvice {
    /// Country slug.
    pub slug: String,
    /// Country name.
    pub country: String,
    /// `public_updated_at`.
    pub updated_at: Option<String>,
    /// Alert statuses (e.g. `avoid_all_but_essential_travel_to_parts`).
    pub alert_status: Vec<String>,
    /// `(slug, title)` of each part.
    pub parts: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct Page {
    public_updated_at: Option<String>,
    details: PageDetails,
}
#[derive(Deserialize)]
struct PageDetails {
    country: Country,
    #[serde(default)]
    alert_status: Vec<String>,
    #[serde(default)]
    parts: Vec<Part>,
}
#[derive(Deserialize)]
struct Part {
    slug: String,
    title: String,
}

/// Parses one country page.
pub fn parse_country(body: &[u8]) -> Result<FcdoAdvice> {
    let p: Page = json(body)?;
    if p.details.parts.is_empty() {
        return Err(Error::Invalid("fcdo: country page without parts".into()));
    }
    Ok(FcdoAdvice {
        slug: p.details.country.slug,
        country: p.details.country.name,
        updated_at: p.public_updated_at,
        alert_status: p.details.alert_status,
        parts: p
            .details
            .parts
            .into_iter()
            .map(|x| (x.slug, x.title))
            .collect(),
    })
}

/// One change-feed entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FcdoChange {
    /// Country slug.
    pub slug: String,
    /// `updated`.
    pub updated: String,
}

/// Parses the Atom change feed (entries whose `alternate` link is a travel-advice page).
pub fn parse_changes(body: &[u8]) -> Result<Vec<FcdoChange>> {
    let text = std::str::from_utf8(body).map_err(|e| Error::Invalid(e.to_string()))?;
    let mut r = quick_xml::Reader::from_str(text);
    let mut out = Vec::new();
    let (mut in_entry, mut in_updated) = (false, false);
    let (mut slug, mut updated): (Option<String>, String) = (None, String::new());
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                "entry" => {
                    in_entry = true;
                    slug = None;
                    updated.clear();
                }
                "updated" if in_entry => in_updated = true,
                _ => {}
            },
            Ok(Event::Empty(e)) if in_entry && e.local_name().as_ref() == "link" => {
                if xml_attr(&e, "rel").as_deref() == Some("alternate")
                    && let Some(h) = xml_attr(&e, "href")
                    && let Some(rest) = h
                        .split_once("/foreign-travel-advice/")
                        .map(|(_, r)| r.trim_end_matches('/'))
                    && !rest.is_empty()
                    && rest.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                {
                    slug = Some(rest.to_string());
                }
            }
            Ok(Event::Text(t)) if in_updated => updated.push_str(&t.into_inner()),
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                "updated" => in_updated = false,
                "entry" => {
                    in_entry = false;
                    if let Some(s) = slug.take() {
                        out.push(FcdoChange {
                            slug: s,
                            updated: updated.trim().to_string(),
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(Error::Invalid(format!("fcdo atom: {e}"))),
            _ => {}
        }
    }
    Ok(out)
}

/// What a leased URL is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Index,
    Country,
    Feed,
}

fn row_of(u: &CanonicalUrl) -> Option<Row> {
    let p = u.as_url().path().trim_end_matches('/');
    if p == INDEX_PATH {
        Some(Row::Index)
    } else if p.starts_with(&format!("{INDEX_PATH}/")) {
        Some(Row::Country)
    } else if p == FEED_PATH {
        Some(Row::Feed)
    } else {
        None
    }
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct FcdoContentApi;

#[async_trait]
impl SourceAdapter for FcdoContentApi {
    fn id(&self) -> &'static str {
        "fcdo_content_api"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let url = ctx.url.url.clone();
        let row = row_of(&url)
            .ok_or_else(|| Error::Invalid("fcdo: not an index, country or feed URL".into()))?;
        let (accept, hint) = match row {
            Row::Feed => ("application/atom+xml", Some(MediaType::Atom)),
            _ => ("application/json", Some(MediaType::Json)),
        };
        let opts = Fetch {
            accept: Some(accept),
            hint,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url.clone(), &opts, &mut rep).await?;
        check_status(&ex)?;
        match row {
            Row::Index => {
                let countries = parse_index(&ex.body)?;
                let mut urls = countries
                    .iter()
                    .map(|c| on_origin(&url, &c.api_path))
                    .collect::<Result<Vec<_>>>()?;
                urls.sort();
                enqueue_adapter_rows(ctx, self.id(), urls, 0.6, &mut rep).await?;
                let feed = on_origin(&url, FEED_PATH)?;
                enqueue_adapter_rows(ctx, self.id(), vec![feed], 0.8, &mut rep).await?;
            }
            Row::Country => {
                parse_country(&ex.body)?;
            }
            Row::Feed => {
                let changes = parse_changes(&ex.body)?;
                let urls = changes
                    .iter()
                    .map(|c| on_origin(&url, &format!("{INDEX_PATH}/{}", c.slug)))
                    .collect::<Result<Vec<_>>>()?;
                enqueue_adapter_rows(ctx, self.id(), urls, 0.9, &mut rep).await?;
            }
        }
        rep.complete = true;
        Ok(rep)
    }
}
