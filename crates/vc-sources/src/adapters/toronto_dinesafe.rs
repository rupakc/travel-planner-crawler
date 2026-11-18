//! `toronto_dinesafe`: Toronto Public Health's DineSafe register on the City's CKAN portal
//! (`ckan0.cf.opendata.inter.prod-toronto.ca`). The leased row is the dataset's
//! `package_show`; the adapter takes the resource that is loaded in the CKAN datastore and
//! pages it with `datastore_search` (robots.txt allows `/api/` and disallows the resources'
//! `/download/` URLs). The datastore has one row per infraction; the adapter asks for the
//! distinct (establishment, inspection) rows of the fields it uses, sorted by establishment id,
//! in group-aligned pages ([`super::inspections`]). Each establishment becomes one `restaurant`
//! record with its latest inspection status (Pass, Conditional Pass, Closed) and date.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::ResumableAdapter;
use super::common::{Fetch, check_status, fetch_raw, json, on_origin};
use super::inspections::{
    Hygiene, Place, complete_prefix, coord, day, phone, query, restaurant, text, window, with_param,
};

/// Adapter id.
pub const ID: &str = "toronto_dinesafe";

/// The dataset metadata call (the leased row).
pub const PACKAGE_PATH: &str = "/api/3/action/package_show";

/// The datastore page call.
pub const SEARCH_PATH: &str = "/api/3/action/datastore_search";

/// Rows per page.
pub const LIMIT: u32 = 2000;

/// The fields asked for.
pub const FIELDS: &str =
    "estId,estName,address,inspectionStatus,phone,inspectionDate,latitude,longitude";

/// The inspecting authority.
pub const AUTHORITY: &str = "Toronto Public Health";

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    result: Option<T>,
}

#[derive(Deserialize)]
struct Package {
    #[serde(default)]
    resources: Vec<Resource>,
}

#[derive(Deserialize)]
struct Resource {
    id: String,
    #[serde(default)]
    datastore_active: bool,
}

#[derive(Deserialize)]
struct Search {
    #[serde(default)]
    records: Vec<Value>,
}

/// The id of the dataset's datastore resource, from a `package_show` body.
pub fn datastore_resource(body: &[u8]) -> Result<String> {
    let env: Envelope<Package> = json(body)?;
    let p = env
        .result
        .filter(|_| env.success)
        .ok_or_else(|| Error::Invalid(format!("{ID}: package_show failed")))?;
    p.resources
        .into_iter()
        .find(|r| r.datastore_active && !r.id.trim().is_empty())
        .map(|r| r.id)
        .ok_or_else(|| Error::NotFound(format!("{ID}: no datastore resource")))
}

/// The first datastore page for `resource`.
pub fn first_page(row: &CanonicalUrl, resource: &str) -> Result<CanonicalUrl> {
    let mut u = on_origin(row, SEARCH_PATH)?.as_url().clone();
    u.query_pairs_mut()
        .append_pair("resource_id", resource)
        .append_pair("fields", FIELDS)
        .append_pair("distinct", "true")
        .append_pair("sort", "estId asc,inspectionDate desc")
        .append_pair("limit", &LIMIT.to_string())
        .append_pair("offset", "0");
    CanonicalUrl::from_url(u).map_err(|e| Error::Invalid(e.to_string()))
}

/// Parses one `datastore_search` page.
pub fn parse(body: &[u8]) -> Result<Vec<Value>> {
    let env: Envelope<Search> = json(body)?;
    env.result
        .filter(|_| env.success)
        .map(|r| r.records)
        .ok_or_else(|| Error::Invalid(format!("{ID}: datastore_search failed")))
}

fn est(row: &Value) -> Option<String> {
    text(row.get("estId"))
}

