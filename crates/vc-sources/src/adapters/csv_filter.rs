//! A CSV dump filter by column predicate: the header row is kept (and repeated on top of each
//! output batch), data rows are kept verbatim when the predicate accepts them.

use std::collections::HashMap;

use vc_core::{Error, Result};

use super::dump::{DumpFilter, OutputKind, Split, Unit};

/// Column lookup for one row.
pub(crate) struct Row<'a> {
    cols: &'a HashMap<String, usize>,
    rec: &'a csv::StringRecord,
}

impl Row<'_> {
    /// The value of a column (empty when missing).
    pub(crate) fn get(&self, name: &str) -> &str {
        self.cols
            .get(name)
            .and_then(|i| self.rec.get(*i))
            .unwrap_or("")
    }
}

pub(crate) struct CsvFilter {
    version: &'static str,
    required: &'static [&'static str],
    keep: fn(&Row<'_>) -> bool,
    cols: Option<HashMap<String, usize>>,
}

impl CsvFilter {
    pub(crate) fn new(
        version: &'static str,
        required: &'static [&'static str],
        keep: fn(&Row<'_>) -> bool,
    ) -> Self {
        CsvFilter {
            version,
            required,
            keep,
            cols: None,
        }
    }
}

fn record(unit: &[u8]) -> Result<csv::StringRecord> {
    let mut r = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(unit);
    match r.records().next() {
        Some(Ok(rec)) => Ok(rec),
        Some(Err(e)) => Err(Error::Invalid(format!("csv: {e}"))),
        None => Ok(csv::StringRecord::new()),
    }
}

impl DumpFilter for CsvFilter {
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
        let rec = record(unit)?;
        match &self.cols {
            None => {
                let cols: HashMap<String, usize> = rec
                    .iter()
                    .enumerate()
                    .map(|(i, h)| (h.trim().to_string(), i))
                    .collect();
                for r in self.required {
                    if !cols.contains_key(*r) {
                        return Err(Error::Invalid(format!("csv: missing column {r}")));
                    }
                }
                self.cols = Some(cols);
                Ok(Unit::Header(unit.to_vec()))
            }
            Some(cols) => {
                let row = Row { cols, rec: &rec };
                Ok(if (self.keep)(&row) {
                    Unit::Keep(unit.to_vec())
                } else {
                    Unit::Drop
                })
            }
        }
    }
}
