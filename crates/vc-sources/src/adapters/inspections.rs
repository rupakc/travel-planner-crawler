//! Shared pieces of the food-hygiene register adapters (`fsa_fhrs`, `nyc_restaurant_inspections`,
//! `toronto_dinesafe`): offset paging that never splits one establishment's rows across two
//! pages, and the `restaurant` record they all map to.
//!
//! **Group-aligned offset paging.** The NYC and Toronto registers have one row per inspection
//! (or violation), sorted by establishment id. A full page may end in the middle of an
//! establishment's rows, so the last establishment of a full page is left for the next page:
//! the next offset is `offset + rows of the complete groups`. The record mapping applies the
//! same rule to the stored page (it knows `limit` and the rows), so every establishment is
//! mapped from exactly one page, with all of its rows.

use serde_json::{Map, Value, json};
use vc_core::{CanonicalUrl, Error, Result};

/// The number of leading rows that form complete groups, and whether the list goes on.
///
/// `keys` are the rows' group keys in order. A page shorter than `limit` is the last one: all
/// of its rows count. A full page drops its trailing group (it may continue on the next page),
/// unless the whole page is one group (then it is kept, so paging always advances).
pub fn complete_prefix<K: PartialEq>(keys: &[K], limit: usize) -> (usize, bool) {
    if keys.len() < limit || keys.is_empty() {
        return (keys.len(), false);
    }
    let last = &keys[keys.len() - 1];
    let start = keys.iter().rposition(|k| k != last).map_or(0, |i| i + 1);
    if start == 0 {
        (keys.len(), true)
    } else {
        (start, true)
    }
}

/// The value of query parameter `name`.
pub fn query(url: &CanonicalUrl, name: &str) -> Option<String> {
    url.as_url()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// `url` with query parameter `name` set to `value` (other parameters kept in order).
pub fn with_param(url: &CanonicalUrl, name: &str, value: &str) -> Result<CanonicalUrl> {
    let mut u = url.as_url().clone();
    let mut found = false;
    let pairs: Vec<(String, String)> = u
        .query_pairs()
        .map(|(k, v)| {
            if k == name {
                found = true;
                (k.into_owned(), value.to_string())
            } else {
                (k.into_owned(), v.into_owned())
            }
        })
        .collect();
    {
        let mut q = u.query_pairs_mut();
        q.clear().extend_pairs(pairs);
        if !found {
            q.append_pair(name, value);
        }
    }
    CanonicalUrl::from_url(u).map_err(|e| Error::Invalid(e.to_string()))
}

/// The `(offset, limit)` of a paged URL (`offset_param`, `limit_param`).
pub fn window(url: &CanonicalUrl, offset_param: &str, limit_param: &str) -> Option<(u64, usize)> {
    let off = query(url, offset_param)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let lim = query(url, limit_param)?.parse().ok()?;
    Some((off, lim))
}

/// Text of a JSON scalar, whitespace collapsed; `None` when empty or the literal `None`/`null`.
pub fn text(v: Option<&Value>) -> Option<String> {
    let s = match v? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let s = s
        .split_whitespace()
        .filter(|w| *w != "None")
        .collect::<Vec<_>>()
        .join(" ");
    (!s.is_empty() && s != "null").then_some(s)
}

/// A coordinate (number or numeric string) within `±lim`, never `0`.
pub fn coord(v: Option<&Value>, lim: f64) -> Option<f64> {
    let x = match v? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (x.is_finite() && x.abs() <= lim && x != 0.0).then_some(x)
}

/// The `YYYY-MM-DD` prefix of a date or timestamp, from 2000 on (registers use `1901-01-01`
/// for "never rated").
pub fn day(v: &str) -> Option<String> {
    let d = v.trim().get(..10)?;
    let b = d.as_bytes();
    let ok = b[4] == b'-' && b[7] == b'-' && d.bytes().filter(u8::is_ascii_digit).count() == 8;
    (ok && d >= "2000-01-01").then(|| d.to_string())
}

/// A phone number as written, when it looks like one (digits and separators, 6–20 digits).
pub fn phone(v: Option<&Value>) -> Option<String> {
    let p = text(v)?;
    let digits = p.bytes().filter(u8::is_ascii_digit).count();
    let shape = p.starts_with(|c: char| c.is_ascii_digit() || c == '+')
        && p.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ' ' | '(' | ')' | '.' | '-' | '/' | '+'));
    (shape && (6..=20).contains(&digits) && p.len() <= 32).then_some(p)
}

fn cut(s: &str, max: usize) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars()
            .take(max)
            .collect::<String>()
            .trim_end()
            .to_string()
    })
}

