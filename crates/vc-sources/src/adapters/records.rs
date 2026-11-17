//! What an adapter's stored output **means**: committed adapter captures → kind records
//! (spec §5.7 step 2, "source adapters/parsers": free extraction, no LLM).
//!
//! The fetcher stores adapter output raw (an API `response`, or filtered dump `resource`
//! batches). The processor calls [`records`] with the capture's adapter id, URL and stored
//! body; each item names its kind and carries fields of the v1 kind schema
//! (`schemas/<kind>.v1.json`). The extraction pipeline then fits, validates, gates (high-stakes
//! kinds from official sources only), resolves places and keys them like any other record.
//!
//! Country names: items carry `country_code` (ISO 3166-1 alpha-2) and the source's own country
//! name when it has one; the pipeline fills a missing `country` from the code, and a missing
//! code from the place resolver (Wikivoyage articles only give coordinates).
//!
//! Pure: no I/O, no logging of content. [`records`] returns `None` for an adapter without a
//! mapping (its captures go through the content handlers as before).

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};
use vc_core::{CanonicalUrl, Error, Result};

use super::{canada_advisories, ecb_fx, fcdo_content_api, state_dept_advisories};

/// Bumped when a mapping changes (part of the extractor version string of adapter records).
pub const MAPPING_VERSION: u32 = 1;

/// One record an adapter capture yields.
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterItem {
    /// Kind id (`kinds.toml`).
    pub kind: &'static str,
    /// Stable id within the source (record key input), e.g. an ISO code or an airport ident.
    pub id: Option<String>,
    /// Observation series (observation kinds only), e.g. `fx:EUR:USD`.
    pub series_key: Option<String>,
    /// Fields of the kind's v1 schema.
    pub data: Value,
}

/// The adapters that have a record mapping.
pub const MAPPED: &[&str] = &[
    "canada_advisories",
    "ecb_fx",
    "fcdo_content_api",
    "ourairports_csv",
    "state_dept_advisories",
    "wikivoyage_dump",
];

/// The records of one stored adapter capture: `None` when `adapter` has no mapping,
/// `Some(Err)` when the body does not parse as that adapter's output.
pub fn records(adapter: &str, url: &CanonicalUrl, body: &[u8]) -> Option<Result<Vec<AdapterItem>>> {
    Some(match adapter {
        "canada_advisories" => canada(body),
        "ecb_fx" => ecb(body),
        "fcdo_content_api" => fcdo(url, body),
        "ourairports_csv" => ourairports(body),
        "state_dept_advisories" => state_dept(body),
        "wikivoyage_dump" => wikivoyage(body),
        _ => return None,
    })
}

fn obj(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        if let Some(v) = v.filter(|v| !v.is_null() && v.as_str() != Some("")) {
            m.insert(k.to_string(), v);
        }
    }
    Value::Object(m)
}

fn s(v: impl Into<String>) -> Option<Value> {
    let v: String = v.into();
    let v = v.trim();
    (!v.is_empty()).then(|| Value::String(v.to_string()))
}

/// An upper-case ISO 3166-1 alpha-2 code, if `c` is one.
fn iso2(c: &str) -> Option<String> {
    let c = c.trim();
    (c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())).then(|| c.to_ascii_uppercase())
}

/// The `YYYY-MM-DD` prefix of a date or timestamp, if it has one.
fn day(d: &str) -> Option<String> {
    let d = d.trim();
    let p = d.get(..10)?;
    let b = p.as_bytes();
    (b[4] == b'-' && b[7] == b'-' && p.bytes().filter(u8::is_ascii_digit).count() == 8)
        .then(|| p.to_string())
}

// ---------------------------------------------------------------------------------------------
// ECB reference rates → fx_rate observations
// ---------------------------------------------------------------------------------------------

