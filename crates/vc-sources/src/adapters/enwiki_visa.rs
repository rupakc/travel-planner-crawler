//! `enwiki_visa`: the "Visa requirements for X citizens" articles of English Wikipedia
//! (CC BY-SA 4.0), read from the `pages-articles-multistream` dump without downloading it.
//!
//! The leased row is the dump's multistream index
//! (`…/enwiki-latest-pages-articles-multistream-index.txt.bz2`, about 290 MB): it is streamed
//! through the dump runner (window, lease extension, manifest) and filtered on the fly to the
//! lines whose title is a visa-requirements article; the kept bzip2 streams (offset and length,
//! each about 100 pages) are spooled as a JSON `resource`. Then, in the same run, each stream is
//! fetched with one `Range` request on the multistream dump (about 200 requests, each through
//! the permit client: lease check, rate token, the key's delay) and spooled raw. A `Range` that
//! is not honoured (anything but `206` with exactly the requested length) stops the run.
//!
//! The record mapping ([`parse_stream`]) decodes a stream, keeps the visa articles and reads
//! their main table into `visa_reference` records: Wikipedia is a reference source, so these
//! are cross-check data, never `visa_rule`.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Serialize;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, fetch_raw, xml_unescape};
use super::countries;
use super::dump::{DumpFilter, DumpSettings, OutputKind, Split, Unit, run_dump};

/// Filter version recorded in the manifest.
pub const FILTER_VERSION: &str = "enwiki-visa-index-v1";
/// Suffix of the multistream index file.
pub const INDEX_SUFFIX: &str = "-pages-articles-multistream-index.txt.bz2";
/// Suffix of the multistream dump file.
pub const DUMP_SUFFIX: &str = "-pages-articles-multistream.xml.bz2";
/// Article URL prefix.
pub const WIKI_BASE: &str = "https://en.wikipedia.org/wiki/";
/// Cap on stream fetches per run.
pub const MAX_STREAMS: usize = 600;
/// Cap on one stream's compressed size.
pub const MAX_STREAM_BYTES: u64 = 32 * 1024 * 1024;
/// Cap on one stream's decoded size.
pub const MAX_DECODED_BYTES: usize = 128 * 1024 * 1024;

static VISA_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Visa requirements for (.+) citizens$").expect("regex"));

/// One bzip2 stream of the multistream dump that holds visa articles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stream {
    /// Byte offset in the dump.
    pub offset: u64,
    /// Byte offset of the next stream (`None` for the last one).
    pub end: Option<u64>,
    /// The visa article titles in it.
    pub titles: Vec<String>,
}

#[derive(Default)]
struct IndexFilter {
    pending: Option<Stream>,
    streams: Vec<Stream>,
}

fn index_line(line: &[u8]) -> Option<(u64, &str)> {
    let s = std::str::from_utf8(line).ok()?;
    let (off, rest) = s.split_once(':')?;
    let (_, title) = rest.split_once(':')?;
    Some((off.parse().ok()?, title))
}

impl DumpFilter for IndexFilter {
    fn version(&self) -> &'static str {
        FILTER_VERSION
    }

    fn split(&self) -> Split {
        Split::Lines
    }

    fn output(&self) -> OutputKind {
        OutputKind::JsonArray
    }

    fn filter(&mut self, unit: &[u8]) -> Result<Unit> {
        let Some((offset, title)) = index_line(unit) else {
            return Ok(Unit::Drop);
        };
        let mut out = Unit::Drop;
        if let Some(p) = self.pending.as_mut()
            && offset != p.offset
        {
            p.end = Some(offset);
            let done = self.pending.take().expect("pending");
            out = Unit::Keep(serde_json::to_vec(&done)?);
            self.streams.push(done);
        }
        if VISA_TITLE.is_match(title) {
            match self.pending.as_mut() {
                Some(p) => p.titles.push(title.to_string()),
                None => {
                    self.pending = Some(Stream {
                        offset,
                        end: None,
                        titles: vec![title.to_string()],
                    })
                }
            }
        }
        Ok(out)
    }
}

