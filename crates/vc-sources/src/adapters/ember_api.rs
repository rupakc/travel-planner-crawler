//! `ember_api`: carbon intensity of electricity generation per country from the Ember API
//! (2026-11-19; data CC BY 4.0, attribution "Ember"; the owner's free key).
//!
//! Two leased rows cover every country, one request each:
//! - `GET /v1/carbon-intensity/yearly?is_aggregate_entity=false&start_date=2000`
//! - `GET /v1/carbon-intensity/monthly?is_aggregate_entity=false&start_date=2019-01`
//!
//! Leaving `entity_code` out returns all countries (about 5,300 yearly and 7,900 monthly rows,
//! under 1.1 MB each), so a weekly refresh costs two calls. Ember states no rate limit; the
//! source's `daily_budget` still caps calls per day.
//!
//! Each response is spooled raw (raw first) before a parse check; the processor calls
//! [`items`] on the stored body, and every non-aggregate country row becomes one
//! `grid_carbon_intensity` observation (series `grid_ci:<ISO2>:<annual|monthly>:lifecycle`;
//! Ember's intensity is life-cycle emissions of generation). Values are copied as published.
//!
//! **The key** is read from the source's `key_secret` and sent as the `api_key` query
//! parameter. It is **stripped** from every stored URL, redirect and error ([`Fetch::redact`]),
//! and a leased row that carries it is refused, so it never reaches a WARC record, a log line
//! or the frontier.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json};
use super::{countries, iso3};

/// Adapter id.
pub const ID: &str = "ember_api";

/// The query parameter carrying the key.
pub const KEY_PARAM: &str = "api_key";

/// Yearly endpoint path.
pub const YEARLY: &str = "/v1/carbon-intensity/yearly";

/// Monthly endpoint path.
pub const MONTHLY: &str = "/v1/carbon-intensity/monthly";

/// Publisher written on every observation.
pub const PUBLISHER: &str = "Ember";

/// Query parameters a leased row may carry (never the key).
const ALLOWED: &[&str] = &[
    "is_aggregate_entity",
    "start_date",
    "end_date",
    "entity_code",
];

/// Body cap per response (the full responses are about 1 MB).
const MAX_BODY: u64 = 16 * 1024 * 1024;

/// The period a leased row covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// `/yearly`.
    Annual,
    /// `/monthly`.
    Monthly,
}

impl Period {
    /// The `period` value of `grid_carbon_intensity`.
    pub fn as_str(self) -> &'static str {
        match self {
            Period::Annual => "annual",
            Period::Monthly => "monthly",
        }
    }
}

/// Classifies a leased (or stored) URL: one of the two endpoints, only [`ALLOWED`]
/// parameters, never the key.
pub fn period(url: &CanonicalUrl) -> Result<Period> {
    let u = url.as_url();
    let p = match u.path() {
        YEARLY => Period::Annual,
        MONTHLY => Period::Monthly,
        _ => return Err(Error::Invalid("ember: unknown endpoint".into())),
    };
    for (k, _) in u.query_pairs() {
        if !ALLOWED.contains(&&*k) {
            return Err(Error::Invalid("ember: unexpected parameter".into()));
        }
    }
    Ok(p)
}

/// The request URL: the leased row plus the key.
pub fn with_key(url: &CanonicalUrl, key: &str) -> Result<CanonicalUrl> {
    let mut u = url.as_url().clone();
    u.query_pairs_mut().append_pair(KEY_PARAM, key);
    CanonicalUrl::from_url(u).map_err(|e| Error::Invalid(e.to_string()))
}

/// One row of a response.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EmberRow {
    /// Country or region name.
    pub entity: String,
    /// ISO 3166-1 alpha-3 code (absent for regions).
    #[serde(default)]
    pub entity_code: Option<String>,
    /// Region aggregate.
    #[serde(default)]
    pub is_aggregate_entity: bool,
    /// `YYYY` (yearly) or `YYYY-MM-DD` (monthly).
    pub date: String,
    /// gCO2e per kWh generated.
    #[serde(default)]
    pub emissions_intensity_gco2_per_kwh: Option<f64>,
}

#[derive(Deserialize)]
struct Doc {
    data: Vec<EmberRow>,
}

/// Parses a response body.
pub fn parse(body: &[u8]) -> Result<Vec<EmberRow>> {
    Ok(json::<Doc>(body)?.data)
}

/// `(year, first day of the period)` of an Ember date.
fn period_start(p: Period, date: &str) -> Option<(i32, String)> {
    let d = date.trim();
    let year: i32 = d.get(..4)?.parse().ok()?;
    if !(1950..=2100).contains(&year) {
        return None;
    }
    match p {
        Period::Annual if d.len() == 4 => Some((year, format!("{year:04}-01-01"))),
        Period::Monthly => {
            let month: u32 = d.get(5..7)?.parse().ok()?;
            (d.as_bytes().get(4) == Some(&b'-') && (1..=12).contains(&month))
                .then(|| (year, format!("{year:04}-{month:02}-01")))
        }
        Period::Annual => None,
    }
}

/// The `grid_carbon_intensity` observations of a stored response: `(id, kind, data)`.
/// Aggregates, rows without a known ISO code, missing or negative values and malformed dates
/// are skipped.
pub fn items(
    url: &CanonicalUrl,
    body: &[u8],
) -> Result<Vec<(Option<String>, &'static str, Value)>> {
    let p = period(url)?;
    let mut out = Vec::new();
    for r in parse(body)? {
        if r.is_aggregate_entity {
            continue;
        }
        let Some(cc) = r.entity_code.as_deref().and_then(iso3::alpha2) else {
            continue;
        };
        let Some(v) = r
            .emissions_intensity_gco2_per_kwh
            .filter(|v| v.is_finite() && *v >= 0.0)
        else {
            continue;
        };
        let Some((year, date)) = period_start(p, &r.date) else {
            continue;
        };
        let country: String = countries::name(cc)
            .map(str::to_string)
            .unwrap_or_else(|| r.entity.trim().chars().take(80).collect());
        let dataset = match p {
            Period::Annual => "Ember electricity data: carbon intensity of generation (yearly)",
            Period::Monthly => "Ember electricity data: carbon intensity of generation (monthly)",
        };
        out.push((
            Some(format!("ember:{}:{cc}:{date}", p.as_str())),
            "grid_carbon_intensity",
            json!({
                "country": country,
                "country_code": cc,
                "period": p.as_str(),
                "year": year,
                "date": date,
                "intensity_g_per_kwh": v,
                "basis": "lifecycle",
                "publisher": PUBLISHER,
                "dataset": dataset,
            }),
        ));
    }
    Ok(out)
}

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct EmberApi;

#[async_trait]
impl SourceAdapter for EmberApi {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let row = ctx.url.url.clone();
        period(&row)?;
        let secret = ctx
            .source
            .key_secret
            .clone()
            .ok_or_else(|| Error::Invalid("ember: source has no key_secret".into()))?;
        let secrets = ctx.secrets;
        let key = secrets.get(&secret).await?;
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            redact: &[KEY_PARAM],
            max_body_bytes: MAX_BODY,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, with_key(&row, key.expose())?, &opts, &mut rep).await?;
        check_status(&ex)?;
        let n = parse(&ex.body)?.len();
        tracing::info!(adapter = ID, rows = n, "parsed");
        rep.complete = true;
        Ok(rep)
    }
}
