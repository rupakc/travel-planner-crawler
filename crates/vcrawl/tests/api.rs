//! The `api` role's data path (plan Task 15): records written by the processor are served by
//! `/v1` over Postgres (`PgIndex` + `PgApiStore`) with provenance, licence and versions; query
//! embeddings go through the budget; the role fails closed without TLS material.

#[path = "processor_support.rs"]
mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::StatusCode;
use serde_json::Value;
use support::*;
use testkit::llm::{FakeLlm, MemLedger};
use vc_api::auth::{StoredClient, hash_token};
use vc_api::pg::PgApiStore;
use vc_core::Clock;
use vc_core::licence::LicenceClass;
use vc_core::llm::Usd;
use vc_index::QueryEmbedder;
use vcrawl::data::embed::{BudgetQueryEmbedder, estimator};

const EVENT_URL: &str = "https://events.example.org/e/jazz-night";
const TOKEN: &str = "voyager.s3cret";

async fn serve(state: vc_api::ApiState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = vc_api::router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn get(base: &str, path: &str) -> Value {
    let r = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "GET {path}");
    r.json().await.unwrap()
}

#[tokio::test]
async fn api_answers_with_provenance() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    commit(
        &r.pool,
        &r.blobs,
        Cap::html(EVENT_URL, "events-test", &fixture("event_v1.html"), at(0)),
    )
    .await;
    r.proc.tick(8).await.unwrap();
    commit(
        &r.pool,
        &r.blobs,
        Cap::html(EVENT_URL, "events-test", &fixture("event_v2.html"), at(60)),
    )
    .await;
    r.proc.tick(8).await.unwrap();

    PgApiStore::new(r.pool.clone())
        .put_client(&StoredClient {
            client_id: "voyager".into(),
            name: "Voyager".into(),
            token_hash: hash_token(TOKEN).unwrap(),
            licence_classes: vec![
                LicenceClass::Open,
                LicenceClass::ShareAlike,
                LicenceClass::FactsOnly,
                LicenceClass::ApiTerms,
            ],
            rate_per_min: 600,
            enabled: true,
        })
        .await
        .unwrap();
    let state = vcrawl::data::api::state(
        r.pool.clone(),
        vc_config::ApiConfig::default(),
        vc_config::IndexConfig::default(),
        Arc::new(places()),
        None,
    );
    let base = serve(state).await;

    // Unauthenticated → 401.
    let anon = reqwest::get(format!("{base}/v1/search?q=jazz"))
        .await
        .unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);

    let page = get(&base, "/v1/search?q=jazz&kind=event").await;
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{page}");
    let it = &items[0];
    assert_eq!(it["kind"], "event");
    assert_eq!(it["schema"], "event@1");
    let prov = it["provenance"].as_array().expect("provenance");
    assert_eq!(prov.len(), 1);
    assert_eq!(prov[0]["source_id"], "events-test");
    assert_eq!(prov[0]["url"], EVENT_URL);
    assert!(prov[0]["captured_at"].is_string());
    assert_eq!(it["licence"]["class"], "facts_only");
    assert_eq!(it["conflict"], false);
    // facts_only: at most the 300-char excerpt is served.
    if let Some(e) = it["excerpt"].as_str() {
        assert!(e.chars().count() <= 300);
    }
    assert!(
        it["data"].to_string().contains("22"),
        "the current version is served: {it}"
    );

    // Versions, newest first, with their own provenance.
    let rid: i64 = sqlx::query_scalar("SELECT record_id FROM records WHERE kind = 'event'")
        .fetch_one(&r.pool)
        .await
        .unwrap();
    let v = get(&base, &format!("/v1/records/{rid}/versions")).await;
    let vs = v["items"].as_array().expect("versions");
    assert_eq!(vs.len(), 2, "{v}");
    assert_eq!(vs[0]["version"], 2);
    assert_eq!(vs[1]["version"], 1);

    // The change feed lists the record; sources and stats come from Postgres.
    let ch = get(&base, "/v1/changes?kinds=event").await;
    assert!(
        ch["changes"].as_array().is_some_and(|c| !c.is_empty()),
        "{ch}"
    );
    let srcs = get(&base, "/v1/sources").await;
    assert!(srcs.to_string().contains("events-test"));
    let ready = reqwest::get(format!("{base}/readyz")).await.unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
}

#[tokio::test]
async fn query_embedder_respects_the_budget() {
    let clock = Arc::new(testkit::FakeClock::new(t0()));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let llm = Arc::new(FakeLlm::new().with_dim(8));
    let cfg = vc_config::LlmConfig {
        embedding_usd_per_1k_chars: 1.0,
        ..vc_config::LlmConfig::default()
    };

    // Exhausted: no call, text-only search (None).
    let empty = Arc::new(MemLedger::new(dyn_clock.clone(), Usd::ZERO, Usd::ZERO));
    let qe = BudgetQueryEmbedder::new(llm.clone(), empty.clone(), estimator(&cfg));
    assert_eq!(qe.embed_query("jazz in lisbon").await.unwrap(), None);
    assert_eq!(llm.embed_calls(), 0);
    assert_eq!(empty.open_reservations(), 0);

    // Room for one query: one call, settled at the estimate, then refused.
    let est = estimator(&cfg)("jazz in lisbon".chars().count());
    let one = Arc::new(MemLedger::new(dyn_clock, est, est));
    let qe = BudgetQueryEmbedder::new(llm.clone(), one.clone(), estimator(&cfg));
    let v = qe
        .embed_query("  jazz in lisbon ")
        .await
        .unwrap()
        .expect("vector");
    assert_eq!(v.len(), 8);
    assert_eq!(llm.embed_calls(), 1);
    assert_eq!(one.open_reservations(), 0);
    assert_eq!(one.totals("2026-11-15"), (Usd::ZERO, est));
    assert_eq!(qe.embed_query("jazz in lisbon").await.unwrap(), None);
    assert_eq!(llm.embed_calls(), 1);
    // Blank queries never reserve.
    assert_eq!(qe.embed_query("   ").await.unwrap(), None);
}

#[tokio::test]
async fn api_role_fails_closed_without_tls_material() {
    let mut cfg = vc_config::Config::default();
    cfg.secrets.backend = vc_config::SecretBackend::Env;
    cfg.secrets.env_prefix = "VC_TEST_T15_NO_SUCH_SECRET_".into();
    let err = vcrawl::roles::api::run(vcrawl::Ctx::new(cfg))
        .await
        .expect_err("no TLS material → no server");
    assert!(format!("{err:#}").contains("TLS"), "{err:#}");
}
