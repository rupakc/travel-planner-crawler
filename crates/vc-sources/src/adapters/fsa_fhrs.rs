//! `fsa_fhrs`: the UK Food Standards Agency's food hygiene ratings (FHRS API v2,
//! `api.ratings.food.gov.uk`, keyless, header `x-api-version: 2`, Open Government Licence v3).
//!
//! Scope: the local authorities of the main tourist cities ([`AUTHORITIES`]), and only the food
//! businesses a traveller eats at ([`BUSINESS_TYPES`]: restaurants and cafes, pubs and bars,
//! takeaways). The leased row is the bare `/Establishments` seed; the adapter walks every
//! (authority, business type) pair in page-number order ([`PAGE_SIZE`] per page), stopping
//! after `max_pages` per run with a cursor (the next page's path and query), so one weekly
//! sweep is about a hundred requests spread over short runs. Each establishment with a
//! published rating becomes one `restaurant` record (FHRS 0–5 in England, Wales and Northern
//! Ireland; FHIS Pass / Improvement Required in Scotland) with its rating date.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::ResumableAdapter;
use super::common::{Fetch, check_status, fetch_raw, json, on_origin};
use super::inspections::{Hygiene, Place, coord, day, phone, query, restaurant, text};

/// Adapter id.
pub const ID: &str = "fsa_fhrs";

/// The establishments endpoint.
pub const PATH: &str = "/Establishments";

/// Headers the API requires.
pub const HEADERS: &[(&str, &str)] = &[("x-api-version", "2")];

/// Establishments per page.
pub const PAGE_SIZE: u32 = 1000;

/// One local authority in scope: FHRS `LocalAuthorityId`, the authority's name, the city its
/// establishments are in (English), and, for authorities that also cover other towns, the
/// city's postcode districts (a kept establishment's postcode must be in one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Authority {
    /// `LocalAuthorityId`.
    pub id: u32,
    /// `LocalAuthorityName`.
    pub name: &'static str,
    /// City (English).
    pub city: &'static str,
    /// The city's postcode districts (outward codes), if the authority covers more than the city.
    pub postcodes: &'static [&'static str],
}

const fn la(id: u32, name: &'static str, city: &'static str) -> Authority {
    Authority {
        id,
        name,
        city,
        postcodes: &[],
    }
}

/// The authorities read (ids from the API's `/Authorities/basic`, 2026-11-18).
pub const AUTHORITIES: &[Authority] = &[
    // Central London boroughs.
    la(120, "Westminster", "London"),
    la(95, "City of London Corporation", "London"),
    la(93, "Camden", "London"),
    la(107, "Kensington and Chelsea", "London"),
    la(117, "Tower Hamlets", "London"),
    la(115, "Southwark", "London"),
    la(109, "Lambeth", "London"),
    la(106, "Islington", "London"),
    la(99, "Hackney", "London"),
    la(98, "Greenwich", "London"),
    // Other visitor cities.
    la(210, "Edinburgh (City of)", "Edinburgh"),
    la(213, "Glasgow City", "Glasgow"),
    la(180, "Manchester", "Manchester"),
    la(179, "Liverpool", "Liverpool"),
    la(374, "Birmingham", "Birmingham"),
    la(324, "Bristol", "Bristol"),
    la(286, "Brighton and Hove", "Brighton"),
    la(406, "York", "York"),
    la(262, "Oxford City", "Oxford"),
    la(1, "Cambridge City", "Cambridge"),
    la(339, "Cardiff", "Cardiff"),
    la(138, "Belfast City", "Belfast"),
    Authority {
        id: 326,
        name: "Bath and North East Somerset",
        city: "Bath",
        postcodes: &["BA1", "BA2"],
    },
];

/// Business types read: `(BusinessTypeID, establishment_type)`.
pub const BUSINESS_TYPES: &[(u32, &str)] =
    &[(1, "restaurant"), (7843, "pub_bar"), (7844, "takeaway")];

/// The authority with this id.
pub fn authority(id: u32) -> Option<&'static Authority> {
    AUTHORITIES.iter().find(|a| a.id == id)
}

fn page_url(row: &CanonicalUrl, la: u32, bt: u32, page: u32) -> Result<CanonicalUrl> {
    on_origin(
        row,
        &format!(
            "{PATH}?localAuthorityId={la}&businessTypeId={bt}&pageNumber={page}&pageSize={PAGE_SIZE}"
        ),
    )
}

/// The first page of the sweep.
pub fn first_page(row: &CanonicalUrl) -> Result<CanonicalUrl> {
    page_url(row, AUTHORITIES[0].id, BUSINESS_TYPES[0].0, 1)
}

