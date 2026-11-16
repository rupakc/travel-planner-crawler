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

/// Kinds that come only from `official` sources, whatever the registry says (no agreement
/// path, no registry loosening).
pub const OFFICIAL_ONLY_KINDS: &[&str] = &["visa_rule", "advisory"];

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
    if OFFICIAL_ONLY_KINDS.contains(&&*kind.id.0) {
        return if class == SourceClass::Official {
            Gate::Allowed
        } else {
            Gate::Denied
        };
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
    /// Its publisher (registrable domain of its URL, see [`publisher_of`]). Agreement counts
    /// independent *publishers*: two sources of one publisher are one voice. `None` (unknown)
    /// falls back to the source id.
    #[serde(default)]
    pub publisher: Option<String>,
}

/// The publisher of a URL host: its registrable domain (Public Suffix List), so
/// `wiki-a.example.org` and `wiki-b.example.org` are one publisher.
pub fn publisher_of(host: &str) -> String {
    vc_core::PolitenessKey::for_host(host).0
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

/// The country a country-level record is about (`country_code`, else `country`), folded.
fn country_of(data: &Value) -> Option<String> {
    ["country_code", "country"].iter().find_map(|f| {
        data.get(*f)
            .and_then(Value::as_str)
            .map(fold)
            .filter(|s| !s.is_empty())
    })
}

/// Whether two records are about the same place: they share a resolved place id, or neither
/// has a place id and both name the same country (country-level records such as national
/// emergency numbers). Records without any location never agree (nor conflict), and a
/// city-level record never matches a country-level one.
fn same_place(a: &[i64], a_data: &Value, b: &[i64], b_data: &Value) -> bool {
    match (a.is_empty(), b.is_empty()) {
        (false, false) => a.iter().any(|x| b.contains(x)),
        (true, true) => match (country_of(a_data), country_of(b_data)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
        _ => false,
    }
}

/// Agreement and conflict of a record with its peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerVerdict {
    /// Independent sources (this one included) whose verbatim values equal this record's.
    pub agreeing_sources: u8,
    /// Some peer from another source disagrees on a verbatim field both carry.
    pub conflict: bool,
}

/// Compares a record with peers of the same kind and place from other sources and other
/// publishers (`publisher` is this record's, from [`publisher_of`]). Only `official` and
/// `reference` peers count towards agreement, once per independent publisher.
pub fn compare_with_peers(
    kind: &KindDef,
    source: &SourceId,
    publisher: Option<&str>,
    place_ids: &[i64],
    data: &Value,
    peers: &[PeerRecord],
) -> PeerVerdict {
    let mine = verbatim_values(kind, data);
    let mut agree: BTreeSet<String> = BTreeSet::new();
    let mut conflict = false;
    for p in peers {
        let same_publisher =
            matches!((publisher, p.publisher.as_deref()), (Some(a), Some(b)) if a == b);
        if p.kind != kind.id
            || &p.source_id == source
            || same_publisher
            || !same_place(place_ids, data, &p.place_ids, &p.data)
        {
            continue;
        }
        let theirs = verbatim_values(kind, &p.data);
        // Agreement: the peer covers and equals EVERY non-empty fact group of this record (a
        // peer agreeing on `numbers` alone never vouches for an embassy phone it lacks).
        // Conflict: some group both carry differs.
        let mut covers_all = true;
        let mut differs = false;
        let mut compared = false;
        for ((_, a), (_, b)) in mine.iter().zip(theirs.iter()) {
            if a.is_empty() {
                continue;
            }
            if b.is_empty() {
                covers_all = false;
                continue;
            }
            compared = true;
            if a != b {
                differs = true;
            }
        }
        if !compared {
            continue;
        }
        if differs {
            conflict = true;
        } else if covers_all
            && matches!(
                p.source_class,
                SourceClass::Official | SourceClass::Reference
            )
        {
            agree.insert(match &p.publisher {
                Some(pb) => format!("publisher:{pb}"),
                None => format!("source:{}", p.source_id.0),
            });
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
            place_ids: vec![1],
            data: b,
            publisher: None,
        };
        let v = compare_with_peers(&k, &SourceId("a".into()), None, &[1], &a, &[peer]);
        assert_eq!(
            v,
            PeerVerdict {
                agreeing_sources: 2,
                conflict: false
            }
        );
    }

    fn peer(src: &str, class: SourceClass, places: Vec<i64>, data: Value) -> PeerRecord {
        PeerRecord {
            source_id: SourceId(src.into()),
            source_class: class,
            kind: KindId::new("emergency_numbers"),
            place_ids: places,
            data,
            publisher: None,
        }
    }

    #[test]
    fn independent_means_another_publisher_not_another_source_id() {
        let k = em(&["/numbers/*/number"]);
        let d = json!({"numbers": [{"number": "112"}]});
        let with_pub = |src: &str, host: &str| PeerRecord {
            publisher: Some(publisher_of(host)),
            ..peer(src, SourceClass::Reference, vec![7], d.clone())
        };
        let me = publisher_of("wiki-a.example.org");
        assert_eq!(me, "example.org");
        // Another source id on the same registrable domain is the same voice.
        let v = compare_with_peers(
            &k,
            &SourceId("a".into()),
            Some(&me),
            &[7],
            &d,
            &[with_pub("b", "wiki-b.example.org")],
        );
        assert_eq!(v.agreeing_sources, 1);
        // Two peer source ids of one other publisher count once.
        let v = compare_with_peers(
            &k,
            &SourceId("a".into()),
            Some(&me),
            &[7],
            &d,
            &[
                with_pub("b", "www.other.example.net"),
                with_pub("c", "m.other.example.net"),
            ],
        );
        assert_eq!(v.agreeing_sources, 2);
        // Two publishers: three voices.
        let v = compare_with_peers(
            &k,
            &SourceId("a".into()),
            Some(&me),
            &[7],
            &d,
            &[
                with_pub("b", "other.example.net"),
                with_pub("c", "third.example.com"),
            ],
        );
        assert_eq!(v.agreeing_sources, 3);
    }

    #[test]
    fn peer_agreeing_only_on_numbers_does_not_vouch_for_planted_embassy_phone() {
        let k = em(&[
            "/numbers/*/number",
            "/embassies/*/phone",
            "/embassies/*/website_url",
        ]);
        let mine = json!({"numbers": [{"number": "112"}],
            "embassies": [{"phone": "+34 806 123 456", "website_url": "https://evil.example/e"}]});
        let p = peer(
            "b",
            SourceClass::Reference,
            vec![7],
            json!({"numbers": [{"number": "112"}]}),
        );
        let v = compare_with_peers(&k, &SourceId("a".into()), None, &[7], &mine, &[p]);
        assert_eq!(v.agreeing_sources, 1, "peer must cover every fact group");
        assert!(!v.conflict, "a missing group is not a disagreement");

        // A peer covering and equalling every non-empty group does agree.
        let p = peer(
            "b",
            SourceClass::Reference,
            vec![7],
            json!({"numbers": [{"number": "112"}],
                "embassies": [{"phone": "+34806123456", "website_url": "https://evil.example/e"}]}),
        );
        let v = compare_with_peers(&k, &SourceId("a".into()), None, &[7], &mine, &[p]);
        assert_eq!(v.agreeing_sources, 2);
    }

    #[test]
    fn records_without_place_ids_never_agree() {
        let k = em(&["/numbers/*/number"]);
        let d = json!({"numbers": [{"number": "112"}]});
        let p = peer("b", SourceClass::Reference, vec![], d.clone());
        let v = compare_with_peers(&k, &SourceId("a".into()), None, &[], &d, &[p]);
        assert_eq!(v.agreeing_sources, 1);
        let p = peer("b", SourceClass::Reference, vec![], d.clone());
        let v = compare_with_peers(&k, &SourceId("a".into()), None, &[3], &d, &[p]);
        assert_eq!(v.agreeing_sources, 1);
        let p = peer("b", SourceClass::Reference, vec![4], d.clone());
        let v = compare_with_peers(&k, &SourceId("a".into()), None, &[3], &d, &[p]);
        assert_eq!(v.agreeing_sources, 1);

        // Country-level records (no place id) match on their country only.
        let es = json!({"country": "ES", "numbers": [{"number": "112"}]});
        let p = peer("b", SourceClass::Reference, vec![], es.clone());
        let v = compare_with_peers(&k, &SourceId("a".into()), None, &[], &es, &[p]);
        assert_eq!(v.agreeing_sources, 2);
        let fr = json!({"country": "FR", "numbers": [{"number": "112"}]});
        let p = peer("b", SourceClass::Reference, vec![], fr);
        let v = compare_with_peers(&k, &SourceId("a".into()), None, &[], &es, &[p]);
        assert_eq!(v.agreeing_sources, 1);
        // A city-level peer never matches a country-level record.
        let p = peer("b", SourceClass::Reference, vec![3], es.clone());
        let v = compare_with_peers(&k, &SourceId("a".into()), None, &[], &es, &[p]);
        assert_eq!(v.agreeing_sources, 1);
    }

    fn kind(id: &str, classes: &[SourceClass], high: bool, agree: u8) -> KindDef {
        KindDef {
            id: KindId::new(id),
            allowed_source_classes: classes.to_vec(),
            high_stakes: high,
            min_reference_agreement: agree,
            ..em(&[])
        }
    }

    #[test]
    fn visa_rule_and_advisory_stay_official_only_whatever_kinds_toml_says() {
        use SourceClass::*;
        for id in ["visa_rule", "advisory"] {
            // A loosened registry entry: reference/editorial allowed, not high-stakes,
            // with an agreement threshold. The code floor ignores all of it.
            for (high, agree) in [(false, 0), (true, 2), (false, 2)] {
                let k = kind(id, &[Official, Reference, Editorial], high, agree);
                assert_eq!(source_gate(&k, Official), Gate::Allowed, "{id}");
                assert_eq!(source_gate(&k, Reference), Gate::Denied, "{id}");
                assert_eq!(source_gate(&k, Editorial), Gate::Denied, "{id}");
            }
            // kinds.toml can still make it stricter.
            let k = kind(id, &[Reference], true, 0);
            assert_eq!(source_gate(&k, Official), Gate::Denied, "{id}");
        }
        // emergency_numbers keeps the agreement path.
        let k = kind("emergency_numbers", &[Official, Reference], true, 2);
        assert_eq!(source_gate(&k, Reference), Gate::NeedsAgreement(2));
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
