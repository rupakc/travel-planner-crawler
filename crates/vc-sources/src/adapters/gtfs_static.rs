//! `gtfs_static`: a GTFS Schedule feed (a `.zip` of CSV tables, gtfs.org/schedule/reference),
//! downloaded as one permit-gated stream and read **as it arrives** ([`super::gtfs_zip`]):
//! only the traveller-facing tables are kept, the big ones (`stop_times`, `trips`, `shapes`,
//! `calendar_dates`) are skipped or inflated and discarded, never stored.
//!
//! Kept, per feed (a zip of zips, e.g. Victoria's one-folder-per-mode archive, is one feed per
//! inner zip):
//! - `agency.txt` → one `operator` item (name, url, fare url, phone, timezone, language, the
//!   modes and number of its routes, its fares);
//! - `routes.txt` → one `line` item per route (mode from `route_type`, short/long name, operator);
//! - `stops.txt` → one `station` item per station (`location_type = 1`, the parent stations of
//!   platforms; name, coordinates, step-free access); platforms and bus stops are not kept;
//! - `fare_attributes.txt` / `fare_products.txt` → fares (price, currency, transfers) on the
//!   operator they belong to;
//! - `feed_info.txt` → the validity window and publisher on every item.
//!
//! Malformed rows (CSV errors, missing ids or names, bad coordinates or route types) are
//! counted and skipped; a table whose CSV breaks (an unbalanced quote) is cut there and named
//! in the manifest. Items are written as JSON-array `resource` records **after** the whole
//! archive has been read and checked (a truncated or oversized download stores nothing but its
//! manifest), followed by a `dump_manifest` and a status capture of the leased URL that carries
//! the response's validators, so the next run is a conditional GET (an unchanged feed costs
//! one request and a 304).
//!
//! Size: a `Content-Length` over [`GtfsSettings::max_bytes`] is skipped before reading the
//! body (manifest `skipped: over_size_cap`); a body that grows past it is cut by the permit
//! client. Inflated bytes are capped too (zip bombs).
//!
//! Keys (sources off until the owner registers one) go where each provider wants them
//! ([`auth_for`]); query keys are stripped from every stored URL. Entur asks every client to
//! identify itself with `ET-Client-Name` ([`ENTUR_CLIENT_NAME`]); the value is fixed, never
//! rotated.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bytes::Bytes;
use chrono::Timelike;
use futures::StreamExt;
use http::{HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use vc_core::fetch::{FetchError, FetchRequest, RequestPurpose};
use vc_core::frontier::Validators;
use vc_core::ids::PayloadHash;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::store::{CaptureRole, SpoolPayload, SpoolRecord};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{append_manifest, append_resource, meta, redact_query};
use super::dump::{Split, Splitter};
use super::gtfs_zip::{EntrySink, ZipStream};
use crate::sha256::{Sha256, hex};

/// Filter version recorded in the manifest (bump on any change to what is kept).
pub const FILTER_VERSION: &str = "gtfs-static-v1";
/// The `ET-Client-Name` Entur asks clients to send (`<company>-<application>`).
pub const ENTUR_CLIENT_NAME: &str = "voyagerbot-crawler";

/// Run-time limits.
#[derive(Debug, Clone)]
pub struct GtfsSettings {
    /// Largest feed downloaded, bytes (`[sources] gtfs_max_bytes`).
    pub max_bytes: u64,
    /// Largest total of inflated bytes per feed (zip-bomb guard).
    pub max_inflated: u64,
    /// Extend the lease every this long while streaming.
    pub extend_every: Duration,
    /// UTC hour window `[start, end)` in which feeds are downloaded (the dump window).
    pub window_utc: (u8, u8),
    /// Target size of one `resource` record.
    pub batch_bytes: usize,
    /// Rows kept per table and feed (more are counted as capped).
    pub max_rows: usize,
}

impl Default for GtfsSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl GtfsSettings {
    /// The defaults (equal to `from_config(&SourcesConfig::default())`).
    pub const DEFAULT: GtfsSettings = GtfsSettings {
        max_bytes: 600 * 1024 * 1024,
        max_inflated: 16 * 1024 * 1024 * 1024,
        extend_every: Duration::from_secs(60),
        window_utc: (2, 5),
        batch_bytes: 2 * 1024 * 1024,
        max_rows: 200_000,
    };

    /// Settings from the `[sources]` section.
    pub fn from_config(c: &vc_config::SourcesConfig) -> Self {
        GtfsSettings {
            max_bytes: c.gtfs_max_bytes,
            extend_every: Duration::from_secs(c.dump_extend_every_s.max(1)),
            window_utc: c.dump_window_utc,
            ..Self::DEFAULT
        }
    }

    fn in_window(&self, hour: u32) -> bool {
        super::dump::DumpSettings {
            window_utc: self.window_utc,
            ..super::dump::DumpSettings::DEFAULT
        }
        .in_window(hour)
    }
}

/// The adapter.
#[derive(Debug, Clone)]
pub struct GtfsStatic {
    settings: GtfsSettings,
}

impl GtfsStatic {
    /// Default settings.
    pub const DEFAULT: GtfsStatic = GtfsStatic {
        settings: GtfsSettings::DEFAULT,
    };

    /// With settings.
    pub fn new(settings: GtfsSettings) -> Self {
        GtfsStatic { settings }
    }
}

// ---------------------------------------------------------------------------------------------
// Keys and identification headers
// ---------------------------------------------------------------------------------------------

/// Where a provider wants its API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPlacement {
    /// A query parameter (stripped from every stored URL).
    Query(&'static str),
    /// A request header, with a value prefix (e.g. `apikey `).
    Header(&'static str, &'static str),
}

/// The key placement for a download host (`None`: the adapter does not know how this host
/// takes a key, so a keyed source on it fails rather than guessing).
pub fn auth_for(host: &str) -> Option<KeyPlacement> {
    Some(match host {
        "api.transport.nsw.gov.au" => KeyPlacement::Header("authorization", "apikey "),
        "api.511.org" => KeyPlacement::Query("api_key"),
        "opendata.samtrafiken.se" => KeyPlacement::Query("key"),
        "api.digitransit.fi" => KeyPlacement::Header("digitransit-subscription-key", ""),
        "otd.delhi.gov.in" => KeyPlacement::Query("key"),
        _ => return None,
    })
}

/// Query parameters that may carry a key (stripped from stored URLs).
const KEY_PARAMS: &[&str] = &["api_key", "key", "apikey", "token"];

fn is_entur(u: &CanonicalUrl) -> bool {
    let h = u.host();
    h == "entur.io"
        || h.ends_with(".entur.io")
        || h == "entur.org"
        || h.ends_with(".entur.org")
        || (h == "storage.googleapis.com" && u.as_url().path().starts_with("/marduk-production/"))
}

// ---------------------------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Table {
    Agency,
    Routes,
    Stops,
    FareAttributes,
    FareProducts,
    FeedInfo,
}

