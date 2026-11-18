//! Mapping-driven records for the generic open-data readers (`socrata`, `udata_resource`,
//! `wfs_geojson`): one table row (a JSON object, a GeoJSON feature or a CSV record) becomes at
//! most one record of a fixed kind. What differs per dataset is data, not code: a [`RowMapping`]
//! names, for each record field, the column (or JSON pointer) that holds it, the kind, the fixed
//! city/country when the rows carry none, and a small category table.
//!
//! **Adding a dataset:** write a [`RowMapping`] in the reader's mapping table (with the reader's
//! own part: Socrata path and query, data.gouv.fr dataset and file, WFS layer), record a
//! fixture, and point the source's `adapter` at the reader. Columns that are not named are never
//! read; readers that can choose their columns server-side (Socrata `$select`) ask only for the
//! named ones, so personal data in a register (owner names) is never even stored.
//!
//! Pure: no I/O, no logging of content.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::inspections::{coord, phone};

/// One row of a table, whatever its format.
pub trait Row {
    /// The value of a column (`name`) or JSON pointer (`/a/0/b`): trimmed, whitespace collapsed;
    /// `None` when missing, empty, or a placeholder (`-`, `null`, `None`, `n.a.`).
    fn get(&self, field: &str) -> Option<String>;
}

fn clean(s: &str) -> Option<String> {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let placeholder = matches!(
        s.to_ascii_lowercase().as_str(),
        "" | "-" | "null" | "none" | "n.a." | "n/a" | "no aplica" | "sense especificar"
    );
    (!placeholder).then_some(s)
}

impl Row for Value {
    fn get(&self, field: &str) -> Option<String> {
        if field.is_empty() {
            return None;
        }
        let v = if field.starts_with('/') {
            self.pointer(field)?
        } else {
            self.get(field)?
        };
        match v {
            Value::String(s) => clean(s),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
}

/// A CSV record with its header.
pub struct CsvRow<'a> {
    /// Column name → index.
    pub cols: &'a HashMap<String, usize>,
    /// The record.
    pub rec: &'a csv::StringRecord,
}

impl Row for CsvRow<'_> {
    fn get(&self, field: &str) -> Option<String> {
        self.cols
            .get(field)
            .and_then(|i| self.rec.get(*i))
            .and_then(clean)
    }
}

/// Parses a CSV body (header row first; batches of a dump repeat the header on top) with
/// `delimiter`, calling `f` for every data row. A BOM is ignored.
pub fn csv_rows(body: &[u8], delimiter: u8, mut f: impl FnMut(&CsvRow<'_>)) -> vc_core::Result<()> {
    let body = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    let mut r = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .has_headers(true)
        .from_reader(body);
    let cols: HashMap<String, usize> = r
        .headers()
        .map_err(|e| vc_core::Error::Invalid(format!("csv: {e}")))?
        .iter()
        .enumerate()
        .map(|(i, h)| (h.trim().to_string(), i))
        .collect();
    for rec in r.records() {
        let rec = rec.map_err(|e| vc_core::Error::Invalid(format!("csv: {e}")))?;
        f(&CsvRow {
            cols: &cols,
            rec: &rec,
        });
    }
    Ok(())
}

/// The record kind of a mapping, with what is fixed per dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `restaurant`: an official food-hygiene listing.
    Restaurant {
        /// `hygiene.scheme`.
        scheme: &'static str,
        /// `hygiene.authority`.
        authority: &'static str,
        /// The published results kept (exact, case-insensitive); empty = any.
        ratings: &'static [&'static str],
    },
    /// `hotel`: an official accommodation register.
    Hotel,
    /// `attraction`.
    Attraction,
    /// `amenity`: public toilets, drinking water, free Wi-Fi.
    Amenity,
}

impl Kind {
    /// The kind id (`kinds.toml`).
    pub fn id(&self) -> &'static str {
        match self {
            Kind::Restaurant { .. } => "restaurant",
            Kind::Hotel => "hotel",
            Kind::Attraction => "attraction",
            Kind::Amenity => "amenity",
        }
    }
}