/// Filters index lines (pure; for tests): the visa-article streams with their end offsets.
pub fn index_streams(lines: &str) -> Vec<Stream> {
    let mut f = IndexFilter::default();
    for l in lines.lines() {
        let _ = f.filter(l.as_bytes());
    }
    f.streams
}

/// The multistream dump URL next to an index URL, with the stream as query
/// (`?offset=…&length=…`; the server ignores it, it names the capture).
pub fn stream_url(index: &CanonicalUrl, s: &Stream) -> Option<CanonicalUrl> {
    let end = s.end?;
    let path = index.as_url().path();
    let dump = path.strip_suffix(INDEX_SUFFIX)?;
    let mut u = index.as_url().clone();
    u.set_path(&format!("{dump}{DUMP_SUFFIX}"));
    u.set_query(Some(&format!(
        "offset={}&length={}",
        s.offset,
        end - s.offset
    )));
    CanonicalUrl::from_url(u).ok()
}

/// `(offset, length)` of a stream URL.
pub fn stream_range(u: &CanonicalUrl) -> Option<(u64, u64)> {
    if !u.as_url().path().ends_with(DUMP_SUFFIX) {
        return None;
    }
    let mut off = None;
    let mut len = None;
    for (k, v) in u.as_url().query_pairs() {
        match &*k {
            "offset" => off = v.parse().ok(),
            "length" => len = v.parse().ok(),
            _ => {}
        }
    }
    Some((off?, len?)).filter(|(_, l)| *l > 0)
}

// ---------------------------------------------------------------------------------------------
// Article parsing
// ---------------------------------------------------------------------------------------------

