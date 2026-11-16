//! The source-class gate (spec §5.7 step 4, §17.8) and cross-source agreement/conflicts.
//!
//! The gate is enforced from the kind registry **and** from a hard-coded floor: whatever the
//! registry says, `visa_rule` and `advisory` come only from `official` sources and
//! `emergency_numbers` from `official`, or from `reference` when two independent reference
//! sources agree. Kind data can only make the gate stricter.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vc_core::kinds::KindDef;
use vc_core::licence::SourceClass;
use vc_core::{KindId, SourceId};

use crate::fold::fold;
use crate::grounding::{FieldType, expand_pointer, get_path, infer_type, normalise_url};

/// Kinds that are high-stakes regardless of the registry's flag.
pub const HIGH_STAKES_KINDS: &[&str] = &["visa_rule", "advisory", "emergency_numbers"];

/// Kinds that may come from `reference` sources when enough of them agree.
pub const REFERENCE_AGREEMENT_KINDS: &[&str] = &["emergency_numbers"];

/// The gate's verdict for one (kind, source class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// The class may produce the kind.
    Allowed,
    /// Allowed only when this many independent sources (including this one) agree.
    NeedsAgreement(u8),
    /// Never.
    Denied,
}

/// Whether a kind is high-stakes (registry flag or the hard-coded list).
pub fn is_high_stakes(kind: &KindDef) -> bool {
    kind.high_stakes || HIGH_STAKES_KINDS.contains(&&*kind.id.0)
}

/// The source-class gate.
pub fn source_gate(kind: &KindDef, class: SourceClass) -> Gate {
    if !kind.allows_source_class(class) {
        return Gate::Denied;
    }
    if !is_high_stakes(kind) {
        return Gate::Allowed;
    }
    match class {
        SourceClass::Official => Gate::Allowed,
        SourceClass::Reference
            if kind.min_reference_agreement > 0
                || REFERENCE_AGREEMENT_KINDS.contains(&&*kind.id.0) =>
        {
            Gate::NeedsAgreement(kind.min_reference_agreement.max(2))
        }
        _ => Gate::Denied,
    }
}

/// A record of the same kind from another source, supplied by the caller (from the index, or
/// held-back candidates awaiting agreement) for agreement and conflict checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerRecord {
    /// Its source.
    pub source_id: SourceId,
    /// Its source class.
    pub source_class: SourceClass,
    /// Its kind.
    pub kind: KindId,
    /// Its places.
    pub place_ids: Vec<i64>,
    /// Its data.
    pub data: Value,
}

