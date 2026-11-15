//! Grounding checks (spec §5.7 step 4, prompt-injection containment).
//!
//! * Fields marked `verbatim` in the kind (phones, URLs, prices, dates, emergency numbers) must
//!   occur in the source text after normalisation, or they are dropped.
//! * Every URL in an LLM record (verbatim or not) must be on the source's own registrable
//!   domain or be one of the page's links; for high-stakes kinds only the own domain counts.
//!   Non-HTTP(S) schemes and URLs with credentials are always dropped.
//!
//! Dropping a field inside an array element drops the whole element (a phone number without
//! its number is worthless and could mislead); the record is re-validated afterwards by the
//! pipeline and dropped entirely if it no longer fits its schema.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use url::Url;
use vc_core::{CanonicalUrl, PolitenessKey};

use crate::fold::fold;

/// How a verbatim value is normalised and compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    /// Digits (and a leading `+`); national/international prefixes tolerated for ≥ 7 digits.
    Phone,
    /// Scheme-less, `www.`-less, lower-cased host, no fragment, no trailing slash.
    Url,
    /// Every number in the value must occur as a number in the source.
    Price,
    /// A calendar date (ISO or English/numeric forms in the source).
    Date,
    /// Folded substring of the source text.
    Text,
}

/// Why a value failed grounding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroundFailure {
    /// Not found in the source text (after normalisation).
    NotInSource,
    /// A URL on a foreign domain that is not one of the page's links.
    ForeignDomain,
    /// A URL that is not plain http(s), or carries credentials.
    BadScheme,
}

/// Everything grounding compares against, computed once per capture.
#[derive(Debug, Clone)]
pub struct SourceEvidence {
    folded: String,
    phones: HashSet<String>,
    numbers: Vec<f64>,
    dates: HashSet<(i32, u32, u32)>,
    month_days: HashSet<(u32, u32)>,
    text_urls: HashSet<String>,
    link_urls: HashSet<String>,
    own_key: PolitenessKey,
}

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:https?://|www\.)[^\s<>"'\)\]\}]+"#).expect("url regex")
});
static PHONE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\+?\d(?:[\d \t().\-/]*\d)?").expect("phone regex"));
static DIGITS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").expect("digits regex"));
static NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(?:[.,' \u{a0}]\d+)*").expect("number regex"));
static ISO_DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\d{4})-(\d{1,2})-(\d{1,2})(?:[T ]\d|\b)").expect("iso date regex")
});
static NUM_DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\d{1,2})[/.\-](\d{1,2})[/.\-](\d{4}|\d{2})\b").expect("numeric date regex")
});
static DMY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(\d{1,2})(?:st|nd|rd|th)?(?:\s+of)?\s+(jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?,?(?:\s+(\d{4}))?",
    )
    .expect("dmy regex")
});
static MDY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?\s+(\d{1,2})(?:st|nd|rd|th)?\b,?(?:\s+(\d{4}))?",
    )
    .expect("mdy regex")
});

fn month_no(m: &str) -> u32 {
    match &m.to_ascii_lowercase()[..3] {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        _ => 12,
    }
}

fn valid_md(m: u32, d: u32) -> bool {
    (1..=12).contains(&m) && (1..=31).contains(&d)
}

fn year4(y: &str) -> i32 {
    let n: i32 = y.parse().unwrap_or(0);
    if y.len() == 2 { 2000 + n } else { n }
}

/// All dates mentioned in `s`: full dates and year-less (month, day) mentions.
type DateSets = (HashSet<(i32, u32, u32)>, HashSet<(u32, u32)>);