/// Demonyms of the article titles ("Visa requirements for <demonym> citizens"); titles that
/// name the country itself resolve through [`countries::iso2`]. Titles that are neither (e.g.
/// "European Union", "British Overseas Territories", unrecognised states) are skipped.
const DEMONYMS: &[(&str, &str)] = &[
    ("Afghan", "AF"),
    ("Albanian", "AL"),
    ("Algerian", "DZ"),
    ("American", "US"),
    ("Andorran", "AD"),
    ("Angolan", "AO"),
    ("Argentine", "AR"),
    ("Armenian", "AM"),
    ("Australian", "AU"),
    ("Austrian", "AT"),
    ("Azerbaijani", "AZ"),
    ("Bahamian", "BS"),
    ("Bahraini", "BH"),
    ("Bangladeshi", "BD"),
    ("Barbadian", "BB"),
    ("Belarusian", "BY"),
    ("Belgian", "BE"),
    ("Belizean", "BZ"),
    ("Beninese", "BJ"),
    ("Bhutanese", "BT"),
    ("Bolivian", "BO"),
    ("Bosnian", "BA"),
    ("Botswana", "BW"),
    ("Brazilian", "BR"),
    ("British", "GB"),
    ("Bruneian", "BN"),
    ("Bulgarian", "BG"),
    ("Burkinabe", "BF"),
    ("Burkinabé", "BF"),
    ("Burmese", "MM"),
    ("Burundian", "BI"),
    ("Cambodian", "KH"),
    ("Cameroonian", "CM"),
    ("Canadian", "CA"),
    ("Cape Verdean", "CV"),
    ("Central African", "CF"),
    ("Chadian", "TD"),
    ("Chilean", "CL"),
    ("Chinese", "CN"),
    ("Colombian", "CO"),
    ("Comorian", "KM"),
    ("Costa Rican", "CR"),
    ("Croatian", "HR"),
    ("Cuban", "CU"),
    ("Cypriot", "CY"),
    ("Czech", "CZ"),
    ("Danish", "DK"),
    ("Djiboutian", "DJ"),
    ("Dutch", "NL"),
    ("East Timorese", "TL"),
    ("Ecuadorian", "EC"),
    ("Egyptian", "EG"),
    ("Emirati", "AE"),
    ("Equatorial Guinean", "GQ"),
    ("Eritrean", "ER"),
    ("Estonian", "EE"),
    ("Ethiopian", "ET"),
    ("Fijian", "FJ"),
    ("Filipino", "PH"),
    ("Finnish", "FI"),
    ("French", "FR"),
    ("Gabonese", "GA"),
    ("Gambian", "GM"),
    ("Georgian", "GE"),
    ("German", "DE"),
    ("Ghanaian", "GH"),
    ("Greek", "GR"),
    ("Grenadian", "GD"),
    ("Guatemalan", "GT"),
    ("Guinea-Bissauan", "GW"),
    ("Bissau-Guinean", "GW"),
    ("Guinean", "GN"),
    ("Guyanese", "GY"),
    ("Haitian", "HT"),
    ("Honduran", "HN"),
    ("Hungarian", "HU"),
    ("Icelandic", "IS"),
    ("Indian", "IN"),
    ("Indonesian", "ID"),
    ("Iranian", "IR"),
    ("Iraqi", "IQ"),
    ("Irish", "IE"),
    ("Israeli", "IL"),
    ("Italian", "IT"),
    ("Ivorian", "CI"),
    ("Jamaican", "JM"),
    ("Japanese", "JP"),
    ("Jordanian", "JO"),
    ("Kazakh", "KZ"),
    ("Kazakhstani", "KZ"),
    ("Kenyan", "KE"),
    ("Kosovar", "XK"),
    ("Kosovan", "XK"),
    ("Kuwaiti", "KW"),
    ("Kyrgyz", "KG"),
    ("Kyrgyzstani", "KG"),
    ("Laotian", "LA"),
    ("Latvian", "LV"),
    ("Lebanese", "LB"),
    ("Liberian", "LR"),
    ("Libyan", "LY"),
    ("Liechtensteiner", "LI"),
    ("Lithuanian", "LT"),
    ("Luxembourgish", "LU"),
    ("Luxembourgian", "LU"),
    ("Macanese", "MO"),
    ("Macedonian", "MK"),
    ("North Macedonian", "MK"),
    ("Malagasy", "MG"),
    ("Malawian", "MW"),
    ("Malaysian", "MY"),
    ("Maldivian", "MV"),
    ("Malian", "ML"),
    ("Maltese", "MT"),
    ("Marshallese", "MH"),
    ("Mauritanian", "MR"),
    ("Mauritian", "MU"),
    ("Mexican", "MX"),
    ("Micronesian", "FM"),
    ("Moldovan", "MD"),
    ("Monégasque", "MC"),
    ("Monegasque", "MC"),
    ("Mongolian", "MN"),
    ("Montenegrin", "ME"),
    ("Moroccan", "MA"),
    ("Mozambican", "MZ"),
    ("Namibian", "NA"),
    ("Nauruan", "NR"),
    ("Nepalese", "NP"),
    ("Nicaraguan", "NI"),
    ("Nigerian", "NG"),
    ("Nigerien", "NE"),
    ("North Korean", "KP"),
    ("Norwegian", "NO"),
    ("Omani", "OM"),
    ("Pakistani", "PK"),
    ("Palauan", "PW"),
    ("Palestinian", "PS"),
    ("Panamanian", "PA"),
    ("Papua New Guinean", "PG"),
    ("Paraguayan", "PY"),
    ("Peruvian", "PE"),
    ("Philippine", "PH"),
    ("Polish", "PL"),
    ("Portuguese", "PT"),
    ("Qatari", "QA"),
    ("Romanian", "RO"),
    ("Russian", "RU"),
    ("Rwandan", "RW"),
    ("Saint Lucian", "LC"),
    ("Salvadoran", "SV"),
    ("Sammarinese", "SM"),
    ("Samoan", "WS"),
    ("Santomean", "ST"),
    ("Saudi", "SA"),
    ("Saudi Arabian", "SA"),
    ("Senegalese", "SN"),
    ("Serbian", "RS"),
    ("Seychellois", "SC"),
    ("Sierra Leonean", "SL"),
    ("Singaporean", "SG"),
    ("Slovak", "SK"),
    ("Slovenian", "SI"),
    ("Somali", "SO"),
    ("Somalian", "SO"),
    ("South African", "ZA"),
    ("South Korean", "KR"),
    ("South Sudanese", "SS"),
    ("Spanish", "ES"),
    ("Sri Lankan", "LK"),
    ("Sudanese", "SD"),
    ("Surinamese", "SR"),
    ("Swazi", "SZ"),
    ("Swedish", "SE"),
    ("Swiss", "CH"),
    ("Syrian", "SY"),
    ("Taiwanese", "TW"),
    ("Tajik", "TJ"),
    ("Tajikistani", "TJ"),
    ("Tanzanian", "TZ"),
    ("Thai", "TH"),
    ("Togolese", "TG"),
    ("Tongan", "TO"),
    ("Tunisian", "TN"),
    ("Turkish", "TR"),
    ("Turkmen", "TM"),
    ("Turkmenistani", "TM"),
    ("Tuvaluan", "TV"),
    ("Ugandan", "UG"),
    ("Ukrainian", "UA"),
    ("Uruguayan", "UY"),
    ("Uzbek", "UZ"),
    ("Uzbekistani", "UZ"),
    ("Vanuatuan", "VU"),
    ("Venezuelan", "VE"),
    ("Vietnamese", "VN"),
    ("Yemeni", "YE"),
    ("Zambian", "ZM"),
    ("Zimbabwean", "ZW"),
];

