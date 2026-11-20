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

/// Inserts one current `emission_factor` record (version 1).
async fn put_factor(
    pool: &sqlx::PgPool,
    key: &str,
    source: &str,
    class: &str,
    state: &str,
    data: serde_json::Value,
    policy: vc_core::usepolicy::UsePolicy,
) -> i64 {
    put_record(
        pool,
        "emission_factor",
        key,
        source,
        class,
        state,
        data,
        policy,
    )
    .await
}

/// Inserts one current record of `kind` (version 1, adapter `factor_table`).
#[allow(clippy::too_many_arguments)]
async fn put_record(
    pool: &sqlx::PgPool,
    kind: &str,
    key: &str,
    source: &str,
    class: &str,
    state: &str,
    data: serde_json::Value,
    policy: vc_core::usepolicy::UsePolicy,
) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO records (kind, source_id, url_key, record_key, licence_class, state,
                              current_version, licence, url)
         VALUES ($7, $1, $2, $3, $4, $5, 1, $6, 'https://example.org/f')
         RETURNING record_id",
    )
    .bind(source)
    .bind(key.as_bytes())
    .bind(key)
    .bind(class)
    .bind(state)
    .bind(serde_json::json!({"class": class, "attribution": "Example", "spdx": null, "url": null}))
    .bind(kind)
    .fetch_one(pool)
    .await
    .expect("record");
    sqlx::query(
        "INSERT INTO record_versions (record_id, version, capture_id, captured_at, data, data_hash,
                                      kind_schema, extractor_ver, method, licence_class, use_policy)
         VALUES ($1, 1, 'c1', now(), $2, '\\x00', $5 || '@1', 'adapter:factor_table', 'adapter',
                 $3, $4)",
    )
    .bind(id)
    .bind(data)
    .bind(class)
    .bind(serde_json::to_value(policy).expect("policy"))
    .bind(kind)
    .execute(pool)
    .await
    .expect("version");
    id
}