/// Columns (or JSON pointers) for each record field; `""` / `&[]` = not provided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    /// Stable id: the values of these columns joined with `:` (all must be present).
    pub id: &'static [&'static str],
    /// Name.
    pub name: &'static str,
    /// When the row has no name: `(label, column)` → `"<label>, <value>"`.
    pub name_fallback: Option<(&'static str, &'static str)>,
    /// Address parts, joined with a space (e.g. house number, street).
    pub address: &'static [&'static str],
    /// Postcode (appended to the address).
    pub postcode: &'static str,
    /// City (else the mapping's fixed city).
    pub city: &'static str,
    /// Borough, district or region.
    pub district: &'static str,
    /// Latitude.
    pub lat: &'static str,
    /// Longitude.
    pub lng: &'static str,
    /// Phone as written.
    pub phone: &'static str,
    /// Website or information page.
    pub url: &'static str,
    /// The register's own category (these columns joined with ` / `), matched against the
    /// mapping's category table.
    pub category: &'static [&'static str],
    /// Restaurants: the published rating or result.
    pub rating: &'static str,
    /// Restaurants: the rating's date; other kinds: `as_of`.
    pub date: &'static str,
    /// Hotels: the official classification (`"3 étoiles"`, `"4 estrelles"`).
    pub stars: &'static str,
    /// Opening hours as written.
    pub hours: &'static str,
    /// Free text (description, remarks).
    pub description: &'static str,
    /// Amenities: the operator.
    pub operator: &'static str,
    /// A column whose value says the place is wheelchair accessible (see
    /// [`RowMapping::accessible`]).
    pub accessible: &'static str,
}

impl Fields {
    /// No fields.
    pub const NONE: Fields = Fields {
        id: &[],
        name: "",
        name_fallback: None,
        address: &[],
        postcode: "",
        city: "",
        district: "",
        lat: "",
        lng: "",
        phone: "",
        url: "",
        category: &[],
        rating: "",
        date: "",
        stars: "",
        hours: "",
        description: "",
        operator: "",
        accessible: "",
    };
}

/// One dataset's row mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowMapping {
    /// The source id it was written for (record-id prefix, docs and tests).
    pub source: &'static str,
    /// Record kind.
    pub kind: Kind,
    /// Field columns.
    pub fields: Fields,
    /// City when rows carry none (English).
    pub city: &'static str,
    /// Country (English).
    pub country: &'static str,
    /// ISO 3166-1 alpha-2.
    pub country_code: &'static str,
    /// Category table: `(needle, value)`; the first needle the lower-cased category contains
    /// wins, `""` matches anything (a default). No match → the row is dropped. The value is the
    /// kind's enum (`establishment_type`, `property_type`, attraction `category`,
    /// `amenity_type`). Without a category column, the `""` entry is used.
    pub categories: &'static [(&'static str, &'static str)],
    /// Needles in [`Fields::accessible`] meaning "wheelchair accessible" (case-insensitive,
    /// substring). With an `accessible` column and a value, `wheelchair_accessible` is set
    /// either way.
    pub accessible: &'static [&'static str],
}

/// The `YYYY-MM-DD` of a date written `YYYY-MM-DD…`, `DD/MM/YYYY` or `YYYYMMDD…`, from 2000 on.
pub fn date(v: &str) -> Option<String> {
    let v = v.trim();
    let v = v.get(..v.len().min(10))?;
    if !v.is_ascii() || v.len() < 8 {
        return None;
    }
    let d = if v.len() >= 10 && v.as_bytes()[2] == b'/' && v.as_bytes()[5] == b'/' {
        format!("{}-{}-{}", &v[6..10], &v[3..5], &v[0..2])
    } else if v.len() >= 10 && v.as_bytes()[4] == b'-' {
        v.to_string()
    } else if v.len() >= 8 && v.as_bytes()[..8].iter().all(u8::is_ascii_digit) {
        format!("{}-{}-{}", &v[0..4], &v[4..6], &v[6..8])
    } else {
        return None;
    };
    let b = d.as_bytes();
    let ok = d.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && d.bytes().filter(u8::is_ascii_digit).count() == 8;
    let (m, dd): (u32, u32) = (d[5..7].parse().ok()?, d[8..10].parse().ok()?);
    (ok && d.as_str() >= "2000-01-01" && (1..=12).contains(&m) && (1..=31).contains(&dd))
        .then_some(d)
}

/// An official star classification: the first digit 1–5 of a value that names stars
/// (`étoile`, `estrell`, `star`, `stern`); keys, ears and other symbols are not stars.
pub fn stars(v: &str) -> Option<f64> {
    let l = v.to_lowercase();
    if !["étoile", "etoile", "estrell", "star", "stern"]
        .iter()
        .any(|w| l.contains(w))
    {
        return None;
    }
    let d = l.chars().find(char::is_ascii_digit)?.to_digit(10)?;
    (1..=5).contains(&d).then_some(f64::from(d))
}

