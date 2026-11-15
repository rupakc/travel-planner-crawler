//! `kinds.toml`: per-kind policy (spec §5.7, §17.8) and the rules no data file may break.

use std::collections::BTreeMap;

use serde::Deserialize;
use vc_core::kinds::KindStorage;
use vc_core::licence::SourceClass;

/// The parsed `kinds.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KindsFile {
    pub kinds: BTreeMap<String, KindPolicy>,
}

/// One `[kinds.<id>]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KindPolicy {
    /// Schema version: selects `schemas/<id>.v<version>.json`.
    pub version: u32,
    /// Record or observation.
    pub storage: KindStorage,
    /// Allowed source classes (empty = all; never allowed for high-stakes kinds).
    #[serde(default)]
    pub source_classes: Vec<SourceClass>,
    /// JSON pointers that must appear verbatim in the source text (`*` = every array element).
    #[serde(default)]
    pub verbatim: Vec<String>,
    /// High-stakes kind.
    #[serde(default)]
    pub high_stakes: bool,
    /// Independent reference sources that must agree (high-stakes kinds with `reference`).
    #[serde(default)]
    pub min_reference_agreement: u8,
    /// Observation series key template, e.g. `fx:{base_currency}:{quote_currency}`.
    #[serde(default)]
    pub series_key: Option<String>,
}

/// Kinds that are high-stakes whatever `kinds.toml` says (spec §5.7 step 4, §17.8).
pub const HIGH_STAKES_KINDS: [&str; 3] = ["visa_rule", "advisory", "emergency_numbers"];

/// High-stakes kinds that come from `official` sources only (no reference agreement route).
pub const OFFICIAL_ONLY_KINDS: [&str; 2] = ["visa_rule", "advisory"];

/// The smallest `min_reference_agreement` that lets `reference` sources produce a
/// high-stakes kind.
pub const MIN_REFERENCE_AGREEMENT: u8 = 2;

/// Checks one kind's policy against the hard rules; returns the first violation.
pub(crate) fn check(id: &str, p: &KindPolicy) -> Result<(), String> {
    if !valid_id(id) {
        return Err(format!("kind id {id:?} must match ^[a-z][a-z0-9_]*$"));
    }
    if p.version == 0 {
        return Err(format!("{id}: version starts at 1"));
    }
    if HIGH_STAKES_KINDS.contains(&id) && !p.high_stakes {
        return Err(format!(
            "{id}: is a high-stakes kind; high_stakes = true is required"
        ));
    }
    if p.high_stakes {
        if p.source_classes.is_empty() {
            return Err(format!(
                "{id}: a high-stakes kind must list its source classes (empty means all)"
            ));
        }
        let official_only = OFFICIAL_ONLY_KINDS.contains(&id);
        for c in &p.source_classes {
            match c {
                SourceClass::Official => {}
                SourceClass::Reference if !official_only => {
                    if p.min_reference_agreement < MIN_REFERENCE_AGREEMENT {
                        return Err(format!(
                            "{id}: reference sources need min_reference_agreement >= {MIN_REFERENCE_AGREEMENT}"
                        ));
                    }
                }
                other => {
                    return Err(format!(
                        "{id}: high-stakes kind may not come from {} sources",
                        other.as_str()
                    ));
                }
            }
        }
    }
    let has_reference = p.source_classes.contains(&SourceClass::Reference);
    if p.min_reference_agreement > 0 && !(p.high_stakes && has_reference) {
        return Err(format!(
            "{id}: min_reference_agreement only applies to high-stakes kinds that allow reference"
        ));
    }
    match (p.storage, &p.series_key) {
        (KindStorage::Observation, None) => Err(format!(
            "{id}: observation kinds need a series_key template"
        )),
        (KindStorage::Record, Some(_)) => Err(format!("{id}: record kinds have no series_key")),
        _ => Ok(()),
    }
}

fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// One piece of a parsed series key template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    Lit(String),
    /// `{field}` or `{field:0N}` (zero-padded integer).
    Field {
        name: String,
        pad: usize,
    },
}

/// The placeholder for the first resolved GeoNames id (not a data field).
pub(crate) const PLACE_ID: &str = "place_id";

/// Parses `fx:{base_currency}:{quote_currency}` / `weather_normal:{place_id}:{month:02}`.
pub(crate) fn parse_template(t: &str) -> Result<Vec<Piece>, String> {
    let mut out = Vec::new();
    let mut rest = t;
    while let Some(open) = rest.find('{') {
        if open > 0 {
            out.push(Piece::Lit(rest[..open].to_string()));
        }
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| format!("unclosed '{{' in series_key {t:?}"))?
            + open;
        let inner = &rest[open + 1..close];
        let (name, pad) = match inner.split_once(':') {
            Some((n, spec)) => {
                let pad = spec
                    .strip_prefix('0')
                    .and_then(|w| w.parse::<usize>().ok())
                    .filter(|w| (1..=12).contains(w))
                    .ok_or_else(|| format!("bad format {spec:?} in series_key {t:?}"))?;
                (n, pad)
            }
            None => (inner, 0),
        };
        if !valid_id(name) {
            return Err(format!("bad placeholder {inner:?} in series_key {t:?}"));
        }
        out.push(Piece::Field {
            name: name.to_string(),
            pad,
        });
        rest = &rest[close + 1..];
    }
    if rest.contains('}') {
        return Err(format!("stray '}}' in series_key {t:?}"));
    }
    if !rest.is_empty() {
        out.push(Piece::Lit(rest.to_string()));
    }
    Ok(out)
}

/// Renders a template; `None` when a field is missing, empty or not a scalar.
pub(crate) fn render(
    pieces: &[Piece],
    data: &serde_json::Value,
    place_id: Option<i64>,
) -> Option<String> {
    let mut s = String::new();
    for p in pieces {
        match p {
            Piece::Lit(l) => s.push_str(l),
            Piece::Field { name, pad } => {
                let v = if name == PLACE_ID {
                    place_id?.to_string()
                } else {
                    match data.get(name)? {
                        serde_json::Value::String(x) if !x.trim().is_empty() => {
                            x.trim().to_string()
                        }
                        serde_json::Value::Number(n) => n.to_string(),
                        _ => return None,
                    }
                };
                if *pad > 0 {
                    s.push_str(&format!("{v:0>pad$}"));
                } else {
                    s.push_str(&v);
                }
            }
        }
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn template_round_trip() {
        let p = parse_template("weather_normal:{place_id}:{month:02}").unwrap();
        assert_eq!(
            render(&p, &json!({"month": 7}), Some(42)).as_deref(),
            Some("weather_normal:42:07")
        );
        assert_eq!(
            render(&p, &json!({"month": 11}), Some(42)).as_deref(),
            Some("weather_normal:42:11")
        );
        assert!(parse_template("x:{a").is_err());
        assert!(parse_template("x:a}").is_err());
        assert!(parse_template("x:{a:2}").is_err());
        assert!(parse_template("x:{A}").is_err());
        assert_eq!(
            parse_template("plain").unwrap(),
            vec![Piece::Lit("plain".into())]
        );
    }

    #[test]
    fn render_rejects_non_scalars_and_blanks() {
        let p = parse_template("k:{a}").unwrap();
        assert_eq!(render(&p, &json!({"a": " "}), None), None);
        assert_eq!(render(&p, &json!({"a": [1]}), None), None);
        assert_eq!(render(&p, &json!({"a": true}), None), None);
    }
}
