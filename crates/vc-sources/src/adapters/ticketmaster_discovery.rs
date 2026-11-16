//! `ticketmaster_discovery`: Ticketmaster Discovery API v2 event search (`api_terms`; the
//! source sets `max_retention_days` and the display requirements).
//!
//! The leased URL is an `events.json` search (e.g. `?countryCode=FR&size=100`). The API key
//! is read from the source's `key_secret` and sent as the `apikey` query parameter, which is
//! **stripped** from every stored URL and error. Pages are followed through `_links.next`, up
//! to `max_pages` per run and never past Ticketmaster's 1000-item deep-paging limit; each page
//! is spooled before it is parsed.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json, on_origin};

/// The query parameter carrying the key.
pub const KEY_PARAM: &str = "apikey";

/// One event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TmEvent {
    /// Ticketmaster id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Deep link (display requirement).
    pub url: Option<String>,
    /// Local start date.
    pub local_date: Option<String>,
    /// Time zone.
    pub timezone: Option<String>,
    /// Segment (Music, Sports, …).
    pub segment: Option<String>,
    /// First venue: name, city, country code, lat, lon.
    pub venue: Option<TmVenue>,
}

/// A venue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TmVenue {
    /// Name.
    pub name: String,
    /// City.
    pub city: Option<String>,
    /// ISO country code.
    pub country_code: Option<String>,
    /// Latitude.
    pub lat: Option<f64>,
    /// Longitude.
    pub lon: Option<f64>,
}

/// One page of results.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TmPage {
    /// Events.
    pub events: Vec<TmEvent>,
    /// The `next` link (template parts removed), if any.
    pub next: Option<String>,
    /// `page.number`, `page.size`, `page.totalPages` when present.
    pub page: Option<(u32, u32, u32)>,
}

#[derive(Deserialize)]
struct Doc {
    #[serde(rename = "_links", default)]
    links: Option<Links>,
    #[serde(rename = "_embedded", default)]
    embedded: Option<Embedded>,
    page: Option<PageInfo>,
}
#[derive(Deserialize)]
struct Links {
    next: Option<Href>,
}
#[derive(Deserialize)]
struct Href {
    href: String,
}
#[derive(Deserialize)]
struct Embedded {
    #[serde(default)]
    events: Vec<RawEvent>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    size: u32,
    total_pages: u32,
    number: u32,
}
#[derive(Deserialize)]
struct RawEvent {
    id: String,
    name: String,
    url: Option<String>,
    dates: Option<Dates>,
    #[serde(default)]
    classifications: Vec<Classification>,
    #[serde(rename = "_embedded")]
    embedded: Option<EventEmbedded>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Dates {
    start: Option<Start>,
    timezone: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Start {
    local_date: Option<String>,
}
#[derive(Deserialize)]
struct Classification {
    segment: Option<Named>,
}
#[derive(Deserialize)]
struct Named {
    name: String,
}
#[derive(Deserialize)]
struct EventEmbedded {
    #[serde(default)]
    venues: Vec<RawVenue>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVenue {
    name: String,
    city: Option<Named>,
    country: Option<CountryCode>,
    location: Option<Location>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountryCode {
    country_code: Option<String>,
}
#[derive(Deserialize)]
struct Location {
    latitude: Option<String>,
    longitude: Option<String>,
}

/// Removes URI-template parts (`{&page,sort}`) from a HAL link.
pub fn strip_template(href: &str) -> String {
    let mut out = String::with_capacity(href.len());
    let mut depth = 0u32;
    for c in href.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Parses one search page.
pub fn parse(body: &[u8]) -> Result<TmPage> {
    let d: Doc = json(body)?;
    let events = d
        .embedded
        .map(|e| e.events)
        .unwrap_or_default()
        .into_iter()
        .map(|e| {
            let venue = e
                .embedded
                .and_then(|x| x.venues.into_iter().next())
                .map(|v| {
                    let (lat, lon) = v
                        .location
                        .map(|l| {
                            (
                                l.latitude.and_then(|s| s.parse().ok()),
                                l.longitude.and_then(|s| s.parse().ok()),
                            )
                        })
                        .unwrap_or((None, None));
                    TmVenue {
                        name: v.name,
                        city: v.city.map(|c| c.name),
                        country_code: v.country.and_then(|c| c.country_code),
                        lat,
                        lon,
                    }
                });
            let (local_date, timezone) = e
                .dates
                .map(|d| (d.start.and_then(|s| s.local_date), d.timezone))
                .unwrap_or((None, None));
            TmEvent {
                id: e.id,
                name: e.name,
                url: e.url,
                local_date,
                timezone,
                segment: e
                    .classifications
                    .into_iter()
                    .find_map(|c| c.segment.map(|s| s.name)),
                venue,
            }
        })
        .collect();
    Ok(TmPage {
        events,
        next: d
            .links
            .and_then(|l| l.next)
            .map(|n| strip_template(&n.href)),
        page: d.page.map(|p| (p.number, p.size, p.total_pages)),
    })
}

fn with_key(u: &CanonicalUrl, key: &str) -> Result<CanonicalUrl> {
    let mut x = u.as_url().clone();
    let kept: Vec<(String, String)> = x
        .query_pairs()
        .filter(|(k, _)| k != KEY_PARAM)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    x.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair(KEY_PARAM, key);
    CanonicalUrl::from_url(x).map_err(|e| Error::Invalid(e.to_string()))
}

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct TicketmasterDiscovery {
    /// Pages per run.
    pub max_pages: u32,
}

impl TicketmasterDiscovery {
    /// Five pages per run.
    pub const DEFAULT: TicketmasterDiscovery = TicketmasterDiscovery { max_pages: 5 };
}

impl Default for TicketmasterDiscovery {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[async_trait]
impl SourceAdapter for TicketmasterDiscovery {
    fn id(&self) -> &'static str {
        "ticketmaster_discovery"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let secret = ctx
            .source
            .key_secret
            .clone()
            .ok_or_else(|| Error::Invalid("ticketmaster: source has no key_secret".into()))?;
        let secrets = ctx.secrets;
        let key = secrets.get(&secret).await?;
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            redact: &[KEY_PARAM],
        };
        let mut next = Some(ctx.url.url.clone());
        let mut pages = 0;
        while let Some(u) = next.take() {
            if pages >= self.max_pages {
                rep.cursor = Some(super::common::redact_query(&u, &[KEY_PARAM]).path_and_query());
                return Ok(rep);
            }
            let ex = fetch_raw(ctx, with_key(&u, key.expose())?, &opts, &mut rep).await?;
            check_status(&ex)?;
            pages += 1;
            let page = parse(&ex.body)?;
            let deep_limit = page
                .page
                .is_some_and(|(n, size, total)| n + 1 >= total || (n + 2) * size > 1000);
            if !deep_limit && let Some(href) = page.next {
                next = Some(on_origin(&ctx.url.url, &href)?);
            }
        }
        rep.complete = true;
        Ok(rep)
    }
}