/// Normalised value sets of a record's verbatim fields, per pointer.
/// Concrete indices are folded into `*`, so `/numbers/0/number` and `/numbers/1/number`
/// compare as one order-independent set.
pub fn verbatim_values(kind: &KindDef, data: &Value) -> Vec<(String, BTreeSet<String>)> {
    let mut groups: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
    for ptr in &kind.verbatim_fields {
        let pattern: String = ptr
            .split('/')
            .map(|s| {
                if s.parse::<usize>().is_ok() || s == "-" {
                    "*"
                } else {
                    s
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        if !is_fact_pointer(&pattern) {
            continue;
        }
        let set = groups.entry(pattern).or_default();
        {
            for p in expand_pointer(data, ptr) {
                let Some(v) = get_path(data, &p) else {
                    continue;
                };
                let leaves: Vec<&Value> = match v {
                    Value::Array(a) => a.iter().collect(),
                    o => vec![o],
                };
                for leaf in leaves {
                    set.insert(norm_value(infer_type(&kind.schema, ptr, leaf), leaf));
                }
            }
        }
    }
    groups.into_iter().collect()
}

/// Provenance-like verbatim fields (where a source quotes itself) never count as agreement
/// or conflict: `source_url`, `as_of`, `evidence`, free-text `text`, page dates.
fn is_fact_pointer(pattern: &str) -> bool {
    const NOT_FACTS: &[&str] = &[
        "source_url",
        "as_of",
        "evidence",
        "text",
        "published_date",
        "updated_date",
    ];
    !pattern.split('/').any(|seg| NOT_FACTS.contains(&seg))
}

fn norm_value(ft: FieldType, v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        o => o.to_string(),
    };
    match ft {
        FieldType::Phone => {
            let d: String = s.chars().filter(char::is_ascii_digit).collect();
            if d.is_empty() { fold(&s) } else { d }
        }
        FieldType::Url => normalise_url(&s)
            .map(|(n, _)| n)
            .unwrap_or_else(|| fold(&s)),
        FieldType::Date => s.chars().take(10).collect(),
        FieldType::Price | FieldType::Text => fold(&s),
    }
}

fn same_place(a: &[i64], b: &[i64]) -> bool {
    (a.is_empty() && b.is_empty()) || a.iter().any(|x| b.contains(x))
}

/// Agreement and conflict of a record with its peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerVerdict {
    /// Independent sources (this one included) whose verbatim values equal this record's.
    pub agreeing_sources: u8,
    /// Some peer from another source disagrees on a verbatim field both carry.
    pub conflict: bool,
}

/// Compares a record with peers of the same kind and place from other sources. Only
/// `official` and `reference` peers count towards agreement.
pub fn compare_with_peers(
    kind: &KindDef,
    source: &SourceId,
    place_ids: &[i64],
    data: &Value,
    peers: &[PeerRecord],
) -> PeerVerdict {
    let mine = verbatim_values(kind, data);
    let mut agree: BTreeSet<&SourceId> = BTreeSet::new();
    let mut conflict = false;
    for p in peers {
        if p.kind != kind.id || &p.source_id == source || !same_place(place_ids, &p.place_ids) {
            continue;
        }
        let theirs = verbatim_values(kind, &p.data);
        let mut all_equal = true;
        let mut compared = false;
        for ((_, a), (_, b)) in mine.iter().zip(theirs.iter()) {
            if a.is_empty() || b.is_empty() {
                continue;
            }
            compared = true;
            if a != b {
                all_equal = false;
            }
        }
        if !compared {
            continue;
        }
        if all_equal {
            if matches!(
                p.source_class,
                SourceClass::Official | SourceClass::Reference
            ) {
                agree.insert(&p.source_id);
            }
        } else {
            conflict = true;
        }
    }
    PeerVerdict {
        agreeing_sources: u8::try_from(agree.len() + 1).unwrap_or(u8::MAX),
        conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vc_core::kinds::KindStorage;

    fn em(ptrs: &[&str]) -> KindDef {
        KindDef {
            id: KindId::new("emergency_numbers"),
            version: 1,
            schema: json!({"type": "object"}),
            prompt: String::new(),
            prompt_hash: String::new(),
            verbatim_fields: ptrs.iter().map(|s| s.to_string()).collect(),
            allowed_source_classes: vec![],
            storage: KindStorage::Record,
            high_stakes: false,
            min_reference_agreement: 0,
        }
    }

    #[test]
    fn concrete_indices_compare_as_sets_and_provenance_is_ignored() {
        let k = em(&[
            "/numbers/0/number",
            "/numbers/1/number",
            "/source_url",
            "/evidence/0",
        ]);
        let a = json!({"numbers": [{"number": "112"}, {"number": "091"}], "source_url": "https://a.example/x", "evidence": ["x"]});
        let b = json!({"numbers": [{"number": "091"}, {"number": "112"}], "source_url": "https://b.example/y", "evidence": ["y"]});
        let peer = PeerRecord {
            source_id: SourceId("b".into()),
            source_class: SourceClass::Reference,
            kind: k.id.clone(),
            place_ids: vec![],
            data: b,
        };
        let v = compare_with_peers(&k, &SourceId("a".into()), &[], &a, &[peer]);
        assert_eq!(
            v,
            PeerVerdict {
                agreeing_sources: 2,
                conflict: false
            }
        );
    }

    #[test]
    fn hard_floor() {
        let k = em(&[]);
        assert_eq!(source_gate(&k, SourceClass::Official), Gate::Allowed);
        assert_eq!(
            source_gate(&k, SourceClass::Reference),
            Gate::NeedsAgreement(2)
        );
        assert_eq!(source_gate(&k, SourceClass::Editorial), Gate::Denied);
    }
}