fn ecb(body: &[u8]) -> Result<Vec<AdapterItem>> {
    Ok(ecb_fx::parse(body)?
        .into_iter()
        .map(|r| AdapterItem {
            kind: "fx_rate",
            id: Some(format!("EUR:{}", r.currency)),
            series_key: Some(format!("fx:EUR:{}", r.currency)),
            data: obj(vec![
                ("base_currency", s("EUR")),
                ("quote_currency", s(r.currency.clone())),
                (
                    "rate",
                    serde_json::Number::from_f64(r.rate).map(Value::Number),
                ),
                ("rate_date", s(r.date.clone())),
                ("rate_type", s("reference")),
                ("publisher", s("European Central Bank")),
                ("as_of", s(r.date)),
            ]),
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Advisories: Global Affairs Canada, US State Department, UK FCDO
// ---------------------------------------------------------------------------------------------

/// Global Affairs Canada's four advisory levels (`advisory-state` 0–3), as worded on
/// travel.gc.ca.
const CANADA_LEVELS: [&str; 4] = [
    "Take normal security precautions",
    "Exercise a high degree of caution",
    "Avoid non-essential travel",
    "Avoid all travel",
];

fn canada(body: &[u8]) -> Result<Vec<AdapterItem>> {
    Ok(canada_advisories::parse(body)?
        .into_iter()
        .filter_map(|a| {
            let code = iso2(&a.iso)?;
            let label = a
                .advisory_text
                .clone()
                .filter(|t| !t.trim().is_empty())
                .or_else(|| {
                    CANADA_LEVELS
                        .get(usize::from(a.advisory_state))
                        .map(|s| s.to_string())
                })?;
            Some(AdapterItem {
                kind: "advisory",
                id: Some(code.clone()),
                series_key: None,
                data: obj(vec![
                    ("country", s(a.name)),
                    ("country_code", s(code)),
                    ("issuer", s("Global Affairs Canada")),
                    ("issuer_country_code", s("CA")),
                    ("level", Some(json!(a.advisory_state))),
                    ("level_max", Some(json!(3))),
                    ("level_label", s(label)),
                    (
                        "updated_date",
                        a.published.as_deref().and_then(day).map(Value::String),
                    ),
                    (
                        "as_of",
                        a.published.as_deref().and_then(day).map(Value::String),
                    ),
                    (
                        "source_url",
                        a.slug
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .map(|slug| json!(format!("https://travel.gc.ca/destinations/{slug}"))),
                    ),
                ]),
            })
        })
        .collect())
}

fn state_dept(body: &[u8]) -> Result<Vec<AdapterItem>> {
    let mut out = Vec::new();
    for a in state_dept_advisories::parse(body)? {
        // "France - Level 2: Exercise Increased Caution"
        let Some((country, rest)) = a.title.split_once(" - ") else {
            continue;
        };
        let Some(level) = a.level else { continue };
        let label = rest
            .split_once(':')
            .map(|(_, l)| l.trim())
            .unwrap_or_default();
        if label.is_empty() {
            continue;
        }
        // One advisory per country code (a few advisories cover several).
        let codes: Vec<String> = a.country_codes.iter().filter_map(|c| iso2(c)).collect();
        let targets: Vec<Option<String>> = if codes.len() == 1 {
            vec![Some(codes[0].clone())]
        } else {
            vec![None]
        };
        for code in targets {
            out.push(AdapterItem {
                kind: "advisory",
                id: Some(a.link.clone()),
                series_key: None,
                data: obj(vec![
                    ("country", s(country)),
                    ("country_code", code.map(Value::String)),
                    ("issuer", s("US Department of State")),
                    ("issuer_country_code", s("US")),
                    ("level", Some(json!(level))),
                    ("level_max", Some(json!(4))),
                    ("level_label", s(label)),
                    (
                        "updated_date",
                        a.updated.as_deref().and_then(day).map(Value::String),
                    ),
                    (
                        "as_of",
                        a.updated.as_deref().and_then(day).map(Value::String),
                    ),
                    ("source_url", s(a.link.clone())),
                ]),
            });
        }
    }
    Ok(out)
}

/// FCDO `alert_status` values, most severe first, with GOV.UK's wording.
const FCDO_ALERTS: &[(&str, u8, &str)] = &[
    (
        "avoid_all_travel_to_whole_country",
        3,
        "FCDO advises against all travel",
    ),
    (
        "avoid_all_but_essential_travel_to_whole_country",
        2,
        "FCDO advises against all but essential travel",
    ),
    (
        "avoid_all_travel_to_parts",
        1,
        "FCDO advises against all travel to parts of the country",
    ),
    (
        "avoid_all_but_essential_travel_to_parts",
        1,
        "FCDO advises against all but essential travel to parts of the country",
    ),
];

fn fcdo(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<AdapterItem>> {
    let path = url.as_url().path().trim_end_matches('/');
    let Some(slug) = path
        .strip_prefix(fcdo_content_api::INDEX_PATH)
        .and_then(|r| r.strip_prefix('/'))
        .filter(|r| !r.is_empty() && !r.contains('/'))
    else {
        return Ok(Vec::new()); // the index and the change feed carry no advice
    };
    let a = fcdo_content_api::parse_country(body)?;
    let (level, label) = FCDO_ALERTS
        .iter()
        .find(|(k, _, _)| a.alert_status.iter().any(|s| s == k))
        .map(|(_, l, t)| (*l, *t))
        .unwrap_or((0, "No FCDO advice against travel"));
    Ok(vec![AdapterItem {
        kind: "advisory",
        id: Some(a.slug.clone()),
        series_key: None,
        data: obj(vec![
            ("country", s(a.country)),
            ("issuer", s("Foreign, Commonwealth & Development Office")),
            ("issuer_country_code", s("GB")),
            ("level", Some(json!(level))),
            ("level_max", Some(json!(3))),
            ("level_label", s(label)),
            (
                "updated_date",
                a.updated_at.as_deref().and_then(day).map(Value::String),
            ),
            (
                "as_of",
                a.updated_at.as_deref().and_then(day).map(Value::String),
            ),
            (
                "source_url",
                s(format!("https://www.gov.uk/foreign-travel-advice/{slug}")),
            ),
        ]),
    }])
}

// ---------------------------------------------------------------------------------------------
// OurAirports → airport records
// ---------------------------------------------------------------------------------------------

fn ourairports(body: &[u8]) -> Result<Vec<AdapterItem>> {
    let mut r = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(body);
    let headers = r
        .headers()
        .map_err(|e| Error::Invalid(format!("ourairports csv: {e}")))?
        .clone();
    let col = |n: &str| headers.iter().position(|h| h == n);
    let (Some(ident), Some(ty), Some(name)) = (col("ident"), col("type"), col("name")) else {
        return Err(Error::Invalid("ourairports csv: missing columns".into()));
    };
    let (lat, lng, country, city, iata, icao, sched, link) = (
        col("latitude_deg"),
        col("longitude_deg"),
        col("iso_country"),
        col("municipality"),
        col("iata_code"),
        col("icao_code"),
        col("scheduled_service"),
        col("home_link"),
    );
    let mut out = Vec::new();
    for rec in r.records() {
        let Ok(rec) = rec else { continue };
        let get = |i: Option<usize>| i.and_then(|i| rec.get(i)).unwrap_or_default().trim();
        let iata_code =
            Some(get(iata)).filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase()));
        // Airports a traveller flies into: large and medium ones, and any with scheduled
        // service or an IATA code (small fields without either are left out).
        let wanted = matches!(get(Some(ty)), "large_airport" | "medium_airport")
            || get(sched) == "yes"
            || iata_code.is_some();
        if !wanted || get(city).is_empty() {
            continue;
        }
        let Some(code) = iso2(get(country)) else {
            continue;
        };
        let num = |i| get(i).parse::<f64>().ok().filter(|v| v.is_finite());
        out.push(AdapterItem {
            kind: "airport",
            id: Some(get(Some(ident)).to_string()).filter(|s| !s.is_empty()),
            series_key: None,
            data: obj(vec![
                ("name", s(get(Some(name)))),
                ("iata", iata_code.map(|c| json!(c))),
                (
                    "icao",
                    Some(get(icao))
                        .filter(|c| c.len() == 4 && c.bytes().all(|b| b.is_ascii_uppercase()))
                        .map(|c| json!(c)),
                ),
                ("city", s(get(city))),
                ("country_code", s(code)),
                (
                    "lat",
                    num(lat)
                        .filter(|v| (-90.0..=90.0).contains(v))
                        .map(|v| json!(v)),
                ),
                (
                    "lng",
                    num(lng)
                        .filter(|v| (-180.0..=180.0).contains(v))
                        .map(|v| json!(v)),
                ),
                (
                    "website_url",
                    Some(get(link))
                        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                        .filter(|u| !u.contains(char::is_whitespace))
                        .map(|u| json!(u)),
                ),
            ]),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Wikivoyage → place records
// ---------------------------------------------------------------------------------------------

static GEO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\{\{\s*geo\s*\|\s*(-?[0-9]+(?:\.[0-9]+)?)\s*\|\s*(-?[0-9]+(?:\.[0-9]+)?)")
        .expect("regex")
});
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[(?:[^|\]]*\|)?([^\]]*)\]\]").expect("regex"));
static TEMPLATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{[^{}]*\}\}").expect("regex"));
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("regex"));
static TYPE_TEMPLATE: LazyLock<Regex> = LazyLock::new(|| {
    // Article status templates: `{{usablecity}}`, `{{outlinecountry}}`, `{{guideregion}}`…
    Regex::new(r"(?i)\{\{\s*(?:outline|usable|guide|star)\s*(country|region|city|district|park|island)\s*[|}]")
        .expect("regex")
});

/// The first prose paragraph of an article (wiki markup stripped), at most 600 chars.
fn lead(text: &str) -> Option<String> {
    let mut t = text.to_string();
    for _ in 0..4 {
        let n = TEMPLATE.replace_all(&t, "").into_owned();
        if n == t {
            break;
        }
        t = n;
    }
    for line in t.lines() {
        let l = line.trim();
        if l.is_empty()
            || l.starts_with('=')
            || l.starts_with('[')
            || l.starts_with('{')
            || l.starts_with('|')
            || l.starts_with('*')
            || l.starts_with('<')
            || l.starts_with("__")
        {
            continue;
        }
        let l = LINK.replace_all(l, "$1");
        let l = TAG.replace_all(&l, "");
        let l = l.replace("'''", "").replace("''", "");
        let l = l.split_whitespace().collect::<Vec<_>>().join(" ");
        if l.chars().count() < 20 {
            continue;
        }
        return Some(l.chars().take(600).collect());
    }
    None
}

fn place_type(text: &str) -> &'static str {
    match TYPE_TEMPLATE
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_ascii_lowercase())
        .as_deref()
    {
        Some("country") => "country",
        Some("region") => "region",
        Some("city") => "city",
        Some("district") => "neighbourhood",
        Some("park") => "park",
        Some("island") => "island",
        _ => "other",
    }
}

