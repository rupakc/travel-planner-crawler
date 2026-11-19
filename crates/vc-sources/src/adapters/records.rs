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

use super::{
    canada_advisories, canada_offices, countries, ecb_fx, ember_api, enwiki_visa,
    eurostat_jsonstat, factor_table, fcdo_content_api, fsa_fhrs, govuk_medical_lists,
    govuk_world_orgs, html_text, iso3, json_events, nyc_restaurant_inspections, openholidays,
    phone, socrata, state_dept_advisories, state_dept_country_info, toronto_dinesafe,
    travelpayouts_fares, udata_resource, wfs_geojson,
};

/// Bumped when a mapping changes (part of the extractor version string of adapter records).
pub const MAPPING_VERSION: u32 = 2;

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
    "canada_offices",
    "ecb_fx",
    "ember_api",
    "enwiki_visa",
    "eurostat_jsonstat",
    "factor_table",
    "fcdo_content_api",
    "fsa_fhrs",
    "govuk_medical_lists",
    "govuk_world_orgs",
    "json_events",
    "nyc_restaurant_inspections",
    "openholidays",
    "gtfs_static",
    "ourairports_csv",
    "socrata",
    "state_dept_advisories",
    "state_dept_country_info",
    "toronto_dinesafe",
    "travelpayouts_fares",
    "udata_resource",
    "wdi_csv",
    "wfs_geojson",
    "wikivoyage_dump",
];