#[tokio::test]
async fn pg_carbon_factors_and_grid() {
    use vc_api::catalog::{FactorQuery, GridQuery};
    use vc_core::usepolicy::UsePolicy;
    let pool = testkit::pg().await;
    let store = PgApiStore::new(pool.clone());
    let full = UsePolicy::ALLOW_ALL;
    let lh = serde_json::json!({"activity": "flights_long_haul_economy", "category": "flights",
        "country_code": "GB", "gas_basis": "co2e_with_rf", "year": 2026, "value": 0.117});
    let a = put_factor(
        &pool,
        "a",
        "od-desnz-ghg-factors",
        "open",
        "active",
        lh.clone(),
        full,
    )
    .await;
    let mut old = lh.clone();
    old["year"] = serde_json::json!(2025);
    let b = put_factor(
        &pool,
        "b",
        "od-desnz-ghg-factors",
        "open",
        "stale",
        old,
        full,
    )
    .await;
    let camx = serde_json::json!({"activity": "electricity_camx", "category": "electricity",
        "country_code": "US", "region_code": "CAMX", "gas_basis": "co2e", "year": 2023,
        "value": 429.983});
    let c = put_factor(&pool, "c", "od-epa-egrid", "open", "active", camx, full).await;
    // Never served: tombstoned, share_alike for an open-only client, a deny-all policy.
    put_factor(
        &pool,
        "d",
        "od-desnz-ghg-factors",
        "open",
        "tombstoned",
        lh.clone(),
        full,
    )
    .await;
    let sa = put_factor(
        &pool,
        "e",
        "od-concito",
        "share_alike",
        "active",
        lh.clone(),
        full,
    )
    .await;
    put_factor(
        &pool,
        "f",
        "od-desnz-ghg-factors",
        "open",
        "active",
        lh.clone(),
        UsePolicy::DENY_ALL,
    )
    .await;

    let q = |f: FactorQuery| FactorQuery {
        licence_classes: vec![LicenceClass::Open],
        limit: 10,
        ..f
    };
    let ids = |v: Vec<vc_core::index::Item>| v.into_iter().map(|i| i.id).collect::<Vec<_>>();
    let s = |i: i64| i.to_string();
    assert_eq!(
        ids(store
            .carbon_factors(&q(FactorQuery::default()))
            .await
            .expect("q")),
        [s(a), s(b), s(c)],
        "newest year first; tombstoned, share_alike and deny-all rows never served"
    );
    let flights = FactorQuery {
        category: Some("flights".into()),
        country: Some("GB".into()),
        year: Some(2026),
        ..FactorQuery::default()
    };
    assert_eq!(
        ids(store.carbon_factors(&q(flights)).await.expect("q")),
        [s(a)]
    );
    let region = FactorQuery {
        region: Some("CAMX".into()),
        source: Some("od-epa-egrid".into()),
        activity: Some("camx".into()),
        ..FactorQuery::default()
    };
    let got = store.carbon_factors(&q(region)).await.expect("q");
    assert_eq!(ids(got.clone()), [s(c)]);
    assert_eq!(got[0].data["value"], 429.983);
    assert_eq!(got[0].provenance[0].source_id.0, "od-epa-egrid");
    let page2 = FactorQuery {
        offset: 1,
        limit: 1,
        licence_classes: vec![LicenceClass::Open],
        ..FactorQuery::default()
    };
    assert_eq!(ids(store.carbon_factors(&page2).await.expect("q")), [s(b)]);
    let both = FactorQuery {
        licence_classes: vec![LicenceClass::Open, LicenceClass::ShareAlike],
        limit: 10,
        category: Some("flights".into()),
        ..FactorQuery::default()
    };
    assert!(ids(store.carbon_factors(&both).await.expect("q")).contains(&s(sa)));

    for (series, year, g, tomb) in [
        ("grid_ci:FR:annual:lifecycle", 2023, 56.0, false),
        ("grid_ci:FR:annual:lifecycle", 2024, 40.48, false),
        ("grid_ci:FR:monthly:lifecycle", 2025, 35.0, false),
        ("grid_ci:FR:annual:lifecycle", 2022, 60.0, true),
        ("grid_ci:FRX:annual:lifecycle", 2024, 1.0, false),
        ("grid_ci:DE:annual:lifecycle", 2024, 330.0, false),
    ] {
        sqlx::query(
            "INSERT INTO observations (series_key, observed_at, source_id, value, capture_id,
                                       captured_at, licence, tombstoned, kind)
             VALUES ($1, make_timestamptz($2, 1, 1, 0, 0, 0, 'UTC'), 'carbon-owid-grapher-ci', $3,
                     'c1', now(), $4, $5, 'grid_carbon_intensity')",
        )
        .bind(series)
        .bind(year)
        .bind(serde_json::json!({"intensity_g_per_kwh": g}))
        .bind(serde_json::json!({"class": "open", "attribution": "OWID", "spdx": "CC-BY-4.0", "url": null}))
        .bind(tomb)
        .execute(&pool)
        .await
        .expect("obs");
    }
    let t = |y: i32| {
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, y, 1, 1, 0, 0, 0)
            .single()
            .expect("t")
    };
    let g = GridQuery {
        country: "FR".into(),
        period: None,
        basis: None,
        from: t(2000),
        to: t(2030),
        limit: 10,
    };
    let pts = store.carbon_grid(&g).await.expect("grid");
    let got: Vec<(String, f64)> = pts
        .iter()
        .map(|o| {
            (
                o.series_key.clone(),
                o.value["intensity_g_per_kwh"].as_f64().expect("g"),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("grid_ci:FR:monthly:lifecycle".to_string(), 35.0),
            ("grid_ci:FR:annual:lifecycle".to_string(), 40.48),
            ("grid_ci:FR:annual:lifecycle".to_string(), 56.0),
        ],
        "newest first, tombstoned and other countries excluded"
    );
    assert_eq!(pts[0].licence.class, LicenceClass::Open);
    let annual = GridQuery {
        period: Some("annual".into()),
        basis: Some("lifecycle".into()),
        limit: 1,
        ..g
    };
    assert_eq!(store.carbon_grid(&annual).await.expect("grid").len(), 1);
}

