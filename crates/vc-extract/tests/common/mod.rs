//! Small local test kinds (vc-kinds is built in parallel) and builders.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use testkit::{FakeClock, FakeLlm, MemLedger};
use vc_config::{ExtractConfig, LlmConfig};
use vc_core::ids::{CaptureId, CaptureRef};
use vc_core::kinds::{KindDef, KindRegistry, KindStorage};
use vc_core::licence::{Licence, LicenceClass, SourceClass};
use vc_core::llm::Usd;
use vc_core::parse::{Link, LinkKind, Parsed};
use vc_core::records::Provenance;
use vc_core::usepolicy::UsePolicy;
use vc_core::{CanonicalUrl, KindId, SourceId};
use vc_extract::Extractor;

/// A map-backed registry.
pub struct MapKinds(pub BTreeMap<KindId, KindDef>);

impl KindRegistry for MapKinds {
    fn get(&self, id: &KindId) -> Option<&KindDef> {
        self.0.get(id)
    }
    fn all(&self) -> Vec<&KindDef> {
        self.0.values().collect()
    }
}

fn kind(
    id: &str,
    schema: Value,
    verbatim: &[&str],
    classes: &[SourceClass],
    high: bool,
    agree: u8,
    storage: KindStorage,
) -> KindDef {
    let prompt = format!("Extract {id} records.");
    KindDef {
        id: KindId::new(id),
        version: 1,
        schema,
        prompt_hash: blake3::hash(prompt.as_bytes()).to_hex().to_string(),
        prompt,
        verbatim_fields: verbatim.iter().map(|s| s.to_string()).collect(),
        allowed_source_classes: classes.to_vec(),
        storage,
        high_stakes: high,
        min_reference_agreement: agree,
    }
}

pub fn geo_schema() -> Value {
    json!({"type": "object", "additionalProperties": false,
           "properties": {"lat": {"type": "number"}, "lon": {"type": "number"}}, "required": ["lat", "lon"]})
}

pub fn kinds() -> Arc<MapKinds> {
    use SourceClass::*;
    let mut m = BTreeMap::new();
    // The real v1 event kind (T7): schema file + kinds.toml policy, `*` expanded as the
    // vc-kinds loader does.
    let event_schema: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schemas/event.v1.json"
        ))
        .expect("schemas/event.v1.json"),
    )
    .expect("event schema");
    let event = kind(
        "event",
        event_schema,
        &[
            "/start_date",
            "/end_date",
            "/start_time",
            "/price/text",
            "/price/amount",
            "/ticket_url",
            "/source_url",
            "/as_of",
            "/evidence/0",
            "/evidence/1",
            "/evidence/2",
        ],
        &[Official, Reference, Editorial, CommercialApi],
        false,
        0,
        KindStorage::Record,
    );
    let attraction = kind(
        "attraction",
        json!({"type": "object", "additionalProperties": false, "required": ["name"],
          "properties": {
            "name": {"type": "string", "minLength": 1}, "description": {"type": "string"},
            "city": {"type": "string"}, "country": {"type": "string"}, "address": {"type": "string"},
            "phone": {"type": "string"}, "website": {"type": "string"}, "url": {"type": "string"},
            "booking_url": {"type": "string"}, "price": {"type": ["number", "string"]},
            "opening_hours": {"type": "string"}, "geo": geo_schema()}}),
        &["/phone", "/website", "/booking_url", "/price"],
        &[],
        false,
        0,
        KindStorage::Record,
    );
    let place = kind(
        "place",
        json!({"type": "object", "additionalProperties": false, "required": ["name"],
          "properties": {"name": {"type": "string"}, "description": {"type": "string"},
            "url": {"type": "string"}, "country": {"type": "string"}, "city": {"type": "string"},
            "address": {"type": "string"}, "region": {"type": "string"}, "geo": geo_schema()}}),
        &[],
        &[],
        false,
        0,
        KindStorage::Record,
    );
    let emergency = kind(
        "emergency_numbers",
        json!({"type": "object", "additionalProperties": false, "required": ["country", "numbers"],
          "properties": {"country": {"type": "string"},
            "numbers": {"type": "array", "minItems": 1, "items": {"type": "object",
              "additionalProperties": false, "required": ["service", "value"],
              "properties": {"service": {"type": "string"}, "value": {"type": "string"}}}}}}),
        &["/numbers/*/value"],
        &[Official, Reference],
        true,
        2,
        KindStorage::Record,
    );
    let visa = kind(
        "visa_rule",
        json!({"type": "object", "additionalProperties": false, "required": ["to_country", "visa_required"],
          "properties": {"from_country": {"type": "string"}, "to_country": {"type": "string"},
            "visa_required": {"type": "boolean"}, "max_stay_days": {"type": "integer"},
            "apply_url": {"type": "string"}}}),
        &["/apply_url"],
        &[Official],
        true,
        0,
        KindStorage::Record,
    );
    let fx = kind(
        "fx_rate",
        json!({"type": "object", "additionalProperties": false, "required": ["base", "quote", "rate"],
          "properties": {"base": {"type": "string"}, "quote": {"type": "string"},
            "rate": {"type": "number"}, "date": {"type": "string"}, "series_key": {"type": "string"}}}),
        &["/rate", "/date"],
        &[Official, Reference],
        false,
        0,
        KindStorage::Observation,
    );
    // A misconfigured registry entry: high-stakes kind with no class restriction. The hard
    // floor must still refuse non-official sources.
    let advisory = kind(
        "advisory",
        json!({"type": "object", "properties": {"country": {"type": "string"}, "level": {"type": "integer"}}}),
        &[],
        &[],
        false,
        0,
        KindStorage::Record,
    );
    for k in [event, attraction, place, emergency, visa, fx, advisory] {
        m.insert(k.id.clone(), k);
    }
    Arc::new(MapKinds(m))
}