fn scan_dates(s: &str) -> DateSets {
    let mut full = HashSet::new();
    let mut md = HashSet::new();
    for c in ISO_DATE_RE.captures_iter(s) {
        let (y, m, d) = (
            year4(&c[1]),
            c[2].parse().unwrap_or(0),
            c[3].parse().unwrap_or(0),
        );
        if valid_md(m, d) {
            full.insert((y, m, d));
        }
    }
    for c in NUM_DATE_RE.captures_iter(s) {
        let (a, b, y) = (
            c[1].parse().unwrap_or(0),
            c[2].parse().unwrap_or(0),
            year4(&c[3]),
        );
        // Ambiguous d/m vs m/d: accept both readings.
        if valid_md(b, a) {
            full.insert((y, b, a));
        }
        if valid_md(a, b) {
            full.insert((y, a, b));
        }
    }
    for c in DMY_RE.captures_iter(s) {
        let (d, m) = (c[1].parse().unwrap_or(0), month_no(&c[2]));
        if !valid_md(m, d) {
            continue;
        }
        match c.get(3) {
            Some(y) => full.insert((year4(y.as_str()), m, d)),
            None => md.insert((m, d)),
        };
    }
    for c in MDY_RE.captures_iter(s) {
        let (m, d) = (month_no(&c[1]), c[2].parse().unwrap_or(0));
        if !valid_md(m, d) {
            continue;
        }
        match c.get(3) {
            Some(y) => full.insert((year4(y.as_str()), m, d)),
            None => md.insert((m, d)),
        };
    }
    (full, md)
}

