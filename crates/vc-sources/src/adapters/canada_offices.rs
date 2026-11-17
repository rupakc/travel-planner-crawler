//! `canada_offices`: Global Affairs Canada's consular offices abroad
//! (`data.international.gc.ca/travel-voyage/opendata-offices-list-alpha-eng.json`, OGL-Canada):
//! one JSON object keyed by country, each with the offices that serve travellers there
//! (Canadian embassies, high commissions, consulates and honorary consuls, plus the partner
//! offices of other countries that give Canadians consular help where Canada has none).
//!
//! One leased row, fetched with a conditional GET; the record mapping reads [`parse`].

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json, not_modified};

/// One office as the list gives it (English fields).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CanadaOffice {
    /// The list's `country-iso` (ISO 3166-1 alpha-2, or a subdivision such as `PT-20`).
    pub list_iso: String,
    /// The list's country name (English).
    pub list_country: String,
    /// `type` ("Embassy of Canada", "Honorary consul of Canada", "High Commission of
    /// Australia"…).
    pub office_type: String,
    /// City.
    pub city: Option<String>,
    /// Address as written.
    pub address: Option<String>,
    /// Coordinates.
    pub lat: Option<f64>,
    /// Coordinates.
    pub lng: Option<f64>,
    /// `tel-legacy` as written.
    pub tel: Option<String>,
    /// `emergency-toll-free-legacy` as written.
    pub emergency: Option<String>,
    /// `fax-legacy` as written.
    pub fax: Option<String>,
    /// `email-1..3`.
    pub emails: Vec<String>,
    /// `internet`.
    pub website: Option<String>,
    /// `(title, text)` of each note (HTML fragments).
    pub notes: Vec<(String, String)>,
    /// `has-passport-services`.
    pub passport_services: Option<bool>,
    /// `honorary-consul`.
    pub honorary: bool,
}

/// The parsed list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CanadaOffices {
    /// `metadata.generated.date` (the list's own date).
    pub generated: Option<String>,
    /// Every office, list order.
    pub offices: Vec<CanadaOffice>,
}

#[derive(Deserialize)]
struct List {
    #[serde(default)]
    metadata: Value,
    data: BTreeMap<String, Entry>,
}
#[derive(Deserialize)]
struct Entry {
    country: EntryCountry,
    #[serde(default)]
    offices: Vec<Value>,
}
#[derive(Deserialize)]
struct EntryCountry {
    #[serde(rename = "country-iso")]
    iso: String,
    eng: Option<Name>,
}
#[derive(Deserialize)]
struct Name {
    name: String,
}

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn coord(v: &Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

fn flag(v: &Value, k: &str) -> Option<bool> {
    match v.get(k)? {
        Value::Number(n) => n.as_i64().map(|n| n == 1),
        Value::String(s) if s == "1" => Some(true),
        Value::String(s) if s == "0" => Some(false),
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Parses the offices list.
pub fn parse(body: &[u8]) -> Result<CanadaOffices> {
    let l: List = json(body)?;
    let mut offices = Vec::new();
    for e in l.data.into_values() {
        let list_country = e.country.eng.map(|n| n.name).unwrap_or_default();
        for o in e.offices {
            let Some(eng) = o.get("eng") else { continue };
            let Some(office_type) = text(eng, "type") else {
                continue;
            };
            let notes = (1..=3)
                .filter_map(|i| {
                    let t = text(eng, &format!("note-{i}-title")).unwrap_or_default();
                    let x = text(eng, &format!("note-{i}-text"))?;
                    Some((t, x))
                })
                .collect();
            offices.push(CanadaOffice {
                list_iso: e.country.iso.trim().to_string(),
                list_country: list_country.clone(),
                office_type,
                city: text(eng, "city"),
                address: text(eng, "address"),
                lat: coord(&o, "lat").filter(|x| (-90.0..=90.0).contains(x)),
                lng: coord(&o, "lng").filter(|x| (-180.0..=180.0).contains(x)),
                tel: text(eng, "tel-legacy"),
                emergency: text(eng, "emergency-toll-free-legacy"),
                fax: text(eng, "fax-legacy"),
                emails: (1..=3)
                    .filter_map(|i| text(eng, &format!("email-{i}")))
                    .filter(|m| m.contains('@') && !m.contains(char::is_whitespace))
                    .collect(),
                website: text(eng, "internet"),
                notes,
                passport_services: flag(&o, "has-passport-services"),
                honorary: flag(&o, "honorary-consul").unwrap_or(false),
            });
        }
    }
    if offices.is_empty() {
        return Err(Error::Invalid("canada offices: no offices".into()));
    }
    Ok(CanadaOffices {
        generated: l
            .metadata
            .pointer("/generated/date")
            .and_then(Value::as_str)
            .map(str::to_string),
        offices,
    })
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanadaOfficesAdapter;

#[async_trait]
impl SourceAdapter for CanadaOfficesAdapter {
    fn id(&self) -> &'static str {
        "canada_offices"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            conditional: true,
            ..Fetch::default()
        };
        let url = ctx.url.url.clone();
        let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
        if !not_modified(&ex) {
            check_status(&ex)?;
            parse(&ex.body)?;
        }
        rep.complete = true;
        Ok(rep)
    }
}
