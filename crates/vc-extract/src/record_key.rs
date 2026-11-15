//! Stable record keys (spec §5.6): the JSON-LD `@id` or a same-page URL anchor, else a hash of
//! (kind, normalised name, date/geo); a nameless record gets a per-page singleton key.

use serde_json::Value;
use vc_core::{CanonicalUrl, KindId, RecordKey};

use crate::fold::fold;

const NAME_FIELDS: &[&str] = &["name", "title", "headline", "question"];
const DATE_FIELDS: &[&str] = &["start_date", "date", "start", "published", "valid_from"];

fn str_field<'a>(data: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields
        .iter()
        .filter_map(|f| data.get(*f).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
}

fn geo_part(data: &Value) -> Option<String> {
    let g = data.get("geo")?;
    let lat = g.get("lat").and_then(Value::as_f64)?;
    let lon = g.get("lon").and_then(Value::as_f64)?;
    Some(format!("{lat:.3},{lon:.3}"))
}

/// The key for one record. `id` is the JSON-LD `@id` (structured data), `ordinal` the index
/// of this record among the page's nameless records of the same kind.
pub fn record_key(
    kind: &KindId,
    page: &CanonicalUrl,
    id: Option<&str>,
    data: &Value,
    ordinal: usize,
) -> RecordKey {
    if let Some(id) = id.map(str::trim).filter(|s| !s.is_empty()) {
        return RecordKey::from_parts(&["@id", &kind.0, id]);
    }
    if let Some(anchor) = str_field(data, &["url"]).and_then(|u| same_page_anchor(page, u)) {
        return RecordKey::from_parts(&["#", &kind.0, &anchor]);
    }
    match str_field(data, NAME_FIELDS) {
        Some(name) => {
            let when = str_field(data, DATE_FIELDS)
                .map(|d| d.chars().take(10).collect::<String>())
                .or_else(|| geo_part(data))
                .unwrap_or_default();
            RecordKey::from_parts(&[&kind.0, &fold(name), &when])
        }
        None => RecordKey::from_parts(&[&kind.0, "page", &ordinal.to_string()]),
    }
}

fn same_page_anchor(page: &CanonicalUrl, u: &str) -> Option<String> {
    let full = page.as_url().join(u).ok()?;
    let frag = full.fragment()?.trim();
    if frag.is_empty() {
        return None;
    }
    let mut base = full.clone();
    base.set_fragment(None);
    let mut p = page.as_url().clone();
    p.set_fragment(None);
    (base == p).then(|| frag.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys() {
        let k = KindId::new("event");
        let page = CanonicalUrl::parse("https://example.org/events").unwrap();
        let a = record_key(
            &k,
            &page,
            None,
            &json!({"name": "Jazz  Night", "start_date": "2026-11-20T19:00"}),
            0,
        );
        let b = record_key(
            &k,
            &page,
            None,
            &json!({"name": "jazz night", "start_date": "2026-11-20"}),
            3,
        );
        assert_eq!(a, b);
        let c = record_key(
            &k,
            &page,
            None,
            &json!({"name": "Jazz Night", "start_date": "2026-11-21"}),
            0,
        );
        assert_ne!(a, c);
        let d = record_key(&k, &page, Some("https://example.org/e/1"), &json!({}), 0);
        assert_eq!(
            d,
            record_key(
                &k,
                &page,
                Some("https://example.org/e/1"),
                &json!({"name": "x"}),
                9
            )
        );
        let e = record_key(&k, &page, None, &json!({"url": "#jazz", "name": "x"}), 0);
        assert_eq!(
            e,
            record_key(
                &k,
                &page,
                None,
                &json!({"url": "https://example.org/events#jazz", "name": "y"}),
                0
            )
        );
        assert_ne!(
            record_key(&k, &page, None, &json!({}), 0),
            record_key(&k, &page, None, &json!({}), 1)
        );
    }
}