fn wikivoyage(body: &[u8]) -> Result<Vec<AdapterItem>> {
    let pages: Vec<Value> =
        serde_json::from_slice(body).map_err(|e| Error::Invalid(format!("wikivoyage: {e}")))?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in pages {
        let (Some(title), Some(text)) = (
            p.get("title").and_then(Value::as_str),
            p.get("text").and_then(Value::as_str),
        ) else {
            continue; // redirects and empty pages
        };
        // Only geo-tagged articles: the coordinates place them (and give their country).
        let Some(g) = GEO.captures(text) else {
            continue;
        };
        let (Ok(lat), Ok(lng)) = (g[1].parse::<f64>(), g[2].parse::<f64>()) else {
            continue;
        };
        if !((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lng)) {
            continue;
        }
        if !seen.insert(title.to_string()) {
            continue;
        }
        out.push(AdapterItem {
            kind: "place",
            id: p
                .get("page_id")
                .and_then(Value::as_u64)
                .map(|id| format!("enwikivoyage:{id}")),
            series_key: None,
            data: obj(vec![
                ("name", s(title)),
                ("place_type", s(place_type(text))),
                ("lat", Some(json!(lat))),
                ("lng", Some(json!(lng))),
                ("summary", lead(text).map(Value::String)),
                (
                    "source_url",
                    p.get("url").and_then(Value::as_str).and_then(s),
                ),
                (
                    "as_of",
                    p.get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(day)
                        .map(Value::String),
                ),
            ]),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_prefix() {
        assert_eq!(
            day("2026-10-22T20:00:00-04:00").as_deref(),
            Some("2026-10-22")
        );
        assert_eq!(day("2026-11-06 15:55:58").as_deref(), Some("2026-11-06"));
        assert_eq!(day("Nov 6"), None);
    }

    #[test]
    fn lead_strips_markup() {
        let t = "{{pagebanner|X.jpg}}\n'''Abakan''' is the capital of [[Khakassia|the Republic of Khakassia]] in [[Siberia]].\n==Understand==";
        assert_eq!(
            lead(t).as_deref(),
            Some("Abakan is the capital of the Republic of Khakassia in Siberia.")
        );
    }

    #[test]
    fn article_type_from_status_template() {
        assert_eq!(place_type("x\n{{usablecity}}\n"), "city");
        assert_eq!(place_type("{{outlinecountry}}"), "country");
        assert_eq!(place_type("{{ guideregion | x }}"), "region");
        assert_eq!(place_type("{{outlinephrasebook}}"), "other");
    }

    #[test]
    fn unknown_adapter_has_no_mapping() {
        let u = CanonicalUrl::parse("https://example.org/x").unwrap();
        assert!(records("met_norway", &u, b"{}").is_none());
    }
}
