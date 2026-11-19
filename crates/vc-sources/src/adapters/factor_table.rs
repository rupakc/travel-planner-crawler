//! `factor_table`: published emission-factor and grid-intensity tables (CSV or XLSX) for the
//! trip carbon footprint (2026-11-19), driven by a [`FactorMapping`] per dataset.
//!
//! One leased row = one file: a single request (conditional on the stored validators, capped
//! at the mapping's `max_bytes`), spooled raw as a WARC `response` (raw first). Nothing is
//! derived at fetch time beyond a parse check; the processor calls [`items`] on the stored
//! body and every mapped row becomes one `emission_factor` record or one
//! `grid_carbon_intensity` observation. No LLM, no unit conversion: values and units are
//! copied as published.
//!
//! **Adding a dataset:** what differs per dataset is data, not code. Add a mapping (how to
//! recognise the file's URL, format and sheet, the header's required columns, row filters,
//! the columns that make the activity label, the value and year columns, the unit, and the
//! category / gas-basis tables), record a small fixture excerpt, and point the source's
//! `adapter` at `factor_table` with the file URL as its seed.

use std::collections::HashMap;
use std::io::Cursor;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, fetch_raw, not_modified};
use super::{countries, iso3};

/// Adapter id.
pub const ID: &str = "factor_table";

/// Bumped when a mapping's output changes.
pub const FILTER_VERSION: &str = "factor-table-v1";

/// File format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Comma-separated values with a header row.
    Csv,
    /// An Excel workbook: the named sheet; the header is the first row that holds every
    /// required column.
    Xlsx {
        /// Sheet name.
        sheet: &'static str,
    },
}

/// How a column is found in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    /// Not provided.
    None,
    /// Exactly this header.
    Name(&'static str),
    /// The first header starting with this text (e.g. `"GHG Conversion Factor "` + year).
    Prefix(&'static str),
}

/// A row filter: keep the row only when the column's value is one of `values` (exact,
/// case-sensitive, after trimming).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keep {
    /// Column.
    pub column: &'static str,
    /// Accepted values.
    pub values: &'static [&'static str],
}

/// Where the year comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Year {
    /// A column holding the year.
    Column(&'static str),
    /// The four digits at the end of the value column's header (DESNZ
    /// `"GHG Conversion Factor 2026"`).
    ValueHeaderSuffix,
}

/// Where the geography comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Geo {
    /// Always this geography (`"global"`, `"United Kingdom"`), with its code if one country.
    Fixed(&'static str, &'static str),
    /// From an English country-name column for rows whose category is one of `when`, else the
    /// fixed fallback.
    CountryColumn {
        /// Column holding the country name.
        column: &'static str,
        /// Categories the column applies to.
        when: &'static [&'static str],
        /// Fallback geography name and code.
        fallback: (&'static str, &'static str),
    },
    /// An ISO 3166-1 alpha-3 column (`code`), with the name column; rows without a valid
    /// country code (aggregates, regions) are skipped.
    Iso3 {
        /// Alpha-3 column.
        code: &'static str,
        /// Name column.
        name: &'static str,
    },
}

/// What a mapped row becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// `emission_factor` records.
    Factor {
        /// Columns joined with `" / "` into the activity label (empty cells skipped).
        activity: &'static [&'static str],
        /// `(substring of "<category columns>", category)`: the first match wins; no match →
        /// the row is skipped.
        categories: &'static [(&'static str, &'static str)],
        /// Columns joined with `" / "` that the category table is matched against.
        category_from: &'static [&'static str],
        /// `(substring of the activity label, gas basis)`: first match wins, else `default`.
        gas_basis: &'static [(&'static str, &'static str)],
        /// Gas basis when no rule matches.
        gas_default: &'static str,
        /// Unit numerator (`"kg CO2e"`) as a fixed text, or from this column when non-empty.
        unit_mass: (&'static str, &'static str),
        /// Unit denominator as a fixed text, or from this column when non-empty.
        unit_per: (&'static str, &'static str),
        /// Scope column (or `""`).
        scope: &'static str,
        /// The publisher's row id column (or `""`).
        factor_id: &'static str,
        /// Geography.
        geo: Geo,
    },
    /// `grid_carbon_intensity` observations (annual, life-cycle unless stated).
    GridIntensity {
        /// Geography (an [`Geo::Iso3`] column pair).
        geo: Geo,
        /// `lifecycle` or `direct`.
        basis: &'static str,
    },
}

