//! Pure helpers: canonical JSON and hashing, name normalisation, geo and text extraction.

use serde_json::{Map, Value};
use vc_core::Error;
use vc_core::licence::LicenceClass;
use vc_core::records::{ExtractionMethod, MAX_EXCERPT_CHARS};
use vc_core::usepolicy::{IndexUse, ServeUse, UsePolicy};

/// Max characters of body text put into the FTS column / an embedding input.
pub(crate) const MAX_SEARCH_TEXT: usize = 20_000;
/// Max characters of an embedding input.
pub(crate) const MAX_EMBED_TEXT: usize = 8_000;

/// Maps a database error to [`Error::Db`] (the message only; never row content).
pub(crate) fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

/// The JSON value with every object's keys sorted (independent of serde_json's map order).
pub(crate) fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), canonical(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// blake3 of the canonical JSON (32 bytes), as stored in `record_versions.data_hash`.
pub(crate) fn data_hash(v: &Value) -> Vec<u8> {
    let s = canonical(v).to_string();
    blake3::hash(s.as_bytes()).as_bytes().to_vec()
}

/// Lowercase hex.
pub(crate) fn to_hex(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// Normalised entity name: lowercase alphanumerics, single spaces.
pub(crate) fn norm_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The display name of a record: its title, else `data.name`.
pub(crate) fn display_name(title: Option<&str>, data: &Value) -> Option<String> {
    title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| {
            data.get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        })
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn point_in(o: &Value) -> Option<(f64, f64)> {
    let lat = num(o.get("lat")).or_else(|| num(o.get("latitude")))?;
    let lon = num(o.get("lon"))
        .or_else(|| num(o.get("lng")))
        .or_else(|| num(o.get("longitude")))?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// A record's point: `data.geo`, `data.location`, `data.coordinates`, or top-level lat/lon.
pub(crate) fn geo_of(data: &Value) -> Option<(f64, f64)> {
    ["geo", "location", "coordinates"]
        .iter()
        .filter_map(|k| data.get(*k))
        .find_map(point_in)
        .or_else(|| point_in(data))
}

fn push_strings(v: &Value, out: &mut String) {
    if out.len() >= MAX_SEARCH_TEXT {
        return;
    }
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push(' ');
        }
        Value::Array(a) => a.iter().for_each(|x| push_strings(x, out)),
        Value::Object(m) => m.values().for_each(|x| push_strings(x, out)),
        _ => {}
    }
}

/// Truncates to at most `n` characters.
pub(crate) fn truncate_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// The body text indexed for FTS: the excerpt plus every string leaf of the data.
pub(crate) fn body_text(excerpt: Option<&str>, data: &Value) -> String {
    let mut out = String::new();
    if let Some(e) = excerpt {
        out.push_str(e);
        out.push(' ');
    }
    push_strings(data, &mut out);
    truncate_chars(out.trim(), MAX_SEARCH_TEXT)
}

/// The text embedded for a record.
pub(crate) fn embed_text(title: Option<&str>, excerpt: Option<&str>, data: &Value) -> String {
    let mut s = String::new();
    if let Some(t) = title {
        s.push_str(t);
        s.push('\n');
    }
    s.push_str(&body_text(excerpt, data));
    truncate_chars(s.trim(), MAX_EMBED_TEXT)
}

/// The Postgres text-search configuration for a language tag (`simple` when unknown).
pub(crate) fn fts_config(lang: Option<&str>) -> &'static str {
    let l = lang
        .unwrap_or("")
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match l.as_str() {
        "en" => "english",
        "de" => "german",
        "fr" => "french",
        "es" => "spanish",
        "it" => "italian",
        "pt" => "portuguese",
        "nl" => "dutch",
        "ru" => "russian",
        "sv" => "swedish",
        "no" | "nb" | "nn" => "norwegian",
        "da" => "danish",
        "fi" => "finnish",
        "hu" => "hungarian",
        "ro" => "romanian",
        "tr" => "turkish",
        "el" => "greek",
        "id" => "indonesian",
        "ga" => "irish",
        "lt" => "lithuanian",
        "ne" => "nepali",
        "ar" => "arabic",
        "hy" => "armenian",
        "eu" => "basque",
        "ca" => "catalan",
        "hi" => "hindi",
        "sr" => "serbian",
        "ta" => "tamil",
        "yi" => "yiddish",
        _ => "simple",
    }
}

/// How much of a record may be indexed for full-text search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Indexing {
    /// Not indexed (noindex).
    None,
    /// Title only.
    Title,
    /// Title and body.
    Full,
}

pub(crate) fn indexing(p: &UsePolicy) -> Indexing {
    match p.index {
        IndexUse::None => Indexing::None,
        IndexUse::TitleAndLink => Indexing::Title,
        IndexUse::Full if p.serve == ServeUse::LinkOnly => Indexing::Title,
        IndexUse::Full => Indexing::Full,
    }
}

/// Whether a record may contribute claims to an entity (its facts are served).
pub(crate) fn links_entities(p: &UsePolicy) -> bool {
    p.index == IndexUse::Full && p.serve != ServeUse::LinkOnly
}

/// The data and excerpt a client may receive under `p`. Excerpts are capped at
/// [`MAX_EXCERPT_CHARS`] for every licence class, so `facts_only` never gets more.
pub(crate) fn servable(
    p: &UsePolicy,
    data: Value,
    excerpt: Option<String>,
) -> (Value, Option<String>) {
    if p.index != IndexUse::Full || p.serve == ServeUse::LinkOnly {
        return (Value::Object(Map::new()), None);
    }
    let excerpt = match p.serve {
        ServeUse::Full => excerpt.map(|e| truncate_chars(&e, MAX_EXCERPT_CHARS)),
        _ => None,
    };
    (data, excerpt)
}