/// Every plausible numeric reading of a number token (`1,234.50`, `1.234,50`, `25,5`).
fn number_readings(tok: &str) -> Vec<f64> {
    let t: String = tok
        .chars()
        .filter(|c| !matches!(c, ' ' | '\u{a0}' | '\''))
        .collect();
    let mut out = Vec::new();
    let push = |s: String, out: &mut Vec<f64>| {
        if let Ok(v) = s.parse::<f64>() {
            out.push(v);
        }
    };
    // Last separator is decimal, the others thousands.
    if let Some(i) = t.rfind(['.', ',']) {
        let (int, frac) = t.split_at(i);
        let int: String = int.chars().filter(char::is_ascii_digit).collect();
        push(format!("{int}.{}", &frac[1..]), &mut out);
    }
    // Every separator is a thousands separator.
    push(t.chars().filter(char::is_ascii_digit).collect(), &mut out);
    out
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// Normalises a URL for comparison; `None` for non-http(s), credentials or garbage.
pub fn normalise_url(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim().trim_end_matches(['.', ',', ';', ':', '!', '?']);
    let with_scheme = if raw.to_ascii_lowercase().starts_with("www.") {
        format!("https://{raw}")
    } else {
        raw.to_string()
    };
    let u = Url::parse(&with_scheme).ok()?;
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some()
    {
        return None;
    }
    let host = u.host_str()?.to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    let host = host.strip_prefix("www.").unwrap_or(host).to_string();
    let path = u.path().trim_end_matches('/');
    let q = u.query().map(|q| format!("?{q}")).unwrap_or_default();
    Some((format!("{host}{path}{q}"), host))
}

impl SourceEvidence {
    /// Evidence from the source text, the page's links and the page URL (own domain).
    pub fn new(text: &str, links: &[CanonicalUrl], page: &CanonicalUrl) -> Self {
        let mut phones = HashSet::new();
        for m in PHONE_RE.find_iter(text) {
            let d = digits(m.as_str());
            if !d.is_empty() {
                phones.insert(d);
            }
            for g in DIGITS_RE.find_iter(m.as_str()) {
                phones.insert(g.as_str().to_string());
            }
        }
        let numbers = NUMBER_RE
            .find_iter(text)
            .flat_map(|m| number_readings(m.as_str()))
            .chain(
                DIGITS_RE
                    .find_iter(text)
                    .filter_map(|m| m.as_str().parse::<f64>().ok()),
            )
            .collect();
        let (dates, month_days) = scan_dates(text);
        let text_urls = URL_RE
            .find_iter(text)
            .filter_map(|m| normalise_url(m.as_str()).map(|(n, _)| n))
            .collect();
        let mut link_urls: HashSet<String> = links
            .iter()
            .filter_map(|l| normalise_url(l.as_str()).map(|(n, _)| n))
            .collect();
        if let Some((n, _)) = normalise_url(page.as_str()) {
            link_urls.insert(n);
        }
        SourceEvidence {
            folded: format!(" {} ", fold(text)),
            phones,
            numbers,
            dates,
            month_days,
            text_urls,
            link_urls,
            own_key: page.politeness_key(),
        }
    }

    /// Checks one scalar value of type `ft`. `strict_domain`: URLs must be on the own domain.
    pub fn check(
        &self,
        ft: FieldType,
        v: &Value,
        strict_domain: bool,
    ) -> Result<(), GroundFailure> {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(_) | Value::Null => return Ok(()),
            _ => return Err(GroundFailure::NotInSource),
        };
        match ft {
            FieldType::Url => self.check_url(&s, strict_domain),
            FieldType::Phone => self.check_phone(&s),
            FieldType::Price => self.check_price(&s),
            FieldType::Date => self.check_date(&s),
            FieldType::Text => self.check_text(&s),
        }
    }

    fn check_text(&self, s: &str) -> Result<(), GroundFailure> {
        let f = fold(s);
        if f.is_empty() || self.folded.contains(&format!(" {f} ")) {
            Ok(())
        } else {
            Err(GroundFailure::NotInSource)
        }
    }

    /// The URL rule alone (scheme, own domain or page link), without the verbatim test.
    pub fn check_url_domain(&self, s: &str, strict_domain: bool) -> Result<(), GroundFailure> {
        let (norm, host) = normalise_url(s).ok_or(GroundFailure::BadScheme)?;
        if PolitenessKey::for_host(&host) == self.own_key {
            return Ok(());
        }
        if !strict_domain && self.link_urls.contains(&norm) {
            return Ok(());
        }
        Err(GroundFailure::ForeignDomain)
    }

    fn check_url(&self, s: &str, strict_domain: bool) -> Result<(), GroundFailure> {
        self.check_url_domain(s, strict_domain)?;
        let (norm, _) = normalise_url(s).ok_or(GroundFailure::BadScheme)?;
        if self.text_urls.contains(&norm) || self.link_urls.contains(&norm) {
            Ok(())
        } else {
            Err(GroundFailure::NotInSource)
        }
    }

    fn check_phone(&self, s: &str) -> Result<(), GroundFailure> {
        let d = digits(s);
        if d.is_empty() {
            return self.check_text(s);
        }
        if s.chars().any(|c| c.is_ascii_alphabetic()) {
            // Vanity numbers ("1-800-FLOWERS") must occur as written.
            return self.check_text(s);
        }
        if self.phones.contains(&d) {
            return Ok(());
        }
        if d.len() >= 7 {
            let dz = d.trim_start_matches('0');
            for p in &self.phones {
                if p.len() < 7 {
                    continue;
                }
                let pz = p.trim_start_matches('0');
                if (pz.len() >= 7 && d.ends_with(pz)) || (dz.len() >= 7 && p.ends_with(dz)) {
                    return Ok(());
                }
            }
        }
        Err(GroundFailure::NotInSource)
    }

    fn check_price(&self, s: &str) -> Result<(), GroundFailure> {
        let toks: Vec<&str> = NUMBER_RE.find_iter(s).map(|m| m.as_str()).collect();
        if toks.is_empty() {
            return self.check_text(s);
        }
        for t in toks {
            let ok = number_readings(t)
                .iter()
                .any(|v| self.numbers.iter().any(|n| (n - v).abs() < 0.005));
            if !ok {
                return Err(GroundFailure::NotInSource);
            }
        }
        Ok(())
    }

    fn check_date(&self, s: &str) -> Result<(), GroundFailure> {
        let (full, md) = scan_dates(s);
        if full.is_empty() && md.is_empty() {
            return self.check_text(s);
        }
        let ok_full = full.iter().all(|(y, m, d)| {
            self.dates.contains(&(*y, *m, *d)) || self.month_days.contains(&(*m, *d))
        });
        let ok_md = md.iter().all(|k| {
            self.month_days.contains(k) || self.dates.iter().any(|(_, m, d)| (*m, *d) == *k)
        });
        if ok_full && ok_md {
            Ok(())
        } else {
            Err(GroundFailure::NotInSource)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// JSON pointers with wildcards
// ---------------------------------------------------------------------------------------------

/// One step of a concrete path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// An array index.
    Index(usize),
    /// An object key.
    Key(String),
}

fn unescape(seg: &str) -> String {
    seg.replace("~1", "/").replace("~0", "~")
}

/// Expands a pointer (`/a/0/b`, `/a/*/b`, or `/a/b` through an array) to the concrete paths of
/// the values that exist in `v`.
pub fn expand_pointer(v: &Value, pointer: &str) -> Vec<Vec<Step>> {
    let segs: Vec<String> = pointer.split('/').skip(1).map(unescape).collect();
    let mut out = Vec::new();
    walk(v, &segs, &mut Vec::new(), &mut out);
    out
}

fn walk(v: &Value, segs: &[String], cur: &mut Vec<Step>, out: &mut Vec<Vec<Step>>) {
    let Some((seg, rest)) = segs.split_first() else {
        out.push(cur.clone());
        return;
    };
    match v {
        Value::Object(m) => {
            if let Some(child) = m.get(seg.as_str()) {
                cur.push(Step::Key(seg.clone()));
                walk(child, rest, cur, out);
                cur.pop();
            }
        }
        Value::Array(a) => {
            if let Ok(i) = seg.parse::<usize>() {
                if let Some(child) = a.get(i) {
                    cur.push(Step::Index(i));
                    walk(child, rest, cur, out);
                    cur.pop();
                }
            } else {
                // `*`, `-` or an implicit wildcard (a key applied to every element).
                let next = if seg == "*" || seg == "-" { rest } else { segs };
                for (i, child) in a.iter().enumerate() {
                    cur.push(Step::Index(i));
                    walk(child, next, cur, out);
                    cur.pop();
                }
            }
        }
        _ => {}
    }
}

/// The value at a concrete path.
pub fn get_path<'a>(v: &'a Value, path: &[Step]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, s| match (cur, s) {
        (Value::Object(m), Step::Key(k)) => m.get(k),
        (Value::Array(a), Step::Index(i)) => a.get(*i),
        _ => None,
    })
}