/// The passport country of an article title: `(alpha-2, English name)`.
pub fn nationality_of_title(title: &str) -> Option<(&'static str, &'static str)> {
    let who = VISA_TITLE.captures(title)?.get(1)?.as_str().trim();
    let code = DEMONYMS
        .iter()
        .find(|(d, _)| d.eq_ignore_ascii_case(who))
        .map(|(_, c)| *c)
        .or_else(|| countries::iso2(who))?;
    Some((code, countries::name(code)?))
}

static TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<title>(.*?)</title>").expect("regex"));
static NS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<ns>(-?\d+)</ns>").expect("regex"));
static REV_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<revision>\s*<id>(\d+)</id>").expect("regex"));
static TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<timestamp>([^<]*)</timestamp>").expect("regex"));
static TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<text\b[^>]*?(?:/>|>(.*?)</text>)").expect("regex"));
static PAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<page>.*?</page>").expect("regex"));
static TIMATIC_NAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\{\{\s*timatic[^}]*?nationality\s*=\s*([A-Z]{2})\b").expect("regex")
});
static TIMATIC_DEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\{\{\s*timatic[^}]*?destination\s*=\s*([A-Z]{2})\b").expect("regex")
});
static FLAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\{\{\s*(?:flag|flagcountry|flagu|flag country)\s*\|\s*([^|}]+)")
        .expect("regex")
});
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[(?:[^|\]]*\|)?([^\]]*)\]\]").expect("regex"));
static REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<ref\b[^>/]*/>|<ref\b[^>]*>.*?</ref>|<!--.*?-->").expect("regex")
});
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("regex"));
static TEMPLATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{([^{}]*)\}\}").expect("regex"));
static ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:data-sort-value|style|class|colspan|rowspan|align)\s*=\s*("[^"]*"|\S+)"#)
        .expect("regex")
});
static STAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(\d{1,4})\s*(day|days|week|weeks)$").expect("regex"));

/// Templates that carry no visible text in a table cell.
const SILENT: &[&str] = &[
    "efn",
    "refn",
    "ref",
    "r",
    "sfn",
    "timatic",
    "ntsh",
    "nts",
    "flagicon",
    "citation needed",
    "cn",
    "clarify",
    "when",
    "update",
    "dead link",
    "better source needed",
];

