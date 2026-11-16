//! CSV: rows as JSON objects keyed by the header row.

use serde_json::{Map, Value};
use vc_core::media::{MediaType, MediaTypeMatch};
use vc_core::parse::{ContentHandler, Limits, Parsed};
use vc_core::store::CaptureMeta;
use vc_core::{Error, Result};

use crate::text::{Deadline, decode, maybe_gunzip};

/// Max rows kept from one CSV capture.
pub const MAX_CSV_ROWS: usize = 50_000;

/// The CSV handler: the first row is the header; each further row becomes an object of its
/// non-empty columns (short rows are allowed, extra cells are dropped). At most
/// [`MAX_CSV_ROWS`] rows.
#[derive(Debug, Default, Clone, Copy)]
pub struct CsvHandler;

impl ContentHandler for CsvHandler {
    fn media(&self) -> &'static [MediaTypeMatch] {
        &[MediaTypeMatch::Is(MediaType::Csv)]
    }

    fn handle(&self, _meta: &CaptureMeta, body: &[u8], lim: &Limits) -> Result<Parsed> {
        let deadline = Deadline::new(lim.cpu_time);
        let body = maybe_gunzip(body)?;
        let text = decode(&body, None);
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .has_headers(true)
            .from_reader(text.as_bytes());
        let headers: Vec<String> = rdr
            .headers()
            .map_err(|e| Error::Invalid(format!("csv header: {e}")))?
            .iter()
            .map(|h| h.trim().to_string())
            .collect();
        let mut structured = Vec::new();
        for (i, rec) in rdr.records().enumerate() {
            if structured.len() >= MAX_CSV_ROWS {
                break;
            }
            if i % 4096 == 0 {
                deadline.check("csv")?;
            }
            let Ok(rec) = rec else { continue };
            let mut row = Map::new();
            for (h, cell) in headers.iter().zip(rec.iter()) {
                if !h.is_empty() && !cell.is_empty() {
                    row.insert(h.clone(), Value::String(cell.to_string()));
                }
            }
            if !row.is_empty() {
                structured.push(Value::Object(row));
            }
        }
        Ok(Parsed {
            structured,
            ..Parsed::default()
        })
    }
}
