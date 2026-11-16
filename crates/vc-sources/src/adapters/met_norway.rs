//! `met_norway`: MET Norway Locationforecast 2.0 (`compact`), CC BY 4.0. Rows are on demand:
//! the leased URL carries `lat` and `lon` (at most 4 decimals, as MET's terms require), and
//! the identifying User-Agent is the crawler's one constant. The response's `Expires` header
//! travels with the raw capture; the scheduler must not refetch before it.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json};

/// One forecast step.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ForecastStep {
    /// Time (UTC, RFC 3339).
    pub time: String,
    /// Air temperature, °C.
    pub air_temperature: Option<f64>,
    /// Precipitation over the next hour, mm.
    pub precipitation_1h: Option<f64>,
    /// Symbol code for the next hour.
    pub symbol_1h: Option<String>,
}

/// A parsed forecast.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Forecast {
    /// `[lon, lat, altitude]`.
    pub coordinates: Vec<f64>,
    /// `meta.updated_at`.
    pub updated_at: String,
    /// Steps.
    pub steps: Vec<ForecastStep>,
}

#[derive(Deserialize)]
struct Doc {
    geometry: Geometry,
    properties: Props,
}
#[derive(Deserialize)]
struct Geometry {
    coordinates: Vec<f64>,
}
#[derive(Deserialize)]
struct Props {
    meta: Meta,
    timeseries: Vec<Ts>,
}
#[derive(Deserialize)]
struct Meta {
    updated_at: String,
}
#[derive(Deserialize)]
struct Ts {
    time: String,
    data: TsData,
}
#[derive(Deserialize)]
struct TsData {
    instant: Instant,
    next_1_hours: Option<Next>,
}
#[derive(Deserialize)]
struct Instant {
    details: InstantDetails,
}
#[derive(Deserialize)]
struct InstantDetails {
    air_temperature: Option<f64>,
}
#[derive(Deserialize)]
struct Next {
    summary: Option<Summary>,
    details: Option<NextDetails>,
}
#[derive(Deserialize)]
struct Summary {
    symbol_code: Option<String>,
}
#[derive(Deserialize)]
struct NextDetails {
    precipitation_amount: Option<f64>,
}

/// Parses a compact forecast.
pub fn parse(body: &[u8]) -> Result<Forecast> {
    let d: Doc = json(body)?;
    if d.properties.timeseries.is_empty() {
        return Err(Error::Invalid("met: empty timeseries".into()));
    }
    Ok(Forecast {
        coordinates: d.geometry.coordinates,
        updated_at: d.properties.meta.updated_at,
        steps: d
            .properties
            .timeseries
            .into_iter()
            .map(|t| {
                let (sym, pr) = match t.data.next_1_hours {
                    Some(n) => (
                        n.summary.and_then(|s| s.symbol_code),
                        n.details.and_then(|d| d.precipitation_amount),
                    ),
                    None => (None, None),
                };
                ForecastStep {
                    time: t.time,
                    air_temperature: t.data.instant.details.air_temperature,
                    precipitation_1h: pr,
                    symbol_1h: sym,
                }
            })
            .collect(),
    })
}

/// Checks that a forecast URL carries `lat`/`lon` with at most 4 decimals.
pub fn check_coordinates(u: &url::Url) -> Result<()> {
    let mut seen = 0;
    for (k, v) in u.query_pairs() {
        if k == "lat" || k == "lon" {
            let decimals = v.split_once('.').map_or(0, |(_, d)| d.len());
            if v.parse::<f64>().is_err() || decimals > 4 {
                return Err(Error::Invalid(format!("met: bad {k}")));
            }
            seen += 1;
        }
    }
    if seen != 2 {
        return Err(Error::Invalid("met: lat and lon are required".into()));
    }
    Ok(())
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetNorway;

#[async_trait]
impl SourceAdapter for MetNorway {
    fn id(&self) -> &'static str {
        "met_norway"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        // No request without valid coordinates (MET answers 400/403 to bad ones).
        check_coordinates(url.as_url())?;
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
        check_status(&ex)?;
        parse(&ex.body)?;
        rep.complete = true;
        Ok(rep)
    }
}
