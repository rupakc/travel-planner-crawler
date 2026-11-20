//! `factbook_json`: country facts from the CIA World Factbook, final edition (2026-02-04,
//! frozen; the CIA discontinued the Factbook), as converted to JSON by the factbook.json
//! project (`github.com/factbook/factbook.json`, CC0 1.0; the Factbook itself is a US
//! Government work in the public domain). One leased row = one country file
//! (`raw.githubusercontent.com/factbook/factbook.json/<ref>/<region>/<code>.json`; robots.txt of
//! raw.githubusercontent.com answers 404 = allowed), spooled raw and mapped by [`items`].
//!
//! Facts (templated, our own sentences; `as_of` = the edition date):
//! - Geography / Elevation: highest and lowest point (`unbelievable`, with the metres);
//! - Geography / Area / total (`popular`, km²);
//! - Government / National heritage / total World Heritage Sites (`popular`);
//! - Government / National symbol(s) (`popular`);
//! - Government / Country name / former (`historical`);
//! - Government / Independence: its first date (`historical`, `date_ref`).
//!
//! The country is the file's "conventional short form" name resolved to ISO 3166-1 alpha-2
//! ([`super::countries::iso2`]); a file whose name does not resolve (oceans, the World, the EU)
//! yields nothing.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Map, Value, json};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, fetch_raw};
use super::countries;
use super::place_facts::{Fact, TextOrigin, number, parse_english_date, strip_html};

/// Adapter id.
pub const ID: &str = "factbook_json";
/// The final edition's date (`as_of` of every fact).
pub const EDITION: &str = "2026-02-04";

const SOURCE: (&str, &str) = (
    "The World Factbook, final edition (via factbook.json)",
    "US Central Intelligence Agency",
);
const ATTRIBUTION: &str =
    "The World Factbook (CIA, public domain), final edition 2026-02-04, via factbook.json (CC0)";

/// The Factbook country code of a file URL (`…/europe/gm.json` → `gm`).
pub fn file_code(url: &CanonicalUrl) -> Option<String> {
    let p = url.as_url().path();
    if !p.contains("/factbook.json/") {
        return None;
    }
    let name = p.rsplit('/').next()?.strip_suffix(".json")?;
    (name.len() == 2 && name.bytes().all(|b| b.is_ascii_lowercase())).then(|| name.to_string())
}

fn text(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for k in path {
        // Some keys carry a trailing space in the source ("total ").
        cur = cur.get(*k).or_else(|| cur.get(format!("{k} ").as_str()))?;
    }
    let t = strip_html(cur.get("text")?.as_str()?);
    (!t.is_empty() && t != "none" && t != "NA").then_some(t)
}

static POINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<name>[^0-9(;]+?)\s+(?P<m>-?[0-9][0-9,]*(?:\.[0-9]+)?)\s*m\b").expect("re")
});
static LEADING_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9][0-9,]*(?:\.[0-9]+)?)").expect("re"));

fn parse_num(s: &str) -> Option<f64> {
    s.replace(',', "")
        .parse()
        .ok()
        .filter(|x: &f64| x.is_finite())
}