/// Wikitext → plain text (references, comments, templates, links and tags removed).
pub fn clean(wikitext: &str) -> String {
    let t = REF.replace_all(wikitext, "");
    let mut t = LINK.replace_all(&t, "$1").into_owned();
    for _ in 0..8 {
        let n = TEMPLATE
            .replace_all(&t, |c: &regex::Captures<'_>| {
                let parts: Vec<&str> = c[1].split('|').map(str::trim).collect();
                let name = parts
                    .first()
                    .copied()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if name == "!" {
                    return "|".to_string();
                }
                if name == "nbsp" {
                    return " ".to_string();
                }
                if SILENT.contains(&name.as_str()) || name.starts_with("cite") {
                    return String::new();
                }
                parts[1..]
                    .iter()
                    .rev()
                    .find(|p| !p.contains('=') && !p.is_empty())
                    .map(|p| p.to_string())
                    .unwrap_or_default()
            })
            .into_owned();
        if n == t {
            break;
        }
        t = n;
    }
    let t = LINK.replace_all(&t, "$1");
    let t = TAG.replace_all(&t, " ");
    let t = ATTR.replace_all(&t, "");
    let t = t.replace("'''", "").replace("''", "");
    t.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A requirement cell's wording → the `requirement` enum (one clear match, else `other`).
pub fn classify(text: &str) -> &'static str {
    let t = text.to_lowercase();
    let mut hits: Vec<&'static str> = Vec::new();
    let hit = |hits: &mut Vec<&'static str>, h: &'static str| {
        if !hits.contains(&h) {
            hits.push(h);
        }
    };
    if t.contains("freedom of movement") {
        hit(&mut hits, "freedom_of_movement");
    }
    if t.contains("visa not required") || t.contains("visa-free") || t.contains("visa free") {
        hit(&mut hits, "visa_free");
    }
    if t.contains("visa on arrival") {
        hit(&mut hits, "visa_on_arrival");
    }
    if t.contains("evisa") || t.contains("e-visa") || t.contains("electronic visa") {
        hit(&mut hits, "e_visa");
    }
    let tokens: Vec<&str> = t
        .split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    if t.contains("electronic travel authori")
        || tokens
            .iter()
            .any(|w| matches!(*w, "eta" | "esta" | "k-eta" | "etias" | "nzeta"))
    {
        hit(&mut hits, "eta");
    }
    if t.contains("admission refused") || t.contains("entry banned") || t.contains("travel banned")
    {
        hit(&mut hits, "entry_banned");
    }
    if hits.is_empty() && t.contains("visa required") {
        hit(&mut hits, "visa_required");
    }
    match hits.as_slice() {
        [one] => one,
        _ => "other",
    }
}

/// One row of a visa-requirements table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VisaRow {
    /// Destination as the table names it.
    pub destination: String,
    /// Its alpha-2 code, when the name (or the row's own Timatic code) gives one unambiguously.
    pub destination_code: Option<String>,
    /// `requirement` enum value.
    pub requirement: &'static str,
    /// Requirement cell text.
    pub requirement_text: String,
    /// Allowed-stay cell text.
    pub allowed_stay: Option<String>,
    /// Days, when the cell states days or weeks.
    pub max_stay_days: Option<u32>,
    /// Notes cell text.
    pub notes: Option<String>,
}

/// One visa-requirements article.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VisaArticle {
    /// Title.
    pub title: String,
    /// Passport country code.
    pub nationality_code: String,
    /// Passport country name.
    pub nationality: String,
    /// Revision id.
    pub rev_id: Option<u64>,
    /// Revision timestamp.
    pub timestamp: Option<String>,
    /// The article URL.
    pub url: String,
    /// The rows of its country table(s).
    pub rows: Vec<VisaRow>,
}

