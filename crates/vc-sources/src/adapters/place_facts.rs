//! Shared helpers of the `place_fact` adapters (`wikidata_facts`, `factbook_json`): one fact =
//! one short sentence in our own words, keyed to a place, with its source and licence. Source
//! text is never copied: a short quoted value (a name, a nickname, a motto) is the most a
//! sentence holds.
//!
//! Pure: no I/O, no logging of content.

use serde_json::{Map, Value, json};

/// Longest fact text (`schemas/place_fact.v1.json`).
pub const MAX_TEXT_CHARS: usize = 280;

/// The fact types (`fact_type`).
pub const FACT_TYPES: &[&str] = &[
    "historical",
    "fun",
    "popular",
    "unbelievable",
    "little_known",
];

/// How a fact's `text` was made (`text_origin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOrigin {
    /// Our own sentence template over typed fields.
    Templated,
}

impl TextOrigin {
    fn as_str(self) -> &'static str {
        match self {
            TextOrigin::Templated => "templated",
        }
    }
}

/// One fact before it becomes a `place_fact` record.
#[derive(Debug, Clone)]
pub struct Fact {
    /// Stable id within the source (record key input).
    pub id: String,
    /// The `place` object.
    pub place: Map<String, Value>,
    /// One of [`FACT_TYPES`].
    pub fact_type: &'static str,
    /// The sentence.
    pub text: String,
    /// Language of `text`.
    pub lang: String,
    /// How `text` was made.
    pub origin: TextOrigin,
    /// `date_ref` (`YYYY`, `YYYY-MM` or `YYYY-MM-DD`, `-` for BCE).
    pub date_ref: Option<String>,
    /// `quantity` (`value`, `unit`, optional `rank_scope`).
    pub quantity: Option<(f64, &'static str, Option<String>)>,
    /// `source_field`.
    pub source_field: String,
    /// `source.name`, `source.publisher`, `source.sourced`.
    pub source: (&'static str, &'static str, &'static str),
    /// `licence.class`, `licence.spdx`, `licence.attribution`.
    pub licence: (&'static str, &'static str, String),
    /// `confidence`.
    pub confidence: &'static str,
    /// `source_url`.
    pub source_url: Option<String>,
    /// `as_of`.
    pub as_of: Option<String>,
    /// `value_lang`: the language of a quoted value (a name, nickname or motto) in `text`.
    pub value_lang: Option<String>,
}

impl Fact {
    /// The record data (fields of `place_fact.v1`); `None` when the text is empty or too long,
    /// or the place has no key.
    pub fn into_data(self) -> Option<(String, Value)> {
        let text = squash(&self.text);
        let n = text.chars().count();
        if !(8..=MAX_TEXT_CHARS).contains(&n) || !FACT_TYPES.contains(&self.fact_type) {
            return None;
        }
        let keyed = ["geonames_id", "wikidata_qid", "country"]
            .iter()
            .any(|k| self.place.contains_key(*k));
        if !keyed {
            return None;
        }
        let mut d = Map::new();
        d.insert("place".into(), Value::Object(self.place));
        d.insert("fact_type".into(), json!(self.fact_type));
        d.insert("text".into(), json!(text));
        d.insert("lang".into(), json!(self.lang));
        d.insert("text_origin".into(), json!(self.origin.as_str()));
        d.insert("llm_ok".into(), json!(self.licence.0 != "share_alike"));
        if let Some(l) = self.value_lang {
            d.insert("value_lang".into(), json!(l));
        }
        if let Some(r) = self.date_ref {
            d.insert("date_ref".into(), json!(r));
        }
        if let Some((value, unit, scope)) = self.quantity {
            let mut q = Map::new();
            q.insert("value".into(), json!(value));
            q.insert("unit".into(), json!(unit));
            if let Some(s) = scope {
                q.insert("rank_scope".into(), json!(clip(&s, 120)));
            }
            d.insert("quantity".into(), Value::Object(q));
        }
        d.insert("source_field".into(), json!(clip(&self.source_field, 120)));
        d.insert(
            "source".into(),
            json!({"name": self.source.0, "publisher": self.source.1, "sourced": self.source.2}),
        );
        d.insert(
            "licence".into(),
            json!({"class": self.licence.0, "spdx": self.licence.1,
                   "attribution": clip(&self.licence.2, 300)}),
        );
        d.insert("confidence".into(), json!(self.confidence));
        if let Some(u) = self.source_url.filter(|u| u.len() <= 2048) {
            d.insert("source_url".into(), json!(u));
        }
        if let Some(a) = self.as_of {
            d.insert("as_of".into(), json!(a));
        }
        Some((self.id, Value::Object(d)))
    }
}

/// Collapses whitespace and trims.
pub fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first `n` characters of `s`.
pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `a`, `a and b`, `a, b and c` (English).
pub fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// An integer with thousands separators (`3,700,577`).
pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// A decimal for a sentence: integers without a fraction, others with at most two decimals
/// (thousands separated).
pub fn number(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        #[allow(clippy::cast_possible_truncation)]
        return thousands(x as i64);
    }
    let s = format!("{:.2}", x.abs());
    let s = s.trim_end_matches('0').trim_end_matches('.');
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let int = thousands(int.parse::<i64>().unwrap_or(0));
    let sign = if x < 0.0 { "-" } else { "" };
    if frac.is_empty() {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// A date at a precision: `(date_ref, phrase)` where the phrase reads after a verb
/// ("in 1244", "in October 1806", "on 27 October 1806", "in 500 BCE"). Precision: 9 = year,
/// 10 = month, 11 = day. `None` for coarser precisions or bad parts.
pub fn date_phrase(year: i64, month: u32, day: u32, precision: u8) -> Option<(String, String)> {
    if year == 0 || year.abs() > 9999 || precision < 9 {
        return None;
    }
    let y = if year < 0 {
        format!("{} BCE", -year)
    } else {
        year.to_string()
    };
    let yref = if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    };
    match precision {
        9 => Some((yref, format!("in {y}"))),
        10 if (1..=12).contains(&month) => Some((
            format!("{yref}-{month:02}"),
            format!("in {} {y}", MONTHS[month as usize - 1]),
        )),
        _ if (1..=12).contains(&month) && (1..=31).contains(&day) => Some((
            format!("{yref}-{month:02}-{day:02}"),
            format!("on {day} {} {y}", MONTHS[month as usize - 1]),
        )),
        _ => Some((yref, format!("in {y}"))),
    }
}

/// Parses `1 January 1871` / `January 1871` / `1871` (English) into a date reference.
pub fn parse_english_date(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let month = |m: &str| MONTHS.iter().position(|x| x.eq_ignore_ascii_case(m));
    let year = |y: &str| {
        y.trim_end_matches([',', ';', '.'])
            .parse::<i64>()
            .ok()
            .filter(|y| (1..=9999).contains(y))
    };
    match parts.as_slice() {
        [d, m, y, ..] if month(m).is_some() && d.parse::<u32>().is_ok() => {
            let (d, m, y) = (d.parse::<u32>().ok()?, month(m)? + 1, year(y)?);
            (1..=31)
                .contains(&d)
                .then(|| format!("{y:04}-{m:02}-{d:02}"))
        }
        [m, y, ..] if month(m).is_some() => Some(format!("{:04}-{:02}", year(y)?, month(m)? + 1)),
        [y, ..] => year(y).map(|y| format!("{y:04}")),
        [] => None,
    }
}

/// Removes HTML tags and decodes the common entities.
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    squash(
        &out.replace("&nbsp;", " ")
            .replace("&amp;", "&")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">"),
    )
    .replace(" ,", ",")
    .replace(" .", ".")
}