fn http(v: &str) -> Option<String> {
    let v = v.trim();
    let v = if v.starts_with("www.") {
        format!("https://{v}")
    } else {
        v.to_string()
    };
    ((v.starts_with("https://") || v.starts_with("http://"))
        && !v.contains(char::is_whitespace)
        && v.len() <= 2048)
        .then_some(v)
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Strips HTML line breaks and tags from register text.
fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The register's own category label of a row (its category columns joined with ` / `).
pub fn category_label(m: &RowMapping, row: &dyn Row) -> Option<String> {
    let parts: Vec<String> = m
        .fields
        .category
        .iter()
        .filter_map(|c| row.get(c))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" / "))
}

/// The category value of a row, per the mapping's table.
pub fn category(m: &RowMapping, row: &dyn Row) -> Option<&'static str> {
    let label = category_label(m, row).unwrap_or_default().to_lowercase();
    m.categories
        .iter()
        .find(|(needle, _)| label.contains(needle))
        .map(|(_, v)| *v)
}

/// The rating date of a row (restaurants), or its `as_of`.
pub fn row_date(m: &RowMapping, row: &dyn Row) -> Option<String> {
    row.get(m.fields.date).as_deref().and_then(date)
}

/// The record id of a row: `<source>:<id columns joined with ':'>`.
pub fn row_id(m: &RowMapping, row: &dyn Row) -> Option<String> {
    if m.fields.id.is_empty() {
        return None;
    }
    let parts: Option<Vec<String>> = m
        .fields
        .id
        .iter()
        .map(|c| row.get(c).map(|v| v.to_lowercase()))
        .collect();
    Some(format!("{}:{}", m.source, parts?.join(":")))
}