/// One dataset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FactorMapping {
    /// Short id of the dataset (record-id prefix).
    pub dataset: &'static str,
    /// The dataset's name and edition, as stored in `dataset`.
    pub dataset_label: &'static str,
    /// Publisher.
    pub publisher: &'static str,
    /// The file row's URL path must contain every one of these.
    pub path_contains: &'static [&'static str],
    /// Format.
    pub format: Format,
    /// Columns the header must have (else the file is refused, nothing mapped).
    pub required: &'static [&'static str],
    /// Row filters (all must accept).
    pub keep: &'static [Keep],
    /// The value column.
    pub value: Col,
    /// The year.
    pub year: Year,
    /// Body cap in bytes.
    pub max_bytes: u64,
    /// Row → record.
    pub output: Output,
}

const DESNZ_CATEGORIES: &[(&str, &str)] = &[
    ("Business travel- air / Flights", "flights"),
    ("Business travel- sea / Ferry", "ferry"),
    ("Business travel- land / Rail", "rail"),
    ("Business travel- land / Bus", "bus"),
    ("Business travel- land / Taxis", "taxi"),
    ("Business travel- land / Motorbike", "motorbike"),
    ("Business travel- land / Cars", "car"),
    ("Hotel stay / Hotel stay", "accommodation"),
    ("UK electricity / Electricity generated", "electricity"),
];

/// Every built-in mapping.
pub const MAPPINGS: &[FactorMapping] = &[
    // UK DESNZ GHG conversion factors, flat file (yearly in June, OGL v3): flights by haul and
    // cabin (with and without RF), ferry, rail, bus, taxi, motorbike, cars, hotel stays per
    // country, UK grid electricity. Only kg CO2e rows (not the per-gas splits).
    FactorMapping {
        dataset: "desnz",
        dataset_label: "UK GHG conversion factors for company reporting, flat file",
        publisher: "UK Department for Energy Security and Net Zero (DESNZ)",
        path_contains: &["ghg-conversion-factors-", "flat-format"],
        format: Format::Xlsx {
            sheet: "Factors by Category",
        },
        required: &[
            "ID",
            "Scope",
            "Level 1",
            "Level 2",
            "Level 3",
            "Level 4",
            "Column Text",
            "UOM",
            "GHG/Unit",
        ],
        keep: &[
            Keep {
                column: "Level 1",
                values: &[
                    "Business travel- air",
                    "Business travel- sea",
                    "Business travel- land",
                    "Hotel stay",
                    "UK electricity",
                ],
            },
            Keep {
                column: "GHG/Unit",
                values: &["kg CO2e"],
            },
        ],
        value: Col::Prefix("GHG Conversion Factor "),
        year: Year::ValueHeaderSuffix,
        max_bytes: 8 * 1024 * 1024,
        output: Output::Factor {
            activity: &["Level 2", "Level 3", "Level 4", "Column Text"],
            categories: DESNZ_CATEGORIES,
            category_from: &["Level 1", "Level 2"],
            gas_basis: &[
                ("With RF", "co2e_with_rf"),
                ("Without RF", "co2e_without_rf"),
            ],
            gas_default: "co2e",
            unit_mass: ("kg CO2e", "GHG/Unit"),
            unit_per: ("", "UOM"),
            scope: "Scope",
            factor_id: "ID",
            geo: Geo::CountryColumn {
                column: "Level 3",
                when: &["accommodation"],
                fallback: ("United Kingdom", "GB"),
            },
        },
    },
    // Our World in Data grapher: GHG emissions per kg of food product (Poore & Nemecek 2018,
    // global means, farm to retail), CC BY 4.0.
    FactorMapping {
        dataset: "owid-food-poore",
        dataset_label: "Our World in Data: greenhouse gas emissions per kilogram of food product (Poore & Nemecek 2018)",
        publisher: "Our World in Data, based on Poore & Nemecek (2018)",
        path_contains: &["/grapher/ghg-per-kg-poore.csv"],
        format: Format::Csv,
        required: &[
            "entity",
            "year",
            "ghg_emissions_per_kilogram__poore__and__nemecek__2018",
        ],
        keep: &[],
        value: Col::Name("ghg_emissions_per_kilogram__poore__and__nemecek__2018"),
        year: Year::Column("year"),
        max_bytes: 1024 * 1024,
        output: Output::Factor {
            activity: &["entity"],
            categories: &[("", "food")],
            category_from: &[],
            gas_basis: &[],
            gas_default: "co2e",
            unit_mass: ("kg CO2e", ""),
            unit_per: ("kg", ""),
            scope: "",
            factor_id: "",
            geo: Geo::Fixed("global", ""),
        },
    },
    // Our World in Data grapher: life-cycle carbon intensity of electricity per country and
    // year (Ember and Energy Institute), CC BY 4.0.
    FactorMapping {
        dataset: "owid-grid-ci",
        dataset_label: "Our World in Data: carbon intensity of electricity generation",
        publisher: "Our World in Data, based on Ember and Energy Institute",
        path_contains: &["/grapher/electricity-mix.csv"],
        format: Format::Csv,
        required: &["entity", "code", "year", "co2_intensity__gco2_kwh"],
        keep: &[],
        value: Col::Name("co2_intensity__gco2_kwh"),
        year: Year::Column("year"),
        max_bytes: 4 * 1024 * 1024,
        output: Output::GridIntensity {
            geo: Geo::Iso3 {
                code: "code",
                name: "entity",
            },
            basis: "lifecycle",
        },
    },
];