/// `(authority, business type, page)` of a page URL.
pub fn position(url: &CanonicalUrl) -> Option<(u32, u32, u32)> {
    let n = |k: &str| query(url, k).and_then(|v| v.parse::<u32>().ok());
    Some((
        n("localAuthorityId")?,
        n("businessTypeId")?,
        n("pageNumber")?,
    ))
}

#[derive(Deserialize)]
struct Meta {
    #[serde(rename = "totalPages", default)]
    total_pages: u32,
    #[serde(rename = "pageNumber", default)]
    page_number: u32,
}

#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    establishments: Vec<Value>,
    meta: Option<Meta>,
}

/// `(page number, total pages)` of a response.
pub type Pages = Option<(u32, u32)>;

/// One parsed page: the establishments and `(page, total pages)`.
pub fn parse(body: &[u8]) -> Result<(Vec<Value>, Pages)> {
    let b: Body = json(body)?;
    Ok((
        b.establishments,
        b.meta.map(|m| (m.page_number, m.total_pages)),
    ))
}

/// The page after `url` in the sweep: the next page of the same pair, else the first page of
/// the next (authority, business type) pair, else `None`.
pub fn next_page(
    row: &CanonicalUrl,
    url: &CanonicalUrl,
    pages: Option<(u32, u32)>,
) -> Result<Option<CanonicalUrl>> {
    let Some((la, bt, page)) = position(url) else {
        return Ok(None);
    };
    if let Some((cur, total)) = pages
        && cur.max(page) < total
    {
        return page_url(row, la, bt, page + 1).map(Some);
    }
    let pairs: Vec<(u32, u32)> = AUTHORITIES
        .iter()
        .flat_map(|a| BUSINESS_TYPES.iter().map(move |(b, _)| (a.id, *b)))
        .collect();
    let Some(i) = pairs.iter().position(|p| *p == (la, bt)) else {
        return Ok(None);
    };
    match pairs.get(i + 1) {
        Some((la, bt)) => page_url(row, *la, *bt, 1).map(Some),
        None => Ok(None),
    }
}

/// The published rating of an establishment: FHRS `0`–`5`, FHIS `Pass`,
/// `Pass and Eat Safe`, `Improvement Required`; `None` for exempt or awaiting.
pub fn rating(value: &str) -> Option<(&'static str, Option<f64>)> {
    let v = value.trim();
    if v.len() == 1
        && let Some(d) = v.chars().next().and_then(|c| c.to_digit(10))
        && d <= 5
    {
        return Some(("uk_fhrs", Some(f64::from(d))));
    }
    match v {
        "Pass" | "Pass and Eat Safe" | "Improvement Required" => Some(("uk_fhis", None)),
        _ => None,
    }
}

