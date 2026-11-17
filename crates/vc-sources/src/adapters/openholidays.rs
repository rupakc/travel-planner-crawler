//! `openholidays`: the OpenHolidays API (openholidaysapi.org; data ODbL 1.0, so the source is
//! `share_alike` and never merged with other licences). No key.
//!
//! - `/Countries` → spool, then enqueue `/PublicHolidays` and `/SchoolHolidays` for every
//!   country, for this year and next (local clock; `validFrom`/`validTo` are required, English
//!   names requested);
//! - `/PublicHolidays?…`, `/SchoolHolidays?…` → spool (records: [`super::records`]).

use async_trait::async_trait;
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, enqueue_adapter_rows, fetch_raw, json, on_origin};

/// The holiday endpoints, fanned out per country.
pub const HOLIDAY_PATHS: [&str; 2] = ["/PublicHolidays", "/SchoolHolidays"];

/// A text in one language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalizedText {
    /// ISO 639-1 code (upper case in the API).
    pub language: String,
    /// The text.
    pub text: String,
}

/// A country the API covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OhCountry {
    /// ISO 3166-1 alpha-2.
    pub iso_code: String,
    /// Names.
    #[serde(default)]
    pub name: Vec<LocalizedText>,
}

/// A subdivision or group reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OhRef {
    /// Code (e.g. `DE-BY`).
    pub code: String,
    /// Short name.
    #[serde(default)]
    pub short_name: String,
}

/// One holiday (`HolidayResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OhHoliday {
    /// UUID.
    pub id: String,
    /// `YYYY-MM-DD`.
    pub start_date: String,
    /// `YYYY-MM-DD`.
    pub end_date: String,
    /// `Public`, `Bank`, `Optional`, `School`, `BackToSchool`, `EndOfLessons`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Names.
    #[serde(default)]
    pub name: Vec<LocalizedText>,
    /// Nationwide.
    #[serde(default)]
    pub nationwide: bool,
    /// `National`, `Regional`, `Local`.
    #[serde(default)]
    pub regional_scope: Option<String>,
    /// `FullDay`, `HalfDay`.
    #[serde(default)]
    pub temporal_scope: Option<String>,
    /// Subdivisions when not nationwide.
    #[serde(default)]
    pub subdivisions: Option<Vec<OhRef>>,
    /// Groups (school holiday groups).
    #[serde(default)]
    pub groups: Option<Vec<OhRef>>,
    /// Comments.
    #[serde(default)]
    pub comment: Option<Vec<LocalizedText>>,
}

/// Parses `/Countries`.
pub fn parse_countries(body: &[u8]) -> Result<Vec<OhCountry>> {
    let v: Vec<OhCountry> = json(body)?;
    let ok =
        |c: &OhCountry| c.iso_code.len() == 2 && c.iso_code.bytes().all(|b| b.is_ascii_uppercase());
    if v.is_empty() || !v.iter().all(ok) {
        return Err(Error::Invalid("openholidays: bad country list".into()));
    }
    Ok(v)
}

/// Parses `/PublicHolidays` or `/SchoolHolidays`.
pub fn parse_holidays(body: &[u8]) -> Result<Vec<OhHoliday>> {
    json(body)
}

/// The holiday query URLs of one country for `year` and the year after.
pub fn holiday_urls(base: &CanonicalUrl, country: &str, year: i32) -> Result<Vec<CanonicalUrl>> {
    HOLIDAY_PATHS
        .iter()
        .map(|p| {
            on_origin(
                base,
                &format!(
                    "{p}?countryIsoCode={country}&languageIsoCode=EN&validFrom={year}-01-01&validTo={}-12-31",
                    year + 1
                ),
            )
        })
        .collect()
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenHolidays;

#[async_trait]
impl SourceAdapter for OpenHolidays {
    fn id(&self) -> &'static str {
        "openholidays"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let url = ctx.url.url.clone();
        let path = url.as_url().path().trim_end_matches('/').to_string();
        let countries_row = path == "/Countries";
        if !countries_row && !HOLIDAY_PATHS.contains(&path.as_str()) {
            return Err(Error::Invalid("openholidays: unknown endpoint".into()));
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
            let mut urls = Vec::new();
            for c in &countries {
                urls.extend(holiday_urls(&url, &c.iso_code, year)?);
            }
            enqueue_adapter_rows(ctx, self.id(), urls, 0.5, &mut rep).await?;
        } else {
            parse_holidays(&ex.body)?;
        }
        rep.complete = true;
        Ok(rep)
    }
}