/// The `place_fact` items of one Factbook country file. Pure.
pub fn items(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<(String, Value)>> {
    let code =
        file_code(url).ok_or_else(|| Error::Invalid("factbook_json: not a country file".into()))?;
    let v: Value =
        serde_json::from_slice(body).map_err(|e| Error::Invalid(format!("factbook_json: {e}")))?;
    if !v.is_object() || v.get("Government").is_none() && v.get("Geography").is_none() {
        return Err(Error::Invalid(
            "factbook_json: not a Factbook country file".into(),
        ));
    }
    let Some(name) = text(
        &v,
        &["Government", "Country name", "conventional short form"],
    ) else {
        return Ok(Vec::new());
    };
    let Some(iso) = countries::iso2(&name) else {
        return Ok(Vec::new());
    };
    let mut place = Map::new();
    place.insert("country".into(), json!(iso));
    place.insert("admin_level".into(), json!("country"));
    place.insert("name".into(), json!(name));
    place.insert("name_lang".into(), json!("en"));
    place.insert("native_id".into(), json!(format!("factbook:{code}")));
    let src_url = url.as_str().to_string();
    let mk = |field: &str,
              fact_type: &'static str,
              text: String,
              date_ref: Option<String>,
              quantity: Option<(f64, &'static str, Option<String>)>| Fact {
        id: format!(
            "factbook:{code}:{}",
            field
                .to_ascii_lowercase()
                .replace([' ', '/', '(', ')'], "_")
        ),
        place: place.clone(),
        fact_type,
        text,
        lang: "en".into(),
        origin: TextOrigin::Templated,
        date_ref,
        quantity,
        source_field: format!("factbook:{field}"),
        source: (SOURCE.0, SOURCE.1, "partial"),
        licence: ("open", "CC0-1.0", ATTRIBUTION.into()),
        confidence: "structured",
        source_url: Some(src_url.clone()),
        as_of: Some(EDITION.into()),
    };
    let mut out = Vec::new();
    for (key, word) in [("highest point", "highest"), ("lowest point", "lowest")] {
        let Some(t) = text(&v, &["Geography", "Elevation", key]) else {
            continue;
        };
        let Some(c) = POINT.captures(&t) else {
            continue;
        };
        let (pt, m) = (
            c["name"].trim().trim_end_matches(',').to_string(),
            parse_num(&c["m"]),
        );
        let Some(m) = m else { continue };
        if pt.is_empty() || pt.chars().count() > 100 {
            continue;
        }
        out.push(mk(
            &format!("Geography/Elevation/{key}"),
            "unbelievable",
            format!("The {word} point of {name} is {pt} ({} m).", number(m)),
            None,
            Some((m, "m", Some(format!("{word} point of {name}")))),
        ));
    }
    if let Some(t) = text(&v, &["Geography", "Area", "total"])
        && t.contains("sq km")
        && let Some(a) = LEADING_NUMBER.captures(&t).and_then(|c| parse_num(&c[1]))
        && a > 0.0
    {
        out.push(mk(
            "Geography/Area/total",
            "popular",
            format!("{name} covers a total area of {} km\u{b2}.", number(a)),
            None,
            Some((a, "km2", None)),
        ));
    }
    if let Some(t) = text(
        &v,
        &[
            "Government",
            "National heritage",
            "total World Heritage Sites",
        ],
    ) && let Some(n) = LEADING_NUMBER.captures(&t).and_then(|c| parse_num(&c[1]))
        && n >= 1.0
    {
        let s = if (n - 1.0).abs() < f64::EPSILON {
            ""
        } else {
            "s"
        };
        out.push(mk(
            "Government/National heritage/total World Heritage Sites",
            "popular",
            format!("{name} has {} UNESCO World Heritage Site{s}.", number(n)),
            None,
            Some((n, "sites", None)),
        ));
    }
    if let Some(t) = text(&v, &["Government", "National symbol(s)"])
        && t.chars().count() <= 200
    {
        out.push(mk(
            "Government/National symbol(s)",
            "popular",
            format!("National symbols of {name}: {}.", t.trim_end_matches('.')),
            None,
            None,
        ));
    }
    if let Some(t) = text(&v, &["Government", "Country name", "former"])
        && t.chars().count() <= 200
    {
        out.push(mk(
            "Government/Country name/former",
            "historical",
            format!("Former names of {name}: {}.", t.trim_end_matches('.')),
            None,
            None,
        ));
    }
    if let Some(t) = text(&v, &["Government", "Independence"]) {
        let first = t.split([';', '(']).next().unwrap_or("").trim();
        if let Some(d) = parse_english_date(first).filter(|_| first.chars().count() <= 40) {
            out.push(mk(
                "Government/Independence",
                "historical",
                format!("{name} dates its independence to {first}."),
                Some(d),
                None,
            ));
        }
    }
    Ok(out.into_iter().filter_map(Fact::into_data).collect())
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct FactbookJson;

#[async_trait]
impl SourceAdapter for FactbookJson {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        if file_code(&url).is_none() {
            return Err(Error::Invalid("factbook_json: not a country file".into()));
        }
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            conditional: true,
            max_body_bytes: 4 * 1024 * 1024,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
        if ex.status.as_u16() != 304 {
            check_status(&ex)?;
        }
        rep.complete = true;
        Ok(rep)
    }
}