impl Table {
    fn of(file: &str) -> Option<Table> {
        Some(match file.to_ascii_lowercase().as_str() {
            "agency.txt" => Table::Agency,
            "routes.txt" => Table::Routes,
            "stops.txt" => Table::Stops,
            "fare_attributes.txt" => Table::FareAttributes,
            "fare_products.txt" => Table::FareProducts,
            "feed_info.txt" => Table::FeedInfo,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Table::Agency => "agency",
            Table::Routes => "routes",
            Table::Stops => "stops",
            Table::FareAttributes => "fare_attributes",
            Table::FareProducts => "fare_products",
            Table::FeedInfo => "feed_info",
        }
    }

    /// The columns kept.
    fn columns(self) -> &'static [&'static str] {
        match self {
            Table::Agency => &[
                "agency_id",
                "agency_name",
                "agency_url",
                "agency_timezone",
                "agency_lang",
                "agency_phone",
                "agency_fare_url",
            ],
            Table::Routes => &[
                "route_id",
                "agency_id",
                "route_short_name",
                "route_long_name",
                "route_type",
                "route_url",
            ],
            Table::Stops => &[
                "stop_id",
                "stop_code",
                "stop_name",
                "stop_lat",
                "stop_lon",
                "location_type",
                "wheelchair_boarding",
                "stop_timezone",
                "stop_url",
            ],
            Table::FareAttributes => &[
                "fare_id",
                "price",
                "currency_type",
                "transfers",
                "agency_id",
                "transfer_duration",
            ],
            Table::FareProducts => &["fare_product_id", "fare_product_name", "amount", "currency"],
            Table::FeedInfo => &[
                "feed_publisher_name",
                "feed_publisher_url",
                "feed_lang",
                "feed_start_date",
                "feed_end_date",
                "feed_version",
            ],
        }
    }
}

type Row = BTreeMap<&'static str, String>;

/// What was kept of one feed.
#[derive(Debug, Default)]
struct Feed {
    rows: BTreeMap<Table, Vec<Row>>,
    malformed: u64,
    capped: u64,
    broken: BTreeSet<&'static str>,
}

