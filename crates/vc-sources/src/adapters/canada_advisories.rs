//! `canada_advisories`: Global Affairs Canada travel advice and advisories
//! (`data.international.gc.ca/travel-voyage/index-alpha-eng.json`), one JSON object keyed by
//! ISO code with the advisory level and its text per country. The per-country detail files
//! named in the source notes are not used: their URL pattern is not documented in the index.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json};

/// One country's advisory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanadaAdvisory {
    /// ISO 3166-1 alpha-2.
    pub iso: String,
    /// English name.
    pub name: String,
    /// `advisory-state` (0 normal precautions … 3 avoid all travel).
    pub advisory_state: u8,
    /// English advisory text.
    pub advisory_text: Option<String>,
    /// `date-published.date`.
    pub published: Option<String>,
    /// English URL slug on travel.gc.ca.
    pub slug: Option<String>,
}

#[derive(Deserialize)]
struct Index {
    data: BTreeMap<String, Entry>,
}
#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "country-iso")]
    iso: String,
    #[serde(rename = "country-eng")]
    name: String,
    #[serde(rename = "advisory-state")]
    advisory_state: u8,
    #[serde(rename = "date-published")]
    published: Option<Published>,
    eng: Option<Lang>,
}
#[derive(Deserialize)]
struct Published {
    date: Option<String>,
}
#[derive(Deserialize)]
struct Lang {
    #[serde(rename = "url-slug")]
    slug: Option<String>,
    #[serde(rename = "advisory-text")]
    advisory_text: Option<String>,
}

/// Parses the index.
pub fn parse(body: &[u8]) -> Result<Vec<CanadaAdvisory>> {
    let idx: Index = json(body)?;
    if idx.data.is_empty() {
        return Err(Error::Invalid("canada: empty index".into()));
    }
    Ok(idx
        .data
        .into_values()
        .map(|e| {
            let (slug, text) = e
                .eng
                .map(|l| (l.slug, l.advisory_text))
                .unwrap_or((None, None));
            CanadaAdvisory {
                iso: e.iso,
                name: e.name,
                advisory_state: e.advisory_state,
                advisory_text: text,
                published: e.published.and_then(|p| p.date),
                slug,
            }
        })
        .collect())
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanadaAdvisories;

#[async_trait]
impl SourceAdapter for CanadaAdvisories {
    fn id(&self) -> &'static str {
        "canada_advisories"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let url = ctx.url.url.clone();
        let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
        check_status(&ex)?;
        parse(&ex.body)?;
        rep.complete = true;
        Ok(rep)
    }
}
