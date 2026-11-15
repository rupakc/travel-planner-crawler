//! `PgApiStore` against a migrated Postgres (testkit `pg()`, Docker or `VC_TEST_DATABASE_URL`).

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::StatusCode;
use testkit::vc_api_fixtures::{MemIndex, MemPlaces};
use vc_api::auth::{ClientStore, StoredClient, hash_token_with};
use vc_api::catalog::Catalog;
use vc_api::dto::LicenceClassDto;
use vc_api::pg::PgApiStore;
use vc_api::{ApiState, router};
use vc_config::ApiConfig;
use vc_core::licence::LicenceClass;

fn cheap() -> argon2::Params {
    argon2::Params::new(8, 1, 1, None).expect("params")
}

#[tokio::test]
async fn pg_store_clients_sources_stats() {
    let pool = testkit::pg().await;
    let store = PgApiStore::new(pool.clone());
    store.ping().await.expect("ping");
    store.ready().await.expect("ready");
    assert!(store.get("voyager").await.expect("get").is_none());

    let c = StoredClient {
        client_id: "voyager".into(),
        name: "Voyager app".into(),
        token_hash: hash_token_with("voyager.pw", cheap()).expect("hash"),
        licence_classes: vec![LicenceClass::Open, LicenceClass::FactsOnly],
        rate_per_min: 120,
        enabled: true,
    };
    store.put_client(&c).await.expect("put");
    assert_eq!(store.get("voyager").await.expect("get"), Some(c.clone()));

    // The column default grants the four servable classes.
    sqlx::query("INSERT INTO api_clients (client_id, name, token_hash) VALUES ('d', 'd', 'x')")
        .execute(&pool)
        .await
        .expect("insert");
    let d = store.get("d").await.expect("get").expect("row");
    assert_eq!(d.licence_classes.len(), 4);
    assert_eq!(d.rate_per_min, 600);

    for (id, class, enabled) in [
        ("wikivoyage", "share_alike", true),
        ("meteostat", "restricted", false),
    ] {
        sqlx::query(
            "INSERT INTO sources (source_id, name, category, source_class, licence_class, licence,
                                  method, enabled, spec, spec_hash)
             VALUES ($1, $1, 'guide', 'reference', $2, $3, 'dump', $4, '{}', 'h')",
        )
        .bind(id)
        .bind(class)
        .bind(
            serde_json::json!({"class": class, "attribution": "Wikivoyage contributors",
                                 "spdx": "CC-BY-SA-4.0"}),
        )
        .bind(enabled)
        .execute(&pool)
        .await
        .expect("source");
    }
    let sources = store.sources().await.expect("sources");
    assert_eq!(sources.len(), 1, "restricted sources are not listed");
    assert_eq!(sources[0].licence.class, LicenceClassDto::ShareAlike);
    let stats = store.stats().await.expect("stats");
    assert_eq!(stats.sources_enabled, 1);
    assert_eq!(stats.records_active, 0);
    assert_eq!(stats.changes_seq_max, None);

    // End to end: the router authenticates against Postgres and touches last_used_at.
    let state = ApiState::new(
        ApiConfig::default(),
        Arc::new(MemIndex::new()),
        Arc::new(MemPlaces::default()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    let http = reqwest::Client::new();
    let url = format!("http://{addr}/v1/stats");
    let r = http
        .get(&url)
        .bearer_auth("voyager.pw")
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::OK);
    let r = http
        .get(&url)
        .bearer_auth("voyager.nope")
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let used: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM api_clients WHERE client_id = 'voyager'")
            .fetch_one(&pool)
            .await
            .expect("select");
    assert!(used.is_some());
    let r = http
        .get(format!("http://{addr}/readyz"))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::OK);
}
