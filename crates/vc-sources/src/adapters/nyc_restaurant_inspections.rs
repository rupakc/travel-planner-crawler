//! `nyc_restaurant_inspections`: New York City restaurant inspection results (NYC Open Data,
//! Socrata dataset `43nn-pn8j`, DOHMH). The dataset has one row per violation; the adapter asks
//! only for rows with a letter grade (A, B, C) from the last [`WINDOW_DAYS`], de-duplicated on
//! the server (`$group` over the selected columns), sorted by restaurant (`camis`) and newest
//! grade first, in group-aligned pages ([`super::inspections`]). Each restaurant becomes one
//! `restaurant` record with its latest grade and grade date.
//!
//! Not `json_events`: that adapter maps one item to one event; here a restaurant's rows must be
//! grouped by `camis` (and never split across pages) before anything is mapped.

use async_trait::async_trait;
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::ResumableAdapter;
use super::common::{Fetch, check_status, fetch_raw, json, on_origin};
use super::inspections::{
    Hygiene, Place, complete_prefix, coord, day, phone, restaurant, text, window, with_param,
};

/// Adapter id.
pub const ID: &str = "nyc_restaurant_inspections";

/// The dataset's resource path.
pub const PATH: &str = "/resource/43nn-pn8j.json";

/// Rows per page.
pub const LIMIT: u32 = 2000;

/// Only grades from this many days back.
pub const WINDOW_DAYS: i64 = 3 * 365;

/// The columns asked for (and grouped on).
pub const COLUMNS: &str = "camis,dba,boro,building,street,zipcode,phone,cuisine_description,latitude,longitude,grade,grade_date";

/// The inspecting authority.
pub const AUTHORITY: &str = "NYC Department of Health and Mental Hygiene";

/// The first page for the seed, with grades since `today - WINDOW_DAYS`.
pub fn first_page(seed: &CanonicalUrl, today: chrono::NaiveDate) -> Result<CanonicalUrl> {
    let since = today - chrono::Duration::days(WINDOW_DAYS);
    let mut u = seed.as_url().clone();
    u.set_query(None);
    u.query_pairs_mut()
        .append_pair("$select", COLUMNS)
        .append_pair(
            "$where",
            &format!(
                "grade in('A','B','C') AND grade_date >= '{}'",
                since.format("%Y-%m-%d")
            ),
        )
        .append_pair("$group", COLUMNS)
        .append_pair("$order", "camis,grade_date DESC")
        .append_pair("$limit", &LIMIT.to_string())
        .append_pair("$offset", "0");
    CanonicalUrl::from_url(u).map_err(|e| Error::Invalid(e.to_string()))
}

/// Parses one page (a JSON array of rows).
pub fn parse(body: &[u8]) -> Result<Vec<Value>> {
    json(body)
}

fn camis(row: &Value) -> Option<String> {
    text(row.get("camis"))
}

/// The rows of complete restaurants on a page, and the next page's URL if the list goes on.
pub fn page<'a>(
    url: &CanonicalUrl,
    rows: &'a [Value],
) -> Result<(&'a [Value], Option<CanonicalUrl>)> {
    let (offset, limit) = window(url, "$offset", "$limit")
        .ok_or_else(|| Error::Invalid(format!("{ID}: page URL without $limit")))?;
    let keys: Vec<Option<String>> = rows.iter().map(camis).collect();
    let (kept, more) = complete_prefix(&keys, limit);
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

/// `restaurant` records (`(id, data)`) of a stored page: one per restaurant, its newest grade.
pub fn items(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<(String, Value)>> {
    let rows = parse(body)?;
    let (rows, _) = page(url, &rows)?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let key = camis(&rows[i]);
        let mut j = i + 1;
        while j < rows.len() && camis(&rows[j]) == key {
            j += 1;
        }
        let group = &rows[i..j];
        i = j;
        let Some(key) = key else { continue };
        let Some((row, date)) = group
            .iter()
            .filter_map(|r| {
                let g = text(r.get("grade"))?;
                (["A", "B", "C"].contains(&g.as_str()))
                    .then(|| r.get("grade_date").and_then(Value::as_str).and_then(day))
                    .flatten()
                    .map(|d| (r, d))
            })
            .max_by(|a, b| a.1.cmp(&b.1))
        else {
            continue;
        };
        let Some(name) = text(row.get("dba")) else {
            continue;
        };
        let boro = text(row.get("boro")).filter(|b| b != "0");
        let street = [text(row.get("building")), text(row.get("street"))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let tail = [
            boro.clone(),
            Some("NY".to_string()),
            text(row.get("zipcode")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        let address = (!street.is_empty()).then(|| format!("{street}, {tail}"));
        let place = Place {
            name,
            establishment_type: Some("restaurant"),
            cuisine: text(row.get("cuisine_description")),
            address,
            district: boro,
            city: "New York City",
            country: "United States",
            country_code: "US",
            lat: coord(row.get("latitude"), 90.0),
            lng: coord(row.get("longitude"), 180.0),
            phone: phone(row.get("phone")),
            ..Place::default()
        };
        let h = Hygiene {
            scheme: "nyc_letter_grade",
            rating: text(row.get("grade")).unwrap_or_default(),
            value: None,
            date,
            authority: AUTHORITY.to_string(),
        };
        out.push((format!("nyc-dohmh:{key}"), restaurant(&place, &h)));
    }
    Ok(out)
}

/// The page URL a cursor (path and query) points at, if it stays on the dataset's path.
pub fn resume_url(row: &CanonicalUrl, cursor: &str) -> Option<CanonicalUrl> {
    if !cursor.starts_with('/') {
        return None;
    }
    let u = on_origin(row, cursor).ok()?;
    (u.as_url().path() == PATH && window(&u, "$offset", "$limit").is_some()).then_some(u)
}

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct NycRestaurantInspections {
    /// Pages per run.
    pub max_pages: u32,
}

impl NycRestaurantInspections {
    /// Five pages per run.
    pub const DEFAULT: NycRestaurantInspections = NycRestaurantInspections { max_pages: 5 };
}

impl Default for NycRestaurantInspections {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[async_trait]
impl SourceAdapter for NycRestaurantInspections {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        self.run_from(ctx, None).await
    }
}

#[async_trait]
impl ResumableAdapter for NycRestaurantInspections {
    async fn run_from(
        &self,
        ctx: &mut AdapterCtx<'_>,
        cursor: Option<&str>,
    ) -> Result<AdapterReport> {
        let row = ctx.url.url.clone();
        if row.as_url().path() != PATH {
            return Err(Error::Invalid(format!("{ID}: not the {PATH} dataset")));
        }
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let today = ctx.clock.now().date_naive();
        let mut next = Some(match cursor.and_then(|c| resume_url(&row, c)) {
            Some(u) => u,
            None => first_page(&row, today)?,
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
            next = page(&u, &rows)?.1;
        }
        rep.complete = true;
        Ok(rep)
    }
}
