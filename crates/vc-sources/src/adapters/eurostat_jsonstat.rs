//! `eurostat_jsonstat`: one Eurostat dissemination API dataset in JSON-stat 2.0
//! (`/eurostat/api/dissemination/statistics/1.0/data/<dataset>?format=JSON&…`; CC BY 4.0,
//! "Source: Eurostat"). One request per run; the leased URL carries the dataset filters.
//!
//! [`parse`] flattens the cube into cells (one coordinate per dimension, a value and its status
//! flag). The record mapping ([`super::records`]) keeps price level indices (`na_item`
//! `PLI_EU27_2020`) of countries for the spending categories Voyager uses.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json};

/// One dimension: its category codes in index order, with labels.
#[derive(Debug, Clone, PartialEq)]
pub struct Dimension {
    /// Dimension id (`geo`, `time`, …).
    pub id: String,
    /// Category codes, by position.
    pub codes: Vec<String>,
    /// Labels by code.
    pub labels: BTreeMap<String, String>,
}

/// One non-empty cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    /// Category code per dimension, in `Cube::dims` order.
    pub coords: Vec<String>,
    /// The value.
    pub value: f64,
    /// The status flag (`p`, `e`, `b`, …), if any.
    pub status: Option<String>,
}

/// A parsed dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct Cube {
    /// Dataset label.
    pub label: Option<String>,
    /// Last update (`updated`, ISO 8601).
    pub updated: Option<String>,
    /// Dimensions, in cube order.
    pub dims: Vec<Dimension>,
    /// Cells with a value.
    pub cells: Vec<Cell>,
}

impl Cube {
    /// Position of a dimension.
    pub fn dim(&self, id: &str) -> Option<usize> {
        self.dims.iter().position(|d| d.id == id)
    }

    /// The label of a category code of dimension `i`.
    pub fn label(&self, i: usize, code: &str) -> Option<&str> {
        self.dims.get(i)?.labels.get(code).map(String::as_str)
    }
}

fn dimension(id: &str, d: &Value, size: usize) -> Result<Dimension> {
    let cat = d
        .get("category")
        .ok_or_else(|| Error::Invalid(format!("jsonstat: dimension {id} has no category")))?;
    let mut codes = vec![String::new(); size];
    match cat.get("index") {
        Some(Value::Object(m)) => {
            for (code, pos) in m {
                let p = pos
                    .as_u64()
                    .and_then(|p| usize::try_from(p).ok())
                    .filter(|p| *p < size)
                    .ok_or_else(|| Error::Invalid(format!("jsonstat: bad index in {id}")))?;
                codes[p] = code.clone();
            }
        }
        Some(Value::Array(a)) if a.len() == size => {
            for (p, c) in a.iter().enumerate() {
                codes[p] = c.as_str().unwrap_or_default().to_string();
            }
        }
        // A single-category dimension may omit `index`.
        None if size == 1 => {
            if let Some(Value::Object(l)) = cat.get("label")
                && let Some(k) = l.keys().next()
            {
                codes[0] = k.clone();
            }
        }
        _ => return Err(Error::Invalid(format!("jsonstat: bad index in {id}"))),
    }
    if codes.iter().any(String::is_empty) {
        return Err(Error::Invalid(format!(
            "jsonstat: incomplete index in {id}"
        )));
    }
    let labels = cat
        .get("label")
        .and_then(Value::as_object)
        .map(|l| {
            l.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(Dimension {
        id: id.to_string(),
        codes,
        labels,
    })
}

/// Parses a JSON-stat 2.0 dataset (`value` and `status` as an object keyed by flat index, or
/// as an array).
pub fn parse(body: &[u8]) -> Result<Cube> {
    let doc: Value = json(body)?;
    let ids: Vec<String> = doc
        .get("id")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Invalid("jsonstat: no id".into()))?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let sizes: Vec<usize> = doc
        .get("size")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Invalid("jsonstat: no size".into()))?
        .iter()
        .filter_map(|v| v.as_u64().and_then(|n| usize::try_from(n).ok()))
        .collect();
    if ids.is_empty() || ids.len() != sizes.len() {
        return Err(Error::Invalid("jsonstat: id/size mismatch".into()));
    }
    let total = sizes
        .iter()
        .try_fold(1usize, |a, s| a.checked_mul(*s))
        .ok_or_else(|| Error::Invalid("jsonstat: cube too large".into()))?;
    let dims_v = doc
        .get("dimension")
        .ok_or_else(|| Error::Invalid("jsonstat: no dimension".into()))?;
    let dims = ids
        .iter()
        .zip(&sizes)
        .map(|(id, s)| {
            dims_v
                .get(id)
                .ok_or_else(|| Error::Invalid(format!("jsonstat: missing dimension {id}")))
                .and_then(|d| dimension(id, d, *s))
        })
        .collect::<Result<Vec<_>>>()?;

    let entries: Vec<(usize, f64)> = match doc.get("value") {
        Some(Value::Object(m)) => m
            .iter()
            .filter_map(|(k, v)| Some((k.parse::<usize>().ok()?, v.as_f64()?)))
            .collect(),
        Some(Value::Array(a)) => a
            .iter()
            .enumerate()
            .filter_map(|(i, v)| Some((i, v.as_f64()?)))
            .collect(),
        _ => return Err(Error::Invalid("jsonstat: no value".into())),
    };
    let status = |i: usize| -> Option<String> {
        let s = match doc.get("status")? {
            Value::Object(m) => m.get(&i.to_string())?,
            Value::Array(a) => a.get(i)?,
            _ => return None,
        };
        s.as_str().map(str::to_string).filter(|s| !s.is_empty())
    };
    let mut cells = Vec::with_capacity(entries.len());
    for (flat, value) in entries {
        if flat >= total || !value.is_finite() {
            continue;
        }
        let mut rest = flat;
        let mut coords = vec![String::new(); dims.len()];
        for (i, d) in dims.iter().enumerate().rev() {
            let s = sizes[i];
            coords[i] = d.codes[rest % s].clone();
            rest /= s;
        }
        cells.push(Cell {
            coords,
            value,
            status: status(flat),
        });
    }
    cells.sort_by(|a, b| a.coords.cmp(&b.coords));
    Ok(Cube {
        label: doc.get("label").and_then(Value::as_str).map(str::to_string),
        updated: doc
            .get("updated")
            .and_then(Value::as_str)
            .map(str::to_string),
        dims,
        cells,
    })
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct EurostatJsonStat;

#[async_trait]
impl SourceAdapter for EurostatJsonStat {
    fn id(&self) -> &'static str {
        "eurostat_jsonstat"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let url = ctx.url.url.clone();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
        check_status(&ex)?;
        let cube = parse(&ex.body)?;
        tracing::info!(
            adapter = "eurostat_jsonstat",
            cells = cube.cells.len(),
            "parsed"
        );
        rep.complete = true;
        Ok(rep)
    }
}