/// The record (`(id, kind, data)`) of one row, or `None` when the row lacks a required field,
/// its category is not in the table, or (restaurants) its result is not one that is kept.
pub fn map_row(m: &RowMapping, row: &dyn Row) -> Option<(Option<String>, &'static str, Value)> {
    let f = &m.fields;
    let get = |c: &str| row.get(c);
    let name = get(f.name)
        .or_else(|| {
            let (label, col) = f.name_fallback?;
            Some(format!("{label}, {}", get(col)?))
        })
        .map(|n| plain(&n))
        .filter(|n| !n.is_empty())?;
    let cat = category(m, row)?;
    let street: Vec<String> = f.address.iter().filter_map(|c| get(c)).collect();
    let street = street.join(" ");
    let city = get(f.city).unwrap_or_else(|| m.city.to_string());
    if city.is_empty() {
        return None;
    }
    let address = (!street.is_empty()).then(|| match get(f.postcode) {
        Some(pc) if !street.contains(&pc) => format!("{street}, {pc} {city}"),
        _ => street.clone(),
    });
    let date = row_date(m, row);
    let (lat, lng) = (
        coord(row_value(row, f.lat).as_ref(), 90.0),
        coord(row_value(row, f.lng).as_ref(), 180.0),
    );
    let mut o = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            o.insert(k.to_string(), v);
        }
    };
    put("name", Some(json!(cut(&name, 160))));
    match m.kind {
        Kind::Restaurant { .. } => put("establishment_type", Some(json!(cat))),
        Kind::Hotel => put("property_type", Some(json!(cat))),
        Kind::Attraction => put("category", Some(json!(cat))),
        Kind::Amenity => put("amenity_type", Some(json!(cat))),
    }
    if let Kind::Restaurant { .. } = m.kind {
        put(
            "category_label",
            category_label(m, row).map(|c| json!(cut(&c, 120))),
        );
    }
    if let Kind::Hotel = m.kind {
        put(
            "star_rating",
            get(f.stars).as_deref().and_then(stars).map(|s| json!(s)),
        );
    }
    let desc = get(f.description)
        .map(|d| cut(&plain(&d), 600))
        .filter(|d| !d.is_empty());
    match m.kind {
        Kind::Restaurant { .. } => {}
        _ => put("description", desc.map(Value::String)),
    }
    put("address", address.map(|a| json!(cut(&a, 240))));
    let district = get(f.district).map(|d| json!(cut(&d, 120)));
    match m.kind {
        Kind::Restaurant { .. } | Kind::Amenity => put("district", district),
        Kind::Hotel | Kind::Attraction => put("neighbourhood", district),
    }
    put("city", Some(json!(cut(&city, 120))));
    put("country", Some(json!(m.country)));
    put("country_code", Some(json!(m.country_code)));
    if let (Some(a), Some(b)) = (lat, lng) {
        put("lat", Some(json!(a)));
        put("lng", Some(json!(b)));
    }
    put(
        "phone",
        phone(row_value(row, f.phone).as_ref()).map(Value::String),
    );
    if let Kind::Attraction | Kind::Amenity = m.kind {
        put(
            "opening_hours",
            get(f.hours).map(|h| json!(cut(&plain(&h), 240))),
        );
    }
    let accessible = get(f.accessible).is_some_and(|v| {
        let v = v.to_lowercase();
        m.accessible.iter().any(|a| v.contains(&a.to_lowercase()))
    });
    if let Kind::Attraction | Kind::Amenity = m.kind
        && !f.accessible.is_empty()
        && get(f.accessible).is_some()
    {
        put("wheelchair_accessible", Some(json!(accessible)));
    }
    if let Kind::Amenity = m.kind {
        put("operator", get(f.operator).map(|v| json!(cut(&v, 120))));
    }
    let url = get(f.url).as_deref().and_then(http);
    match m.kind {
        Kind::Hotel => put("website_url", url.map(Value::String)),
        Kind::Attraction | Kind::Amenity => put("info_url", url.map(Value::String)),
        Kind::Restaurant { .. } => {}
    }
    if let Kind::Restaurant {
        scheme,
        authority,
        ratings,
    } = m.kind
    {
        let rating = get(f.rating)?;
        // Scores published as decimals ("94.000000") read as the register shows them ("94").
        let rating = match rating.parse::<f64>() {
            Ok(x) if x.is_finite() && x.fract() == 0.0 && x.abs() < 1e6 => format!("{x:.0}"),
            _ => rating,
        };
        if !ratings.is_empty() && !ratings.iter().any(|r| r.eq_ignore_ascii_case(&rating)) {
            return None;
        }
        let d = date.clone()?;
        let mut h = Map::new();
        h.insert("scheme".into(), json!(scheme));
        h.insert("rating".into(), json!(cut(&rating, 40)));
        h.insert("rating_date".into(), json!(d));
        h.insert("authority".into(), json!(authority));
        put("hygiene", Some(Value::Object(h)));
    }
    put("as_of", date.map(Value::String));
    Some((row_id(m, row), m.kind.id(), Value::Object(o)))
}

/// A row value as JSON (numbers stay numbers for [`coord`]).
fn row_value(row: &dyn Row, field: &str) -> Option<Value> {
    row.get(field).map(Value::String)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_stars() {
        assert_eq!(date("19/06/2023").as_deref(), Some("2023-06-19"));
        assert_eq!(
            date("2026-02-27T00:00:00.000").as_deref(),
            Some("2026-02-27")
        );
        assert_eq!(date("202511061639").as_deref(), Some("2025-11-06"));
        assert_eq!(date("1901-01-01"), None);
        assert_eq!(date("13/45/2023"), None);
        assert_eq!(stars("3 étoiles"), Some(3.0));
        assert_eq!(stars("4 estrelles superior"), Some(4.0));
        assert_eq!(stars("5 claus"), None, "keys are not stars");
        assert_eq!(stars("Sense categoritzar"), None);
    }

    #[test]
    fn json_and_csv_rows() {
        let v = json!({"a": " x  y ", "n": 3, "g": {"c": [1.5, 2.5]}, "d": "-"});
        assert_eq!(Row::get(&v, "a").as_deref(), Some("x y"));
        assert_eq!(Row::get(&v, "n").as_deref(), Some("3"));
        assert_eq!(Row::get(&v, "/g/c/1").as_deref(), Some("2.5"));
        assert_eq!(Row::get(&v, "d"), None, "placeholder");
        let mut seen = Vec::new();
        csv_rows(b"\xef\xbb\xbfA;B\n1;\"x;y\"\n", b';', |r| {
            seen.push((r.get("A"), r.get("B")))
        })
        .unwrap();
        assert_eq!(seen, vec![(Some("1".into()), Some("x;y".into()))]);
    }
}
