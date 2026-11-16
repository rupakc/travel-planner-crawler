//! The `embedder` data path (plan Task 15): `pending_embeddings` → reserve → embed →
//! `store_embedding`, with the real `VertexEmbedder` against the fake Gemini server and the
//! real `PgLedger` (day, month and purpose rows).

#[path = "processor_support.rs"]
mod support;

use std::sync::Arc;

use serde_json::json;
use support::*;
use testkit::FakeGemini;
use vc_config::LlmConfig;
use vc_core::Clock;
use vc_core::index::RecordSink;
use vc_core::licence::{Licence, LicenceClass, SourceClass};
use vc_core::llm::Embedder;
use vc_core::records::{ExtractedRecord, ExtractionMethod, ExtractorInfo, Provenance};
use vc_core::usepolicy::UsePolicy;
use vc_core::{CanonicalUrl, CaptureId, CaptureRef, KindId, RecordKey, SourceId};
use vc_index::PgIndex;
use vcrawl::data::embed::{EmbedLoop, estimator};

const N: usize = 12;

async fn records(ix: &PgIndex, n: usize) {
    for i in 0..n {
        let url = CanonicalUrl::parse(&format!("https://events.example.org/e/{i}")).unwrap();
        let prov = Provenance {
            source_id: SourceId("events-test".into()),
            source_class: SourceClass::Editorial,
            licence: Licence {
                class: LicenceClass::Open,
                spdx: Some("CC0-1.0".into()),
                attribution: "Events Test".into(),
                url: None,
            },
            url_key: url.key(),
            url,
            capture: CaptureRef {
                capture_id: CaptureId::at(at(i as i64)),
                captured_at: at(i as i64),
            },
            use_policy: UsePolicy::ALLOW_ALL,
        };
        let rec = ExtractedRecord {
            kind: KindId::new("event"),
            schema_version: 1,
            record_key: RecordKey(format!("k{i}")),
            data: json!({"name": format!("Concert number {i}"), "category": "music", "city": "Lisbon", "country": "Portugal"}),
            title: Some(format!("Concert number {i}")),
            excerpt: Some("An evening concert in the old town.".into()),
            lang: Some("en".into()),
            place_ids: vec![2267057],
            quality: 0.9,
            conflict: false,
            use_policy: UsePolicy::ALLOW_ALL,
            extractor: ExtractorInfo {
                version: "test".into(),
                method: ExtractionMethod::Structured,
                model: None,
                prompt_hash: None,
                run_id: None,
            },
            observation: None,
        };
        let rep = ix.upsert(vec![rec], &prov).await.unwrap();
        assert_eq!(rep.inserted, 1);
    }
}