fn num(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Whether a kept row is usable (else it counts as malformed).
fn valid(t: Table, r: &Row) -> bool {
    let has = |c: &str| r.get(c).is_some_and(|v| !v.is_empty());
    match t {
        Table::Agency => has("agency_name"),
        Table::Routes => {
            has("route_id")
                && (has("route_short_name") || has("route_long_name"))
                && r.get("route_type")
                    .and_then(|v| v.parse::<i64>().ok())
                    .is_some_and(|v| mode(v).is_some())
        }
        Table::Stops => {
            has("stop_id")
                && has("stop_name")
                && r.get("stop_lat")
                    .and_then(|v| num(v))
                    .is_some_and(|v| (-90.0..=90.0).contains(&v) && v != 0.0)
                && r.get("stop_lon")
                    .and_then(|v| num(v))
                    .is_some_and(|v| (-180.0..=180.0).contains(&v) && v != 0.0)
        }
        Table::FareAttributes => {
            has("fare_id")
                && r.get("price")
                    .and_then(|v| num(v))
                    .is_some_and(|v| v >= 0.0)
                && r.get("currency_type").is_some_and(|c| iso4217(c))
        }
        Table::FareProducts => {
            has("fare_product_id")
                && r.get("amount")
                    .and_then(|v| num(v))
                    .is_some_and(|v| v >= 0.0)
                && r.get("currency").is_some_and(|c| iso4217(c))
        }
        Table::FeedInfo => true,
    }
}

fn iso4217(c: &str) -> bool {
    c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase())
}

/// The traveller-facing mode of a GTFS `route_type` (basic types and the extended
/// "Hierarchical Vehicle Type" codes).
pub fn mode(route_type: i64) -> Option<&'static str> {
    Some(match route_type {
        0 => "tram",
        1 => "metro",
        2 => "rail",
        3 => "bus",
        4 => "ferry",
        5 => "cable_tram",
        6 => "aerial_lift",
        7 => "funicular",
        11 => "trolleybus",
        12 => "monorail",
        100..=199 => "rail",
        200..=299 => "coach",
        400..=699 => "metro",
        700..=799 => "bus",
        800..=899 => "trolleybus",
        900..=999 => "tram",
        1000..=1099 | 1200..=1299 => "ferry",
        1100..=1199 => "air",
        1300..=1399 => "aerial_lift",
        1400..=1499 => "funicular",
        1500..=1599 => "taxi",
        1700..=1799 => "other",
        _ => return None,
    })
}

/// One table being read (a CSV splitter and its header).
struct TableReader {
    table: Table,
    feed: String,
    splitter: Splitter,
    cols: Option<Vec<(usize, &'static str)>>,
    location_type: Option<usize>,
    broken: bool,
}

const MAX_ROW_BYTES: usize = 1024 * 1024;

impl TableReader {
    fn new(table: Table, feed: String) -> Self {
        TableReader {
            table,
            feed,
            splitter: Splitter::new(Split::Csv, MAX_ROW_BYTES),
            cols: None,
            location_type: None,
            broken: false,
        }
    }

    fn push(&mut self, bytes: &[u8], feeds: &mut BTreeMap<String, Feed>, max_rows: usize) {
        if self.broken {
            return;
        }
        let feed = feeds.entry(self.feed.clone()).or_default();
        let (table, cols, lt) = (self.table, &mut self.cols, &mut self.location_type);
        let r = self.splitter.push(bytes, &mut |u| {
            row(table, u, cols, lt, feed, max_rows);
            Ok(())
        });
        if r.is_err() {
            self.cut(feed);
        }
    }

    fn finish(&mut self, feeds: &mut BTreeMap<String, Feed>, max_rows: usize) {
        if self.broken {
            return;
        }
        let feed = feeds.entry(self.feed.clone()).or_default();
        let (table, cols, lt) = (self.table, &mut self.cols, &mut self.location_type);
        let r = self.splitter.finish(&mut |u| {
            row(table, u, cols, lt, feed, max_rows);
            Ok(())
        });
        if r.is_err() {
            self.cut(feed);
        }
    }