pub(crate) fn method_str(m: ExtractionMethod) -> &'static str {
    match m {
        ExtractionMethod::Structured => "structured",
        ExtractionMethod::Adapter => "adapter",
        ExtractionMethod::Llm => "llm",
    }
}

pub(crate) fn method_from(s: &str) -> ExtractionMethod {
    match s {
        "adapter" => ExtractionMethod::Adapter,
        "llm" => ExtractionMethod::Llm,
        _ => ExtractionMethod::Structured,
    }
}

pub(crate) fn licence_class_from(s: &str) -> LicenceClass {
    match s {
        "open" => LicenceClass::Open,
        "share_alike" => LicenceClass::ShareAlike,
        "facts_only" => LicenceClass::FactsOnly,
        "api_terms" => LicenceClass::ApiTerms,
        _ => LicenceClass::Restricted,
    }
}

/// Whether `data` names `author` (`author`, `author.name` or `authors[]`, at any depth).
pub(crate) fn mentions_author(data: &Value, author: &str) -> bool {
    match data {
        Value::Object(m) => {
            let hit = match m.get("author") {
                Some(Value::String(s)) => s == author,
                Some(Value::Object(o)) => o.get("name").and_then(Value::as_str) == Some(author),
                _ => false,
            } || matches!(m.get("authors"), Some(Value::Array(a))
                if a.iter().any(|x| x.as_str() == Some(author)));
            hit || m.values().any(|v| mentions_author(v, author))
        }
        Value::Array(a) => a.iter().any(|v| mentions_author(v, author)),
        _ => false,
    }
}

/// A pgvector text literal (`[1,0.5,…]`); rejects empty and non-finite vectors.
pub(crate) fn vector_literal(v: &[f32]) -> Result<String, Error> {
    if v.is_empty() || v.iter().any(|x| !x.is_finite()) {
        return Err(Error::Invalid(
            "embedding must be non-empty and finite".into(),
        ));
    }
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    Ok(format!("[{}]", parts.join(",")))
}

/// The partial HNSW index name for a model (matches migration 0009's naming).
pub(crate) fn hnsw_index_name(model: &str) -> Result<String, Error> {
    if model.is_empty()
        || model.len() > 40
        || !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ':' | '/'))
    {
        return Err(Error::Invalid("embedding model id".into()));
    }
    let s: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    Ok(format!("embeddings_hnsw_{s}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_ignores_key_order() {
        assert_eq!(
            data_hash(&json!({"a": 1, "b": {"y": 2, "x": [1, {"q": 1, "p": 2}]}})),
            data_hash(&json!({"b": {"x": [1, {"p": 2, "q": 1}], "y": 2}, "a": 1}))
        );
        assert_ne!(data_hash(&json!({"a": 1})), data_hash(&json!({"a": 2})));
        assert_eq!(to_hex(&data_hash(&json!(null))).len(), 64);
    }

    #[test]
    fn names_normalise() {
        assert_eq!(
            norm_name("  The Old-Bridge, (Mostar) "),
            "the old bridge mostar"
        );
        assert_eq!(norm_name("Ünter  DEN"), "ünter den");
    }

    #[test]
    fn geo_shapes() {
        assert_eq!(
            geo_of(&json!({"geo": {"lat": 1.5, "lon": 2}})),
            Some((1.5, 2.0))
        );
        assert_eq!(
            geo_of(&json!({"location": {"latitude": "1", "longitude": "2"}})),
            Some((1.0, 2.0))
        );
        assert_eq!(geo_of(&json!({"lat": 1, "lng": 2})), Some((1.0, 2.0)));
        assert_eq!(geo_of(&json!({"lat": 91, "lon": 2})), None);
        assert_eq!(geo_of(&json!({"name": "x"})), None);
    }

    #[test]
    fn authors_found_at_any_depth() {
        let d = json!({"answers": [{"author": "alice"}, {"author": {"name": "bob"}}],
                       "authors": ["carol"]});
        for a in ["alice", "bob", "carol"] {
            assert!(mentions_author(&d, a));
        }
        assert!(!mentions_author(&d, "dave"));
    }

    #[test]
    fn text_helpers() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(fts_config(Some("en-GB")), "english");
        assert_eq!(fts_config(Some("xx")), "simple");
        assert_eq!(fts_config(None), "simple");
        assert!(body_text(Some("ex"), &json!({"a": ["b", {"c": "d"}], "n": 1})).contains('d'));
        assert_eq!(vector_literal(&[1.0, 0.5]).unwrap(), "[1,0.5]");
        assert!(vector_literal(&[f32::NAN]).is_err());
        assert!(vector_literal(&[]).is_err());
    }

    #[test]
    fn index_names() {
        assert_eq!(
            hnsw_index_name("text-embedding-005").unwrap(),
            "embeddings_hnsw_text_embedding_005"
        );
        assert!(hnsw_index_name("x'; DROP TABLE records; --").is_err());
        assert!(hnsw_index_name(&"a".repeat(41)).is_err());
    }

    #[test]
    fn serving_rules() {
        let (d, e) = servable(
            &UsePolicy::ALLOW_ALL,
            json!({"a": 1}),
            Some("x".repeat(500)),
        );
        assert_eq!(d, json!({"a": 1}));
        assert_eq!(e.unwrap().chars().count(), MAX_EXCERPT_CHARS);
        let link_only = UsePolicy {
            serve: ServeUse::LinkOnly,
            ..UsePolicy::ALLOW_ALL
        };
        let (d, e) = servable(&link_only, json!({"a": 1}), Some("x".into()));
        assert_eq!(d, json!({}));
        assert!(e.is_none());
        assert_eq!(indexing(&link_only), Indexing::Title);
        assert!(!links_entities(&link_only));
    }
}