/// Splits a wikitext line into cells at `||` / `!!` outside templates and links.
fn split_cells(line: &str, sep: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut depth, mut cur) = (0i32, String::new());
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let rest = &line[i..];
        if rest.starts_with("{{") || rest.starts_with("[[") {
            depth += 1;
            cur.push_str(&rest[..2]);
            i += 2;
            continue;
        }
        if rest.starts_with("}}") || rest.starts_with("]]") {
            depth -= 1;
            cur.push_str(&rest[..2]);
            i += 2;
            continue;
        }
        if depth <= 0 && rest.starts_with(sep) {
            out.push(std::mem::take(&mut cur));
            i += sep.len();
            continue;
        }
        let ch = rest.chars().next().unwrap_or(' ');
        cur.push(ch);
        i += ch.len_utf8();
    }
    out.push(cur);
    out
}

/// A cell's content without its attributes (`style="…" | content` → `content`).
fn cell_content(cell: &str) -> &str {
    let mut depth = 0i32;
    let b = cell.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1)) {
            (b'{', Some(b'{')) | (b'[', Some(b'[')) => {
                depth += 1;
                i += 2;
                continue;
            }
            (b'}', Some(b'}')) | (b']', Some(b']')) => {
                depth -= 1;
                i += 2;
                continue;
            }
            (b'|', _) if depth <= 0 => {
                let attrs = &cell[..i];
                return if attrs.contains('=') {
                    &cell[i + 1..]
                } else {
                    cell
                };
            }
            _ => {}
        }
        i += 1;
    }
    cell
}

/// Rows of one `{| … |}` table: `(is_header, cells)`.
fn table_rows(table: &str) -> Vec<(bool, Vec<String>)> {
    let mut rows: Vec<(bool, Vec<String>)> = Vec::new();
    let mut cur: Option<(bool, Vec<String>)> = None;
    for line in table.lines() {
        let l = line.trim_start();
        if l.starts_with("{|") || l.starts_with("|+") {
            continue;
        }
        if l.starts_with("|-") || l.starts_with("|}") {
            if let Some(r) = cur.take()
                && !r.1.is_empty()
            {
                rows.push(r);
            }
            continue;
        }
        if let Some(h) = l.strip_prefix('!') {
            let r = cur.get_or_insert((true, Vec::new()));
            r.1.extend(split_cells(h, "!!"));
        } else if let Some(c) = l.strip_prefix('|') {
            let r = cur.get_or_insert((false, Vec::new()));
            r.0 = false;
            r.1.extend(split_cells(c, "||"));
        } else if let Some(r) = cur.as_mut()
            && let Some(last) = r.1.last_mut()
        {
            last.push('\n');
            last.push_str(line);
        }
    }
    if let Some(r) = cur
        && !r.1.is_empty()
    {
        rows.push(r);
    }
    rows
}

/// The `{| … |}` tables of a page (top level only).
fn wikitables(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    let mut depth = 0;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let l = line.trim_start();
        if l.starts_with("{|") {
            if depth == 0 {
                start = Some(pos);
            }
            depth += 1;
        } else if l.starts_with("|}") && depth > 0 {
            depth -= 1;
            if depth == 0
                && let Some(s) = start.take()
            {
                out.push(&text[s..pos + line.len()]);
            }
        }
        pos += line.len();
    }
    out
}

