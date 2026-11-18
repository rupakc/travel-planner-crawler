//! `socrata`: one reader for Socrata SODA datasets (`/resource/<4x4>.json`), driven by a
//! [`SocrataMapping`] per dataset: the columns asked for (`$select`, so nothing else is ever
//! stored), the row filter (`$where`, with an optional date window), a stable `$order`, and the
//! [`RowMapping`] to a record. Generalises `nyc_restaurant_inspections` (which stays as is).
//!
//! **Paging.** `$limit`/`$offset`, up to `max_pages` per run; a run that stops early leaves a
//! cursor (the next page's path and query). A short page ends the list. With a `group` column
//! (registers with one row per inspection), rows are ordered by that column then newest first,
//! and pages are group-aligned ([`super::inspections::complete_prefix`]): a full page's last
//! group waits for the next page, so each establishment is mapped from exactly one page, from
//! its newest row.
//!
//! **Adding a dataset:** add a mapping (path, columns, filter, order, group, row mapping),
//! record a fixture, point the source's `adapter` at `socrata`.

use async_trait::async_trait;
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::ResumableAdapter;
use super::common::{Fetch, check_status, fetch_raw, json, on_origin};
use super::inspections::{complete_prefix, window, with_param};
use super::row_map::{Fields, Kind, Row, RowMapping, map_row, row_date};

/// Adapter id.
pub const ID: &str = "socrata";

/// One dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocrataMapping {
    /// The resource path (exact, as in the seed URL).
    pub path: &'static str,
    /// `$select` (the only columns ever fetched).
    pub select: &'static str,
    /// `$where`; `{since}` is replaced by `today - window_days` (`YYYY-MM-DD`).
    pub filter: &'static str,
    /// The date window for `{since}`.
    pub window_days: i64,
    /// `$order` (stable; with `group`, the group column first, then newest first).
    pub order: &'static str,
    /// Column whose rows form one establishment (newest row kept), if any.
    pub group: Option<&'static str>,
    /// Rows per page.
    pub limit: u32,
    /// Row → record.
    pub map: RowMapping,
}

const NONE_FIELDS: Fields = Fields::NONE;

