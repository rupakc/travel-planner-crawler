//! Pipeline behaviour (plan Task 9 tests + edge cases).

mod common;

use std::io::Cursor;
use std::sync::Arc;

use common::*;
use serde_json::json;
use vc_config::{ExtractConfig, LlmConfig};
use vc_core::KindId;
use vc_core::licence::{LicenceClass, SourceClass};
use vc_core::llm::{LlmError, Usd};
use vc_core::records::ExtractionMethod;
use vc_core::usepolicy::{UsePolicy, UseSignals};
use vc_extract::{ExtractInput, Extractor, FilePlaceResolver, Skip};

fn kinds_of(ids: &[&str]) -> Vec<KindId> {
    ids.iter().map(|k| KindId::new(k)).collect()
}

/// A tiny place file (same format as `data/places.tsv.gz`).
fn tiny_places() -> FilePlaceResolver {
    let places = "# Contains data from GeoNames (geonames.org), CC BY 4.0\n\
geoname_id\tname\tcountry\tadmin1\tfclass\trank\tlat\tlon\tkeys\n\
2267057\tLisbon\tPT\t14\tP\t500000000\t38.7167\t-9.1333\tlisbon|lisboa|lissabon\n\
2988507\tParis\tFR\t11\tP\t900000000\t48.8534\t2.3488\tparis\n\
4717560\tParis\tUS\tTX\tP\t250000\t33.6609\t-95.5555\tparis\n";
    let countries = "code\tname\tkeys\nPT\tPortugal\tportugal\nFR\tFrance\tfrance\nUS\tUnited States\tunited states|usa\n";
    let admin1 = "code\tname\nPT.14\tLisbon\nFR.11\tÎle-de-France\nUS.TX\tTexas\n";
    FilePlaceResolver::from_readers(
        Cursor::new(places),
        Cursor::new(countries),
        Cursor::new(admin1),
    )
    .unwrap()
}