/// The records of one stored adapter capture: `None` when `adapter` has no mapping,
/// `Some(Err)` when the body does not parse as that adapter's output.
pub fn records(adapter: &str, url: &CanonicalUrl, body: &[u8]) -> Option<Result<Vec<AdapterItem>>> {
    Some(match adapter {
        "canada_advisories" => canada(body),
        "canada_offices" => canada_offices_records(body),
        "ecb_fx" => ecb(body),
        "ember_api" => factors(ember_api::items(url, body)),
        "enwiki_visa" => enwiki_visa_records(url, body),
        "eurostat_jsonstat" => eurostat(body),
        "factor_table" => factors(factor_table::items(url, body)),
        "fcdo_content_api" => fcdo(url, body),
        "fsa_fhrs" => restaurants(fsa_fhrs::items(url, body)),
        "govuk_medical_lists" => govuk_medical(url, body),
        "govuk_world_orgs" => govuk_orgs(url, body),
        "json_events" => json_event_items(url, body),
        "nyc_restaurant_inspections" => restaurants(nyc_restaurant_inspections::items(url, body)),
        "openholidays" => holidays(url, body),
        "gtfs_static" => gtfs(body),
        "ourairports_csv" => ourairports(body),
        "socrata" => rows(socrata::items(url, body)),
        "state_dept_advisories" => state_dept(body),
        "state_dept_country_info" => state_dept_info(body),
        "toronto_dinesafe" => restaurants(toronto_dinesafe::items(url, body)),
        "travelpayouts_fares" => travelpayouts(url, body),
        "udata_resource" => rows(udata_resource::items(url, body)),
        "wdi_csv" => wdi(body),
        "wfs_geojson" => rows(wfs_geojson::items(url, body)),
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
// Travelpayouts cached cheapest fares → cheapest_fare observations
// ---------------------------------------------------------------------------------------------

fn upper_code(c: Option<&str>, len: std::ops::RangeInclusive<usize>) -> Option<String> {
    let c = c?.trim().to_ascii_uppercase();
    (len.contains(&c.len()) && c.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(c)
}

fn travelpayouts(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<AdapterItem>> {
    let travelpayouts_fares::Row::Route(origin, destination) = travelpayouts_fares::row(url)?
    else {
        return Ok(Vec::new()); // the route index makes no request
    };
    let p = travelpayouts_fares::parse(body)?;
    let currency = p
        .currency
        .clone()
        .or_else(|| {
            url.as_url()
                .query_pairs()
                .find(|(k, _)| k == "currency")
                .map(|(_, v)| v.to_ascii_uppercase())
        })
        .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase()));
    let Some(currency) = currency else {
        return Err(Error::Invalid("travelpayouts: no currency".into()));
    };
    Ok(p.months
        .into_iter()
        .map(|f| {
            let series = format!("fare:{origin}:{destination}:{}", f.month);
            let iata3 = |c: Option<&str>| {
                upper_code(c, 3..=3)
                    .filter(|c| c.bytes().all(|b| b.is_ascii_uppercase()))
                    .map(Value::String)
            };
            let round_trip = f.return_at.as_deref().and_then(day).is_some();
            AdapterItem {
                kind: "cheapest_fare",
                id: Some(series.trim_start_matches("fare:").to_string()),
                series_key: Some(series),
                data: obj(vec![
                    ("origin_iata", s(origin.clone())),
                    ("destination_iata", s(destination.clone())),
                    ("origin_airport", iata3(f.origin_airport.as_deref())),
                    (
                        "destination_airport",
                        iata3(f.destination_airport.as_deref()),
                    ),
                    ("month", s(f.month.clone())),
                    (
                        "price",
                        serde_json::Number::from_f64(f.price).map(Value::Number),
                    ),
                    ("currency", s(currency.clone())),
                    (
                        "trip_type",
                        s(if round_trip { "round_trip" } else { "one_way" }),
                    ),
                    (
                        "departure_date",
                        f.departure_at.as_deref().and_then(day).map(Value::String),
                    ),
                    (
                        "return_date",
                        f.return_at.as_deref().and_then(day).map(Value::String),
                    ),
                    (
                        "airline",
                        upper_code(f.airline.as_deref(), 2..=3).map(Value::String),
                    ),
                    (
                        "flight_number",
                        f.flight_number.filter(|n| n.len() <= 8).map(Value::String),
                    ),
                    (
                        "transfers",
                        f.transfers.filter(|n| *n <= 9).map(|n| json!(n)),
                    ),
                    (
                        "return_transfers",
                        f.return_transfers.filter(|n| *n <= 9).map(|n| json!(n)),
                    ),
                    (
                        "duration_min",
                        f.duration.filter(|n| *n <= 20_000).map(|n| json!(n)),
                    ),
                    (
                        "found_at",
                        f.found_at.filter(|t| t.len() <= 32).map(Value::String),
                    ),
                    (
                        "search_url",
                        f.link
                            .filter(|l| l.starts_with('/') && !l.starts_with("//"))
                            .filter(|l| !l.contains(char::is_whitespace))
                            .map(|l| json!(format!("https://www.aviasales.com{l}"))),
                    ),
                    ("publisher", s("Travelpayouts / Aviasales Data API")),
                ]),
            }
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

// ---------------------------------------------------------------------------------------------
// US State Department country information → embassy, visa_rule, tip
// ---------------------------------------------------------------------------------------------

fn state_dept_info(body: &[u8]) -> Result<Vec<AdapterItem>> {
    use state_dept_country_info as sd;
    let mut out = Vec::new();
    for c in sd::parse(body)? {
        let code = c.country_code.map(|c| Value::String(c.to_string()));
        let as_of = c.updated.clone().map(Value::String);
        if let Some(b) = sd::embassy_block(&c.embassy)
            && b.name.chars().count() <= 160
            && let Some(mtype) = mission_type(&b.name)
        {
            let mut phones: Vec<Value> = Vec::new();
            for (field, kind, label) in [
                (&b.phone, "main", "Phone"),
                (&b.emergency_phone, "emergency", "Emergency Phone"),
                (&b.fax, "fax", "Fax"),
            ] {
                if let Some(f) = field {
                    phones.extend(phone_items(f, kind, Some(label)));
                }
            }
            phones.truncate(6);
            let notes: Vec<Value> = b
                .notes
                .iter()
                .filter(|n| n.chars().count() <= 400)
                .take(4)
                .map(|n| json!(n))
                .collect();
            out.push(AdapterItem {
                kind: "embassy",
                id: Some(format!("state-dept:embassy:{}", c.tag)),
                series_key: None,
                data: obj(vec![
                    ("name", s(b.name.clone())),
                    ("mission_type", s(mtype)),
                    ("of_country", s("United States")),
                    ("of_country_code", s("US")),
                    ("country", s(c.country.clone())),
                    ("country_code", code.clone()),
                    ("address", short(b.address.clone(), 400)),
                    (
                        "phones",
                        (!phones.is_empty()).then_some(Value::Array(phones)),
                    ),
                    (
                        "emails",
                        b.email.as_ref().and_then(|e| {
                            emails(
                                &e.split([',', ';', ' '])
                                    .map(str::to_string)
                                    .collect::<Vec<_>>(),
                            )
                        }),
                    ),
                    ("website_url", web_url(b.web.as_deref())),
                    ("notes", (!notes.is_empty()).then_some(Value::Array(notes))),
                    ("as_of", as_of.clone()),
                ]),
            });
        }
        if let Some(v) = sd::visa(&c.entry) {
            out.push(AdapterItem {
                kind: "visa_rule",
                id: Some(format!("state-dept:visa:{}:US", c.tag)),
                series_key: None,
                data: obj(vec![
                    ("destination_country", s(c.country.clone())),
                    ("destination_country_code", code.clone()),
                    ("nationality", s("United States")),
                    ("nationality_code", s("US")),
                    ("purpose", s(v.purpose)),
                    ("visa_type", s(v.visa_type)),
                    ("max_stay_days", v.max_stay_days.map(|d| json!(d))),
                    (
                        "passport_validity_months",
                        v.passport_validity_months.map(|m| json!(m)),
                    ),
                    ("as_of", as_of.clone()),
                    (
                        "evidence",
                        (!v.evidence.is_empty()).then(|| json!(v.evidence)),
                    ),
                ]),
            });
        }
        let mut seen = HashSet::new();
        for (section, html) in [
            (sd::Section::Laws, &c.laws),
            (sd::Section::Health, &c.health),
            (sd::Section::Safety, &c.safety),
        ] {
            for t in sd::tips(section, html) {
                let id = unique(
                    &mut seen,
                    format!(
                        "state-dept:tip:{}:{:?}:{}",
                        c.tag,
                        section,
                        t.title.to_lowercase()
                    ),
                );
                out.push(AdapterItem {
                    kind: "tip",
                    id: Some(id),
                    series_key: None,
                    data: obj(vec![
                        ("title", s(t.title)),
                        ("body", s(t.body)),
                        ("category", s(t.category)),
                        ("severity", s(t.severity)),
                        ("country", s(c.country.clone())),
                        ("country_code", code.clone()),
                        ("as_of", as_of.clone()),
                    ]),
                });
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Food-hygiene registers (FSA FHRS, NYC DOHMH, Toronto DineSafe) → restaurant
// ---------------------------------------------------------------------------------------------

fn restaurants(items: Result<Vec<(String, Value)>>) -> Result<Vec<AdapterItem>> {
    Ok(items?
        .into_iter()
        .map(|(id, data)| AdapterItem {
            kind: "restaurant",
            id: Some(id),
            series_key: None,
            data,
        })
        .collect())
}

/// Records of the mapping-driven open-data readers (`socrata`, `udata_resource`,
/// `wfs_geojson`): each row names its own kind ([`super::row_map`]).
fn rows(items: Result<Vec<(Option<String>, &'static str, Value)>>) -> Result<Vec<AdapterItem>> {
    Ok(items?
        .into_iter()
        .map(|(id, kind, data)| AdapterItem {
            kind,
            id,
            series_key: None,
            data,
        })
        .collect())
}

/// Records of `factor_table` and `ember_api`: `emission_factor` records and `grid_carbon_intensity`
/// observations (series `grid_ci:<ISO2>:<period>:<basis>`, the date is the time axis).
fn factors(items: Result<Vec<(Option<String>, &'static str, Value)>>) -> Result<Vec<AdapterItem>> {
    Ok(items?
        .into_iter()
        .map(|(id, kind, data)| {
            let series_key = (kind == "grid_carbon_intensity").then(|| {
                let f = |k: &str| {
                    data.get(k)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                format!(
                    "grid_ci:{}:{}:{}",
                    f("country_code"),
                    f("period"),
                    f("basis")
                )
            });
            AdapterItem {
                kind,
                id,
                series_key,
                data,
            }
        })
        .collect())
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
// OpenHolidays → public_holiday records
// ---------------------------------------------------------------------------------------------

/// The English text of a localized list, else the first one.
fn english(t: &[openholidays::LocalizedText]) -> Option<String> {
    t.iter()
        .find(|x| x.language.eq_ignore_ascii_case("en"))
        .or_else(|| t.first())
        .map(|x| x.text.trim().to_string())
        .filter(|x| !x.is_empty())
}

/// The first non-English text of a localized list.
fn local(t: &[openholidays::LocalizedText]) -> Option<String> {
    t.iter()
        .find(|x| !x.language.eq_ignore_ascii_case("en"))
        .map(|x| x.text.trim().to_string())
        .filter(|x| !x.is_empty())
}

fn query(url: &CanonicalUrl, name: &str) -> Option<String> {
    url.as_url()
        .query_pairs()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.into_owned())
}

fn holidays(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<AdapterItem>> {
    let path = url.as_url().path().trim_end_matches('/');
    if !openholidays::HOLIDAY_PATHS.contains(&path) {
        return Ok(Vec::new()); // the country list carries no holidays
    }
    // Holidays do not name their country: the request does.
    let Some(code) = query(url, "countryIsoCode").as_deref().and_then(iso2) else {
        return Err(Error::Invalid("openholidays: no countryIsoCode".into()));
    };
    let mut out = Vec::new();
    for h in openholidays::parse_holidays(body)? {
        let kind = match h.kind.as_str() {
            "Public" => "public",
            "Bank" => "bank",
            "Optional" => "optional",
            "School" => "school",
            _ => continue, // BackToSchool / EndOfLessons: single school days, not travel-relevant
        };
        let (Some(start), Some(end)) = (day(&h.start_date), day(&h.end_date)) else {
            continue;
        };
        let Some(name) = english(&h.name) else {
            continue;
        };
        let subdivisions: Vec<Value> = h
            .subdivisions
            .iter()
            .flatten()
            .chain(h.groups.iter().flatten())
            .filter(|r| !r.code.trim().is_empty() && r.code.len() <= 20)
            .take(60)
            .map(|r| {
                obj(vec![
                    ("code", s(r.code.clone())),
                    ("name", s(r.short_name.clone())),
                ])
            })
            .collect();
        out.push(AdapterItem {
            kind: "public_holiday",
            id: Some(format!("openholidays:{}", h.id)),
            series_key: None,
            data: obj(vec![
                ("name", s(name.chars().take(160).collect::<String>())),
                (
                    "local_name",
                    local(&h.name).map(|n| json!(n.chars().take(160).collect::<String>())),
                ),
                ("type", s(kind)),
                ("start_date", s(start)),
                ("end_date", s(end)),
                (
                    "half_day",
                    h.temporal_scope.as_deref().map(|t| json!(t == "HalfDay")),
                ),
                ("country_code", s(code.clone())),
                ("nationwide", Some(json!(h.nationwide))),
                (
                    "regional_scope",
                    h.regional_scope
                        .as_deref()
                        .map(str::to_ascii_lowercase)
                        .filter(|r| matches!(r.as_str(), "national" | "regional" | "local"))
                        .map(Value::String),
                ),
                (
                    "subdivisions",
                    (!subdivisions.is_empty()).then_some(Value::Array(subdivisions)),
                ),
                (
                    "comment",
                    h.comment
                        .as_deref()
                        .and_then(english)
                        .map(|c| json!(c.chars().take(300).collect::<String>())),
                ),
            ]),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Open-data event lists → event records
// ---------------------------------------------------------------------------------------------

fn json_event_items(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<AdapterItem>> {
    let Some(m) = json_events::mapping_for(url) else {
        return Err(Error::Invalid(
            "json_events: no mapping for this endpoint".into(),
        ));
    };
    Ok(json_events::parse(m, body)?
        .items
        .iter()
        .filter_map(|it| json_events::event(m, it))
        .map(|(id, data)| AdapterItem {
            kind: "event",
            id,
            series_key: None,
            data,
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Price levels: Eurostat (EU27_2020 = 100) and the World Bank (US = 1) → price_level
// ---------------------------------------------------------------------------------------------

/// Voyager's spending categories, by the publisher's English label.
const PRICE_CATEGORIES: &[(&str, &[&str])] = &[
    ("gdp", &["gross domestic product"]),
    (
        "actual_individual_consumption",
        &["actual individual consumption"],
    ),
    (
        "household_consumption",
        &["household final consumption expenditure"],
    ),
    (
        "food_non_alcoholic_beverages",
        &["food and non-alcoholic beverages"],
    ),
    (
        "alcoholic_beverages_tobacco",
        &[
            "alcoholic beverages and tobacco",
            "alcoholic beverages, tobacco and narcotics",
        ],
    ),
    ("clothing_footwear", &["clothing and footwear"]),
    (
        "housing",
        &["housing, water, electricity, gas and other fuels"],
    ),
    ("transport", &["transport"]),
    (
        "communication",
        &[
            "communication",
            "communications",
            "information and communication",
        ],
    ),
    (
        "recreation_culture",
        &["recreation and culture", "recreation, sport and culture"],
    ),
    (
        "restaurants_hotels",
        &[
            "restaurants and hotels",
            "restaurants and accommodation services",
        ],
    ),
];

/// The category of a publisher label (exact match after case and space folding).
pub fn price_category(label: &str) -> Option<&'static str> {
    let l = label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    PRICE_CATEGORIES
        .iter()
        .find(|(_, labels)| labels.contains(&l.as_str()))
        .map(|(c, _)| *c)
}

/// Eurostat's country codes that differ from ISO 3166-1 (Greece, United Kingdom).
fn eurostat_country(geo: &str) -> Option<String> {
    match geo {
        "EL" => Some("GR".into()),
        "UK" => Some("GB".into()),
        g if g.len() == 2 => iso2(g),
        _ => None, // aggregates: EU27_2020, EA20, …
    }
}

fn price_item(
    code: &str,
    year: i32,
    index: f64,
    basis: &str,
    category: &str,
    extra: Vec<(&str, Option<Value>)>,
) -> AdapterItem {
    let mut fields = vec![
        ("country_code", s(code)),
        ("year", Some(json!(year))),
        ("date", s(format!("{year:04}-01-01"))),
        (
            "index",
            serde_json::Number::from_f64(index).map(Value::Number),
        ),
        ("basis", s(basis)),
        ("category", s(category)),
    ];
    fields.extend(extra);
    AdapterItem {
        kind: "price_level",
        id: Some(format!("{code}:{category}:{basis}:{year}")),
        series_key: Some(format!("price_level:{code}:{category}:{basis}")),
        data: obj(fields),
    }
}

fn eurostat(body: &[u8]) -> Result<Vec<AdapterItem>> {
    let cube = eurostat_jsonstat::parse(body)?;
    let (Some(geo), Some(time)) = (cube.dim("geo"), cube.dim("time")) else {
        return Err(Error::Invalid("eurostat: no geo/time dimension".into()));
    };
    let (na, cat) = (cube.dim("na_item"), cube.dim("ppp_cat"));
    let as_of = cube.updated.as_deref().and_then(day);
    let mut out = Vec::new();
    for c in &cube.cells {
        // Price level indices only (EU27_2020 = 100).
        let indicator = na.map(|i| c.coords[i].as_str()).unwrap_or_default();
        if indicator != "PLI_EU27_2020" {
            continue;
        }
        let Some(code) = eurostat_country(&c.coords[geo]) else {
            continue;
        };
        let Ok(year) = c.coords[time].trim().parse::<i32>() else {
            continue;
        };
        let (cat_code, cat_label) = match cat {
            Some(i) => (
                Some(c.coords[i].clone()),
                cube.label(i, &c.coords[i]).map(str::to_string),
            ),
            None => (None, Some("Gross domestic product".to_string())),
        };
        let Some(category) = cat_label.as_deref().and_then(price_category) else {
            continue;
        };
        out.push(price_item(
            &code,
            year,
            c.value,
            "eu27_2020_100",
            category,
            vec![
                ("category_code", cat_code.map(Value::String)),
                ("category_label", cat_label.map(Value::String)),
                ("indicator_code", s(indicator)),
                ("flags", c.status.clone().map(Value::String)),
                ("publisher", s("Eurostat")),
                ("as_of", as_of.clone().map(Value::String)),
            ],
        ));
    }
    Ok(out)
}

/// Earliest year of the WDI rows turned into observations.
pub const WDI_FROM_YEAR: i32 = 2000;

fn wdi(body: &[u8]) -> Result<Vec<AdapterItem>> {
    let mut r = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(body);
    let headers = r
        .headers()
        .map_err(|e| Error::Invalid(format!("wdi csv: {e}")))?
        .clone();
    let col = |n: &str| {
        headers
            .iter()
            .position(|h| h.trim_start_matches('\u{feff}') == n)
    };
    let (Some(cc), Some(ic)) = (col("Country Code"), col("Indicator Code")) else {
        return Err(Error::Invalid("wdi csv: missing columns".into()));
    };
    let name = col("Indicator Name");
    let years: Vec<(usize, i32)> = headers
        .iter()
        .enumerate()
        .filter_map(|(i, h)| {
            let h = h.trim();
            (h.len() == 4)
                .then(|| h.parse::<i32>().ok())
                .flatten()
                .filter(|y| *y >= WDI_FROM_YEAR)
                .map(|y| (i, y))
        })
        .collect();
    let mut out = Vec::new();
    for rec in r.records() {
        let Ok(rec) = rec else { continue };
        let indicator = rec.get(ic).unwrap_or_default().trim();
        if indicator != "PA.NUS.PPPC.RF" {
            continue;
        }
        let Some(code) = iso3::alpha2(rec.get(cc).unwrap_or_default()) else {
            continue; // aggregates and unknown codes
        };
        for (i, year) in &years {
            let Some(v) = rec
                .get(*i)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
            else {
                continue;
            };
            out.push(price_item(
                code,
                *year,
                v,
                "us_1",
                "gdp",
                vec![
                    ("category_code", s("GDP")),
                    (
                        "category_label",
                        name.and_then(|n| rec.get(n))
                            .map(|n| json!(n.trim().chars().take(160).collect::<String>())),
                    ),
                    ("indicator_code", s(indicator)),
                    ("publisher", s("World Bank, World Development Indicators")),
                ],
            ));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Embassies and consulates: GOV.UK worldwide organisations, Global Affairs Canada offices
// ---------------------------------------------------------------------------------------------

/// The `mission_type` of an office title; `None` for offices that give travellers no
/// consular help (missions to international bodies, delegations, trade offices).
pub fn mission_type(title: &str) -> Option<&'static str> {
    let t = title.to_lowercase();
    if [
        "permanent mission",
        "delegation",
        "mission to",
        "representation to",
        "trade office",
    ]
    .iter()
    .any(|w| t.contains(w))
    {
        return None;
    }
    Some(if t.contains("honorary") {
        "honorary_consulate"
    } else if t.contains("consular agency") {
        "consular_agency"
    } else if t.contains("consulate general") || t.contains("consulate-general") {
        "consulate_general"
    } else if t.contains("deputy high commission") {
        "other"
    } else if t.contains("high commission") {
        "high_commission"
    } else if t.contains("embassy") {
        "embassy"
    } else if t.contains("consulate") || t.contains("consul") {
        "consulate"
    } else if t.contains("representative office") || t.contains("office") {
        "representative_office"
    } else {
        "other"
    })
}

/// What a labelled phone number is for.
fn phone_type(label: &str) -> &'static str {
    let l = label.to_lowercase();
    if l.contains("fax") {
        "fax"
    } else if l.contains("emergenc")
        || l.contains("24 hour")
        || l.contains("24/7")
        || l.contains("out of hours")
    {
        "emergency"
    } else if l.is_empty()
        || [
            "telephone",
            "tel",
            "phone",
            "switchboard",
            "main",
            "general",
        ]
        .iter()
        .any(|w| l.contains(w))
    {
        "main"
    } else {
        "other"
    }
}

fn phone_items(field: &str, kind: &'static str, label: Option<&str>) -> Vec<Value> {
    phone::parse(field)
        .into_iter()
        .map(|p| {
            let label = label
                .map(str::to_string)
                .filter(|l| !l.trim().is_empty())
                .or(p.note.clone());
            obj(vec![
                ("type", s(kind)),
                (
                    "label",
                    label.filter(|l| l.chars().count() <= 160).and_then(s),
                ),
                ("number", s(p.raw)),
                ("e164", p.e164.map(Value::String)),
            ])
        })
        .collect()
}

fn web_url(u: Option<&str>) -> Option<Value> {
    u.map(str::trim)
        .filter(|u| (u.starts_with("https://") || u.starts_with("http://")) && u.len() <= 2048)
        .filter(|u| !u.contains(char::is_whitespace))
        .map(|u| json!(u))
}

fn emails(list: &[String]) -> Option<Value> {
    let v: Vec<Value> = list
        .iter()
        .map(|m| m.trim())
        .filter(|m| {
            m.len() <= 254
                && m.matches('@').count() == 1
                && !m.starts_with('@')
                && !m.ends_with('@')
                && !m.contains(char::is_whitespace)
        })
        .take(3)
        .map(|m| json!(m))
        .collect();
    (!v.is_empty()).then_some(Value::Array(v))
}

fn short(t: Option<String>, max: usize) -> Option<Value> {
    t.map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && t.chars().count() <= max)
        .map(Value::String)
}

/// A unique id within one capture: repeats get `#2`, `#3`…
fn unique(seen: &mut HashSet<String>, id: String) -> String {
    if seen.insert(id.clone()) {
        return id;
    }
    (2..)
        .map(|n| format!("{id}#{n}"))
        .find(|c| seen.insert(c.clone()))
        .unwrap_or(id)
}

fn govuk_orgs(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<AdapterItem>> {
    if !url
        .as_url()
        .path()
        .starts_with(govuk_world_orgs::ORG_PREFIX)
    {
        return Ok(Vec::new()); // search pages list organisations, they carry no offices
    }
    let org = govuk_world_orgs::parse_org(body)?;
    let mut out = Vec::new();
    for o in org.offices {
        let Some(mtype) = mission_type(&o.title) else {
            continue;
        };
        let country = o.country.clone().or_else(|| org.world_location.clone());
        let Some(country) = country.filter(|c| c.chars().count() <= 80) else {
            continue;
        };
        let code = o
            .country_code
            .clone()
            .filter(|c| countries::name(c).is_some())
            .or_else(|| countries::iso2(&country).map(str::to_string));
        let phones: Vec<Value> = o
            .phones
            .iter()
            .flat_map(|p| phone_items(&p.number, phone_type(&p.title), Some(&p.title)))
            .take(6)
            .collect();
        let as_of = o
            .contact_updated_at
            .as_deref()
            .or(o.updated_at.as_deref())
            .and_then(day);
        out.push(AdapterItem {
            kind: "embassy",
            id: Some(format!("govuk:{}", o.content_id)),
            series_key: None,
            data: obj(vec![
                ("name", short(Some(o.title.clone()), 160)),
                ("mission_type", s(mtype)),
                ("of_country", s("United Kingdom")),
                ("of_country_code", s("GB")),
                ("country", s(country)),
                ("country_code", code.map(Value::String)),
                ("city", short(o.city.clone(), 120)),
                ("address", short(o.address.clone(), 400)),
                (
                    "phones",
                    (!phones.is_empty()).then_some(Value::Array(phones)),
                ),
                ("emails", emails(&o.emails)),
                ("website_url", web_url(o.web_url.as_deref())),
                ("contact_form_url", web_url(o.contact_form_url.as_deref())),
                ("hours", short(o.hours.clone(), 400)),
                ("source_url", web_url(o.web_url.as_deref())),
                ("as_of", as_of.map(Value::String)),
            ]),
        });
    }
    Ok(out)
}

static OF_COUNTRY_CUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s+(?:to|in|at)\s+.*$").expect("regex"));

/// The sending country of a Global Affairs Canada office type ("Embassy of Canada",
/// "High Commission of Australia", "British Embassy").
pub fn sending_country(office_type: &str) -> Option<&'static str> {
    if office_type.to_lowercase().starts_with("british ") {
        return Some("GB");
    }
    office_type.split(" of ").skip(1).find_map(|tail| {
        let name = OF_COUNTRY_CUT.replace(tail.trim(), "");
        countries::iso2(&name)
    })
}

static HOURS_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bhours\b").expect("regex"));

fn canada_offices_records(body: &[u8]) -> Result<Vec<AdapterItem>> {
    let list = canada_offices::parse(body)?;
    let as_of = list.generated.as_deref().and_then(day);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for o in list.offices {
        let Some(mtype) = mission_type(&o.office_type) else {
            continue;
        };
        let mtype = if o.honorary {
            "honorary_consulate"
        } else {
            mtype
        };
        let Some(of_code) = sending_country(&o.office_type) else {
            continue; // unknown sending country: never guessed
        };
        let (code, region) = match o.list_iso.split_once('-') {
            Some((c, _)) => (iso2(c), Some(o.list_country.clone())),
            None => (iso2(&o.list_iso), None),
        };
        let Some(code) = code.filter(|c| countries::name(c).is_some()) else {
            continue;
        };
        let country = countries::name(&code)
            .map(str::to_string)
            .unwrap_or_else(|| o.list_country.clone());
        let mut phones = Vec::new();
        for (field, kind) in [
            (&o.tel, "main"),
            (&o.emergency, "emergency"),
            (&o.fax, "fax"),
        ] {
            if let Some(f) = field {
                phones.extend(phone_items(f, kind, None));
            }
        }
        phones.truncate(6);
        let mut hours = None;
        let mut notes = Vec::new();
        for (title, text) in &o.notes {
            let body = html_text::one_line(text);
            if body.is_empty() {
                continue;
            }
            if hours.is_none() && HOURS_TITLE.is_match(title) {
                hours = Some(body);
                continue;
            }
            let t = html_text::one_line(title);
            let note = if t.is_empty() {
                body
            } else {
                format!("{}: {body}", t.trim_end_matches(':').trim())
            };
            if note.chars().count() <= 400 {
                notes.push(json!(note));
            }
        }
        notes.truncate(4);
        let name = match o.city.as_deref() {
            Some(c) => format!("{} in {c}", o.office_type),
            None => o.office_type.clone(),
        };
        let id = unique(
            &mut seen,
            format!(
                "gac:{}:{}:{}",
                o.list_iso,
                countries::fold(o.city.as_deref().unwrap_or_default()),
                countries::fold(&o.office_type)
            ),
        );
        out.push(AdapterItem {
            kind: "embassy",
            id: Some(id),
            series_key: None,
            data: obj(vec![
                ("name", short(Some(name), 160)),
                ("mission_type", s(mtype)),
                ("of_country", countries::name(of_code).and_then(s)),
                ("of_country_code", s(of_code)),
                ("country", s(country)),
                ("country_code", s(code)),
                ("region", short(region, 120)),
                ("city", short(o.city.clone(), 120)),
                ("address", short(o.address.clone(), 400)),
                ("lat", o.lat.map(|v| json!(v))),
                ("lng", o.lng.map(|v| json!(v))),
                (
                    "phones",
                    (!phones.is_empty()).then_some(Value::Array(phones)),
                ),
                ("emails", emails(&o.emails)),
                ("website_url", web_url(o.website.as_deref())),
                ("hours", short(hours, 400)),
                ("passport_services", o.passport_services.map(Value::Bool)),
                ("notes", (!notes.is_empty()).then_some(Value::Array(notes))),
                ("source_url", web_url(o.website.as_deref())),
                ("as_of", as_of.clone().map(Value::String)),
            ]),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// FCDO medical lists → article (one per country list) and medical_facility records
// ---------------------------------------------------------------------------------------------

/// The FCDO, as the issuer of the medical lists.
const FCDO: &str = "Foreign, Commonwealth & Development Office";

fn govuk_medical(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<AdapterItem>> {
    use govuk_medical_lists::{Row, WEB_ORIGIN, country_of_title, row_of};
    match row_of(url) {
        Some(Row::Publication) => {
            let p = govuk_medical_lists::parse_publication(body)?;
            let Some(country) = p.country.clone() else {
                return Ok(Vec::new());
            };
            let updated = p.updated_at.as_deref().and_then(day);
            let link = format!("{WEB_ORIGIN}{}", p.base_path);
            let summary = format!(
                "Official FCDO list of English-speaking doctors and medical facilities in {country}, for information only{}.",
                updated
                    .as_deref()
                    .map(|d| format!("; last updated {d}"))
                    .unwrap_or_default()
            );
            Ok(vec![AdapterItem {
                kind: "article",
                id: Some(format!("fcdo-medical-list:{}", p.base_path)),
                series_key: None,
                data: obj(vec![
                    ("headline", short(Some(p.title.clone()), 200)),
                    ("summary", short(Some(summary), 600)),
                    ("article_type", s("guide")),
                    ("topics", Some(json!(["health", "safety"]))),
                    ("country_code", countries::iso2(&country).map(|c| json!(c))),
                    ("country", short(Some(country), 80)),
                    (
                        "published_date",
                        p.first_published_at
                            .as_deref()
                            .and_then(day)
                            .map(Value::String),
                    ),
                    ("updated_date", updated.clone().map(Value::String)),
                    ("source_url", web_url(Some(&link))),
                    ("as_of", updated.map(Value::String)),
                ]),
            }])
        }
        Some(Row::List) => {
            let l = govuk_medical_lists::parse_list(body)?;
            let Some(country) = l
                .parent_title
                .as_deref()
                .and_then(country_of_title)
                .filter(|c| c.chars().count() <= 80)
            else {
                return Ok(Vec::new()); // no country we can name: nothing is guessed
            };
            let code = countries::iso2(&country);
            let list_url = format!("{WEB_ORIGIN}{}", l.base_path);
            let updated = l.updated_at.as_deref().and_then(day);
            let list_title = l.parent_title.clone().unwrap_or_else(|| l.title.clone());
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for f in l.facilities {
                let phones: Vec<Value> = f
                    .phones
                    .iter()
                    .map(|p| {
                        obj(vec![
                            ("number", s(p.raw.clone())),
                            ("e164", p.e164.clone().map(Value::String)),
                        ])
                    })
                    .collect();
                let id = unique(
                    &mut seen,
                    format!("fcdo-medical:{}:{}", l.base_path, countries::fold(&f.name)),
                );
                out.push(AdapterItem {
                    kind: "medical_facility",
                    id: Some(id),
                    series_key: None,
                    data: obj(vec![
                        ("name", short(Some(f.name), 200)),
                        ("country", s(country.clone())),
                        ("country_code", code.map(|c| json!(c))),
                        ("section", short(f.section, 200)),
                        ("specialty", short(f.specialty, 200)),
                        ("address", short(f.address, 400)),
                        (
                            "phones",
                            (!phones.is_empty()).then_some(Value::Array(phones)),
                        ),
                        ("emails", emails(&f.emails)),
                        ("website_url", web_url(f.website.as_deref())),
                        ("list_title", short(Some(list_title.clone()), 200)),
                        ("list_url", web_url(Some(&list_url))),
                        ("list_updated", updated.clone().map(Value::String)),
                        ("issuer", s(FCDO)),
                        ("source_url", web_url(Some(&list_url))),
                        ("as_of", updated.clone().map(Value::String)),
                    ]),
                });
            }
            Ok(out)
        }
        _ => Ok(Vec::new()), // the collection only lists publications
    }
}

// ---------------------------------------------------------------------------------------------
// Wikipedia "Visa requirements for X citizens" → visa_reference (cross-check only)
// ---------------------------------------------------------------------------------------------

fn enwiki_visa_records(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<AdapterItem>> {
    if enwiki_visa::stream_range(url).is_none() {
        return Ok(Vec::new()); // the filtered index: stream offsets, no rules
    }
    let mut out = Vec::new();
    for a in enwiki_visa::parse_stream(body)? {
        let mut seen = HashSet::new();
        let as_of = a.timestamp.as_deref().and_then(day);
        for r in &a.rows {
            let dest_key = r
                .destination_code
                .clone()
                .unwrap_or_else(|| countries::fold(&r.destination));
            if !seen.insert(dest_key.clone()) {
                continue; // a destination listed twice: the first row wins
            }
            out.push(AdapterItem {
                kind: "visa_reference",
                id: Some(format!("enwiki:{}:{dest_key}", a.nationality_code)),
                series_key: None,
                data: obj(vec![
                    ("nationality", s(a.nationality.clone())),
                    ("nationality_code", s(a.nationality_code.clone())),
                    (
                        "destination_country",
                        short(Some(r.destination.clone()), 80),
                    ),
                    (
                        "destination_country_code",
                        r.destination_code.clone().map(Value::String),
                    ),
                    ("requirement", s(r.requirement)),
                    (
                        "requirement_text",
                        short(Some(r.requirement_text.clone()), 160),
                    ),
                    ("max_stay_days", r.max_stay_days.map(|d| json!(d))),
                    ("allowed_stay_text", short(r.allowed_stay.clone(), 160)),
                    ("notes", short(r.notes.clone(), 600)),
                    ("article_title", short(Some(a.title.clone()), 200)),
                    ("revision_id", a.rev_id.map(|r| json!(r))),
                    ("provenance", s("wikipedia")),
                    ("source_url", web_url(Some(&a.url))),
                    ("as_of", as_of.clone().map(Value::String)),
                ]),
            });
        }
    }
    Ok(out)
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
// Wikivoyage → place, attraction and hotel records
// ---------------------------------------------------------------------------------------------

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
    use super::wikivoyage_dump::{EDITIONS, article_coords, edition};
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
        // Pages written before filter v2 carry no `lang`: they are English.
        let ed = p
            .get("lang")
            .and_then(Value::as_str)
            .and_then(edition)
            .unwrap_or(&EDITIONS[0]);
        let prefix = ed.id_prefix();
        let page_id = p.get("page_id").and_then(Value::as_u64);
        let source_url = p.get("url").and_then(Value::as_str).and_then(s);
        let as_of = p
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(day)
            .map(Value::String);
        if !seen.insert(title.to_string()) {
            continue;
        }
        // Geo-tagged articles are places: the coordinates place them (and give their country).
        if let Some((lat, lng)) = article_coords(ed, text) {
            out.push(AdapterItem {
                kind: "place",
                id: page_id.map(|id| format!("{prefix}:{id}")),
                series_key: None,
                data: obj(vec![
                    ("name", s(title)),
                    ("place_type", s(place_type(text))),
                    ("lat", Some(json!(lat))),
                    ("lng", Some(json!(lng))),
                    ("summary", lead(text).map(Value::String)),
                    ("source_url", source_url.clone()),
                    ("as_of", as_of.clone()),
                ]),
            });
        }
        // Listings: sights → attraction, places to stay → hotel, in the article's city (the
        // article title without a district suffix, e.g. `Paris/Marais` → `Paris`).
        let city = title.split('/').next().unwrap_or(title).trim();
        let mut names = HashSet::new();
        for l in p
            .get("listings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(ty), Some(name)) = (
                l.get("type").and_then(Value::as_str),
                l.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            let kind = match ty {
                "see" => "attraction",
                "sleep" => "hotel",
                _ => continue,
            };
            if !names.insert((kind, name.to_lowercase())) {
                continue;
            }
            let field = |k: &str| l.get(k).and_then(Value::as_str).map(str::to_string);
            let sub = field("subtype").unwrap_or_default().to_lowercase();
            let phone = field("phone")
                .filter(|v| WV_PHONE.is_match(v))
                .map(|v| json!(v));
            let url = field("url").map(|v| json!(v));
            // A stable id across dumps: the listing's Wikidata item, else its name in the page.
            let id = page_id.map(|pid| match field("wikidata") {
                Some(q) => format!("{prefix}:{pid}:{q}"),
                None => format!("{prefix}:{pid}:{}", name.to_lowercase()),
            });
            let mut fields = vec![
                ("name", s(name)),
                ("city", s(city)),
                ("description", field("description").map(|v| json!(v))),
                ("address", field("address").map(|v| json!(v))),
                ("lat", l.get("lat").cloned()),
                ("lng", l.get("lng").cloned()),
                ("phone", phone),
                ("source_url", source_url.clone()),
                ("as_of", as_of.clone()),
            ];
            if kind == "attraction" {
                fields.push(("category", s(attraction_category(&sub))));
                fields.push(("info_url", url));
                fields.push(("opening_hours", field("hours").map(|v| json!(v))));
                fields.push((
                    "admission",
                    field("price")
                        .filter(|v| v.chars().count() <= 80)
                        .map(|v| json!({ "text": v })),
                ));
            } else {
                fields.push(("property_type", hotel_type(&sub).map(|t| json!(t))));
                fields.push(("website_url", url));
            }
            out.push(AdapterItem {
                kind,
                id,
                series_key: None,
                data: obj(fields),
            });
        }
    }
    Ok(out)
}

static WV_PHONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\+?[0-9][0-9 ().\-/]{1,30}$").expect("regex"));

/// The attraction category of a fine-grained listing type (`{{vCard|type=museum}}`).
fn attraction_category(sub: &str) -> &'static str {
    match sub {
        "museum" | "gallery" => "museum",
        "church" | "cathedral" | "mosque" | "synagogue" | "temple" | "shrine" | "monastery"
        | "chapel" => "religious_site",
        "castle" | "palace" | "fort" | "castle ruin" => "palace_castle",
        "viewpoint" => "viewpoint",
        "park" | "garden" => "park_garden",
        "market" => "market",
        "national park" | "nature reserve" | "waterfall" | "cave" | "mountain" | "lake"
        | "beach" | "landscape" => "natural_wonder",
        "monument" | "memorial" => "monument",
        "zoo" | "aquarium" => "zoo_aquarium",
        "theme park" => "theme_park",
        "archaeological site" | "ruins" | "ruin" | "landmark" | "tower" | "bridge" | "building" => {
            "landmark"
        }
        _ => "other",
    }
}

/// The hotel property type of a fine-grained listing type, if it names one.
fn hotel_type(sub: &str) -> Option<&'static str> {
    Some(match sub {
        "hotel" | "motel" | "inn" => "hotel",
        "hostel" => "hostel",
        "guest house" | "guesthouse" | "pension" | "minshuku" => "guesthouse",
        "ryokan" => "ryokan",
        "bed and breakfast" => "bnb",
        "apartment" | "holiday flat" => "apartment",
        "resort" => "resort",
        "campsite" | "camping" => "campsite",
        "capsule hotel" => "capsule",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// GTFS static feeds → transit_item records (operators, lines, stations)
// ---------------------------------------------------------------------------------------------

/// The country of an IANA time zone, for the zones of the feeds we read and their neighbours
/// (a zone outside the table leaves the country to the place resolver, which uses a station's
/// coordinates).
const TZ_COUNTRY: &[(&str, &str)] = &[
    ("America/Chicago", "US"),
    ("America/Denver", "US"),
    ("America/Los_Angeles", "US"),
    ("America/New_York", "US"),
    ("America/Montreal", "CA"),
    ("America/Toronto", "CA"),
    ("America/Vancouver", "CA"),
    ("America/Sao_Paulo", "BR"),
    ("America/Mexico_City", "MX"),
    ("Asia/Hong_Kong", "HK"),
    ("Asia/Kolkata", "IN"),
    ("Asia/Calcutta", "IN"),
    ("Asia/Singapore", "SG"),
    ("Asia/Tokyo", "JP"),
    ("Asia/Seoul", "KR"),
    ("Asia/Taipei", "TW"),
    ("Australia/Adelaide", "AU"),
    ("Australia/Brisbane", "AU"),
    ("Australia/Darwin", "AU"),
    ("Australia/Hobart", "AU"),
    ("Australia/Melbourne", "AU"),
    ("Australia/Perth", "AU"),
    ("Australia/Sydney", "AU"),
    ("Europe/Amsterdam", "NL"),
    ("Europe/Athens", "GR"),
    ("Europe/Berlin", "DE"),
    ("Europe/Brussels", "BE"),
    ("Europe/Budapest", "HU"),
    ("Europe/Copenhagen", "DK"),
    ("Europe/Dublin", "IE"),
    ("Europe/Helsinki", "FI"),
    ("Europe/Lisbon", "PT"),
    ("Europe/London", "GB"),
    ("Europe/Luxembourg", "LU"),
    ("Europe/Madrid", "ES"),
    ("Europe/Oslo", "NO"),
    ("Europe/Paris", "FR"),
    ("Europe/Prague", "CZ"),
    ("Europe/Rome", "IT"),
    ("Europe/Stockholm", "SE"),
    ("Europe/Vienna", "AT"),
    ("Europe/Warsaw", "PL"),
    ("Europe/Zurich", "CH"),
    ("Pacific/Auckland", "NZ"),
];

fn tz_country(tz: &str) -> Option<&'static str> {
    TZ_COUNTRY
        .iter()
        .find(|(z, _)| z.eq_ignore_ascii_case(tz.trim()))
        .map(|(_, c)| *c)
}

fn http_url(v: &Value) -> Option<Value> {
    v.as_str()
        .map(str::trim)
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .filter(|u| !u.contains(char::is_whitespace) && u.len() <= 2048)
        .map(|u| json!(u))
}

fn clip(v: &Value, n: usize) -> Option<Value> {
    v.as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| json!(t.chars().take(n).collect::<String>()))
}

fn gtfs_fares(v: &Value) -> Option<Value> {
    let fares: Vec<Value> = v
        .as_array()?
        .iter()
        .take(6)
        .filter_map(|f| {
            let amount = f.get("amount")?.as_f64().filter(|a| *a >= 0.0)?;
            let cur = f.get("currency")?.as_str()?;
            let text = format!("{amount:.2} {cur}");
            let mut price = json!({"text": text, "amount": amount, "currency": cur});
            if amount == 0.0 {
                price["is_free"] = json!(true);
            }
            Some(obj(vec![
                (
                    "label",
                    clip(f.get("label").unwrap_or(&Value::Null), 80).or(s("Fare")),
                ),
                ("price", Some(price)),
                (
                    "transfers",
                    f.get("transfers")
                        .and_then(Value::as_u64)
                        .map(|n| json!(n.min(10))),
                ),
                (
                    "unlimited_transfers",
                    f.get("unlimited_transfers")
                        .filter(|b| b.as_bool() == Some(true))
                        .cloned(),
                ),
                (
                    "transfer_window_min",
                    f.get("transfer_duration_s")
                        .and_then(Value::as_u64)
                        .map(|s| json!((s / 60).min(1440))),
                ),
            ]))
        })
        .collect();
    (!fares.is_empty()).then_some(Value::Array(fares))
}

fn gtfs(body: &[u8]) -> Result<Vec<AdapterItem>> {
    let items: Vec<Value> =
        serde_json::from_slice(body).map_err(|e| Error::Invalid(format!("gtfs: {e}")))?;
    let mut out = Vec::new();
    for it in items {
        let g = |k: &str| it.get(k).cloned().unwrap_or(Value::Null);
        let Some(t) = it.get("t").and_then(Value::as_str) else {
            continue;
        };
        let Some(id) = it
            .get("id")
            .and_then(Value::as_str)
            .filter(|i| !i.is_empty())
        else {
            continue;
        };
        let name = match t {
            "line" => {
                let (sn, ln) = (
                    it.get("short_name").and_then(Value::as_str).unwrap_or(""),
                    it.get("long_name").and_then(Value::as_str).unwrap_or(""),
                );
                match (sn.is_empty(), ln.is_empty()) {
                    (false, false) => format!("{sn}: {ln}"),
                    (false, true) => sn.to_string(),
                    (true, false) => ln.to_string(),
                    (true, true) => continue,
                }
            }
            "operator" | "station" => match it.get("name").and_then(Value::as_str) {
                Some(n) if !n.trim().is_empty() => n.to_string(),
                _ => continue,
            },
            _ => continue,
        };
        let tz = it.get("timezone").and_then(Value::as_str).unwrap_or("");
        let feed = it.get("feed").and_then(Value::as_str).unwrap_or("");
        let key = if feed.is_empty() {
            format!("{t}:{id}")
        } else {
            format!("{feed}/{t}:{id}")
        };
        let modes = it
            .get("modes")
            .and_then(Value::as_array)
            .map(|m| Value::Array(m.iter().take(8).cloned().collect()));
        out.push(AdapterItem {
            kind: "transit_item",
            id: Some(key),
            series_key: None,
            data: obj(vec![
                ("item_type", s(t)),
                ("name", clip(&json!(name), 160)),
                ("mode", g("mode").as_str().and_then(s)),
                (
                    "modes",
                    modes.filter(|m| m.as_array().is_some_and(|a| !a.is_empty())),
                ),
                ("short_name", clip(&g("short_name"), 40)),
                ("long_name", clip(&g("long_name"), 200)),
                ("operator", clip(&g("operator"), 160)),
                (
                    "website_url",
                    http_url(&g("url")).or_else(|| http_url(&g("operator_url"))),
                ),
                ("fare_url", http_url(&g("fare_url"))),
                ("phone", clip(&g("phone"), 40)),
                ("time_zone", clip(&g("timezone"), 40)),
                ("language", clip(&g("lang"), 35)),
                ("station_code", clip(&g("code"), 40)),
                (
                    "lat",
                    g("lat")
                        .as_f64()
                        .filter(|v| (-90.0..=90.0).contains(v))
                        .map(|v| json!(v)),
                ),
                (
                    "lng",
                    g("lng")
                        .as_f64()
                        .filter(|v| (-180.0..=180.0).contains(v))
                        .map(|v| json!(v)),
                ),
                (
                    "wheelchair_accessible",
                    g("wheelchair").as_bool().map(Value::Bool),
                ),
                ("route_count", g("route_count").as_u64().map(|n| json!(n))),
                ("fares", gtfs_fares(&g("fares"))),
                (
                    "valid_from",
                    g("valid_from").as_str().and_then(day).map(Value::String),
                ),
                (
                    "valid_to",
                    g("valid_to").as_str().and_then(day).map(Value::String),
                ),
                ("feed_publisher", clip(&g("publisher"), 160)),
                ("country_code", tz_country(tz).map(|c| json!(c))),
                (
                    "as_of",
                    g("valid_from").as_str().and_then(day).map(Value::String),
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