/// A stable short id from text (first 16 hex digits of its sha256).
pub fn short_hash(s: &str) -> String {
    let h = crate::sha256::hex(&crate::sha256::Sha256::digest(s.as_bytes()));
    h[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_lists() {
        assert_eq!(thousands(3_700_577), "3,700,577");
        assert_eq!(thousands(-35), "-35");
        assert_eq!(number(891.12), "891.12");
        assert_eq!(number(34.0), "34");
        assert_eq!(number(-3.5), "-3.5");
        assert_eq!(number(2963.0), "2,963");
        assert_eq!(and_list(&["a".into()]), "a");
        assert_eq!(
            and_list(&["a".into(), "b".into(), "c".into()]),
            "a, b and c"
        );
    }

    #[test]
    fn dates() {
        assert_eq!(
            date_phrase(1244, 0, 0, 9),
            Some(("1244".into(), "in 1244".into()))
        );
        assert_eq!(
            date_phrase(1806, 10, 27, 11),
            Some(("1806-10-27".into(), "on 27 October 1806".into()))
        );
        assert_eq!(
            date_phrase(-500, 0, 0, 9),
            Some(("-0500".into(), "in 500 BCE".into()))
        );
        assert_eq!(date_phrase(1200, 0, 0, 7), None);
        assert_eq!(
            parse_english_date("18 January 1871 (establishment)"),
            Some("1871-01-18".into())
        );
        assert_eq!(parse_english_date("1991"), Some("1991".into()));
        assert_eq!(parse_english_date("none"), None);
    }

    #[test]
    fn html_is_stripped() {
        assert_eq!(
            strip_html("<p>Museum <em>Island</em>, Berlin</p>"),
            "Museum Island, Berlin"
        );
    }
}