/// The mapping of a file row (by URL path).
pub fn mapping_for(url: &CanonicalUrl) -> Option<&'static FactorMapping> {
    let p = url.as_url().path();
    MAPPINGS
        .iter()
        .find(|m| m.path_contains.iter().all(|s| p.contains(s)))
}

/// A table: header and rows of trimmed cell texts.
#[derive(Debug, Default)]
pub struct Table {
    /// Header cells.
    pub header: Vec<String>,
    /// Data rows.
    pub rows: Vec<Vec<String>>,
}

fn cell_text(c: &calamine::Data) -> String {
    use calamine::Data;
    match c {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        other => other.to_string().trim().to_string(),
    }
}

/// Reads the mapping's table from a stored body (CSV or XLSX). The header must hold every
/// required column.
pub fn read_table(m: &FactorMapping, body: &[u8]) -> Result<Table> {
    let has_required = |cells: &[String]| m.required.iter().all(|r| cells.iter().any(|c| c == r));
    match m.format {
        Format::Csv => {
            let body = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
            let mut r = csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .from_reader(body);
            let mut t = Table::default();
            for rec in r.records() {
                let rec = rec.map_err(|e| Error::Invalid(format!("{ID}: csv: {e}")))?;
                let cells: Vec<String> = rec.iter().map(|c| c.trim().to_string()).collect();
                if t.header.is_empty() {
                    if !has_required(&cells) {
                        return Err(Error::Invalid(format!(
                            "{ID}: {}: missing required columns",
                            m.dataset
                        )));
                    }
                    t.header = cells;
                } else {
                    t.rows.push(cells);
                }
            }
            Ok(t)
        }
        Format::Xlsx { sheet } => {
            use calamine::Reader;
            let mut wb = calamine::Xlsx::new(Cursor::new(body))
                .map_err(|e| Error::Invalid(format!("{ID}: xlsx: {e}")))?;
            let range = wb
                .worksheet_range(sheet)
                .map_err(|e| Error::Invalid(format!("{ID}: xlsx sheet {sheet:?}: {e}")))?;
            let mut t = Table::default();
            for row in range.rows() {
                let cells: Vec<String> = row.iter().map(cell_text).collect();
                if t.header.is_empty() {
                    if has_required(&cells) {
                        t.header = cells;
                    }
                } else if cells.iter().any(|c| !c.is_empty()) {
                    t.rows.push(cells);
                }
            }
            if t.header.is_empty() {
                return Err(Error::Invalid(format!(
                    "{ID}: {}: no header row with the required columns",
                    m.dataset
                )));
            }
            Ok(t)
        }
    }
}

