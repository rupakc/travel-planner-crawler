//! `state_dept_advisories`: the US State Department travel advisories API
//! (`cadataapi.state.gov/api/TravelAdvisories`), one JSON array of every advisory.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json};

/// One advisory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateAdvisory {
    /// ISO 3166-1 alpha-2 codes the advisory covers (`Category`).
    pub country_codes: Vec<String>,
    /// Title (`Iran - Level 4: Do Not Travel`).
    pub title: String,
    /// Level 1–4 parsed from the title.
    pub level: Option<u8>,
    /// The advisory page.
    pub link: String,
    /// `Updated` (falls back to `Published`).
    pub updated: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Raw {
    title: String,
    link: String,
    #[serde(default)]
    category: Vec<String>,
    published: Option<String>,
    updated: Option<String>,
}

/// `Level N` in a title.
pub fn level_of(title: &str) -> Option<u8> {
    let i = title.find("Level ")?;
    let d = title[i + 6..].chars().next()?.to_digit(10)?;
    u8::try_from(d).ok().filter(|l| (1..=4).contains(l))
}

/// Parses the array.
pub fn parse(body: &[u8]) -> Result<Vec<StateAdvisory>> {
    let raw: Vec<Raw> = json(body)?;
    if raw.is_empty() {
        return Err(Error::Invalid("state dept: no advisories".into()));
    }
    Ok(raw
        .into_iter()
        .map(|r| StateAdvisory {
            level: level_of(&r.title),
            country_codes: r.category,
            title: r.title,
            link: r.link,
            updated: r.updated.or(r.published),
        })
        .collect())
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct StateDeptAdvisories;

#[async_trait]
impl SourceAdapter for StateDeptAdvisories {
    fn id(&self) -> &'static str {
        "state_dept_advisories"
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
