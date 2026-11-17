//! `json_events`: one adapter for keyless open-data event lists served as paged JSON (Socrata
//! SODA resources, Django REST Framework lists, plain JSON arrays). What differs per source is
//! data, not code: an [`EventMapping`] in [`MAPPINGS`] names the endpoint path, where the item
//! array is, how pages advance, and which JSON pointer holds each `event` field.
//!
//! The leased URL is the source's seed. Each page is spooled before it is parsed (raw first);
//! pages are followed up to `max_pages` per run, and a run that stops early leaves a cursor
//! (the next page's path and query on the same origin) so the next run continues there.
//!
//! **Adding a source:** add a mapping (path, items pointer, pager, field pointers, the fixed
//! city/country when the items carry none) and point the source's `adapter` at `json_events`.
//! Keyed event APIs (DATAtourisme, Korea TourAPI, Germany KG, Edinburgh Festivals, OpenAgenda,
//! RIDB…) would also need a `key_param` here (sent like Ticketmaster's `apikey`, stripped from
//! every stored URL), plus the mapping; none is enabled until the owner registers a key.

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::ResumableAdapter;
use super::common::{Fetch, check_status, fetch_raw, json, on_origin};

/// How a list advances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pager {
    /// One page only.
    Single,
    /// Socrata SODA: `$limit`/`$offset`, ordered by `order`; with `upcoming_field`, only rows
    /// whose date is today or later (`$where`). A short page ends the list.
    SocrataOffset {
        /// Rows per page.
        limit: u32,
        /// `$order` (a stable order is required for offset paging).
        order: &'static str,
        /// Date column for the upcoming-only filter.
        upcoming_field: Option<&'static str>,
    },
    /// A `page` number parameter, with the total page count at a pointer.
    PageNumber {
        /// Query parameter.
        param: &'static str,
        /// Pointer to the current page number.
        current: &'static str,
        /// Pointer to the total page count.
        total: &'static str,
    },
}

/// JSON pointers (RFC 6901) into one item for each `event` field; `""` = not provided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    /// Stable id within the source.
    pub id: &'static str,
    /// Name.
    pub name: &'static str,
    /// Description (HTML stripped, cut to 600 chars).
    pub description: &'static str,
    /// Short description, preferred over `description` when present.
    pub snippet: &'static str,
    /// First day (any `YYYY-MM-DD…` value).
    pub start_date: &'static str,
    /// Last day.
    pub end_date: &'static str,
    /// Start time (`HH:MM`, `HH:MM:SS` or `h:mm am/pm`).
    pub start_time: &'static str,
    /// Venue name.
    pub venue: &'static str,
    /// Address.
    pub address: &'static str,
    /// City (else the mapping's fixed city).
    pub city: &'static str,
    /// Latitude.
    pub lat: &'static str,
    /// Longitude.
    pub lng: &'static str,
    /// The source's page for the event.
    pub url: &'static str,
    /// The organiser's or ticket page.
    pub ticket_url: &'static str,
    /// A free flag (`1`/`true`).
    pub free: &'static str,
    /// Price as written.
    pub price_text: &'static str,
    /// Category title or slug (first array element if an array of objects with `title`).
    pub category: &'static str,
}

impl Fields {
    /// No fields.
    pub const NONE: Fields = Fields {
        id: "",
        name: "",
        description: "",
        snippet: "",
        start_date: "",
        end_date: "",
        start_time: "",
        venue: "",
        address: "",
        city: "",
        lat: "",
        lng: "",
        url: "",
        ticket_url: "",
        free: "",
        price_text: "",
        category: "",
    };
}

/// One source's mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventMapping {
    /// The source id it was written for (docs and tests).
    pub source: &'static str,
    /// The endpoint path it applies to (exact, as in the seed URL).
    pub path: &'static str,
    /// Pointer to the item array (`""` = the body is the array).
    pub items: &'static str,
    /// Paging.
    pub pager: Pager,
    /// Field pointers.
    pub fields: Fields,
    /// City when items carry none (English).
    pub city: &'static str,
    /// Country (English).
    pub country: &'static str,
    /// ISO 3166-1 alpha-2.
    pub country_code: &'static str,
    /// `event.category` when the item's category maps to none.
    pub default_category: &'static str,
}