fn destination_of(cell: &str) -> Option<String> {
    let raw = FLAG
        .captures(cell)
        .map(|c| c[1].trim().to_string())
        .or_else(|| {
            LINK.captures(cell)
                .map(|c| c[1].trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| clean(cell).lines().next().unwrap_or_default().to_string());
    let d = raw.trim().to_string();
    (!d.is_empty() && d.chars().count() <= 80).then_some(d)
}

/// Reads the country tables of one article's wikitext.
pub fn parse_tables(text: &str) -> Vec<VisaRow> {
    let mut out = Vec::new();
    for t in wikitables(text) {
        let rows = table_rows(t);
        let Some((_, headers)) = rows.iter().find(|(h, _)| *h) else {
            continue;
        };
        let headers: Vec<String> = headers
            .iter()
            .map(|h| clean(cell_content(h)).to_lowercase())
            .collect();
        let col = |words: &[&str]| {
            headers
                .iter()
                .position(|h| words.iter().any(|w| h.contains(w)))
        };
        let (Some(c_country), Some(c_req)) = (
            headers.first().filter(|h| h.contains("country")).map(|_| 0),
            col(&["visa requirement"]),
        ) else {
            continue;
        };
        let c_stay = col(&["allowed stay", "length of stay", "stay"]);
        let c_notes = col(&["notes"]);
        for (is_header, cells) in &rows {
            if *is_header || cells.len() <= c_req {
                continue;
            }
            let Some(destination) = destination_of(cell_content(&cells[c_country])) else {
                continue;
            };
            let req_raw = cell_content(&cells[c_req]);
            let requirement_text = clean(req_raw).lines().collect::<Vec<_>>().join(" ");
            if requirement_text.is_empty() || requirement_text.chars().count() > 160 {
                continue;
            }
            let by_name = countries::iso2(&destination);
            let by_timatic = TIMATIC_DEST
                .captures(&cells[c_req])
                .map(|c| c[1].to_ascii_uppercase());
            let destination_code = match (by_name, by_timatic) {
                (Some(a), Some(b)) if a != b => None,
                (Some(a), _) => Some(a.to_string()),
                (None, b) => b.filter(|b| countries::name(b).is_some()),
            };
            let cell = |i: Option<usize>| -> Option<String> {
                let c = cells.get(i?)?;
                let t = clean(cell_content(c));
                (!t.is_empty()).then_some(t)
            };
            let allowed_stay = cell(c_stay)
                .map(|s| s.lines().collect::<Vec<_>>().join(" "))
                .filter(|s| s.chars().count() <= 160);
            let max_stay_days = allowed_stay.as_deref().and_then(|s| {
                let c = STAY.captures(s.trim())?;
                let n: u32 = c[1].parse().ok()?;
                let days = if c[2].to_ascii_lowercase().starts_with("week") {
                    n.checked_mul(7)?
                } else {
                    n
                };
                (1..=3660).contains(&days).then_some(days)
            });
            let notes = cell(c_notes).map(|n| {
                let n = n
                    .lines()
                    .map(|l| l.trim_start_matches(['*', ' ']).trim())
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
                if n.chars().count() > 600 {
                    n.chars().take(599).collect::<String>() + "…"
                } else {
                    n
                }
            });
            out.push(VisaRow {
                destination,
                destination_code,
                requirement: classify(&requirement_text),
                requirement_text,
                allowed_stay,
                max_stay_days,
                notes: notes.filter(|n| !n.is_empty()),
            });
        }
    }
    out
}

/// Parses one `<page>` element: `None` unless it is a visa-requirements article (main
/// namespace, not a redirect) whose passport country is known.
pub fn parse_page(page: &str) -> Option<VisaArticle> {
    let ns: i64 = NS.captures(page)?[1].parse().ok()?;
    if ns != 0 || page.contains("<redirect") {
        return None;
    }
    let title = xml_unescape(TITLE.captures(page)?.get(1)?.as_str());
    if !VISA_TITLE.is_match(&title) {
        return None;
    }
    let text = xml_unescape(TEXT.captures(page)?.get(1)?.as_str());
    let by_title = nationality_of_title(&title);
    let by_timatic = TIMATIC_NAT
        .captures(&text)
        .map(|c| c[1].to_ascii_uppercase());
    let code = match (by_title, by_timatic.as_deref()) {
        (Some((a, _)), Some(b)) if a != b => return None,
        (Some((a, _)), _) => a,
        (None, Some(b)) => countries::name(b).map(|_| b)?,
        (None, None) => return None,
    };
    let code: &str = countries::COUNTRIES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(c, _)| *c)?;
    Some(VisaArticle {
        url: format!("{WIKI_BASE}{}", title.replace(' ', "_")),
        nationality_code: code.to_string(),
        nationality: countries::name(code)?.to_string(),
        rev_id: REV_ID.captures(page).and_then(|c| c[1].parse().ok()),
        timestamp: TIMESTAMP.captures(page).map(|c| c[1].to_string()),
        rows: parse_tables(&text),
        title,
    })
}

/// Decodes one stored stream (a bzip2 stream of the multistream dump) and parses its visa
/// articles. Other pages are ignored.
pub fn parse_stream(body: &[u8]) -> Result<Vec<VisaArticle>> {
    let mut d = bzip2::Decompress::new(false);
    let mut out = Vec::with_capacity(body.len() * 5);
    let mut input = body;
    loop {
        out.reserve(1024 * 1024);
        let before_in = d.total_in();
        let st = d
            .decompress_vec(input, &mut out)
            .map_err(|e| Error::Invalid(format!("enwiki stream: bzip2: {e:?}")))?;
        let used = usize::try_from(d.total_in() - before_in).unwrap_or(usize::MAX);
        input = &input[used.min(input.len())..];
        if out.len() > MAX_DECODED_BYTES {
            return Err(Error::Invalid("enwiki stream: decoded size cap".into()));
        }
        match st {
            bzip2::Status::StreamEnd => break,
            _ if input.is_empty() && used == 0 => {
                return Err(Error::Invalid("enwiki stream: truncated".into()));
            }
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&out);
    Ok(PAGE
        .find_iter(&text)
        .filter_map(|m| parse_page(m.as_str()))
        .collect())
}

// ---------------------------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------------------------

/// The adapter.
#[derive(Debug, Clone)]
pub struct EnwikiVisa {
    settings: DumpSettings,
}

impl EnwikiVisa {
    /// Default settings.
    pub const DEFAULT: EnwikiVisa = EnwikiVisa {
        settings: DumpSettings::DEFAULT,
    };

    /// With settings.
    pub fn new(settings: DumpSettings) -> Self {
        EnwikiVisa { settings }
    }
}

#[async_trait]
impl SourceAdapter for EnwikiVisa {
    fn id(&self) -> &'static str {
        "enwiki_visa"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        if !url.as_url().path().ends_with(INDEX_SUFFIX) {
            return Err(Error::Invalid(
                "enwiki_visa: the leased URL is not a multistream index".into(),
            ));
        }
        let mut filter = IndexFilter::default();
        let mut rep = run_dump(ctx, self.id(), url.clone(), &mut filter, &self.settings).await?;
        if rep.requests == 0 {
            return Ok(rep); // outside the dump window: nothing was fetched
        }
        let mut last_extend = ctx.clock.now();
        let extend_every = chrono::Duration::from_std(self.settings.extend_every)
            .unwrap_or_else(|_| chrono::Duration::seconds(60));
        let mut fetched = 0u64;
        for s in filter.streams.iter().take(MAX_STREAMS) {
            let Some(u) = stream_url(&url, s) else {
                continue;
            };
            let Some((offset, length)) = stream_range(&u) else {
                continue;
            };
            if length > MAX_STREAM_BYTES {
                continue;
            }
            let now = ctx.clock.now();
            if now - last_extend >= extend_every {
                let by = (now - last_extend)
                    .to_std()
                    .unwrap_or(Duration::from_secs(60));
                let (frontier, permit) = (ctx.frontier, ctx.permit);
                frontier
                    .extend(permit, by)
                    .await
                    .map_err(Error::LeaseLost)?;
                last_extend = now;
            }
            let opts = Fetch {
                accept: Some("application/octet-stream"),
                hint: Some(MediaType::Other),
                range: Some((offset, offset + length)),
                max_body_bytes: length,
                ..Fetch::default()
            };
            let ex = fetch_raw(ctx, u, &opts, &mut rep).await?;
            check_status(&ex)?;
            if ex.status.as_u16() != 206 || ex.body.len() as u64 != length {
                return Err(Error::Invalid(format!(
                    "enwiki_visa: range not honoured (http {}, {} bytes)",
                    ex.status.as_u16(),
                    ex.body.len()
                )));
            }
            fetched += 1;
        }
        tracing::info!(
            adapter = self.id(),
            streams = filter.streams.len(),
            fetched,
            "visa article streams fetched"
        );
        rep.complete = true;
        Ok(rep)
    }
}