#[tokio::test]
async fn blog_cannot_produce_visa_rule() {
    let r = rig();
    let p = page("visa_blog");
    let pv = prov(
        "https://www.travelblog-example.com/vietnam-visa",
        "blog",
        SourceClass::Editorial,
    );
    let kinds = kinds_of(&["visa_rule"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    r.llm.push_json(json!({"records": [{"from_country": "US", "to_country": "VN", "visa_required": false, "max_stay_days": 45}]}));
    let out = r.ex.extract(&inp).await.unwrap();
    assert!(out.records.iter().all(|r| &*r.kind.0 != "visa_rule"));
    assert!(out.pending.is_empty());
    assert_eq!(r.llm.calls(), 0, "refused before any model call");
    assert_eq!(r.ledger.open_reservations(), 0);
    assert_eq!(r.ledger.totals("2026-11-15"), (Usd::ZERO, Usd::ZERO));
    assert!(out.report.has("visa_rule", Skip::SourceClass));
    // Nor from community, reference or commercial sources.
    for class in [
        SourceClass::Community,
        SourceClass::Reference,
        SourceClass::CommercialApi,
    ] {
        let pv = prov("https://forum.example.com/t/1", "forum", class);
        let mut inp = ExtractInput::new(&pv, &p);
        inp.llm_kinds = &kinds;
        assert!(r.ex.extract(&inp).await.unwrap().records.is_empty());
    }
    assert_eq!(r.llm.calls(), 0);
    // An official source may.
    let pv = prov(
        "https://www.immigration.gov.example/vn",
        "gov",
        SourceClass::Official,
    );
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    let out = r.ex.extract(&inp).await.unwrap();
    assert_eq!(out.records.len(), 1);
    assert_eq!(out.records[0].data["to_country"], "VN");
}

#[tokio::test]
async fn record_key_stable_across_runs() {
    let run = |name: &'static str| async move {
        let r = rig();
        let p = page("event_jsonld");
        let pv = prov(
            "https://www.blueroom-example.pt/events/jazz",
            "blueroom",
            SourceClass::Editorial,
        );
        let p2 = parsed(
            "Events",
            "Fado Evening at Casa Azul on 21 November 2026. Tickets €18.",
        );
        let pv2 = prov(
            "https://www.blueroom-example.pt/events",
            "blueroom",
            SourceClass::Editorial,
        );
        let kinds = kinds_of(&["event"]);
        let a = r.ex.extract(&ExtractInput::new(&pv, &p)).await.unwrap();
        let mut inp = ExtractInput::new(&pv2, &p2);
        inp.llm_kinds = &kinds;
        r.llm.push_json(
            json!({"records": [{"name": name, "category": "music", "city": "Lisbon", "country": "Portugal", "venue": "Casa Azul", "start_date": "2026-11-21"}]}),
        );
        let b = r.ex.extract(&inp).await.unwrap();
        (
            a.records[0].record_key.clone(),
            b.records[0].record_key.clone(),
        )
    };
    let (s1, l1) = run("Fado Evening").await;
    let (s2, l2) = run("  fado   EVENING ").await;
    assert_eq!(s1, s2, "structured key from @id");
    assert_eq!(l1, l2, "LLM key from (kind, normalised name, date)");
    assert_ne!(s1, l1);
}

#[tokio::test]
async fn structured_data_needs_no_llm() {
    let r = rig();
    let p = page("event_jsonld");
    let pv = prov(
        "https://www.blueroom-example.pt/events/jazz",
        "blueroom",
        SourceClass::Editorial,
    );
    let kinds = kinds_of(&["event"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds; // the source also asks for LLM events: structured data covers it
    let out = r.ex.extract(&inp).await.unwrap();
    assert_eq!(r.llm.calls(), 0);
    assert_eq!(r.ledger.open_reservations(), 0);
    assert_eq!(out.records.len(), 1);
    let rec = &out.records[0];
    assert_eq!(rec.extractor.method, ExtractionMethod::Structured);
    assert!(rec.extractor.model.is_none() && rec.extractor.prompt_hash.is_none());
    assert_eq!(rec.data["name"], "Jazz Night");
    assert_eq!(rec.data["city"], "Lisbon");
    assert_eq!(
        rec.data["ticket_url"],
        "https://tickets.blueroom-example.pt/e/1"
    );
    assert_eq!(rec.data["category"], "music");
    assert_eq!(rec.data["start_date"], "2026-11-20");
    assert_eq!(rec.data["start_time"], "19:00");
    assert_eq!(rec.data["price"]["amount"], 25.0);
    assert_eq!(rec.data["lat"], 38.7167);
    assert_eq!(
        rec.data["source_url"],
        "https://www.blueroom-example.pt/events/jazz"
    );
    assert_eq!(rec.data["venue"], "Blue Room");
    assert_eq!(out.report.structured, 1);
    assert_eq!(rec.schema_version, 1);
    assert!(
        rec.excerpt
            .as_ref()
            .is_some_and(|e| e.chars().count() <= 300)
    );
}

#[tokio::test]
async fn unresolved_place_empty_ids() {
    let r = rig();
    let ex = Extractor::new(common::kinds(), ExtractConfig::default())
        .with_places(Arc::new(tiny_places()))
        .with_llm(r.llm.clone(), r.ledger.clone(), LlmConfig::default());
    let p = parsed(
        "Events",
        "Jazz Night in Lisbon on 20 November 2026. Moon Party in Atlantis on 21 November 2026.",
    );
    let pv = prov(
        "https://www.events-example.com/list",
        "ev",
        SourceClass::Editorial,
    );
    let kinds = kinds_of(&["event"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    r.llm.push_json(json!({"records": [
        {"name": "Jazz Night", "category": "music", "city": "Lisbon", "country": "Portugal", "start_date": "2026-11-20"},
        {"name": "Moon Party", "category": "music", "city": "Atlantis", "country": "Atlantis", "start_date": "2026-11-21"},
        {"name": "Mystery", "category": "music", "city": "", "country": "Portugal", "start_date": "2026-11-21"}
    ]}));
    let out = ex.extract(&inp).await.unwrap();
    assert_eq!(out.records.len(), 3);
    assert_eq!(out.records[0].place_ids, vec![2267057]);
    assert!(
        out.records[1].place_ids.is_empty(),
        "unresolved place keeps place_ids = []"
    );
    assert!(out.records[2].place_ids.is_empty());

    // Country hints pick the namesake; a hint that excludes every candidate leaves it empty.
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    r.llm.push_json(json!({"records": [
        {"name": "A", "category": "music", "city": "Paris", "country": "US", "start_date": "2026-11-20"},
        {"name": "B", "category": "music", "city": "Paris", "country": "", "start_date": "2026-11-20"},
        {"name": "C", "category": "music", "city": "Lisboa", "country": "France", "start_date": "2026-11-20"}
    ]}));
    let out = ex.extract(&inp).await.unwrap();
    assert_eq!(out.records[0].place_ids, vec![4717560]);
    assert_eq!(out.records[1].place_ids, vec![2988507]);
    assert!(out.records[2].place_ids.is_empty());
}

#[tokio::test]
async fn use_policy_blocks_llm() {
    for signals in [
        UseSignals {
            tdm_reserved: true,
            ..Default::default()
        },
        UseSignals {
            ai_input_no: true,
            ..Default::default()
        },
        UseSignals {
            ai_bot_blocked: true,
            ..Default::default()
        },
    ] {
        let r = rig();
        let p = parsed("Events", "Jazz Night on 20 November 2026.");
        let mut pv = prov(
            "https://www.events-example.com/list",
            "ev",
            SourceClass::Editorial,
        );
        pv.use_policy = UsePolicy::from_signals(&signals);
        let kinds = kinds_of(&["event"]);
        let mut inp = ExtractInput::new(&pv, &p);
        inp.llm_kinds = &kinds;
        r.llm
            .push_json(json!({"records": [{"name": "Jazz Night", "category": "music", "city": "Lisbon", "country": "Portugal", "start_date": "2026-11-20"}]}));
        let out = r.ex.extract(&inp).await.unwrap();
        assert!(out.records.is_empty());
        assert_eq!(r.llm.calls(), 0);
        assert_eq!(
            r.ledger.totals("2026-11"),
            (Usd::ZERO, Usd::ZERO),
            "nothing reserved"
        );
        assert!(out.report.has("event", Skip::UsePolicy));
    }
    // noindex still allows LLM extraction (facts only); the record carries the policy.
    let r = rig();
    let p = parsed("Events", "Jazz Night on 20 November 2026.");
    let mut pv = prov(
        "https://www.events-example.com/list",
        "ev",
        SourceClass::Editorial,
    );
    pv.use_policy = UsePolicy::from_signals(&UseSignals {
        noindex: true,
        ..Default::default()
    });
    let kinds = kinds_of(&["event"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    r.llm
        .push_json(json!({"records": [{"name": "Jazz Night", "category": "music", "city": "Lisbon", "country": "Portugal", "start_date": "2026-11-20"}]}));
    let out = r.ex.extract(&inp).await.unwrap();
    assert_eq!(out.records.len(), 1);
    assert_eq!(out.records[0].use_policy, pv.use_policy);
}

#[tokio::test]
async fn llm_gates_unchanged_disabled_budget_restricted() {
    let p = parsed("Events", "Jazz Night on 20 November 2026.");
    let pv = prov(
        "https://www.events-example.com/list",
        "ev",
        SourceClass::Editorial,
    );
    let kinds = kinds_of(&["event"]);

    // Unchanged payload: no call.
    let r = rig();
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    inp.payload_changed = false;
    assert!(
        r.ex.extract(&inp)
            .await
            .unwrap()
            .report
            .has("event", Skip::Unchanged)
    );
    assert_eq!(r.llm.calls(), 0);

    // VC_LLM_ENABLED=false: the reservation is released, nothing spent.
    let r = rig();
    r.llm.set_enabled(false);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    let out = r.ex.extract(&inp).await.unwrap();
    assert!(out.report.has("event", Skip::LlmDisabled));
    assert_eq!(r.ledger.open_reservations(), 0);
    assert_eq!(r.ledger.totals("2026-11"), (Usd::ZERO, Usd::ZERO));

    // No extractor LLM at all.
    let ex = Extractor::new(common::kinds(), ExtractConfig::default());
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    assert!(
        ex.extract(&inp)
            .await
            .unwrap()
            .report
            .has("event", Skip::LlmDisabled)
    );

    // Exhausted budget: the ledger refuses, no call.
    let r = rig_with_caps(Usd::ZERO, Usd::ZERO);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    r.llm.push_json(json!({"records": []}));
    assert!(
        r.ex.extract(&inp)
            .await
            .unwrap()
            .report
            .has("event", Skip::Budget)
    );
    assert_eq!(r.llm.calls(), 0);

    // A breaker error is settled at zero and stops the kind.
    let r = rig();
    r.llm.push(Err(LlmError::BreakerOpen {
        until: chrono::Utc::now(),
    }));
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    assert!(
        r.ex.extract(&inp)
            .await
            .unwrap()
            .report
            .has("event", Skip::Budget)
    );
    assert_eq!(r.ledger.totals("2026-11").1, Usd::ZERO);

    // A transport error is counted at the estimate (conservative).
    let r = rig();
    r.llm.push(Err(LlmError::Timeout));
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    assert!(
        r.ex.extract(&inp)
            .await
            .unwrap()
            .report
            .has("event", Skip::LlmError)
    );
    assert!(r.ledger.totals("2026-11").1 > Usd::ZERO);
    assert_eq!(r.ledger.open_reservations(), 0);

    // Restricted licence: nothing at all, not even structured data.
    let r = rig();
    let pe = page("event_jsonld");
    let mut pv2 = prov(
        "https://www.blueroom-example.pt/events/jazz",
        "x",
        SourceClass::Editorial,
    );
    pv2.licence.class = LicenceClass::Restricted;
    let mut inp = ExtractInput::new(&pv2, &pe);
    inp.llm_kinds = &kinds;
    let out = r.ex.extract(&inp).await.unwrap();
    assert!(out.records.is_empty());
    assert!(out.report.has("*", Skip::RestrictedLicence));
    assert_eq!(r.llm.calls(), 0);
}

#[tokio::test]
async fn long_text_is_chunked_and_capped() {
    let r = rig();
    let text = "Jazz Night on 20 November 2026 at the Blue Room. ".repeat(2000);
    let p = parsed("Events", &text);
    let pv = prov(
        "https://www.events-example.com/list",
        "ev",
        SourceClass::Editorial,
    );
    let kinds = kinds_of(&["event"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    for _ in 0..5 {
        r.llm
            .push_json(json!({"records": [{"name": "Jazz Night", "category": "music", "city": "Lisbon", "country": "Portugal", "start_date": "2026-11-20"}]}));
    }
    let out = r.ex.extract(&inp).await.unwrap();
    assert_eq!(r.llm.calls(), 3, "≤ 3 chunks");
    let cfg = LlmConfig::default();
    let max = (cfg.max_input_tokens as f64 * cfg.chars_per_token) as usize;
    for req in r.llm.requests() {
        let body = req.input.split(">>>\n").nth(1).unwrap();
        assert!(
            body.chars().count() <= max + 400,
            "chunk within the token budget"
        );
    }
    assert_eq!(
        out.records.len(),
        1,
        "same record from every chunk is deduplicated"
    );
    assert!(out.report.has("event", Skip::Duplicate));
    assert_eq!(r.ledger.open_reservations(), 0);
}

#[tokio::test]
async fn repair_fixes_invalid_record_once() {
    let r = rig();
    let p = parsed("Events", "Jazz Night on 20 November 2026, tickets €25.");
    let pv = prov(
        "https://www.events-example.com/list",
        "ev",
        SourceClass::Editorial,
    );
    let kinds = kinds_of(&["event"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    r.llm.push_json(json!({"records": [{"name": "Jazz Night", "category": "music", "city": "Lisbon", "country": "Portugal", "start_date": "2026-11-20", "price": {"text": "€25", "amount": 25}, "extra": 1}]}));
    r.llm.push_json(
        json!({"records": [{"name": "Jazz Night", "category": "music", "city": "Lisbon", "country": "Portugal", "start_date": "2026-11-20", "price": {"text": "€25", "amount": 25}}]}),
    );
    let out = r.ex.extract(&inp).await.unwrap();
    assert_eq!(out.records.len(), 1);
    assert_eq!(out.records[0].data["price"]["amount"], 25.0);
    assert_eq!(r.llm.calls(), 2);
    let repair = &r.llm.requests()[1];
    assert!(
        !repair.input.contains("tickets €25"),
        "the repair call never sees page text"
    );

    // repair_attempts = 0: dropped without a second call.
    let r = rig();
    let cfg = ExtractConfig {
        repair_attempts: 0,
        ..ExtractConfig::default()
    };
    let ex = Extractor::new(common::kinds(), cfg).with_llm(
        r.llm.clone(),
        r.ledger.clone(),
        LlmConfig::default(),
    );
    r.llm
        .push_json(json!({"records": [{"name": "Jazz Night", "category": "music", "city": "Lisbon", "country": "Portugal", "extra": 1}]}));
    let out = ex.extract(&inp).await.unwrap();
    assert!(out.records.is_empty());
    assert_eq!(r.llm.calls(), 1);
}

#[tokio::test]
async fn too_many_records_are_capped() {
    let r = rig();
    let p = parsed("Events", "Lots of events on 20 November 2026.");
    let pv = prov(
        "https://www.events-example.com/list",
        "ev",
        SourceClass::Editorial,
    );
    let kinds = kinds_of(&["event"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    let recs: Vec<_> = (0..50)
        .map(|i| json!({"name": format!("Event {i}"), "category": "music", "city": "Lisbon", "country": "Portugal"}))
        .collect();
    r.llm.push_json(json!({"records": recs}));
    let out = r.ex.extract(&inp).await.unwrap();
    assert_eq!(
        out.records.len(),
        ExtractConfig::default().max_records_per_call
    );
    assert!(out.report.has("event", Skip::TooMany));
}

#[tokio::test]
async fn observation_kinds_get_a_series_point() {
    let r = rig();
    let p = parsed(
        "Rates",
        "Reference rate on 14 November 2026: 1 EUR = 1.0842 USD.",
    );
    let pv = prov(
        "https://www.ecb-example.europa.eu/rates",
        "ecb",
        SourceClass::Official,
    );
    let kinds = kinds_of(&["fx_rate"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    r.llm.push_json(json!({"records": [
        {"base": "EUR", "quote": "USD", "rate": 1.0842, "date": "2026-11-14", "series_key": "fx:EUR:USD"},
        {"base": "EUR", "quote": "GBP", "rate": 0.8611, "date": "2026-11-14"}
    ]}));
    let out = r.ex.extract(&inp).await.unwrap();
    assert_eq!(
        out.records.len(),
        1,
        "an ungrounded rate is dropped (record invalid)"
    );
    let obs = out.records[0].observation.as_ref().unwrap();
    assert_eq!(obs.series_key, "fx:EUR:USD");
    assert_eq!(obs.observed_at.date_naive().to_string(), "2026-11-14");
}

#[tokio::test]
async fn structured_kinds_filter_and_source_gate() {
    let r = rig();
    let p = page("event_jsonld");
    let pv = prov(
        "https://www.blueroom-example.pt/events/jazz",
        "blueroom",
        SourceClass::Editorial,
    );
    let only = kinds_of(&["attraction"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.structured_kinds = &only;
    assert!(r.ex.extract(&inp).await.unwrap().records.is_empty());
}

#[tokio::test]
async fn free_price_is_grounded_by_the_word_free() {
    let r = rig();
    let p = parsed(
        "Events",
        "Christmas market in Lisbon from 1 December 2026. Admission free.",
    );
    let pv = prov(
        "https://www.events-example.com/list",
        "ev",
        SourceClass::Editorial,
    );
    let kinds = kinds_of(&["event"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    r.llm.push_json(json!({"records": [
        {"name": "Christmas market", "category": "market", "city": "Lisbon", "country": "Portugal",
         "start_date": "2026-12-01", "price": {"text": "free", "amount": 0, "is_free": true}}
    ]}));
    let out = r.ex.extract(&inp).await.unwrap();
    assert_eq!(out.records.len(), 1);
    assert_eq!(out.records[0].data["price"]["amount"], 0);
    assert_eq!(out.records[0].data["price"]["is_free"], true);
    assert_eq!(out.report.fields_dropped, 0);
}

#[tokio::test]
async fn the_v1_event_schema_is_used_with_the_records_envelope() {
    let r = rig();
    let p = parsed("Events", "Jazz Night in Lisbon on 20 November 2026.");
    let pv = prov(
        "https://www.events-example.com/list",
        "ev",
        SourceClass::Editorial,
    );
    let kinds = kinds_of(&["event"]);
    let mut inp = ExtractInput::new(&pv, &p);
    inp.llm_kinds = &kinds;
    r.llm.push_json(json!({"records": []}));
    r.ex.extract(&inp).await.unwrap();
    let schema = r.llm.requests()[0].response_schema.clone().unwrap();
    assert_eq!(schema["properties"]["records"]["type"], "array");
    assert_eq!(schema["properties"]["records"]["maxItems"], 20);
    assert_eq!(
        schema["properties"]["records"]["items"]["required"],
        json!(["name", "category", "city", "country"])
    );
    assert!(
        schema["properties"]["records"]["items"]
            .get("$schema")
            .is_none()
    );
}