/// What to remove when the value at `path` fails: the enclosing array element if the value
/// lives inside an object inside an array, else the key (or the array element itself).
pub fn removal_target(path: &[Step]) -> Vec<Step> {
    if let Some(last_idx) = path.iter().rposition(|s| matches!(s, Step::Index(_))) {
        // The value is inside an array element (or is one).
        return path[..=last_idx].to_vec();
    }
    path.to_vec()
}

/// Removes the values at `paths` (deepest/highest index first so indices stay valid).
pub fn remove_paths(v: &mut Value, mut paths: Vec<Vec<Step>>) {
    paths.sort();
    paths.dedup();
    // Drop paths nested under another removed path.
    let mut kept: Vec<Vec<Step>> = Vec::new();
    for p in paths {
        if !kept.iter().any(|k| p.starts_with(k)) {
            kept.push(p);
        }
    }
    kept.sort_by(|a, b| b.cmp(a));
    for p in kept {
        let Some((last, parent)) = p.split_last() else {
            continue;
        };
        let Some(parent) = get_path_mut(v, parent) else {
            continue;
        };
        match (parent, last) {
            (Value::Object(m), Step::Key(k)) => {
                m.remove(k);
            }
            (Value::Array(a), Step::Index(i)) if *i < a.len() => {
                a.remove(*i);
            }
            _ => {}
        }
    }
}

fn get_path_mut<'a>(v: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    path.iter().try_fold(v, |cur, s| match (cur, s) {
        (Value::Object(m), Step::Key(k)) => m.get_mut(k),
        (Value::Array(a), Step::Index(i)) => a.get_mut(*i),
        _ => None,
    })
}