/// A snake_case slug (ASCII letters and digits, `_` between words).
pub fn slug(label: &str) -> String {
    let folded = countries::fold(label);
    let mut out = String::new();
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').chars().take(160).collect()
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

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The records (`(id, kind, data)`) of a stored file.
pub fn items(
    url: &CanonicalUrl,
    body: &[u8],
) -> Result<Vec<(Option<String>, &'static str, Value)>> {
    let m = mapping_for(url)
        .ok_or_else(|| Error::Invalid(format!("{ID}: no mapping for this file")))?;
    let t = read_table(m, body)?;
    let cols: HashMap<&str, usize> = t
        .header
        .iter()
        .enumerate()
        .rev()
        .map(|(i, h)| (h.as_str(), i))
        .collect();
    let idx = |c: &Col| -> Option<usize> {
        match c {
            Col::None => None,
            Col::Name(n) => cols.get(n).copied(),
            Col::Prefix(p) => t.header.iter().position(|h| h.starts_with(p)),
        }
    };
    let value_ix = idx(&m.value)
        .ok_or_else(|| Error::Invalid(format!("{ID}: {}: no value column", m.dataset)))?;
    let header_year = match m.year {
        Year::ValueHeaderSuffix => {
            let h = &t.header[value_ix];
            let y = h
                .get(h.len().saturating_sub(4)..)
                .and_then(|s| s.parse::<i64>().ok());
            Some(y.ok_or_else(|| {
                Error::Invalid(format!("{ID}: {}: no year in the value header", m.dataset))
            })?)
        }
        Year::Column(_) => None,
    };
    let get = |row: &[String], name: &str| -> String {
        if name.is_empty() {
            return String::new();
        }
        cols.get(name)
            .and_then(|i| row.get(*i))
            .cloned()
            .unwrap_or_default()
    };
    let join = |row: &[String], names: &[&str]| -> String {
        names
            .iter()
            .map(|n| get(row, n))
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
            .join(" / ")
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    'rows: for row in &t.rows {
        for k in m.keep {
            if !k.values.contains(&get(row, k.column).as_str()) {
                continue 'rows;
            }
        }
        let Some(value) = row
            .get(value_ix)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
        else {
            continue; // blank = no factor published (never 0)
        };
        let year = match (header_year, m.year) {
            (Some(y), _) => y,
            (None, Year::Column(c)) => match get(row, c).parse::<i64>() {
                Ok(y) => y,
                Err(_) => continue,
            },
            (None, Year::ValueHeaderSuffix) => continue,
        };
        match m.output {
            Output::Factor {
                activity,
                categories,
                category_from,
                gas_basis,
                gas_default,
                unit_mass,
                unit_per,
                scope,
                factor_id,
                geo,
            } => {
                let label = join(row, activity);
                if label.is_empty() {
                    continue;
                }
                let cat_key = join(row, category_from);
                let Some(category) = categories
                    .iter()
                    .find(|(k, _)| cat_key.contains(k))
                    .map(|(_, c)| *c)
                else {
                    continue;
                };
                let gas = gas_basis
                    .iter()
                    .find(|(k, _)| label.contains(k))
                    .map(|(_, g)| *g)
                    .unwrap_or(gas_default);
                let pick = |(fixed, col): (&str, &str)| {
                    let v = get(row, col);
                    if v.is_empty() { fixed.to_string() } else { v }
                };
                let unit = format!("{} per {}", pick(unit_mass), pick(unit_per));
                let (geography, code) = match geo {
                    Geo::Fixed(n, c) => (n.to_string(), c.to_string()),
                    Geo::CountryColumn {
                        column,
                        when,
                        fallback,
                    } if when.contains(&category) => {
                        let name = get(row, column);
                        if name.starts_with("UK") {
                            (fallback.0.to_string(), fallback.1.to_string())
                        } else {
                            let c = countries::iso2(&name).unwrap_or("");
                            (name, c.to_string())
                        }
                    }
                    Geo::CountryColumn { fallback, .. } => {
                        (fallback.0.to_string(), fallback.1.to_string())
                    }
                    Geo::Iso3 { .. } => continue,
                };
                let act = slug(&label);
                if act.is_empty() {
                    continue;
                }
                let fid = get(row, factor_id);
                let id = format!(
                    "{}:{year}:{}",
                    m.dataset,
                    if fid.is_empty() {
                        act.clone()
                    } else {
                        fid.clone()
                    }
                );
                if !seen.insert(id.clone()) {
                    continue;
                }
                let data = obj(vec![
                    ("activity", Some(json!(act))),
                    ("activity_label", Some(json!(clip(&label, 300)))),
                    ("category", Some(json!(category))),
                    ("geography", Some(json!(clip(&geography, 80)))),
                    ("country_code", (!code.is_empty()).then(|| json!(code))),
                    ("unit", Some(json!(clip(&unit, 80)))),
                    ("value", Some(json!(value))),
                    ("gas_basis", Some(json!(gas))),
                    ("scope", Some(json!(clip(&get(row, scope), 40)))),
                    ("year", Some(json!(year))),
                    ("publisher", Some(json!(m.publisher))),
                    (
                        "dataset",
                        Some(json!(clip(&format!("{} {year}", m.dataset_label), 160))),
                    ),
                    ("factor_id", Some(json!(clip(&fid, 80)))),
                ]);
                out.push((Some(id), "emission_factor", data));
            }
            Output::GridIntensity { geo, basis } => {
                let Geo::Iso3 { code, name } = geo else {
                    continue;
                };
                let Some(cc) = iso3::alpha2(&get(row, code)) else {
                    continue; // regions, aggregates
                };
                let country = countries::name(cc)
                    .map(str::to_string)
                    .unwrap_or_else(|| get(row, name));
                let data = obj(vec![
                    ("country", Some(json!(clip(&country, 80)))),
                    ("country_code", Some(json!(cc))),
                    ("period", Some(json!("annual"))),
                    ("year", Some(json!(year))),
                    ("date", Some(json!(format!("{year:04}-01-01")))),
                    ("intensity_g_per_kwh", Some(json!(value))),
                    ("basis", Some(json!(basis))),
                    ("publisher", Some(json!(m.publisher))),
                    ("dataset", Some(json!(m.dataset_label))),
                ]);
                out.push((
                    Some(format!("{}:{cc}:{year}", m.dataset)),
                    "grid_carbon_intensity",
                    data,
                ));
            }
        }
    }
    Ok(out)
}

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct FactorTable;

#[async_trait]
impl SourceAdapter for FactorTable {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        let m = mapping_for(&url)
            .ok_or_else(|| Error::Invalid(format!("{ID}: no mapping for this row")))?;
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            hint: Some(match m.format {
                Format::Csv => MediaType::Csv,
                Format::Xlsx { .. } => MediaType::Other,
            }),
            conditional: true,
            max_body_bytes: m.max_bytes,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
        if !not_modified(&ex) {
            check_status(&ex)?;
            let t = read_table(m, &ex.body)?;
            tracing::info!(
                adapter = ID,
                dataset = m.dataset,
                rows = t.rows.len(),
                "parsed"
            );
        }
        rep.complete = true;
        Ok(rep)
    }
}