#[tokio::test]
async fn embedder_respects_budget_cap() {
    let pool = testkit::pg().await;
    let ix = PgIndex::new(pool.clone());
    records(&ix, N).await;

    let gemini = Arc::new(FakeGemini::new());
    let server = gemini.clone().start().await;
    // Expensive embeddings and a tiny month: the embedding share (8 %) fits a few records only.
    let cfg = LlmConfig {
        enabled: true,
        gcp_project: "test-proj".into(),
        api_endpoint: server.base_url(),
        project_cap_usd: Some(100.0),
        monthly_usd: 10.0,
        daily_usd: 100.0,
        embedding_usd_per_1k_chars: 1.0,
        ..LlmConfig::default()
    };
    let clock = Arc::new(testkit::FakeClock::new(t0()));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let vertex = vc_llm::VertexClient::new(
        &cfg,
        Arc::new(vc_llm::StaticToken("t".into())),
        dyn_clock.clone(),
    )
    .unwrap();
    let ledger = Arc::new(vc_llm::PgLedger::new(pool.clone(), cfg.clone(), dyn_clock));
    let embedder: Arc<dyn Embedder> =
        Arc::new(vc_llm::VertexEmbedder::new(cfg.clone(), vertex).with_ledger(ledger.clone()));
    let lp =
        EmbedLoop::new(ix.clone(), embedder, ledger.clone(), estimator(&cfg)).with_sizes(100, 1);
    lp.ensure_index().await.unwrap();
    assert!(!lp.ensure_index().await.unwrap(), "idempotent");

    let p = lp.pass().await.unwrap();
    assert!(p.budget_refused, "{p:?}");
    assert!(p.embedded > 0 && p.embedded < N, "{p:?}");
    assert_eq!(p.calls, p.embedded);
    let predicts = |g: &FakeGemini| g.calls().iter().filter(|c| c.method == "predict").count();
    assert_eq!(
        predicts(&gemini),
        p.embedded,
        "a refused reservation makes no call"
    );
    for c in gemini.calls() {
        assert_eq!(c.labels()["app"], "voyager-crawler");
        assert_eq!(c.labels()["purpose"], "embedding");
    }
    let stored = count(&pool, "SELECT count(*) FROM embeddings").await as usize;
    assert_eq!(stored, p.embedded);

    // The caps hold on the day, month and purpose rows; nothing stays reserved.
    let over: i64 = count(
        &pool,
        "SELECT count(*) FROM llm_budget WHERE spent + reserved > cap",
    )
    .await;
    assert_eq!(over, 0);
    let over: i64 = count(
        &pool,
        "SELECT count(*) FROM llm_purpose_budget WHERE spent + reserved > cap",
    )
    .await;
    assert_eq!(over, 0);
    let (spent, reserved): (f64, f64) = sqlx::query_as(
        "SELECT spent::float8, reserved::float8 FROM llm_purpose_budget
         WHERE period = '2026-11' AND purpose = 'embedding'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(spent > 0.0 && spent <= 10.0 * 0.08 + 1e-9, "spent {spent}");
    assert_eq!(reserved, 0.0);

    // Another pass the same day: refused at once, no call, nothing stored.
    let p2 = lp.pass().await.unwrap();
    assert!(p2.budget_refused);
    assert_eq!(p2.embedded, 0);
    assert_eq!(predicts(&gemini), p.embedded);
    assert_eq!(
        ix.pending_embeddings(lp.model(), 100).await.unwrap().len(),
        N - p.embedded
    );
}

#[tokio::test]
async fn embedder_embeds_everything_within_budget_and_skips_fresh_embeddings() {
    let pool = testkit::pg().await;
    let ix = PgIndex::new(pool.clone());
    records(&ix, 3).await;
    let gemini = Arc::new(FakeGemini::new());
    let server = gemini.clone().start().await;
    let cfg = LlmConfig {
        enabled: true,
        gcp_project: "test-proj".into(),
        api_endpoint: server.base_url(),
        project_cap_usd: Some(100.0),
        ..LlmConfig::default()
    };
    let clock: Arc<dyn Clock> = Arc::new(testkit::FakeClock::new(t0()));
    let vertex = vc_llm::VertexClient::new(
        &cfg,
        Arc::new(vc_llm::StaticToken("t".into())),
        clock.clone(),
    )
    .unwrap();
    let ledger = Arc::new(vc_llm::PgLedger::new(pool.clone(), cfg.clone(), clock));
    let embedder: Arc<dyn Embedder> =
        Arc::new(vc_llm::VertexEmbedder::new(cfg.clone(), vertex).with_ledger(ledger.clone()));
    let lp = EmbedLoop::new(ix.clone(), embedder, ledger, estimator(&cfg)).with_sizes(100, 5);
    lp.ensure_index().await.unwrap();
    let p = lp.pass().await.unwrap();
    assert_eq!((p.embedded, p.calls, p.budget_refused), (3, 1, false));
    // Up to date: the next pass has nothing to do and makes no call.
    let p = lp.pass().await.unwrap();
    assert_eq!((p.embedded, p.calls), (0, 0));
    assert_eq!(gemini.calls().len(), 1);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM llm_ledger WHERE status = 'reserved'"
        )
        .await,
        0
    );
}