/// The sub-schema at a pointer (through `properties` / `items`), if any.
pub fn schema_at<'a>(schema: &'a Value, pointer: &str) -> Option<&'a Value> {
    let mut cur = schema;
    for seg in pointer.split('/').skip(1).map(unescape) {
        if let Some(items) = cur.get("items") {
            cur = items;
            if seg.parse::<usize>().is_ok() || seg == "*" || seg == "-" {
                continue;
            }
        }
        cur = cur.get("properties")?.get(seg.as_str())?;
    }
    Some(cur)
}

/// Infers how to compare a verbatim field: the schema's `format`, then the field name, then
/// the value's shape.
pub fn infer_type(schema: &Value, pointer: &str, value: &Value) -> FieldType {
    if let Some(s) = schema_at(schema, pointer) {
        match s.get("format").and_then(Value::as_str) {
            Some("uri" | "url" | "iri" | "uri-reference") => return FieldType::Url,
            Some("date" | "date-time") => return FieldType::Date,
            _ => {}
        }
    }
    let name = pointer
        .rsplit('/')
        .find(|s| s.parse::<usize>().is_err() && *s != "*" && *s != "-")
        .unwrap_or("")
        .to_ascii_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| name.contains(w));
    let sv = value.as_str().unwrap_or("");
    if has(&["url", "link", "website", "href", "uri"]) || looks_like_url(sv) {
        return FieldType::Url;
    }
    if has(&[
        "date",
        "_at",
        "start",
        "end",
        "until",
        "valid_from",
        "valid_to",
    ]) && !scan_dates(sv).0.is_empty()
    {
        return FieldType::Date;
    }
    if has(&["phone", "tel", "fax", "hotline", "emergency", "number"]) {
        return FieldType::Phone;
    }
    if has(&["price", "cost", "fee", "amount", "fare", "rate"]) || value.is_number() {
        return FieldType::Price;
    }
    if !scan_dates(sv).0.is_empty() && sv.len() <= 32 {
        return FieldType::Date;
    }
    let d = digits(sv).len();
    if d >= 2
        && sv
            .chars()
            .all(|c| c.is_ascii_digit() || " +-().".contains(c))
    {
        return FieldType::Phone;
    }
    FieldType::Text
}

/// Whether a string looks like a URL (`http(s)://…` or `www.…`).
pub fn looks_like_url(s: &str) -> bool {
    let l = s.trim_start().to_ascii_lowercase();
    l.starts_with("http://")
        || l.starts_with("https://")
        || l.starts_with("www.")
        || l.starts_with("javascript:")
        || l.starts_with("data:")
}