#[tokio::test]
async fn pg_carbon_occupancy() {
    use vc_api::catalog::OccupancyQuery;
    use vc_core::usepolicy::UsePolicy;
    let pool = testkit::pg().await;
    let store = PgApiStore::new(pool.clone());
    let full = UsePolicy::ALLOW_ALL;
    let tag = "od-dft-tag-databook";
    let car = |purpose: &str, year: i64, v: f64| {
        serde_json::json!({"mode": "car", "trip_purpose": purpose, "country_code": "GB",
            "basis": "per_vehicle_km", "year": year, "value": v,
            "unit": "persons per vehicle", "includes_driver": true})
    };
    let put = |key: &'static str,
               source: &'static str,
               class: &'static str,
               state: &'static str,
               data: serde_json::Value,
               policy: UsePolicy| {
        let pool = pool.clone();
        async move { put_record(&pool, "occupancy", key, source, class, state, data, policy).await }
    };
    let a = put("a", tag, "open", "active", car("all", 2023, 1.61), full).await;
    let b = put("b", tag, "open", "stale", car("work", 2023, 1.12), full).await;
    let psv = serde_json::json!({"mode": "bus_coach", "trip_purpose": "all",
        "country_code": "GB", "basis": "per_vehicle_km", "year": 2023, "value": 13.8});
    let c = put("c", tag, "open", "active", psv, full).await;
    let us = serde_json::json!({"mode": "car", "trip_purpose": "all", "country_code": "US",
        "basis": "per_trip", "year": 2022, "value": 1.4});
    let d = put("d", "od-other", "open", "active", us, full).await;
    // Never served: tombstoned, share_alike for an open-only client, a deny-all policy, and an
    // emission factor (another kind).
    put("e", tag, "open", "tombstoned", car("all", 2024, 1.5), full).await;
    let sa = put(
        "f",
        "od-sa",
        "share_alike",
        "active",
        car("all", 2024, 1.5),
        full,
    )
    .await;
    put(
        "g",
        tag,
        "open",
        "active",
        car("all", 2025, 1.7),
        UsePolicy::DENY_ALL,
    )
    .await;
    put_factor(
        &pool,
        "h",
        tag,
        "open",
        "active",
        car("all", 2026, 1.9),
        full,
    )
    .await;

    let q = |f: OccupancyQuery| OccupancyQuery {
        licence_classes: vec![LicenceClass::Open],
        limit: 10,
        ..f
    };
    let ids = |v: Vec<vc_core::index::Item>| v.into_iter().map(|i| i.id).collect::<Vec<_>>();
    let s = |i: i64| i.to_string();
    let all = store
        .carbon_occupancy(&q(OccupancyQuery::default()))
        .await
        .expect("q");
    assert_eq!(
        ids(all.clone()),
        [s(a), s(b), s(c), s(d)],
        "newest year first, then id; never other kinds or unservable rows"
    );
    assert_eq!(all[0].kind.0.as_ref(), "occupancy");
    assert_eq!(all[0].data["value"], 1.61);
    assert_eq!(all[0].provenance[0].source_id.0, tag);
    let work = OccupancyQuery {
        mode: Some("car".into()),
        country: Some("GB".into()),
        year: Some(2023),
        trip_purpose: Some("work".into()),
        basis: Some("per_vehicle_km".into()),
        source: Some(tag.into()),
        ..OccupancyQuery::default()
    };
    assert_eq!(
        ids(store.carbon_occupancy(&q(work)).await.expect("q")),
        [s(b)]
    );
    let bus = OccupancyQuery {
        mode: Some("bus_coach".into()),
        ..OccupancyQuery::default()
    };
    assert_eq!(
        ids(store.carbon_occupancy(&q(bus)).await.expect("q")),
        [s(c)]
    );
    let per_trip = OccupancyQuery {
        basis: Some("per_trip".into()),
        ..OccupancyQuery::default()
    };
    assert_eq!(
        ids(store.carbon_occupancy(&q(per_trip)).await.expect("q")),
        [s(d)]
    );
    let page2 = OccupancyQuery {
        offset: 1,
        limit: 2,
        licence_classes: vec![LicenceClass::Open],
        ..OccupancyQuery::default()
    };
    assert_eq!(
        ids(store.carbon_occupancy(&page2).await.expect("q")),
        [s(b), s(c)]
    );
    let both = OccupancyQuery {
        licence_classes: vec![LicenceClass::Open, LicenceClass::ShareAlike],
        limit: 10,
        ..OccupancyQuery::default()
    };
    assert!(ids(store.carbon_occupancy(&both).await.expect("q")).contains(&s(sa)));
    let none = OccupancyQuery {
        limit: 10,
        ..OccupancyQuery::default()
    };
    assert!(store.carbon_occupancy(&none).await.expect("q").is_empty());
}