pub fn capture() -> CaptureRef {
    CaptureRef {
        capture_id: serde_json::from_value::<CaptureId>(json!("01JD8Z0000000000000000000A"))
            .expect("ulid"),
        captured_at: Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap(),
    }
}

pub fn prov(url: &str, source: &str, class: SourceClass) -> Provenance {
    let url = CanonicalUrl::parse(url).expect("url");
    Provenance {
        source_id: SourceId(source.into()),
        source_class: class,
        licence: Licence {
            class: LicenceClass::FactsOnly,
            spdx: None,
            attribution: "test".into(),
            url: None,
        },
        url_key: url.key(),
        url,
        capture: capture(),
        use_policy: UsePolicy::ALLOW_ALL,
    }
}

pub fn parsed(title: &str, text: &str) -> Parsed {
    Parsed {
        title: Some(title.into()),
        main_text: Some(text.into()),
        lang: Some("en".into()),
        ..Parsed::default()
    }
}

pub fn with_links(mut p: Parsed, links: &[&str]) -> Parsed {
    p.links = links
        .iter()
        .map(|l| Link {
            url: CanonicalUrl::parse(l).expect("link"),
            anchor: String::new(),
            kind: LinkKind::Page,
            rel: vec![],
            lastmod: None,
        })
        .collect();
    p
}

pub struct Rig {
    pub llm: Arc<FakeLlm>,
    pub ledger: Arc<MemLedger>,
    pub ex: Extractor,
}

pub fn rig() -> Rig {
    rig_with_caps(Usd::from_dollars(1.5), Usd::from_dollars(40.0))
}

pub fn rig_with_caps(day: Usd, month: Usd) -> Rig {
    let clock = Arc::new(FakeClock::new(
        Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap(),
    ));
    let llm = Arc::new(FakeLlm::new());
    let ledger = Arc::new(MemLedger::new(clock, day, month));
    let ex = Extractor::new(kinds(), ExtractConfig::default()).with_llm(
        llm.clone(),
        ledger.clone(),
        LlmConfig::default(),
    );
    Rig { llm, ledger, ex }
}

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture")
}

/// Loads `tests/fixtures/<name>.json`: {"title", "text", "links"?, "structured"?}.
pub fn page(name: &str) -> Parsed {
    let v: Value = serde_json::from_str(&fixture(&format!("{name}.json"))).expect("fixture json");
    let mut p = parsed(
        v["title"].as_str().unwrap_or(""),
        v["text"].as_str().unwrap_or(""),
    );
    if let Some(s) = v.get("structured").and_then(Value::as_array) {
        p.structured = s.clone();
    }
    let links: Vec<&str> = v
        .get("links")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    with_links(p, &links)
}