/// Every built-in mapping.
pub const MAPPINGS: &[EventMapping] = &[
    // NYC Parks Events Listing, Events_Events table (NYC Open Data `fudw-fgrp`). Locations are
    // a separate table; `location_description` names the park.
    EventMapping {
        source: "nyc-parks-events",
        path: "/resource/fudw-fgrp.json",
        items: "",
        pager: Pager::SocrataOffset {
            limit: 1000,
            order: "date,event_id",
            upcoming_field: Some("date"),
        },
        fields: Fields {
            id: "/event_id",
            name: "/title",
            description: "/description",
            snippet: "/snippet",
            start_date: "/date",
            end_date: "/date",
            start_time: "/start_time",
            venue: "/location_description",
            url: "/url",
            free: "/cost_free",
            price_text: "/cost_description",
            ..Fields::NONE
        },
        city: "New York City",
        country: "United States",
        country_code: "US",
        default_category: "other",
    },
    // Visit Stockholm public events list (DRF, `?format=json`): `title`/`description` are
    // `{en, sv}` objects; the item's `city` is the district or municipality.
    EventMapping {
        source: "stockholm-open-api",
        path: "/api/public-v1/events/",
        items: "/results",
        pager: Pager::PageNumber {
            param: "page",
            current: "/current_page",
            total: "/total_pages",
        },
        fields: Fields {
            id: "/id",
            name: "/title/en",
            description: "/description/en",
            start_date: "/start_date",
            end_date: "/end_date",
            start_time: "/start_time",
            venue: "/venue_name",
            address: "/address",
            city: "/city",
            lat: "/location/latitude",
            lng: "/location/longitude",
            url: "/url",
            ticket_url: "/external_website_url",
            category: "/categories",
            ..Fields::NONE
        },
        city: "Stockholm",
        country: "Sweden",
        country_code: "SE",
        default_category: "other",
    },
];

/// The mapping for a URL (by path).
pub fn mapping_for(url: &CanonicalUrl) -> Option<&'static EventMapping> {
    let p = url.as_url().path();
    MAPPINGS.iter().find(|m| m.path == p)
}

/// One parsed page.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// The items.
    pub items: Vec<Value>,
    /// `(current, total)` page numbers, for [`Pager::PageNumber`].
    pub pages: Option<(u64, u64)>,
}

fn at<'a>(v: &'a Value, ptr: &str) -> Option<&'a Value> {
    if ptr.is_empty() {
        return None;
    }
    v.pointer(ptr).filter(|x| !x.is_null())
}

fn num(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Parses one page with `m`.
pub fn parse(m: &EventMapping, body: &[u8]) -> Result<Page> {
    let doc: Value = json(body)?;
    let arr = if m.items.is_empty() {
        Some(&doc)
    } else {
        doc.pointer(m.items)
    };
    let items = arr
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Invalid(format!("json_events: no item list for {}", m.source)))?
        .clone();
    let pages = match m.pager {
        Pager::PageNumber { current, total, .. } => doc
            .pointer(current)
            .and_then(num)
            .zip(doc.pointer(total).and_then(num)),
        _ => None,
    };
    Ok(Page { items, pages })
}

/// The first page's URL for a seed: Socrata parameters are set here (the date filter from
/// `today`), other pagers keep the seed's own query.
pub fn first_page(m: &EventMapping, seed: &CanonicalUrl, today: &str) -> Result<CanonicalUrl> {
    match m.pager {
        Pager::SocrataOffset {
            limit,
            order,
            upcoming_field,
        } => {
            let mut u = seed.as_url().clone();
            let kept: Vec<(String, String)> = u
                .query_pairs()
                .filter(|(k, _)| !k.starts_with('$'))
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            {
                let mut q = u.query_pairs_mut();
                q.clear().extend_pairs(kept);
                if let Some(f) = upcoming_field {
                    q.append_pair("$where", &format!("{f} >= '{today}'"));
                }
                q.append_pair("$order", order)
                    .append_pair("$limit", &limit.to_string())
                    .append_pair("$offset", "0");
            }
            CanonicalUrl::from_url(u).map_err(|e| Error::Invalid(e.to_string()))
        }
        _ => Ok(seed.clone()),
    }
}

/// The page after `url`, if the list goes on.
pub fn next_page(
    m: &EventMapping,
    url: &CanonicalUrl,
    page: &Page,
) -> Result<Option<CanonicalUrl>> {
    let set = |name: &str, value: String| -> Result<CanonicalUrl> {
        let mut u = url.as_url().clone();
        let kept: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| k != name)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        u.query_pairs_mut()
            .clear()
            .extend_pairs(kept)
            .append_pair(name, &value);
        CanonicalUrl::from_url(u).map_err(|e| Error::Invalid(e.to_string()))
    };
    match m.pager {
        Pager::Single => Ok(None),
        Pager::SocrataOffset { limit, .. } => {
            if (page.items.len() as u64) < u64::from(limit) {
                return Ok(None);
            }
            let off: u64 = url
                .as_url()
                .query_pairs()
                .find(|(k, _)| k == "$offset")
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            set("$offset", (off + u64::from(limit)).to_string()).map(Some)
        }
        Pager::PageNumber { param, .. } => match page.pages {
            Some((cur, total)) if cur < total => set(param, (cur + 1).to_string()).map(Some),
            _ => Ok(None),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Items → `event` fields (used by `records`)
// ---------------------------------------------------------------------------------------------

fn text(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!s.is_empty()).then_some(s)
}

/// Strips HTML tags and decodes the common entities; collapses whitespace.
pub fn plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t = t.trim_end().to_string();
    t.push('…');
    t
}