/// Every URL in a string leaf of `v` (a leaf that is a URL, or free text containing URLs),
/// with the leaf's path.
pub fn url_leaves(v: &Value) -> Vec<(Vec<Step>, String)> {
    fn rec(v: &Value, cur: &mut Vec<Step>, out: &mut Vec<(Vec<Step>, String)>) {
        match v {
            Value::String(s) if looks_like_url(s) => out.push((cur.clone(), s.clone())),
            Value::String(s) => {
                for m in URL_RE.find_iter(s) {
                    out.push((cur.clone(), m.as_str().to_string()));
                }
            }
            Value::Object(m) => {
                for (k, c) in m {
                    cur.push(Step::Key(k.clone()));
                    rec(c, cur, out);
                    cur.pop();
                }
            }
            Value::Array(a) => {
                for (i, c) in a.iter().enumerate() {
                    cur.push(Step::Index(i));
                    rec(c, cur, out);
                    cur.pop();
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    rec(v, &mut Vec::new(), &mut out);
    out
}

/// What grounding did to one record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroundReport {
    /// Values checked.
    pub checked: u32,
    /// Values that failed (and were removed).
    pub dropped: u32,
    /// Failures by reason.
    pub failures: Vec<GroundFailure>,
}

/// Grounding options for one record.
#[derive(Debug, Clone, Copy)]
pub struct GroundOpts {
    /// Apply the URL domain rule to every URL-looking string, not only verbatim fields (LLM
    /// output).
    pub all_urls: bool,
    /// URLs must be on the own domain (high-stakes kinds).
    pub strict_domain: bool,
    /// Skip the verbatim test (structured data: the values are the publisher's own markup).
    pub skip_verbatim: bool,
}

/// Grounds `data` in place: removes failing verbatim fields and URLs.
pub fn ground(
    data: &mut Value,
    schema: &Value,
    verbatim_fields: &[String],
    ev: &SourceEvidence,
    opts: GroundOpts,
) -> GroundReport {
    let mut rep = GroundReport::default();
    let mut remove = Vec::new();
    let fail =
        |path: &[Step], f: GroundFailure, rep: &mut GroundReport, remove: &mut Vec<Vec<Step>>| {
            rep.dropped += 1;
            rep.failures.push(f);
            remove.push(removal_target(path));
        };
    for ptr in verbatim_fields {
        for path in expand_pointer(data, ptr) {
            let Some(val) = get_path(data, &path) else {
                continue;
            };
            let leaves: Vec<(Vec<Step>, &Value)> = match val {
                Value::Array(a) => a
                    .iter()
                    .enumerate()
                    .map(|(i, x)| {
                        let mut p = path.clone();
                        p.push(Step::Index(i));
                        (p, x)
                    })
                    .collect(),
                other => vec![(path.clone(), other)],
            };
            for (p, leaf) in leaves {
                rep.checked += 1;
                let ft = infer_type(schema, ptr, leaf);
                let res = if opts.skip_verbatim {
                    match (ft, leaf.as_str()) {
                        (FieldType::Url, Some(s)) if opts.strict_domain => {
                            ev.check_url_domain(s, true)
                        }
                        (FieldType::Url, Some(s)) => {
                            normalise_url(s).map(|_| ()).ok_or(GroundFailure::BadScheme)
                        }
                        _ => Ok(()),
                    }
                } else {
                    ev.check(ft, leaf, opts.strict_domain)
                };
                if let Err(f) = res {
                    fail(&p, f, &mut rep, &mut remove);
                }
            }
        }
    }
    if opts.all_urls {
        for (p, s) in url_leaves(data) {
            if remove.iter().any(|r| p.starts_with(r)) {
                continue;
            }
            rep.checked += 1;
            if let Err(f) = ev.check_url_domain(&s, opts.strict_domain) {
                fail(&p, f, &mut rep, &mut remove);
            }
        }
    }
    remove_paths(data, remove);
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(text: &str) -> SourceEvidence {
        let page = CanonicalUrl::parse("https://www.example.org/guide/spain").unwrap();
        let links = vec![CanonicalUrl::parse("https://tickets.partner.com/event/42").unwrap()];
        SourceEvidence::new(text, &links, &page)
    }

    #[test]
    fn phones_normalise() {
        let e =
            ev("Call 112 in an emergency, or +44 (0)20 7946 0000. Tourist line: 020-7946-0001.");
        assert!(e.check(FieldType::Phone, &json!("112"), false).is_ok());
        assert!(
            e.check(FieldType::Phone, &json!("+44 20 7946 0001"), false)
                .is_ok()
        );
        assert!(
            e.check(FieldType::Phone, &json!("02079460001"), false)
                .is_ok()
        );
        assert!(e.check(FieldType::Phone, &json!("911"), false).is_err());
        assert!(
            e.check(FieldType::Phone, &json!("+1 900 555 0199"), false)
                .is_err()
        );
        // A short number doesn't match inside a long one.
        assert!(e.check(FieldType::Phone, &json!("7946"), false).is_ok()); // a digit group
        assert!(e.check(FieldType::Phone, &json!("946"), false).is_err());
    }

    #[test]
    fn urls_need_own_domain_or_link() {
        let e = ev(
            "Book at https://www.example.org/tickets/ or at https://evil-pay.example.net/login now.",
        );
        assert!(
            e.check(FieldType::Url, &json!("https://example.org/tickets"), false)
                .is_ok()
        );
        assert_eq!(
            e.check(
                FieldType::Url,
                &json!("https://evil-pay.example.net/login"),
                false
            ),
            Err(GroundFailure::ForeignDomain)
        );
        assert!(
            e.check(
                FieldType::Url,
                &json!("https://tickets.partner.com/event/42"),
                false
            )
            .is_ok()
        );
        assert_eq!(
            e.check(
                FieldType::Url,
                &json!("https://tickets.partner.com/event/42"),
                true
            ),
            Err(GroundFailure::ForeignDomain)
        );
        assert_eq!(
            e.check(
                FieldType::Url,
                &json!("https://example.org/not-mentioned"),
                false
            ),
            Err(GroundFailure::NotInSource)
        );
        assert_eq!(
            e.check(FieldType::Url, &json!("javascript:alert(1)"), false),
            Err(GroundFailure::BadScheme)
        );
        assert_eq!(
            e.check(
                FieldType::Url,
                &json!("https://user:pw@example.org/tickets"),
                false
            ),
            Err(GroundFailure::BadScheme)
        );
    }

    #[test]
    fn prices_and_dates() {
        let e = ev(
            "Entry €25 (reduced 12,50 €). Open 15 November 2026 to Nov 20, 2026; also 03/12/2026 and 24 December.",
        );
        assert!(e.check(FieldType::Price, &json!("€25"), false).is_ok());
        assert!(e.check(FieldType::Price, &json!(12.5), false).is_ok());
        assert!(e.check(FieldType::Price, &json!("€5"), false).is_err());
        assert!(
            e.check(FieldType::Date, &json!("2026-11-15"), false)
                .is_ok()
        );
        assert!(
            e.check(FieldType::Date, &json!("2026-11-20T19:00:00"), false)
                .is_ok()
        );
        assert!(
            e.check(FieldType::Date, &json!("2026-12-03"), false)
                .is_ok()
        );
        assert!(
            e.check(FieldType::Date, &json!("2026-03-12"), false)
                .is_ok()
        );
        assert!(
            e.check(FieldType::Date, &json!("2026-12-24"), false)
                .is_ok()
        );
        assert!(
            e.check(FieldType::Date, &json!("2026-11-16"), false)
                .is_err()
        );
    }

    #[test]
    fn pointers_expand_and_remove_array_elements() {
        let mut v = json!({"numbers": [{"service": "police", "value": "112"}, {"service": "x", "value": "999"}]});
        let paths = expand_pointer(&v, "/numbers/*/value");
        assert_eq!(paths.len(), 2);
        assert_eq!(expand_pointer(&v, "/numbers/value").len(), 2);
        assert_eq!(expand_pointer(&v, "/numbers/1/value").len(), 1);
        remove_paths(&mut v, vec![removal_target(&paths[1])]);
        assert_eq!(
            v,
            json!({"numbers": [{"service": "police", "value": "112"}]})
        );
    }

    #[test]
    fn infers_types() {
        let schema = json!({"type":"object","properties":{"when":{"type":"string","format":"date"},"site":{"type":"string","format":"uri"}}});
        assert_eq!(infer_type(&schema, "/when", &json!("x")), FieldType::Date);
        assert_eq!(infer_type(&schema, "/site", &json!("x")), FieldType::Url);
        assert_eq!(
            infer_type(&schema, "/phone", &json!("+34 112")),
            FieldType::Phone
        );
        assert_eq!(
            infer_type(&schema, "/numbers/0/value", &json!("112")),
            FieldType::Phone
        );
        assert_eq!(
            infer_type(&schema, "/price", &json!("€25")),
            FieldType::Price
        );
        assert_eq!(
            infer_type(&schema, "/start_date", &json!("2026-11-15")),
            FieldType::Date
        );
        assert_eq!(
            infer_type(&schema, "/booking", &json!("https://a.b/")),
            FieldType::Url
        );
        assert_eq!(
            infer_type(&schema, "/venue", &json!("Teatro Real")),
            FieldType::Text
        );
    }
}