/// The latest published rating of one establishment.
#[derive(Debug, Clone, PartialEq)]
pub struct Hygiene {
    /// `restaurant.hygiene.scheme`.
    pub scheme: &'static str,
    /// As published.
    pub rating: String,
    /// 0–5, UK FHRS only.
    pub value: Option<f64>,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// The inspecting authority.
    pub authority: String,
}

/// One establishment, ready to become a `restaurant` record.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Place {
    /// Business name.
    pub name: String,
    /// `restaurant.establishment_type`.
    pub establishment_type: Option<&'static str>,
    /// The register's own category.
    pub category_label: Option<String>,
    /// Cuisine.
    pub cuisine: Option<String>,
    /// Address as written.
    pub address: Option<String>,
    /// Borough, local authority.
    pub district: Option<String>,
    /// City (English).
    pub city: &'static str,
    /// Country (English).
    pub country: &'static str,
    /// ISO 3166-1 alpha-2.
    pub country_code: &'static str,
    /// Latitude.
    pub lat: Option<f64>,
    /// Longitude.
    pub lng: Option<f64>,
    /// Phone as written.
    pub phone: Option<String>,
}

/// The `restaurant` fields of an establishment and its latest rating.
pub fn restaurant(p: &Place, h: &Hygiene) -> Value {
    let mut o = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            o.insert(k.to_string(), v);
        }
    };
    put("name", cut(&p.name, 160).map(Value::String));
    put("establishment_type", p.establishment_type.map(|t| json!(t)));
    put(
        "category_label",
        p.category_label
            .as_deref()
            .and_then(|c| cut(c, 120))
            .map(Value::String),
    );
    put(
        "cuisine",
        p.cuisine
            .as_deref()
            .and_then(|c| cut(c, 80))
            .map(Value::String),
    );
    put(
        "address",
        p.address
            .as_deref()
            .and_then(|c| cut(c, 240))
            .map(Value::String),
    );
    put(
        "district",
        p.district
            .as_deref()
            .and_then(|c| cut(c, 120))
            .map(Value::String),
    );
    put("city", Some(json!(p.city)));
    put("country", Some(json!(p.country)));
    put("country_code", Some(json!(p.country_code)));
    if let (Some(a), Some(b)) = (p.lat, p.lng) {
        put("lat", Some(json!(a)));
        put("lng", Some(json!(b)));
    }
    put("phone", p.phone.clone().map(Value::String));
    let mut hy = Map::new();
    hy.insert("scheme".into(), json!(h.scheme));
    hy.insert(
        "rating".into(),
        json!(cut(&h.rating, 40).unwrap_or_default()),
    );
    if let Some(v) = h.value {
        hy.insert("rating_value".into(), json!(v));
    }
    hy.insert("rating_date".into(), json!(h.date));
    if let Some(a) = cut(&h.authority, 120) {
        hy.insert("authority".into(), json!(a));
    }
    put("hygiene", Some(Value::Object(hy)));
    put("as_of", Some(json!(h.date)));
    Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_pages_leave_the_trailing_group_for_the_next_page() {
        assert_eq!(
            complete_prefix(&["a", "a", "b"], 5),
            (3, false),
            "short page"
        );
        assert_eq!(complete_prefix(&["a", "a", "b", "b"], 4), (2, true));
        assert_eq!(complete_prefix(&["a", "b", "c", "c"], 4), (2, true));
        assert_eq!(
            complete_prefix(&["a", "a", "a"], 3),
            (3, true),
            "one group fills the page: keep it so paging advances"
        );
        assert_eq!(complete_prefix::<&str>(&[], 3), (0, false));
    }

    #[test]
    fn helpers() {
        let u = CanonicalUrl::parse("https://x.example/r?a=1&%24offset=0&%24limit=2").unwrap();
        assert_eq!(window(&u, "$offset", "$limit"), Some((0, 2)));
        let n = with_param(&u, "$offset", "2").unwrap();
        assert_eq!(window(&n, "$offset", "$limit"), Some((2, 2)));
        assert_eq!(query(&n, "a").as_deref(), Some("1"));
        assert_eq!(day("1901-01-01T00:00:00"), None);
        assert_eq!(day("2026-01-21T00:00:00").as_deref(), Some("2026-01-21"));
        assert_eq!(
            text(Some(&json!("998 Queen St E None M4M 1K1"))).as_deref(),
            Some("998 Queen St E M4M 1K1")
        );
        assert_eq!(
            phone(Some(&json!("2122561960"))).as_deref(),
            Some("2122561960")
        );
        assert_eq!(phone(Some(&json!("__________"))), None);
        assert_eq!(coord(Some(&json!("0")), 90.0), None);
        assert_eq!(coord(Some(&json!("40.7")), 90.0), Some(40.7));
    }
}
