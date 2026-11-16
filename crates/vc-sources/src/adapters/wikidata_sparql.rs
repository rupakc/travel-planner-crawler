//! `wikidata_sparql`: Wikidata through the SPARQL endpoint, paginated (`LIMIT`/`OFFSET`),
//! never the full dump (spec §5.10). Built-in queries: emergency phone numbers (P2852), plug
//! types (P2853) and mains voltage (P2884) per country, and airports with an IATA code (P238).
//!
//! The leased URL is the endpoint (`…/sparql`); a leased URL that already carries `query=`
//! runs just that query. Every page is spooled before it is parsed.
//!
//! **Note:** `query.wikidata.org/robots.txt` disallows `/sparql` for `*` (checked
//! 2026-11-15), so the permit client refuses these requests (`RobotsDisallowed`) until an
//! allowed route exists; the adapter surfaces that error and never works around it.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json};

/// A named built-in query (without `LIMIT`/`OFFSET`; it must have a stable `ORDER BY`).
#[derive(Debug, Clone, Copy)]
pub struct NamedQuery {
    /// Name (used in the resume cursor).
    pub name: &'static str,
    /// SPARQL text.
    pub sparql: &'static str,
}

/// The built-in queries.
pub const QUERIES: &[NamedQuery] = &[
    NamedQuery {
        name: "emergency_numbers",
        sparql: "SELECT ?country ?countryLabel ?iso2 ?number ?numberLabel WHERE { \
                 ?country wdt:P31 wd:Q6256; wdt:P297 ?iso2; wdt:P2852 ?number. \
                 SERVICE wikibase:label { bd:serviceParam wikibase:language \"en\". } } \
                 ORDER BY ?iso2 ?number",
    },
    NamedQuery {
        name: "plugs_voltage",
        sparql: "SELECT ?country ?iso2 ?plug ?plugLabel ?voltage WHERE { \
                 ?country wdt:P31 wd:Q6256; wdt:P297 ?iso2. \
                 OPTIONAL { ?country wdt:P2853 ?plug. } OPTIONAL { ?country wdt:P2884 ?voltage. } \
                 SERVICE wikibase:label { bd:serviceParam wikibase:language \"en\". } } \
                 ORDER BY ?iso2 ?plug",
    },
    NamedQuery {
        name: "airports",
        sparql: "SELECT ?airport ?airportLabel ?iata ?coord ?countryIso WHERE { \
                 ?airport wdt:P238 ?iata. OPTIONAL { ?airport wdt:P625 ?coord. } \
                 OPTIONAL { ?airport wdt:P17/wdt:P297 ?countryIso. } \
                 SERVICE wikibase:label { bd:serviceParam wikibase:language \"en\". } } \
                 ORDER BY ?iata ?airport",
    },
];

/// SPARQL 1.1 JSON results: rows of `var → value`.
pub fn parse(body: &[u8]) -> Result<Vec<BTreeMap<String, String>>> {
    #[derive(Deserialize)]
    struct Doc {
        results: Results,
    }
    #[derive(Deserialize)]
    struct Results {
        bindings: Vec<BTreeMap<String, Term>>,
    }
    #[derive(Deserialize, Serialize)]
    struct Term {
        value: String,
    }
    let d: Doc = json(body)?;
    Ok(d.results
        .bindings
        .into_iter()
        .map(|b| b.into_iter().map(|(k, t)| (k, t.value)).collect())
        .collect())
}

fn page_url(
    endpoint: &CanonicalUrl,
    sparql: &str,
    limit: u32,
    offset: u32,
) -> Result<CanonicalUrl> {
    let mut u = endpoint.as_url().clone();
    u.set_query(None);
    u.query_pairs_mut()
        .append_pair("format", "json")
        .append_pair("query", &format!("{sparql} LIMIT {limit} OFFSET {offset}"));
    CanonicalUrl::from_url(u).map_err(|e| Error::Invalid(e.to_string()))
}

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct WikidataSparql {
    /// Rows per page.
    pub page_size: u32,
    /// Pages per query per run.
    pub max_pages: u32,
}

impl WikidataSparql {
    /// 5000 rows per page, at most 20 pages per query.
    pub const DEFAULT: WikidataSparql = WikidataSparql {
        page_size: 5000,
        max_pages: 20,
    };
}

impl Default for WikidataSparql {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[async_trait]
impl SourceAdapter for WikidataSparql {
    fn id(&self) -> &'static str {
        "wikidata_sparql"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/sparql-results+json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let endpoint = ctx.url.url.clone();
        if endpoint.as_url().query_pairs().any(|(k, _)| k == "query") {
            let ex = fetch_raw(ctx, endpoint, &opts, &mut rep).await?;
            check_status(&ex)?;
            parse(&ex.body)?;
            rep.complete = true;
            return Ok(rep);
        }
        for q in QUERIES {
            let mut offset = 0;
            for page in 0.. {
                if page >= self.max_pages {
                    rep.cursor = Some(format!("{}@{offset}", q.name));
                    return Ok(rep);
                }
                let url = page_url(&endpoint, q.sparql, self.page_size, offset)?;
                let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
                check_status(&ex)?;
                let rows = parse(&ex.body)?;
                let n = u32::try_from(rows.len()).unwrap_or(u32::MAX);
                if n < self.page_size {
                    break;
                }
                offset += n;
            }
        }
        rep.complete = true;
        Ok(rep)
    }
}