/// Every built-in mapping.
pub const MAPPINGS: &[SocrataMapping] = &[
    // City of Chicago Food Inspections (`4ijn-s7e5`, CDPH): one row per inspection. Restaurants,
    // bakeries, taverns and Golden Diners with a Pass / Pass w/ Conditions / Fail result in the
    // last three years; the latest result per licence. Licence 0 means "no licence on file"
    // (many unrelated places), so it is left out.
    SocrataMapping {
        path: "/resource/4ijn-s7e5.json",
        select: "license_,dba_name,facility_type,address,city,zip,inspection_date,results,latitude,longitude",
        filter: "inspection_date >= '{since}' AND license_ > 0 AND results in('Pass','Pass w/ Conditions','Fail') AND facility_type in('Restaurant','Bakery','TAVERN','Golden Diner')",
        window_days: 3 * 365,
        order: "license_,inspection_date DESC",
        group: Some("license_"),
        limit: 2000,
        map: RowMapping {
            source: "chicago-cdph",
            kind: Kind::Restaurant {
                scheme: "other",
                authority: "Chicago Department of Public Health",
                ratings: &["Pass", "Pass w/ Conditions", "Fail"],
            },
            fields: Fields {
                id: &["license_"],
                name: "dba_name",
                address: &["address"],
                postcode: "zip",
                lat: "latitude",
                lng: "longitude",
                category: &["facility_type"],
                rating: "results",
                date: "inspection_date",
                ..NONE_FIELDS
            },
            city: "Chicago",
            country: "United States",
            country_code: "US",
            categories: &[
                ("bakery", "cafe"),
                ("tavern", "pub_bar"),
                ("restaurant", "restaurant"),
                ("golden diner", "restaurant"),
            ],
            accessible: &[],
        },
    },
    // Austin Public Health food establishment inspection scores (`ecmv-9xxi`, public domain):
    // one row per inspection, a 0–100 score, three-year window; the latest score per facility.
    // The register covers Austin and Travis County; `city` is Austin (the address names the
    // town as written).
    SocrataMapping {
        path: "/resource/ecmv-9xxi.json",
        select: "facility_id,restaurant_name,address,zip_code,inspection_date,score,lat,lng",
        filter: "inspection_date >= '{since}'",
        window_days: 3 * 365,
        order: "facility_id,inspection_date DESC",
        group: Some("facility_id"),
        limit: 2000,
        map: RowMapping {
            source: "austin-aph",
            kind: Kind::Restaurant {
                scheme: "other",
                authority: "Austin Public Health",
                ratings: &[],
            },
            fields: Fields {
                id: &["facility_id"],
                name: "restaurant_name",
                address: &["address"],
                postcode: "zip_code",
                lat: "lat",
                lng: "lng",
                rating: "score",
                date: "inspection_date",
                ..NONE_FIELDS
            },
            city: "Austin",
            country: "United States",
            country_code: "US",
            categories: &[("", "restaurant")],
            accessible: &[],
        },
    },
    // Registre de Turisme de Catalunya (`t2h3-cgys`): hotels and hostals, rural houses, tourist
    // apartment blocks and campsites. Holiday flats (HUT, about 105k, many run by private
    // individuals) and shared homes are left out, and the owner columns (names, tax ids) are
    // never selected. The official star category is kept when it is in stars.
    SocrataMapping {
        path: "/resource/t2h3-cgys.json",
        select: "n_mero_inscripci,r_tol,tipus_de_via,nom_de_la_via,numero,codi_postal,municipi,comarca,tipus_establiment,grup,categoria",
        filter: "tipus_establiment in('Hotels','Turisme rural','Apartaments Turístics','Càmpings')",
        window_days: 0,
        order: "n_mero_inscripci",
        group: None,
        limit: 2000,
        map: RowMapping {
            source: "catalonia-rtc",
            kind: Kind::Hotel,
            fields: Fields {
                id: &["n_mero_inscripci"],
                name: "r_tol",
                address: &["tipus_de_via", "nom_de_la_via", "numero"],
                postcode: "codi_postal",
                city: "municipi",
                district: "comarca",
                category: &["tipus_establiment", "grup"],
                stars: "categoria",
                ..NONE_FIELDS
            },
            city: "",
            country: "Spain",
            country_code: "ES",
            categories: &[
                ("hostal", "guesthouse"),
                ("hotel-apartament", "apartment"),
                ("hotel", "hotel"),
                ("turisme rural", "guesthouse"),
                ("apartament", "apartment"),
                ("càmping", "campsite"),
            ],
            accessible: &[],
        },
    },
    // NYC Public Restrooms (`i7jb-7jku`): operational restrooms in parks, libraries, transit
    // and plazas, with hours and accessibility.
    SocrataMapping {
        path: "/resource/i7jb-7jku.json",
        select: "facility_name,location_type,operator,hours_of_operation,accessibility,website,latitude,longitude",
        filter: "status = 'Operational'",
        window_days: 0,
        order: ":id",
        group: None,
        limit: 2000,
        map: RowMapping {
            source: "nyc-restrooms",
            kind: Kind::Amenity,
            fields: Fields {
                id: &["facility_name", "latitude", "longitude"],
                name: "facility_name",
                lat: "latitude",
                lng: "longitude",
                url: "website",
                hours: "hours_of_operation",
                operator: "operator",
                accessible: "accessibility",
                category: &["location_type"],
                ..NONE_FIELDS
            },
            city: "New York City",
            country: "United States",
            country_code: "US",
            categories: &[("", "toilet")],
            accessible: &["fully accessible"],
        },
    },
];

/// The mapping of a page or seed URL (by path).
pub fn mapping_for(url: &CanonicalUrl) -> Option<&'static SocrataMapping> {
    let p = url.as_url().path();
    MAPPINGS.iter().find(|m| m.path == p)
}

/// The first page for the seed.
pub fn first_page(
    m: &SocrataMapping,
    seed: &CanonicalUrl,
    today: chrono::NaiveDate,
) -> Result<CanonicalUrl> {
    let since = today - chrono::Duration::days(m.window_days);
    let mut u = seed.as_url().clone();
    u.set_query(None);
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("$select", m.select);
        if !m.filter.is_empty() {
            q.append_pair(
                "$where",
                &m.filter
                    .replace("{since}", &since.format("%Y-%m-%d").to_string()),
            );
        }
        q.append_pair("$order", m.order)
            .append_pair("$limit", &m.limit.to_string())
            .append_pair("$offset", "0");
    }
    CanonicalUrl::from_url(u).map_err(|e| Error::Invalid(e.to_string()))
}

