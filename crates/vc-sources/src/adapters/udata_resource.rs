//! `udata_resource`: tabular files published through a udata catalogue (www.data.gouv.fr,
//! data.public.lu), driven by a [`UdataMapping`] per dataset.
//!
//! Two kinds of leased row:
//!
//! - **The dataset** (`/api/1/datasets/<slug>/`): one JSON request (spooled); the newest resource
//!   of the mapping's format whose URL path starts with the mapping's file path is the current
//!   file. It is enqueued as an `adapter:udata_resource` row when it is on one of the source's
//!   own sites (its seed hosts' politeness keys, as for redirects), so the frontier's per-key
//!   politeness and that host's robots.txt apply to it; anything else is left alone. The
//!   catalogue's `/datasets/r/<id>` redirect links are never used (www.data.gouv.fr's robots.txt
//!   disallows them), and neither are files on `static.data.gouv.fr/resources/` (disallowed by
//!   that host's robots.txt; the permit client would refuse them anyway).
//! - **The file** (a path under the mapping's file path): streamed by the dump runner (CSV
//!   records, any delimiter), stored verbatim in `resource` batches under its header, and mapped
//!   row by row with the mapping's [`RowMapping`].
//!
//! **Adding a dataset:** add a mapping (slug, file path, format, delimiter, required columns,
//! row mapping), record fixtures, and point the source's `adapter` at `udata_resource` (seeds:
//! the dataset API URL and/or the file URL).

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, PolitenessKey, Result};

use super::common::{Fetch, check_status, enqueue_adapter_rows, fetch_raw, json};
use super::dump::{DumpFilter, DumpSettings, OutputKind, Split, Unit, run_dump};
use super::row_map::{Fields, Kind, RowMapping, csv_rows, map_row};

/// Adapter id.
pub const ID: &str = "udata_resource";

/// The catalogue's dataset API path prefix.
pub const API: &str = "/api/1/datasets/";

/// One dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdataMapping {
    /// The dataset slug (`/api/1/datasets/<slug>/`).
    pub dataset: &'static str,
    /// The file's URL path prefix (identifies the file rows; the resource picked must match).
    pub file_path: &'static str,
    /// Resource format (`csv`).
    pub format: &'static str,
    /// CSV delimiter.
    pub delimiter: u8,
    /// Columns the header must have (else the file is refused, nothing mapped).
    pub required: &'static [&'static str],
    /// Filter version recorded in the dump manifest (bump on any change).
    pub filter_version: &'static str,
    /// Row → record.
    pub map: RowMapping,
}

/// Every built-in mapping.
pub const MAPPINGS: &[UdataMapping] = &[
    // Atout France: every classified tourist accommodation in France (hotels, campsites, tourist
    // residences, holiday villages, leisure parks, hostels), regenerated daily. Licence Ouverte.
    // No register id: the record id is typology + trade name + postcode.
    UdataMapping {
        dataset: "hebergements-touristiques-classes-en-france",
        file_path: "/static/exportHebergementsClasses/",
        format: "csv",
        delimiter: b';',
        required: &[
            "TYPOLOGIE ÉTABLISSEMENT",
            "NOM COMMERCIAL",
            "CODE POSTAL",
            "COMMUNE",
        ],
        filter_version: "atout-france-classes-v1",
        map: RowMapping {
            source: "atout-france",
            kind: Kind::Hotel,
            fields: Fields {
                id: &["TYPOLOGIE ÉTABLISSEMENT", "NOM COMMERCIAL", "CODE POSTAL"],
                name: "NOM COMMERCIAL",
                address: &["ADRESSE"],
                postcode: "CODE POSTAL",
                city: "COMMUNE",
                url: "SITE INTERNET",
                category: &["TYPOLOGIE ÉTABLISSEMENT"],
                stars: "CLASSEMENT",
                date: "DATE DE CLASSEMENT",
                ..Fields::NONE
            },
            city: "",
            country: "France",
            country_code: "FR",
            categories: &[
                ("hôtel", "hotel"),
                ("camping", "campsite"),
                ("résidence de tourisme", "apartment"),
                ("village de vacances", "resort"),
                ("auberge collective", "hostel"),
                ("parc résidentiel", "other"),
            ],
            accessible: &[],
        },
    },
];

/// The dataset slug of an API URL (`/api/1/datasets/<slug>/`).
pub fn slug(url: &CanonicalUrl) -> Option<&str> {
    let rest = url.as_url().path().strip_prefix(API)?;
    let s = rest.strip_suffix('/').unwrap_or(rest);
    (!s.is_empty() && !s.contains('/') && s != "r").then_some(s)
}

/// The mapping of a leased row: by dataset slug (API row) or file path (file row).
pub fn mapping_for(url: &CanonicalUrl) -> Option<&'static UdataMapping> {
    match slug(url) {
        Some(s) => MAPPINGS.iter().find(|m| m.dataset == s),
        None => {
            let p = url.as_url().path();
            MAPPINGS.iter().find(|m| p.starts_with(m.file_path))
        }
    }
}