#[tokio::test]
async fn pg_facts_resolve_bare_countries_and_filter_by_place() {
    use vc_api::catalog::FactQuery;
    use vc_core::usepolicy::UsePolicy;
    let pool = testkit::pg().await;
    let store = PgApiStore::new(pool.clone());
    sqlx::query(
        "INSERT INTO sources (source_id, name, category, source_class, licence_class, licence,
                              method, enabled, spec, spec_hash)
         VALUES ('wikidata-facts', 'Wikidata EntityData', 'facts', 'reference', 'open',
                 '{\"class\": \"open\", \"attribution\": \"Wikidata\"}', 'api', true, '{}', 'h')",
    )
    .execute(&pool)
    .await
    .expect("source");
    let full = UsePolicy::ALLOW_ALL;
    let berlin = serde_json::json!({"geonames_id": 2950159, "wikidata_qid": "Q64",
        "country": "DE", "name": "Berlin"});
    let germany = serde_json::json!({"geonames_id": 2921044, "wikidata_qid": "Q183",
        "admin_level": "country", "country": "DE", "name": "Germany"});
    let wd = |place: &serde_json::Value, text: &str, ty: &str| {
        serde_json::json!({"place": place, "fact_type": ty, "text": text,
            "text_origin": "templated", "llm_ok": true, "confidence": "structured"})
    };
    let bare = |cc: &str, text: &str| {
        serde_json::json!({"place": {"country": cc, "admin_level": "country"},
            "fact_type": "unbelievable", "text": text,
            "text_origin": "templated", "confidence": "structured"})
    };
    let put = |key: &'static str,
               source: &'static str,
               class: &'static str,
               state: &'static str,
               data: serde_json::Value,
               policy: UsePolicy| {
        let pool = pool.clone();
        async move { put_record(&pool, "place_fact", key, source, class, state, data, policy).await }
    };
    let a = put(
        "a",
        "wikidata-facts",
        "open",
        "active",
        wd(&berlin, "Berlin was founded in 1244.", "historical"),
        full,
    )
    .await;
    let b = put(
        "b",
        "wikidata-facts",
        "open",
        "stale",
        wd(&berlin, "Berlin covers an area of 891.12 km².", "popular"),
        full,
    )
    .await;
    let g = put(
        "g",
        "wikidata-facts",
        "open",
        "active",
        wd(
            &germany,
            "Germany covers an area of 357,588 km².",
            "popular",
        ),
        full,
    )
    .await;
    let mut rw = wd(&berlin, "A rewritten share-alike sentence.", "fun");
    rw["text_origin"] = serde_json::json!("rewritten");
    rw["llm_ok"] = serde_json::json!(false);
    let e = put("e", "fx-example", "share_alike", "active", rw, full).await;
    // Source text kept as written, tombstoned, deny-all: never served.
    let mut hook = wd(&berlin, "... that a copied hook is never served?", "fun");
    hook["text_origin"] = serde_json::json!("source_hook");
    put(
        "h",
        "wiki-facts-simple",
        "share_alike",
        "active",
        hook,
        full,
    )
    .await;
    put(
        "t",
        "wikidata-facts",
        "open",
        "tombstoned",
        wd(&berlin, "A tombstoned fact about Berlin.", "fun"),
        full,
    )
    .await;
    put(
        "d",
        "wikidata-facts",
        "open",
        "active",
        wd(&berlin, "A fact no client may see here.", "fun"),
        UsePolicy::DENY_ALL,
    )
    .await;
    let c = put(
        "c",
        "od-factbook-json",
        "open",
        "active",
        bare("DE", "The highest point of Germany is Zugspitze (2,963 m)."),
        full,
    )
    .await;
    let f = put(
        "f",
        "od-factbook-json",
        "open",
        "active",
        bare("FR", "The highest point of France is Mont Blanc (4,805 m)."),
        full,
    )
    .await;
    // Another kind with a `place` key is never a fact.
    put(
        "o",
        "wikidata-facts",
        "open",
        "active",
        wd(&berlin, "Not a fact kind at all.", "fun"),
        full,
    )
    .await;
    sqlx::query("UPDATE records SET kind = 'article' WHERE record_key = 'o'")
        .execute(&pool)
        .await
        .expect("kind");

    let q = |f: FactQuery| FactQuery {
        licence_classes: vec![LicenceClass::Open, LicenceClass::ShareAlike],
        limit: 10,
        ..f
    };
    let ids =
        |v: &[vc_api::catalog::FactRow]| v.iter().map(|r| r.item.id.clone()).collect::<Vec<_>>();
    let s = |i: i64| i.to_string();
    let all = store.facts(&q(FactQuery::default())).await.expect("q");
    assert_eq!(ids(&all), [s(a), s(b), s(g), s(e), s(c), s(f)]);
    assert_eq!(all[0].source_name.as_deref(), Some("Wikidata EntityData"));
    assert_eq!(all[0].source_class.as_deref(), Some("reference"));
    assert_eq!(all[4].place["wikidata_qid"], "Q183", "Germany's QID");
    assert_eq!(all[4].place["geonames_id"], 2921044);
    assert_eq!(all[4].place["country"], "DE");
    assert!(all[5].place.get("wikidata_qid").is_none(), "no Wikidata FR");
    assert_eq!(all[4].source_name, None, "no registry row");
    let by_qid = |qid: &str| FactQuery {
        wikidata_qid: Some(qid.into()),
        ..FactQuery::default()
    };
    assert_eq!(
        ids(&store.facts(&q(by_qid("Q64"))).await.expect("q")),
        [s(a), s(b), s(e)]
    );
    assert_eq!(
        ids(&store.facts(&q(by_qid("Q183"))).await.expect("q")),
        [s(g), s(c)]
    );
    let by_gn = FactQuery {
        geonames_id: Some(2921044),
        ..FactQuery::default()
    };
    assert_eq!(ids(&store.facts(&q(by_gn)).await.expect("q")), [s(g), s(c)]);
    let by_gn_type = FactQuery {
        geonames_id: Some(2950159),
        fact_type: Some("fun".into()),
        ..FactQuery::default()
    };
    assert_eq!(ids(&store.facts(&q(by_gn_type)).await.expect("q")), [s(e)]);
    let by_country = FactQuery {
        country: Some("DE".into()),
        fact_type: Some("unbelievable".into()),
        ..FactQuery::default()
    };
    assert_eq!(ids(&store.facts(&q(by_country)).await.expect("q")), [s(c)]);
    let open_only = FactQuery {
        licence_classes: vec![LicenceClass::Open],
        limit: 10,
        ..FactQuery::default()
    };
    assert_eq!(
        ids(&store.facts(&open_only).await.expect("q")),
        [s(a), s(b), s(g), s(c), s(f)]
    );
    let page2 = FactQuery {
        offset: 1,
        limit: 2,
        ..q(FactQuery::default())
    };
    let page2 = FactQuery { limit: 2, ..page2 };
    assert_eq!(ids(&store.facts(&page2).await.expect("q")), [s(b), s(g)]);
    let none = FactQuery {
        licence_classes: vec![],
        ..FactQuery::default()
    };
    assert!(store.facts(&none).await.expect("q").is_empty());
}
