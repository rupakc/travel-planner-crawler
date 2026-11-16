//! `nager_holidays`: Nager.Date public holidays (`api_terms`: credit Nager.Date with a link).
//!
//! - `/api/v3/AvailableCountries` → spool, then enqueue `/api/v3/PublicHolidays/{year}/{cc}`
//!   for this year and next (local clock);
//! - `/api/v3/PublicHolidays/{year}/{cc}` → spool.

use async_trait::async_trait;
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::{Fetch, check_status, enqueue_adapter_rows, fetch_raw, json, on_origin};

/// A country Nager.Date covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NagerCountry {
    /// ISO 3166-1 alpha-2.
    pub country_code: String,
    /// English name.
    pub name: String,
}

/// One public holiday.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Holiday {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// Local name.
    pub local_name: String,
    /// English name.
    pub name: String,
    /// ISO 3166-1 alpha-2.
    pub country_code: String,
    /// Nationwide.
    #[serde(default)]
    pub global: bool,
    /// Subdivisions (ISO 3166-2) when not global.
    #[serde(default)]
    pub counties: Option<Vec<String>>,
    /// Types (`Public`, `Bank`, …).
    #[serde(default)]
    pub types: Vec<String>,
}

/// Parses `AvailableCountries`.
pub fn parse_countries(body: &[u8]) -> Result<Vec<NagerCountry>> {
    let v: Vec<NagerCountry> = json(body)?;
    let ok = |c: &NagerCountry| {
        c.country_code.len() == 2 && c.country_code.bytes().all(|b| b.is_ascii_uppercase())
    };
    if v.is_empty() || !v.iter().all(ok) {
        return Err(Error::Invalid("nager: bad country list".into()));
    }
    Ok(v)
}

/// Parses `PublicHolidays/{year}/{cc}`.
pub fn parse_holidays(body: &[u8]) -> Result<Vec<Holiday>> {
    json(body)
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct NagerHolidays;

#[async_trait]
impl SourceAdapter for NagerHolidays {
    fn id(&self) -> &'static str {
        "nager_holidays"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let url = ctx.url.url.clone();
        let path = url.as_url().path().to_string();
        let countries_row = path.ends_with("/AvailableCountries");
        if !countries_row && !path.contains("/PublicHolidays/") {
            return Err(Error::Invalid("nager: unknown endpoint".into()));
        }
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url.clone(), &opts, &mut rep).await?;
        check_status(&ex)?;
        if countries_row {
            let countries = parse_countries(&ex.body)?;
            let year = ctx.clock.now().year();
            let base = path.trim_end_matches("/AvailableCountries");
            let mut urls = Vec::new();
            for y in [year, year + 1] {
                for c in &countries {
                    urls.push(on_origin(
                        &url,
                        &format!("{base}/PublicHolidays/{y}/{}", c.country_code),
                    )?);
                }
            }
            enqueue_adapter_rows(ctx, self.id(), urls, 0.5, &mut rep).await?;
        } else {
            parse_holidays(&ex.body)?;
        }
        rep.complete = true;
        Ok(rep)
    }
}
