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
pub fn verbatim_values(kind: &KindDef, data: &Value) -> Vec<(String, BTreeSet<String>)> {
    kind.verbatim_fields
        .iter()
        .map(|ptr| {
            let mut set = BTreeSet::new();
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
            (ptr.clone(), set)
        })
        .collect()
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