/// `restaurant` records (`(id, data)`) of a stored page.
pub fn items(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<(String, Value)>> {
    let (rows, _) = parse(body)?;
    let Some((la_id, bt, _)) = position(url) else {
        return Ok(Vec::new()); // the bare seed (never fetched) or another call
    };
    let Some(la) = authority(la_id) else {
        return Ok(Vec::new());
    };
    let kind = BUSINESS_TYPES
        .iter()
        .find(|(b, _)| *b == bt)
        .map(|(_, k)| *k);
    let mut out = Vec::new();
    for e in &rows {
        let Some(id) = text(e.get("FHRSID")) else {
            continue;
        };
        let Some(name) = text(e.get("BusinessName")) else {
            continue;
        };
        let Some((scheme, value)) = text(e.get("RatingValue")).as_deref().and_then(rating) else {
            continue;
        };
        let Some(date) = e.get("RatingDate").and_then(Value::as_str).and_then(day) else {
            continue;
        };
        let lines: Vec<String> = [
            "AddressLine1",
            "AddressLine2",
            "AddressLine3",
            "AddressLine4",
            "PostCode",
        ]
        .iter()
        .filter_map(|k| text(e.get(*k)))
        .collect();
        if !la.postcodes.is_empty() {
            let outward = text(e.get("PostCode"))
                .and_then(|p| p.split_whitespace().next().map(str::to_uppercase));
            if !outward.is_some_and(|o| la.postcodes.contains(&o.as_str())) {
                continue;
            }
        }
        let geo = e.get("geocode");
        let place = Place {
            name,
            establishment_type: kind.or(Some("other")),
            category_label: text(e.get("BusinessType")),
            address: (!lines.is_empty()).then(|| lines.join(", ")),
            district: text(e.get("LocalAuthorityName")).or(Some(la.name.to_string())),
            city: la.city,
            country: "United Kingdom",
            country_code: "GB",
            lat: coord(geo.and_then(|g| g.get("latitude")), 90.0),
            lng: coord(geo.and_then(|g| g.get("longitude")), 180.0),
            phone: phone(e.get("Phone")),
            ..Place::default()
        };
        let h = Hygiene {
            scheme,
            rating: text(e.get("RatingValue")).unwrap_or_default(),
            value,
            date,
            authority: text(e.get("LocalAuthorityName")).unwrap_or_else(|| la.name.to_string()),
        };
        out.push((format!("fhrs:{id}"), restaurant(&place, &h)));
    }
    Ok(out)
}

/// The page URL a cursor (path and query) points at, if it is an in-scope page.
pub fn resume_url(row: &CanonicalUrl, cursor: &str) -> Option<CanonicalUrl> {
    if !cursor.starts_with('/') {
        return None;
    }
    let u = on_origin(row, cursor).ok()?;
    let (la, bt, page) = position(&u)?;
    (u.as_url().path() == PATH
        && authority(la).is_some()
        && BUSINESS_TYPES.iter().any(|(b, _)| *b == bt)
        && page >= 1)
        .then_some(u)
}

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct FsaFhrs {
    /// Pages per run.
    pub max_pages: u32,
}

impl FsaFhrs {
    /// Five pages per run.
    pub const DEFAULT: FsaFhrs = FsaFhrs { max_pages: 5 };
}

impl Default for FsaFhrs {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[async_trait]
impl SourceAdapter for FsaFhrs {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        self.run_from(ctx, None).await
    }
}

#[async_trait]
impl ResumableAdapter for FsaFhrs {
    async fn run_from(
        &self,
        ctx: &mut AdapterCtx<'_>,
        cursor: Option<&str>,
    ) -> Result<AdapterReport> {
        let row = ctx.url.url.clone();
        if row.as_url().path() != PATH {
            return Err(Error::Invalid(format!("{ID}: not the {PATH} endpoint")));
        }
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            headers: HEADERS,
            ..Fetch::default()
        };
        let mut next = Some(match cursor.and_then(|c| resume_url(&row, c)) {
            Some(u) => u,
            None => first_page(&row)?,
        });
        let mut pages = 0;
        while let Some(u) = next.take() {
            if pages >= self.max_pages {
                rep.cursor = Some(u.path_and_query());
                return Ok(rep);
            }
            let ex = fetch_raw(ctx, u.clone(), &opts, &mut rep).await?;
            check_status(&ex)?;
            pages += 1;
            let (_, meta) = parse(&ex.body)?;
            next = next_page(&row, &u, meta)?;
        }
        rep.complete = true;
        Ok(rep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratings() {
        assert_eq!(rating("5"), Some(("uk_fhrs", Some(5.0))));
        assert_eq!(rating("0"), Some(("uk_fhrs", Some(0.0))));
        assert_eq!(rating("Pass"), Some(("uk_fhis", None)));
        assert_eq!(rating("AwaitingInspection"), None);
        assert_eq!(rating("Exempt"), None);
        assert_eq!(rating("6"), None);
    }

    #[test]
    fn authorities_are_unique() {
        let mut ids: Vec<u32> = AUTHORITIES.iter().map(|a| a.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), AUTHORITIES.len());
    }

    #[test]
    fn the_sweep_walks_pages_then_pairs_then_ends() {
        let row = CanonicalUrl::parse("https://api.ratings.food.gov.uk/Establishments").unwrap();
        let p1 = first_page(&row).unwrap();
        assert_eq!(position(&p1), Some((120, 1, 1)));
        let p2 = next_page(&row, &p1, Some((1, 3))).unwrap().unwrap();
        assert_eq!(position(&p2), Some((120, 1, 2)));
        let p3 = next_page(&row, &p2, Some((3, 3))).unwrap().unwrap();
        assert_eq!(position(&p3), Some((120, 7843, 1)), "next business type");
        let last_la = AUTHORITIES[AUTHORITIES.len() - 1].id;
        let last = page_url(&row, last_la, 7844, 1).unwrap();
        assert_eq!(next_page(&row, &last, Some((1, 1))).unwrap(), None);
        // An empty pair (0 pages) moves on.
        let e = next_page(&row, &p1, Some((1, 0))).unwrap().unwrap();
        assert_eq!(position(&e), Some((120, 7843, 1)));
    }
}