/// `HH:MM` from `HH:MM`, `HH:MM:SS` or `h:mm am/pm` (`None` otherwise).
pub fn hhmm(t: &str) -> Option<String> {
    let t = t.trim().to_ascii_lowercase();
    let (clock, pm) = if let Some(x) = t.strip_suffix("pm").or_else(|| t.strip_suffix("p.m.")) {
        (x.trim().to_string(), Some(true))
    } else if let Some(x) = t.strip_suffix("am").or_else(|| t.strip_suffix("a.m.")) {
        (x.trim().to_string(), Some(false))
    } else {
        (t.clone(), None)
    };
    let mut parts = clock.split(':');
    let h: u32 = parts.next()?.trim().parse().ok()?;
    let m: u32 = parts.next().unwrap_or("0").trim().parse().ok()?;
    if m > 59 {
        return None;
    }
    let h = match pm {
        Some(true) if (1..=11).contains(&h) => h + 12,
        Some(true) if h == 12 => 12,
        Some(false) if h == 12 => 0,
        Some(_) if !(1..=12).contains(&h) => return None,
        _ => h,
    };
    (h <= 23).then(|| format!("{h:02}:{m:02}"))
}

fn day(v: &str) -> Option<String> {
    let d = v.trim().get(..10)?;
    let b = d.as_bytes();
    (b[4] == b'-' && b[7] == b'-' && d.bytes().filter(u8::is_ascii_digit).count() == 8)
        .then(|| d.to_string())
}

fn http(v: &str) -> Option<String> {
    let v = v.trim();
    ((v.starts_with("https://") || v.starts_with("http://"))
        && !v.contains(char::is_whitespace)
        && v.len() <= 2048)
        .then(|| v.to_string())
}

/// `event.category` for a source category title or slug.
pub fn category(label: &str) -> Option<&'static str> {
    let l = label.to_ascii_lowercase();
    let has = |w: &[&str]| w.iter().any(|x| l.contains(x));
    Some(if has(&["exhibition", "museum", "gallery"]) {
        "exhibition"
    } else if has(&["festival"]) {
        "festival"
    } else if has(&["concert", "music", "club"]) {
        "music"
    } else if has(&["theatre", "theater", "stage", "dance", "opera", "comedy"]) {
        "theatre"
    } else if has(&["sport", "fitness"]) {
        "sports"
    } else if has(&["market"]) {
        "market"
    } else if has(&["christmas", "holiday", "season"]) {
        "seasonal"
    } else if has(&["conference", "lecture", "talk", "seminar"]) {
        "conference"
    } else {
        return None;
    })
}

fn category_of(m: &EventMapping, item: &Value) -> &'static str {
    let label = match at(item, m.fields.category) {
        Some(Value::Array(a)) => a.first().and_then(|c| {
            c.get("title")
                .or_else(|| c.get("slug"))
                .and_then(Value::as_str)
                .or_else(|| c.as_str())
                .map(str::to_string)
        }),
        Some(v) => text(v),
        None => None,
    };
    label
        .as_deref()
        .and_then(category)
        .unwrap_or(m.default_category)
}