/// Parses one page (a JSON array of rows).
pub fn parse(body: &[u8]) -> Result<Vec<Value>> {
    json(body)
}

fn key(m: &SocrataMapping, row: &Value) -> Option<String> {
    m.group.and_then(|g| Row::get(row, g))
}

/// The rows a page owns (complete groups only, for grouped datasets) and the next page's URL if
/// the list goes on.
pub fn page<'a>(
    m: &SocrataMapping,
    url: &CanonicalUrl,
    rows: &'a [Value],
) -> Result<(&'a [Value], Option<CanonicalUrl>)> {
    let (offset, limit) = window(url, "$offset", "$limit")
        .ok_or_else(|| Error::Invalid(format!("{ID}: page URL without $limit")))?;
    let (kept, more) = if m.group.is_some() {
        let keys: Vec<Option<String>> = rows.iter().map(|r| key(m, r)).collect();
        complete_prefix(&keys, limit)
    } else {
        (rows.len(), rows.len() >= limit && !rows.is_empty())
    };
    let next = if more {
        Some(with_param(
            url,
            "$offset",
            &(offset + kept as u64).to_string(),
        )?)
    } else {
        None
    };
    Ok((&rows[..kept], next))
}

/// The records (`(id, kind, data)`) of a stored page.
pub fn items(
    url: &CanonicalUrl,
    body: &[u8],
) -> Result<Vec<(Option<String>, &'static str, Value)>> {
    let m = mapping_for(url)
        .ok_or_else(|| Error::Invalid(format!("{ID}: no mapping for this dataset")))?;
    let rows = parse(body)?;
    let (rows, _) = page(m, url, &rows)?;
    let mut out = Vec::new();
    if m.group.is_none() {
        out.extend(rows.iter().filter_map(|r| map_row(&m.map, r)));
        return Ok(out);
    }
    let mut i = 0;
    while i < rows.len() {
        let k = key(m, &rows[i]);
        let mut j = i + 1;
        while j < rows.len() && key(m, &rows[j]) == k {
            j += 1;
        }
        let group = &rows[i..j];
        i = j;
        if k.is_none() {
            continue;
        }
        // The newest row that maps (a result outside the kept list does not hide an older one).
        let mut dated: Vec<(&Value, String)> = group
            .iter()
            .filter_map(|r| row_date(&m.map, r).map(|d| (r, d)))
            .collect();
        dated.sort_by(|a, b| b.1.cmp(&a.1));
        if let Some(rec) = dated.iter().find_map(|(r, _)| map_row(&m.map, *r)) {
            out.push(rec);
        }
    }
    Ok(out)
}

/// The page URL a cursor (path and query) points at, if it stays on the row's dataset.
pub fn resume_url(row: &CanonicalUrl, cursor: &str) -> Option<CanonicalUrl> {
    if !cursor.starts_with('/') {
        return None;
    }
    let u = on_origin(row, cursor).ok()?;
    (u.as_url().path() == row.as_url().path() && window(&u, "$offset", "$limit").is_some())
        .then_some(u)
}

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct Socrata {
    /// Pages per run.
    pub max_pages: u32,
}

impl Socrata {
    /// Five pages per run.
    pub const DEFAULT: Socrata = Socrata { max_pages: 5 };
}

impl Default for Socrata {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[async_trait]
impl SourceAdapter for Socrata {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        self.run_from(ctx, None).await
    }
}

#[async_trait]
impl ResumableAdapter for Socrata {
    async fn run_from(
        &self,
        ctx: &mut AdapterCtx<'_>,
        cursor: Option<&str>,
    ) -> Result<AdapterReport> {
        let row = ctx.url.url.clone();
        let m = mapping_for(&row)
            .ok_or_else(|| Error::Invalid(format!("{ID}: no mapping for this dataset")))?;
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let today = ctx.clock.now().date_naive();
        let mut next = Some(match cursor.and_then(|c| resume_url(&row, c)) {
            Some(u) => u,
            None => first_page(m, &row, today)?,
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
            let rows = parse(&ex.body)?;
            next = page(m, &u, &rows)?.1;
        }
        rep.complete = true;
        Ok(rep)
    }
}