    /// The CSV broke (a row over [`MAX_ROW_BYTES`], usually an unbalanced quote): the rest of
    /// the table is not read.
    fn cut(&mut self, feed: &mut Feed) {
        self.broken = true;
        self.splitter = Splitter::new(Split::Csv, MAX_ROW_BYTES);
        feed.malformed += 1;
        feed.broken.insert(self.table.name());
    }
}

/// One CSV unit of `table`: the header, or a row (kept, skipped or counted as malformed).
fn row(
    table: Table,
    unit: &[u8],
    cols: &mut Option<Vec<(usize, &'static str)>>,
    location_type: &mut Option<usize>,
    feed: &mut Feed,
    max_rows: usize,
) {
    let unit = unit.strip_prefix(b"\xef\xbb\xbf").unwrap_or(unit);
    if unit.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(unit);
    let rec = match rdr.byte_records().next() {
        Some(Ok(r)) => r,
        _ => {
            feed.malformed += 1;
            return;
        }
    };
    let field = |i: usize| -> String {
        rec.get(i)
            .map(|b| String::from_utf8_lossy(b).trim().to_string())
            .unwrap_or_default()
    };
    let Some(c) = cols else {
        let names: Vec<String> = (0..rec.len())
            .map(|i| field(i).to_ascii_lowercase())
            .collect();
        let pos = |n: &str| names.iter().position(|h| h == n);
        *location_type = pos("location_type");
        *cols = Some(
            table
                .columns()
                .iter()
                .filter_map(|n| pos(n).map(|i| (i, *n)))
                .collect(),
        );
        return;
    };
    // Stations only: `location_type = 1` (parent stations of platforms and entrances).
    if table == Table::Stops && location_type.is_none_or(|i| field(i) != "1") {
        return;
    }
    let r: Row = c
        .iter()
        .map(|(i, n)| (*n, field(*i)))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    if !valid(table, &r) {
        feed.malformed += 1;
        return;
    }
    let rows = feed.rows.entry(table).or_default();
    if rows.len() >= max_rows {
        feed.capped += 1;
        return;
    }
    rows.push(r);
}

/// The [`EntrySink`] that reads the kept tables of one archive (and of the zips inside it).
pub(crate) struct Collector {
    feeds: BTreeMap<String, Feed>,
    reader: Option<TableReader>,
    nested: Option<Box<ZipStream>>,
    nested_feed: Option<String>,
    in_nested: bool,
    max_rows: usize,
    max_inflated: u64,
    /// Entries seen inside nested zips.
    inner_entries: u32,
}

impl Collector {
    pub(crate) fn new(max_rows: usize, max_inflated: u64) -> Self {
        Collector {
            feeds: BTreeMap::new(),
            reader: None,
            nested: None,
            nested_feed: None,
            in_nested: false,
            max_rows,
            max_inflated,
            inner_entries: 0,
        }
    }
}

fn split_name(name: &str) -> (&str, &str) {
    match name.rsplit_once('/') {
        Some((dir, file)) => (dir, file),
        None => ("", name),
    }
}

impl EntrySink for Collector {
    fn begin(&mut self, name: &str) -> Result<bool> {
        let (dir, file) = split_name(name);
        if self.in_nested {
            self.inner_entries += 1;
            let Some(t) = Table::of(file) else {
                return Ok(false);
            };
            let feed = self.nested_feed.clone().unwrap_or_default();
            self.reader = Some(TableReader::new(t, feed));
            return Ok(true);
        }
        if file.to_ascii_lowercase().ends_with(".zip") {
            // A feed inside the archive (one zip per mode or operator): its folder names it,
            // else its file stem.
            let feed = if dir.is_empty() {
                file[..file.len() - 4].to_string()
            } else {
                dir.to_string()
            };
            self.nested = Some(Box::new(ZipStream::new(self.max_inflated)));
            self.nested_feed = Some(feed);
            return Ok(true);
        }
        let Some(t) = Table::of(file) else {
            return Ok(false);
        };
        self.reader = Some(TableReader::new(t, dir.to_string()));
        Ok(true)
    }

    fn data(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.in_nested
            && let Some(mut z) = self.nested.take()
        {
            self.in_nested = true;
            let r = z.push(bytes, self);
            self.in_nested = false;
            self.nested = Some(z);
            return r;
        }
        if let Some(rd) = self.reader.as_mut() {
            rd.push(bytes, &mut self.feeds, self.max_rows);
        }
        Ok(())
    }

    fn end(&mut self) -> Result<()> {
        if !self.in_nested
            && let Some(mut z) = self.nested.take()
        {
            self.nested_feed = None;
            return z.finish();
        }
        if let Some(mut rd) = self.reader.take() {
            rd.finish(&mut self.feeds, self.max_rows);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Items (what the resource records hold; [`super::records`] maps them to `transit_item`)
// ---------------------------------------------------------------------------------------------

fn date(d: &str) -> Option<String> {
    let d = d.trim();
    (d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..]))
}

fn put(m: &mut Map<String, Value>, k: &str, v: Option<&String>) {
    if let Some(v) = v.filter(|v| !v.is_empty()) {
        m.insert(k.to_string(), Value::String(v.clone()));
    }
}

fn fare_of(t: Table, r: &Row) -> Value {
    let mut m = Map::new();
    match t {
        Table::FareAttributes => {
            put(&mut m, "label", r.get("fare_id"));
            m.insert("amount".into(), json!(num(&r["price"]).unwrap_or(0.0)));
            put(&mut m, "currency", r.get("currency_type"));
            // Empty `transfers` = unlimited (GTFS); 0, 1, 2 = that many.
            match r.get("transfers").and_then(|v| v.parse::<u8>().ok()) {
                Some(n) => {
                    m.insert("transfers".into(), json!(n));
                }
                None => {
                    m.insert("unlimited_transfers".into(), json!(true));
                }
            }
            if let Some(s) = r
                .get("transfer_duration")
                .and_then(|v| v.parse::<u64>().ok())
            {
                m.insert("transfer_duration_s".into(), json!(s));
            }
        }
        _ => {
            let label = r
                .get("fare_product_name")
                .or_else(|| r.get("fare_product_id"));
            put(&mut m, "label", label);
            m.insert("amount".into(), json!(num(&r["amount"]).unwrap_or(0.0)));
            put(&mut m, "currency", r.get("currency"));
        }
    }
    Value::Object(m)
}

/// The items of everything collected (operators, then lines, then stations, per feed).
fn items(feeds: &BTreeMap<String, Feed>) -> Vec<Value> {
    let mut out = Vec::new();
    let none = Vec::new();
    for (fk, f) in feeds {
        let get = |t: Table| f.rows.get(&t).unwrap_or(&none);
        let (agencies, routes, stations) =
            (get(Table::Agency), get(Table::Routes), get(Table::Stops));
        let info = get(Table::FeedInfo).first();
        let common = |m: &mut Map<String, Value>| {
            if !fk.is_empty() {
                m.insert("feed".into(), json!(fk));
            }
            if let Some(i) = info {
                if let Some(d) = i.get("feed_start_date").and_then(|d| date(d)) {
                    m.insert("valid_from".into(), json!(d));
                }
                if let Some(d) = i.get("feed_end_date").and_then(|d| date(d)) {
                    m.insert("valid_to".into(), json!(d));
                }
                put(m, "publisher", i.get("feed_publisher_name"));
            }
        };
        // Route → agency (an empty agency_id is the feed's only agency).
        let agency_of = |id: Option<&String>| -> Option<&Row> {
            match id.filter(|i| !i.is_empty()) {
                Some(id) => agencies.iter().find(|a| a.get("agency_id") == Some(id)),
                None if agencies.len() == 1 => agencies.first(),
                None => None,
            }
        };
        let route_type = |r: &Row| r.get("route_type").and_then(|v| v.parse::<i64>().ok());
        let first_tz = agencies.first().and_then(|a| a.get("agency_timezone"));
        let single = agencies.len() == 1;
        for a in agencies {
            let aid = a.get("agency_id");
            let mine: Vec<&Row> = routes
                .iter()
                .filter(|r| match (r.get("agency_id"), aid) {
                    (Some(x), Some(y)) => x == y,
                    _ => single,
                })
                .collect();
            let modes: BTreeSet<&str> = mine
                .iter()
                .filter_map(|r| route_type(r).and_then(mode))
                .collect();
            let mut fares: Vec<Value> = get(Table::FareAttributes)
                .iter()
                .filter(|r| match (r.get("agency_id"), aid) {
                    (Some(x), Some(y)) => x == y,
                    _ => single,
                })
                .map(|r| fare_of(Table::FareAttributes, r))
                .collect();
            if single {
                let mut seen = BTreeSet::new();
                fares.extend(
                    get(Table::FareProducts)
                        .iter()
                        .filter(|r| seen.insert(r.get("fare_product_id").cloned()))
                        .map(|r| fare_of(Table::FareProducts, r)),
                );
            }
            fares.sort_by(|x, y| {
                x["amount"]
                    .as_f64()
                    .partial_cmp(&y["amount"].as_f64())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            fares.truncate(20);
            let mut m = Map::new();
            m.insert("t".into(), json!("operator"));
            put(&mut m, "id", aid.or(a.get("agency_name")));
            put(&mut m, "name", a.get("agency_name"));
            put(&mut m, "url", a.get("agency_url"));
            put(&mut m, "fare_url", a.get("agency_fare_url"));
            put(&mut m, "phone", a.get("agency_phone"));
            put(&mut m, "timezone", a.get("agency_timezone"));
            put(&mut m, "lang", a.get("agency_lang"));
            m.insert("modes".into(), json!(modes));
            m.insert("route_count".into(), json!(mine.len()));
            if !fares.is_empty() {
                m.insert("fares".into(), Value::Array(fares));
            }
            common(&mut m);
            out.push(Value::Object(m));
        }
        for r in routes {
            let mut m = Map::new();
            m.insert("t".into(), json!("line"));
            put(&mut m, "id", r.get("route_id"));
            put(&mut m, "short_name", r.get("route_short_name"));
            put(&mut m, "long_name", r.get("route_long_name"));
            if let Some(t) = route_type(r) {
                m.insert("route_type".into(), json!(t));
                if let Some(md) = mode(t) {
                    m.insert("mode".into(), json!(md));
                }
            }
            put(&mut m, "url", r.get("route_url"));
            if let Some(a) = agency_of(r.get("agency_id")) {
                put(&mut m, "operator", a.get("agency_name"));
                put(&mut m, "operator_url", a.get("agency_url"));
                put(&mut m, "timezone", a.get("agency_timezone"));
            }
            common(&mut m);
            out.push(Value::Object(m));
        }
        for s in stations {
            let mut m = Map::new();
            m.insert("t".into(), json!("station"));
            put(&mut m, "id", s.get("stop_id"));
            put(&mut m, "name", s.get("stop_name"));
            put(&mut m, "code", s.get("stop_code"));
            if let (Some(lat), Some(lng)) = (
                s.get("stop_lat").and_then(|v| num(v)),
                s.get("stop_lon").and_then(|v| num(v)),
            ) {
                m.insert("lat".into(), json!(lat));
                m.insert("lng".into(), json!(lng));
            }
            match s.get("wheelchair_boarding").map(String::as_str) {
                Some("1") => {
                    m.insert("wheelchair".into(), json!(true));
                }
                Some("2") => {
                    m.insert("wheelchair".into(), json!(false));
                }
                _ => {}
            }
            put(&mut m, "timezone", s.get("stop_timezone").or(first_tz));
            put(&mut m, "url", s.get("stop_url"));
            common(&mut m);
            out.push(Value::Object(m));
        }
    }
    out
}

/// What the manifest says about each feed.
fn feed_report(feeds: &BTreeMap<String, Feed>) -> Value {
    Value::Array(
        feeds
            .iter()
            .map(|(k, f)| {
                let n = |t: Table| f.rows.get(&t).map_or(0, Vec::len);
                let info = f.rows.get(&Table::FeedInfo).and_then(|v| v.first());
                json!({
                    "feed": k,
                    "agencies": n(Table::Agency),
                    "routes": n(Table::Routes),
                    "stations": n(Table::Stops),
                    "fare_attributes": n(Table::FareAttributes),
                    "fare_products": n(Table::FareProducts),
                    "valid_from": info.and_then(|i| i.get("feed_start_date")).and_then(|d| date(d)),
                    "valid_to": info.and_then(|i| i.get("feed_end_date")).and_then(|d| date(d)),
                    "malformed_rows": f.malformed,
                    "capped_rows": f.capped,
                    "broken_tables": f.broken,
                })
            })
            .collect(),
    )
}

/// Reads a whole GTFS zip held in memory (tests and re-processing): the items it yields, and
/// the per-feed report.
pub fn parse_zip(zip: &[u8], settings: &GtfsSettings) -> Result<(Vec<Value>, Value)> {
    let mut z = ZipStream::new(settings.max_inflated);
    let mut c = Collector::new(settings.max_rows, settings.max_inflated);
    z.push(zip, &mut c)?;
    z.finish()?;
    Ok((items(&c.feeds), feed_report(&c.feeds)))
}

/// Cuts items into JSON-array batches of about `limit` bytes.
fn batches(items: &[Value], limit: usize) -> Result<Vec<Bytes>> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for it in items {
        buf.push(if buf.is_empty() { b'[' } else { b',' });
        serde_json::to_writer(&mut buf, it)?;
        if buf.len() >= limit {
            buf.push(b']');
            out.push(Bytes::from(std::mem::take(&mut buf)));
        }
    }
    if !buf.is_empty() {
        buf.push(b']');
        out.push(Bytes::from(buf));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------

fn status_error(status: u16, retry: Option<String>) -> Error {
    match status {
        401 | 403 => Error::Fetch(FetchError::Challenge { status }),
        404 | 410 => Error::NotFound(format!("http {status}")),
        429 | 503 => Error::Fetch(FetchError::RateLimited {
            status,
            retry_after: retry
                .and_then(|v| super::common::parse_retry_after(&v, chrono::Utc::now())),
        }),
        500..=599 => Error::Fetch(FetchError::ServerError { status }),
        _ => Error::Fetch(FetchError::Transport(format!("http {status}"))),
    }
}

/// The status capture of the leased URL: a primary `metadata` record carrying the response's
/// status and validators (and, for a 200, the zip's SHA-256 as its payload hash), so the
/// frontier sends `If-None-Match` / `If-Modified-Since` next time. It has no body, so the
/// processor derives nothing from it (and it is no evidence that records went missing).
async fn append_status(
    ctx: &mut AdapterCtx<'_>,
    status: u16,
    validators: Validators,
    zip_sha: Option<[u8; 32]>,
    fields: Value,
    report: &mut AdapterReport,
) -> Result<()> {
    let url = ctx.url.url.clone();
    let body = serde_json::to_vec(&fields)?;
    let mut m = meta(
        ctx,
        &url,
        CaptureRole::Primary,
        MediaType::Json,
        Some(status),
        &body,
        validators,
    );
    m.payload_hash = zip_sha.map(PayloadHash);
    m.payload_len = 0;
    ctx.sink
        .append(SpoolRecord {
            meta: m,
            payload: SpoolPayload::Metadata { fields },
        })
        .await?;
    report.records += 1;
    Ok(())
}

#[async_trait::async_trait]
impl SourceAdapter for GtfsStatic {
    fn id(&self) -> &'static str {
        "gtfs_static"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let adapter = self.id();
        let s = &self.settings;
        let mut report = AdapterReport::default();
        let started = ctx.clock.now();
        if !s.in_window(started.hour()) {
            tracing::info!(adapter, "outside the dump window; not started");
            return Ok(report);
        }
        let leased = ctx.url.url.clone();
        let mut req = FetchRequest::get(leased.clone(), RequestPurpose::Dump);
        req.max_body_bytes = s.max_bytes;
        req.validators = ctx.url.validators.clone();
        req.accept = Some("application/zip, application/octet-stream;q=0.9, */*;q=0.1".into());
        if is_entur(&leased) {
            req.headers.insert(
                HeaderName::from_static("et-client-name"),
                HeaderValue::from_static(ENTUR_CLIENT_NAME),
            );
        }
        if ctx.source.needs_key {
            let name = ctx.source.key_secret.clone().ok_or_else(|| {
                Error::Invalid("gtfs_static: keyed source has no key_secret".into())
            })?;
            let place = auth_for(leased.host()).ok_or_else(|| {
                Error::Invalid(format!(
                    "gtfs_static: no known key placement for {}",
                    leased.host()
                ))
            })?;
            let secrets = ctx.secrets;
            let key = secrets.get(&name).await?;
            match place {
                KeyPlacement::Query(p) => {
                    let mut u = leased.as_url().clone();
                    u.query_pairs_mut().append_pair(p, key.expose());
                    req.url =
                        CanonicalUrl::from_url(u).map_err(|e| Error::Invalid(e.to_string()))?;
                }
                KeyPlacement::Header(h, prefix) => {
                    let mut v = HeaderValue::from_str(&format!("{prefix}{}", key.expose()))
                        .map_err(|_| {
                            Error::Invalid("gtfs_static: key is not a header value".into())
                        })?;
                    v.set_sensitive(true);
                    req.headers.insert(HeaderName::from_static(h), v);
                }
            }
        }

        report.requests += 1;
        let stream = ctx
            .client
            .stream(ctx.permit, req)
            .await
            .map_err(|e| Error::Fetch(super::common::redact_fetch_error(e, KEY_PARAMS)))?;
        let head = stream.head;
        let mut body = stream.body;
        let header = |n: &str| {
            head.headers
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let status = head.status.as_u16();
        let validators = Validators {
            etag: header("etag"),
            last_modified: header("last-modified"),
        };
        let lic = &ctx.source.licence;
        let mut manifest = json!({
            "type": "dump_manifest",
            "adapter": adapter,
            "url": leased.as_str(),
            "final_url": redact_query(&head.url, KEY_PARAMS).as_str(),
            "status": status,
            "last_modified": validators.last_modified,
            "etag": validators.etag,
            "content_length": header("content-length"),
            "filter_version": FILTER_VERSION,
            "started_at": started.to_rfc3339(),
            "licence": lic,
        });

        if status == 304 {
            drop(body);
            manifest["not_modified"] = true.into();
            manifest["complete"] = true.into();
            append_manifest(ctx, &leased, manifest, &mut report).await?;
            let v = Validators {
                etag: validators.etag.or_else(|| ctx.url.validators.etag.clone()),
                last_modified: validators
                    .last_modified
                    .or_else(|| ctx.url.validators.last_modified.clone()),
            };
            append_status(
                ctx,
                304,
                v,
                None,
                json!({"type": "gtfs_feed_status", "adapter": adapter, "status": 304}),
                &mut report,
            )
            .await?;
            tracing::info!(adapter, source = %ctx.source.id.0, "gtfs feed not modified");
            report.complete = true;
            return Ok(report);
        }
        if !(200..300).contains(&status) {
            drop(body);
            manifest["complete"] = false.into();
            manifest["error"] = format!("http {status}").into();
            append_manifest(ctx, &leased, manifest, &mut report).await?;
            return Err(status_error(status, header("retry-after")));
        }
        let declared = header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
        if let Some(n) = declared.filter(|n| *n > s.max_bytes) {
            drop(body);
            manifest["complete"] = true.into();
            manifest["skipped"] = "over_size_cap".into();
            manifest["size_cap"] = s.max_bytes.into();
            append_manifest(ctx, &leased, manifest, &mut report).await?;
            tracing::warn!(adapter, source = %ctx.source.id.0, bytes = n, cap = s.max_bytes,
                "gtfs feed larger than gtfs_max_bytes; skipped (raise the cap to take it)");
            report.complete = true;
            return Ok(report);
        }

        let mut sha = Sha256::new();
        let mut zip = ZipStream::new(s.max_inflated);
        let mut coll = Collector::new(s.max_rows, s.max_inflated);
        let mut last_extend = started;
        let extend_every = chrono::Duration::from_std(s.extend_every)
            .unwrap_or_else(|_| chrono::Duration::seconds(60));
        let mut failure: Option<Error> = None;
        loop {
            let chunk = match body.next().await {
                None => break,
                Some(Err(e)) => {
                    failure = Some(Error::Fetch(e));
                    break;
                }
                Some(Ok(c)) => c,
            };
            sha.update(&chunk);
            report.bytes += chunk.len() as u64;
            if let Err(e) = zip.push(&chunk, &mut coll) {
                failure = Some(e);
                break;
            }
            let now = ctx.clock.now();
            if now - last_extend >= extend_every {
                let by = (now - last_extend).to_std().unwrap_or(s.extend_every);
                let (frontier, permit) = (ctx.frontier, ctx.permit);
                if let Err(e) = frontier.extend(permit, by).await {
                    failure = Some(Error::LeaseLost(e));
                    break;
                }
                last_extend = now;
            }
        }
        drop(body);
        if failure.is_none()
            && let Err(e) = zip.finish()
        {
            failure = Some(e);
        }
        manifest["size"] = report.bytes.into();
        manifest["entries"] = zip.entries().into();
        manifest["inner_entries"] = coll.inner_entries.into();
        manifest["ended_at"] = ctx.clock.now().to_rfc3339().into();
        if let Some(e) = failure {
            // Nothing derived from a partial or broken archive is stored.
            manifest["complete"] = false.into();
            manifest["sha256"] = Value::Null;
            manifest["error"] = format!("{:?}", e.class()).into();
            if let Error::Invalid(m) = &e {
                manifest["error_detail"] = m.clone().into();
            }
            append_manifest(ctx, &leased, manifest, &mut report).await?;
            return Err(e);
        }

        let digest = sha.finish();
        let items = items(&coll.feeds);
        let mut resources = 0u64;
        for b in batches(&items, s.batch_bytes)? {
            append_resource(
                ctx,
                &leased,
                MediaType::Json,
                "application/json",
                b,
                &mut report,
            )
            .await?;
            resources += 1;
        }
        manifest["sha256"] = hex(&digest).into();
        manifest["items"] = items.len().into();
        manifest["resource_records"] = resources.into();
        manifest["feeds"] = feed_report(&coll.feeds);
        manifest["complete"] = true.into();
        append_manifest(ctx, &leased, manifest, &mut report).await?;
        append_status(
            ctx,
            status,
            validators,
            Some(digest),
            json!({"type": "gtfs_feed_status", "adapter": adapter, "status": status,
                   "sha256": hex(&digest), "items": items.len()}),
            &mut report,
        )
        .await?;
        report.complete = true;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::gtfs_zip::tests::zip;

    #[test]
    fn route_types_map_to_modes() {
        assert_eq!(mode(0), Some("tram"));
        assert_eq!(mode(2), Some("rail"));
        assert_eq!(mode(3), Some("bus"));
        assert_eq!(mode(4), Some("ferry"));
        assert_eq!(mode(109), Some("rail"));
        assert_eq!(mode(401), Some("metro"));
        assert_eq!(mode(700), Some("bus"));
        assert_eq!(mode(1000), Some("ferry"));
        assert_eq!(mode(1300), Some("aerial_lift"));
        assert_eq!(mode(8), None);
        assert_eq!(mode(-1), None);
    }

    #[test]
    fn dates_and_key_placement() {
        assert_eq!(date("20261231").as_deref(), Some("2026-12-31"));
        assert_eq!(date("2026-12-31"), None);
        assert_eq!(
            auth_for("api.transport.nsw.gov.au"),
            Some(KeyPlacement::Header("authorization", "apikey "))
        );
        assert_eq!(
            auth_for("api.511.org"),
            Some(KeyPlacement::Query("api_key"))
        );
        assert_eq!(auth_for("gtfs.at.govt.nz"), None);
    }

    #[test]
    fn nested_zips_are_one_feed_each() {
        let inner = |agency: &str| {
            zip(&[
                (
                    "agency.txt",
                    format!("agency_id,agency_name,agency_url,agency_timezone\nA,{agency},https://a.example/,Australia/Melbourne\n").as_bytes(),
                    true,
                    false,
                ),
                ("routes.txt", b"route_id,agency_id,route_short_name,route_type\nR1,A,1,0\n", true, false),
            ])
        };
        let (i1, i2) = (inner("Yarra Trams"), inner("Metro Trains"));
        let outer = zip(&[
            ("3/", b"", false, false),
            ("3/google_transit.zip", &i1, false, false),
            ("2/google_transit.zip", &i2, true, true),
        ]);
        let (items, report) = parse_zip(&outer, &GtfsSettings::DEFAULT).unwrap();
        let ops: Vec<(&str, &str)> = items
            .iter()
            .filter(|i| i["t"] == "operator")
            .map(|i| (i["feed"].as_str().unwrap(), i["name"].as_str().unwrap()))
            .collect();
        assert_eq!(ops, [("2", "Metro Trains"), ("3", "Yarra Trams")]);
        assert_eq!(report.as_array().unwrap().len(), 2);
    }
}