/// The rows of complete establishments on a page, and the next page's URL if the list goes on.
pub fn page<'a>(
    url: &CanonicalUrl,
    rows: &'a [Value],
) -> Result<(&'a [Value], Option<CanonicalUrl>)> {
    let (offset, limit) = window(url, "offset", "limit")
        .ok_or_else(|| Error::Invalid(format!("{ID}: page URL without limit")))?;
    let keys: Vec<Option<String>> = rows.iter().map(est).collect();
    let (kept, more) = complete_prefix(&keys, limit);
    let next = if more {
        Some(with_param(
            url,
            "offset",
            &(offset + kept as u64).to_string(),
        )?)
    } else {
        None
    };
    Ok((&rows[..kept], next))
}

/// `restaurant` records (`(id, data)`) of a stored page; a `package_show` body has none.
pub fn items(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<(String, Value)>> {
    if url.as_url().path() == PACKAGE_PATH {
        datastore_resource(body)?;
        return Ok(Vec::new());
    }
    if url.as_url().path() != SEARCH_PATH {
        return Err(Error::Invalid(format!("{ID}: unknown endpoint")));
    }
    let rows = parse(body)?;
    let (rows, _) = page(url, &rows)?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let key = est(&rows[i]);
        let mut j = i + 1;
        while j < rows.len() && est(&rows[j]) == key {
            j += 1;
        }
        let group = &rows[i..j];
        i = j;
        let Some(key) = key else { continue };
        let Some((row, date)) = group
            .iter()
            .filter_map(|r| {
                text(r.get("inspectionStatus"))?;
                let d = r
                    .get("inspectionDate")
                    .and_then(Value::as_str)
                    .and_then(day)?;
                Some((r, d))
            })
            .max_by(|a, b| a.1.cmp(&b.1))
        else {
            continue;
        };
        let Some(name) = text(row.get("estName")) else {
            continue;
        };
        let place = Place {
            name,
            address: text(row.get("address")),
            city: "Toronto",
            country: "Canada",
            country_code: "CA",
            lat: coord(row.get("latitude"), 90.0),
            lng: coord(row.get("longitude"), 180.0),
            phone: phone(row.get("phone")),
            ..Place::default()
        };
        let h = Hygiene {
            scheme: "toronto_dinesafe",
            rating: text(row.get("inspectionStatus")).unwrap_or_default(),
            value: None,
            date,
            authority: AUTHORITY.to_string(),
        };
        out.push((format!("toronto-dinesafe:{key}"), restaurant(&place, &h)));
    }
    Ok(out)
}

/// The page URL a cursor (path and query) points at, if it is a datastore page on this origin.
pub fn resume_url(row: &CanonicalUrl, cursor: &str) -> Option<CanonicalUrl> {
    if !cursor.starts_with('/') {
        return None;
    }
    let u = on_origin(row, cursor).ok()?;
    (u.as_url().path() == SEARCH_PATH
        && query(&u, "resource_id").is_some()
        && window(&u, "offset", "limit").is_some())
    .then_some(u)
}

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct TorontoDinesafe {
    /// Datastore pages per run.
    pub max_pages: u32,
}

impl TorontoDinesafe {
    /// Five pages per run.
    pub const DEFAULT: TorontoDinesafe = TorontoDinesafe { max_pages: 5 };
}

impl Default for TorontoDinesafe {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[async_trait]
impl SourceAdapter for TorontoDinesafe {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        self.run_from(ctx, None).await
    }
}

#[async_trait]
impl ResumableAdapter for TorontoDinesafe {
    async fn run_from(
        &self,
        ctx: &mut AdapterCtx<'_>,
        cursor: Option<&str>,
    ) -> Result<AdapterReport> {
        let row = ctx.url.url.clone();
        if row.as_url().path() != PACKAGE_PATH {
            return Err(Error::Invalid(format!(
                "{ID}: the leased row is not package_show"
            )));
        }
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let start = match cursor.and_then(|c| resume_url(&row, c)) {
            Some(u) => u,
            None => {
                let ex = fetch_raw(ctx, row.clone(), &opts, &mut rep).await?;
                check_status(&ex)?;
                first_page(&row, &datastore_resource(&ex.body)?)?
            }
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
            let rows = parse(&ex.body)?;
            next = page(&u, &rows)?.1;
        }
        rep.complete = true;
        Ok(rep)
    }
}