/// The `event` fields of one item (`None` without a name or a start date).
pub fn event(m: &EventMapping, item: &Value) -> Option<(Option<String>, Value)> {
    let f = &m.fields;
    let get = |p: &str| at(item, p).and_then(text);
    let name = get(f.name)?;
    let start = get(f.start_date).as_deref().and_then(day)?;
    let end = get(f.end_date)
        .as_deref()
        .and_then(day)
        .filter(|e| *e >= start);
    let desc = get(f.snippet)
        .or_else(|| get(f.description))
        .map(|d| cut(&plain(&d), 600))
        .filter(|d| !d.is_empty());
    let coord = |p: &str, lim: f64| {
        at(item, p)
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()))
            .filter(|x| x.is_finite() && x.abs() <= lim)
    };
    let free = at(item, f.free).is_some_and(|v| {
        v.as_bool() == Some(true) || num(v) == Some(1) || v.as_str() == Some("true")
    });
    let price_text = get(f.price_text).map(|t| cut(&plain(&t), 80));
    let price = (free || price_text.is_some()).then(|| {
        let mut p = Map::new();
        if let Some(t) = price_text {
            p.insert("text".into(), json!(t));
        }
        if free {
            p.insert("is_free".into(), json!(true));
        }
        Value::Object(p)
    });
    let city = get(f.city).unwrap_or_else(|| m.city.to_string());
    let mut o = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            o.insert(k.to_string(), v);
        }
    };
    put("name", Some(json!(cut(&name, 160))));
    put("category", Some(json!(category_of(m, item))));
    put("description", desc.map(Value::String));
    put("start_date", Some(json!(start)));
    put("end_date", end.map(Value::String));
    put(
        "start_time",
        get(f.start_time)
            .as_deref()
            .and_then(hhmm)
            .map(Value::String),
    );
    put("venue", get(f.venue).map(|v| json!(cut(&plain(&v), 160))));
    put("address", get(f.address).map(|v| json!(cut(&v, 240))));
    put("city", Some(json!(cut(&city, 120))));
    put("country", Some(json!(m.country)));
    put("country_code", Some(json!(m.country_code)));
    let (lat, lng) = (coord(f.lat, 90.0), coord(f.lng, 180.0));
    if let (Some(a), Some(b)) = (lat, lng) {
        put("lat", Some(json!(a)));
        put("lng", Some(json!(b)));
    }
    put("price", price);
    put(
        "ticket_url",
        get(f.ticket_url)
            .as_deref()
            .and_then(http)
            .map(Value::String),
    );
    put(
        "source_url",
        get(f.url).as_deref().and_then(http).map(Value::String),
    );
    let id = get(f.id).map(|id| {
        if f.start_date == f.end_date || f.end_date.is_empty() {
            // One row per occurrence (Socrata event tables): the date is part of the key.
            format!("{}:{id}:{start}", m.source)
        } else {
            format!("{}:{id}", m.source)
        }
    });
    Some((id, Value::Object(o)))
}

// ---------------------------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------------------------

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct JsonEvents {
    /// Pages per run.
    pub max_pages: u32,
}

impl JsonEvents {
    /// Five pages per run.
    pub const DEFAULT: JsonEvents = JsonEvents { max_pages: 5 };
}

impl Default for JsonEvents {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[async_trait]
impl SourceAdapter for JsonEvents {
    fn id(&self) -> &'static str {
        "json_events"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        self.run_from(ctx, None).await
    }
}

/// The page URL a cursor (a path and query on the leased row's origin) points at, if it stays
/// on the leased row's origin and endpoint path.
pub fn resume_url(row: &CanonicalUrl, cursor: &str) -> Option<CanonicalUrl> {
    if !cursor.starts_with('/') {
        return None;
    }
    let u = on_origin(row, cursor).ok()?;
    (u.as_url().path() == row.as_url().path()).then_some(u)
}

#[async_trait]
impl ResumableAdapter for JsonEvents {
    async fn run_from(
        &self,
        ctx: &mut AdapterCtx<'_>,
        cursor: Option<&str>,
    ) -> Result<AdapterReport> {
        let row = ctx.url.url.clone();
        let m = mapping_for(&row)
            .ok_or_else(|| Error::Invalid("json_events: no mapping for this endpoint".into()))?;
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let today = ctx.clock.now().format("%Y-%m-%d").to_string();
        let start = match cursor.and_then(|c| resume_url(&row, c)) {
            Some(u) => u,
            None => first_page(m, &row, &today)?,
        };
        let mut next = Some(start);
        let mut pages = 0;
        while let Some(u) = next.take() {
            if pages >= self.max_pages {
                rep.cursor = Some(u.path_and_query());
                return Ok(rep);
            }
            let ex = fetch_raw(ctx, u.clone(), &opts, &mut rep).await?;
            check_status(&ex)?;
            pages += 1;
            let page = parse(m, &ex.body)?;
            next = next_page(m, &u, &page)?;
        }
        rep.complete = true;
        Ok(rep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_normalise() {
        assert_eq!(hhmm("7:30 pm").as_deref(), Some("19:30"));
        assert_eq!(hhmm("12:00 am").as_deref(), Some("00:00"));
        assert_eq!(hhmm("12:15 p.m.").as_deref(), Some("12:15"));
        assert_eq!(hhmm("09:05:00").as_deref(), Some("09:05"));
        assert_eq!(hhmm("25:00"), None);
        assert_eq!(hhmm("noon"), None);
    }

    #[test]
    fn html_is_stripped() {
        assert_eq!(
            plain("<p>Free&nbsp;<b>tour</b></p>\n of the park"),
            "Free tour of the park"
        );
    }

    #[test]
    fn mappings_have_unique_paths_and_names() {
        let mut paths: Vec<&str> = MAPPINGS.iter().map(|m| m.path).collect();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), MAPPINGS.len());
        for m in MAPPINGS {
            assert!(
                !m.fields.name.is_empty() && !m.fields.start_date.is_empty(),
                "{}",
                m.source
            );
            assert!(
                m.country_code.len() == 2 && !m.city.is_empty(),
                "{}",
                m.source
            );
        }
    }
}