/// The current file of a dataset API response: the newest (`last_modified`) resource of the
/// mapping's format whose URL is `https` and whose path starts with the mapping's file path.
pub fn pick_resource(m: &UdataMapping, body: &[u8]) -> Result<Option<CanonicalUrl>> {
    let v: Value = json(body)?;
    let mut best: Option<(String, CanonicalUrl)> = None;
    for r in v
        .get("resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let fmt = r.get("format").and_then(Value::as_str).unwrap_or("");
        if !fmt.eq_ignore_ascii_case(m.format) {
            continue;
        }
        let Some(u) = r
            .get("url")
            .and_then(Value::as_str)
            .and_then(|s| CanonicalUrl::parse(s).ok())
        else {
            continue;
        };
        if u.as_url().scheme() != "https" || !u.as_url().path().starts_with(m.file_path) {
            continue;
        }
        let modified = r
            .get("last_modified")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if best.as_ref().is_none_or(|(b, _)| modified > *b) {
            best = Some((modified, u));
        }
    }
    Ok(best.map(|(_, u)| u))
}

/// The records (`(id, kind, data)`) of a stored capture: none for the dataset API response; the
/// mapped rows of a file batch (CSV with its header on top).
pub fn items(
    url: &CanonicalUrl,
    body: &[u8],
) -> Result<Vec<(Option<String>, &'static str, Value)>> {
    if url.as_url().path().starts_with(API) {
        return Ok(Vec::new());
    }
    let m = mapping_for(url)
        .ok_or_else(|| Error::Invalid(format!("{ID}: no mapping for this file")))?;
    let mut out = Vec::new();
    csv_rows(body, m.delimiter, |r| {
        if let Some(rec) = map_row(&m.map, r) {
            out.push(rec);
        }
    })?;
    Ok(out)
}

/// A CSV dump filter that keeps every row under its header (any delimiter), checking the
/// required columns.
pub(crate) struct RowCsv {
    version: &'static str,
    delimiter: u8,
    required: &'static [&'static str],
    header: bool,
}

impl RowCsv {
    pub(crate) fn new(m: &UdataMapping) -> Self {
        RowCsv {
            version: m.filter_version,
            delimiter: m.delimiter,
            required: m.required,
            header: false,
        }
    }
}

impl DumpFilter for RowCsv {
    fn version(&self) -> &'static str {
        self.version
    }

    fn split(&self) -> Split {
        Split::Csv
    }

    fn output(&self) -> OutputKind {
        OutputKind::Csv
    }

    fn filter(&mut self, unit: &[u8]) -> Result<Unit> {
        let unit = unit.strip_prefix(b"\xef\xbb\xbf").unwrap_or(unit);
        if unit.iter().all(u8::is_ascii_whitespace) {
            return Ok(Unit::Drop);
        }
        if self.header {
            return Ok(Unit::Keep(unit.to_vec()));
        }
        let mut r = csv::ReaderBuilder::new()
            .delimiter(self.delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(unit);
        let rec = match r.records().next() {
            Some(Ok(rec)) => rec,
            Some(Err(e)) => return Err(Error::Invalid(format!("csv: {e}"))),
            None => return Ok(Unit::Drop),
        };
        let cols: HashMap<&str, usize> =
            rec.iter().enumerate().map(|(i, h)| (h.trim(), i)).collect();
        for c in self.required {
            if !cols.contains_key(c) {
                return Err(Error::Invalid(format!("csv: missing column {c}")));
            }
        }
        self.header = true;
        Ok(Unit::Header(unit.to_vec()))
    }
}

/// Whether the source owns `url`'s site (its seed, sitemap or feed hosts' politeness keys, or
/// the leased row's own key).
fn owned(ctx: &AdapterCtx<'_>, url: &CanonicalUrl) -> bool {
    let key = url.politeness_key();
    if key == *ctx.permit.key() {
        return true;
    }
    ctx.source
        .seeds
        .iter()
        .chain(&ctx.source.sitemaps)
        .chain(&ctx.source.feeds)
        .filter_map(|u| u.host_str())
        .any(|h| PolitenessKey::for_host(h) == key)
}

/// The adapter.
#[derive(Debug, Clone)]
pub struct UdataResource {
    settings: DumpSettings,
}

impl UdataResource {
    /// Default settings.
    pub const DEFAULT: UdataResource = UdataResource {
        settings: DumpSettings::DEFAULT,
    };

    /// With settings.
    pub fn new(settings: DumpSettings) -> Self {
        UdataResource { settings }
    }
}

#[async_trait]
impl SourceAdapter for UdataResource {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let row = ctx.url.url.clone();
        let m = mapping_for(&row)
            .ok_or_else(|| Error::Invalid(format!("{ID}: no mapping for this row")))?;
        if slug(&row).is_none() {
            let mut f = RowCsv::new(m);
            return run_dump(ctx, ID, row, &mut f, &self.settings).await;
        }
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, row.clone(), &opts, &mut rep).await?;
        check_status(&ex)?;
        match pick_resource(m, &ex.body)? {
            Some(file) if owned(ctx, &file) => {
                enqueue_adapter_rows(ctx, ID, vec![file], 0.6, &mut rep).await?;
            }
            Some(_) => tracing::info!(
                adapter = ID,
                dataset = m.dataset,
                "current file is not on the source's own sites: not followed"
            ),
            None => tracing::info!(adapter = ID, dataset = m.dataset, "no matching resource"),
        }
        rep.complete = true;
        Ok(rep)
    }
}
