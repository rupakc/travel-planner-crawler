//! HTTP-level tests of the `/v1` read API against in-memory fakes (vc-index is built in
//! parallel, so the read model is `testkit::vc_api_fixtures::MemIndex`).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::StatusCode;
use serde_json::Value;
use testkit::vc_api_fixtures::{MemIndex, MemPlaces, item, licence, provenance, t0};
use vc_api::auth::{MemClientStore, StoredClient, hash_token_with};
use vc_api::catalog::MemCatalog;
use vc_api::{ApiState, router};
use vc_config::ApiConfig;
use vc_core::extract::{GeoPoint, PlaceMatch};
use vc_core::ids::{EntityId, RecordId, SourceId};
use vc_core::index::{ClaimView, EntityView, ObservationView, RecordVersionView};
use vc_core::licence::LicenceClass;
use vc_core::records::{ExtractionMethod, ExtractorInfo, MAX_EXCERPT_CHARS};
use vc_core::usepolicy::{IndexUse, ServeUse, UsePolicy};

const ALL: &[LicenceClass] = &[
    LicenceClass::Open,
    LicenceClass::ShareAlike,
    LicenceClass::FactsOnly,
    LicenceClass::ApiTerms,
];

/// Cheap argon2 parameters so debug-build tests stay fast (production uses the defaults).
fn cheap() -> argon2::Params {
    argon2::Params::new(8, 1, 1, None).expect("params")
}

fn client(id: &str, secret: &str, classes: &[LicenceClass], rate: u32) -> StoredClient {
    StoredClient {
        client_id: id.into(),
        name: format!("client {id}"),
        token_hash: hash_token_with(&format!("{id}.{secret}"), cheap()).expect("hash"),
        licence_classes: classes.to_vec(),
        rate_per_min: rate,
        enabled: true,
    }
}

struct Harness {
    base: String,
    http: reqwest::Client,
    index: Arc<MemIndex>,
    token: String,
}

impl Harness {
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    async fn json(&self, path: &str) -> Value {
        let r = self.get(path).send().await.expect("send");
        assert_eq!(r.status(), StatusCode::OK, "GET {path}");
        r.json().await.expect("json")
    }
}

async fn start(state: ApiState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}")
}

async fn harness_with(config: ApiConfig, clients: Vec<StoredClient>, token: &str) -> Harness {
    let index = Arc::new(MemIndex::new());
    let places = Arc::new(MemPlaces {
        matches: vec![PlaceMatch {
            geoname_id: 2_643_743,
            name: "London".into(),
            country: "GB".into(),
            admin1: Some("England".into()),
            point: GeoPoint {
                lat: 51.5,
                lon: -0.12,
            },
            score: 0.97,
        }],
    });
    let state = ApiState::new(
        config,
        index.clone(),
        places,
        Arc::new(MemClientStore::new(clients)),
        Arc::new(MemCatalog::default()),
    );
    let base = start(state).await;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    Harness {
        base,
        http,
        index,
        token: token.into(),
    }
}

async fn harness() -> Harness {
    harness_with(
        ApiConfig::default(),
        vec![client("voyager", "s3cret", ALL, 10_000)],
        "voyager.s3cret",
    )
    .await
}

const V1_ROUTES: &[&str] = &[
    "/v1/entities",
    "/v1/entities/01JCQ5Z7G8N3V2K4M6P8R0T2W4",
    "/v1/records/1/versions",
    "/v1/search?q=x",
    "/v1/articles",
    "/v1/observations?series=fx:USD:EUR",
    "/v1/carbon/factors?category=flights",
    "/v1/carbon/grid?country=FR",
    "/v1/carbon/occupancy?mode=car",
    "/v1/facts?country=DE",
    "/v1/places/resolve?name=London",
    "/v1/changes",
    "/v1/sources",
    "/v1/stats",
];

#[tokio::test]
async fn unauthenticated_401() {
    let mut disabled = client("off", "pw", ALL, 100);
    disabled.enabled = false;
    let h = harness_with(
        ApiConfig::default(),
        vec![client("voyager", "s3cret", ALL, 10_000), disabled],
        "voyager.s3cret",
    )
    .await;
    let bad_tokens: &[Option<&str>] = &[
        None,
        Some("voyager.wrong"),
        Some("voyager"),
        Some("nobody.s3cret"),
        Some("off.pw"),
        Some(""),
    ];
    for route in V1_ROUTES {
        for tok in bad_tokens {
            let mut req = h.http.get(format!("{}{route}", h.base));
            if let Some(t) = tok {
                req = req.bearer_auth(t);
            }
            let r = req.send().await.expect("send");
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{route} with {tok:?}");
            assert_eq!(
                r.headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok()),
                Some("Bearer"),
            );
            let body: Value = r.json().await.expect("json");
            assert_eq!(body["error"], "unauthorized");
        }
        // Basic auth is not accepted either.
        let r = h
            .http
            .get(format!("{}{route}", h.base))
            .basic_auth("voyager", Some("s3cret"))
            .send()
            .await
            .expect("send");
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{route} basic");
    }
    // The valid token works (and a second call hits the verified-token cache).
    for _ in 0..2 {
        assert_eq!(
            h.get("/v1/sources").send().await.expect("send").status(),
            StatusCode::OK
        );
    }
    // Probes and the contract are public.
    for p in ["/healthz", "/readyz", "/v1/openapi.json"] {
        let r = h
            .http
            .get(format!("{}{p}", h.base))
            .send()
            .await
            .expect("send");
        assert_eq!(r.status(), StatusCode::OK, "{p}");
    }
}

#[tokio::test]
async fn openapi_snapshot() {
    let doc = vc_api::openapi();
    let json: Value = serde_json::to_value(&doc).expect("json");
    insta::assert_json_snapshot!("openapi", json);
    // The served document is the same one.
    let h = harness().await;
    let served: Value = h
        .http
        .get(format!("{}/v1/openapi.json", h.base))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(served, json);
}

async fn drain_changes(h: &Harness, seen: &mut Vec<String>, cur: Option<String>) -> Option<String> {
    let mut cur = cur;
    loop {
        let path = match &cur {
            Some(c) => format!("/v1/changes?limit=1&since={c}"),
            None => "/v1/changes?limit=1".to_string(),
        };
        let body = h.json(&path).await;
        let changes = body["changes"].as_array().expect("changes").clone();
        if changes.is_empty() {
            assert_eq!(body["next"].as_str().map(str::to_string), cur);
            return cur;
        }
        for c in &changes {
            seen.push(c["owner_id"].as_str().expect("owner").to_string());
            assert!(c["cursor"].is_string());
        }
        cur = body["next"].as_str().map(str::to_string);
    }
}

#[tokio::test]
async fn changes_cursor_no_skips() {
    let h = harness().await;
    let idx = &h.index;
    idx.change("r0", "event");
    // A long transaction takes a low xid and writes late; others commit around it, with
    // lower seqs than its second row (a seq-only cursor would skip that row).
    let long = idx.begin();
    idx.change("r1", "event");
    idx.write_change(long, "r-long-1", "event");
    let mid = idx.begin();
    idx.write_change(mid, "r2", "place");
    idx.commit(mid);
    idx.change("r3", "event");

    let mut seen: Vec<String> = Vec::new();
    // While `long` is open nothing at or after its xid is visible: r0 only.
    let cursor = drain_changes(&h, &mut seen, None).await;
    assert_eq!(seen, vec!["r0"]);
    idx.write_change(long, "r-long-2", "event");
    idx.commit(long);
    idx.change("r4", "place");
    let cursor = drain_changes(&h, &mut seen, cursor).await;
    drain_changes(&h, &mut seen, cursor).await;

    let expected = ["r0", "r-long-1", "r-long-2", "r1", "r2", "r3", "r4"];
    assert_eq!(seen, expected, "every commit exactly once, in cursor order");
    let uniq: HashSet<&String> = seen.iter().collect();
    assert_eq!(uniq.len(), seen.len());

    // Kind filter and a bad cursor.
    let body = h.json("/v1/changes?kinds=place&limit=50").await;
    let owners: Vec<&str> = body["changes"]
        .as_array()
        .expect("arr")
        .iter()
        .map(|c| c["owner_id"].as_str().expect("owner"))
        .collect();
    assert_eq!(owners, vec!["r2", "r4"]);
    let r = h
        .get("/v1/changes?since=not-a-cursor")
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

fn long_text(n: usize) -> String {
    // Multi-byte characters, so byte and char counts differ.
    "é".repeat(n)
}

fn all_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| all_strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| all_strings(x, out)),
        _ => {}
    }
}

#[tokio::test]
async fn facts_only_excerpt_capped() {
    let h = harness().await;
    let mut fo = item("blog-1", "article", LicenceClass::FactsOnly);
    fo.title = Some(long_text(500));
    fo.excerpt = Some(long_text(1200));
    fo.data = serde_json::json!({
        "headline": long_text(400),
        "body": long_text(5000),
        "nested": {"text": long_text(2000), "list": [long_text(301)]},
        "price": 12.5,
    });
    h.index.add_item(fo);
    let mut open = item("wiki-1", "article", LicenceClass::Open);
    open.excerpt = Some(long_text(1000));
    open.data = serde_json::json!({"summary": long_text(800)});
    h.index.add_item(open);

    for path in [
        "/v1/search?q=blog",
        "/v1/articles",
        "/v1/entities?kind=article",
    ] {
        let body = h.json(path).await;
        let items = body["items"].as_array().expect("items");
        let fo = items
            .iter()
            .find(|i| i["id"] == "blog-1")
            .expect("facts_only item");
        let excerpt = fo["excerpt"].as_str().expect("excerpt");
        assert!(excerpt.chars().count() <= MAX_EXCERPT_CHARS, "{path}");
        let mut strings = Vec::new();
        all_strings(fo, &mut strings);
        for s in &strings {
            assert!(
                s.chars().count() <= MAX_EXCERPT_CHARS,
                "{path}: a facts_only string of {} chars",
                s.chars().count()
            );
        }
        assert!(fo["data"].get("body").is_none(), "full text keys dropped");
        // Every excerpt is capped, whatever the licence; open data is served whole.
        let open = items.iter().find(|i| i["id"] == "wiki-1").expect("open");
        assert!(open["excerpt"].as_str().expect("e").chars().count() <= MAX_EXCERPT_CHARS);
        assert_eq!(
            open["data"]["summary"].as_str().expect("s").chars().count(),
            800
        );
    }

    // A configured excerpt length below 300 is honoured; above 300 is clamped.
    for (cfg_chars, want) in [(120usize, 120usize), (5000, MAX_EXCERPT_CHARS)] {
        let cfg = ApiConfig {
            excerpt_chars: cfg_chars,
            ..ApiConfig::default()
        };
        let h2 = harness_with(
            cfg,
            vec![client("voyager", "s3cret", ALL, 10_000)],
            "voyager.s3cret",
        )
        .await;
        let mut fo = item("blog-2", "article", LicenceClass::FactsOnly);
        fo.excerpt = Some(long_text(1000));
        h2.index.add_item(fo);
        let body = h2.json("/v1/articles").await;
        let n = body["items"][0]["excerpt"]
            .as_str()
            .expect("e")
            .chars()
            .count();
        assert_eq!(n, want);
    }

    // Record versions of facts_only records are capped the same way.
    h.index.add_versions(
        RecordId(7),
        vec![version(7, LicenceClass::FactsOnly, long_text(3000))],
    );
    let body = h.json("/v1/records/7/versions").await;
    let mut strings = Vec::new();
    all_strings(&body["items"][0], &mut strings);
    assert!(
        strings
            .iter()
            .all(|s| s.chars().count() <= MAX_EXCERPT_CHARS)
    );
}

fn version(id: i64, class: LicenceClass, text: String) -> RecordVersionView {
    RecordVersionView {
        record_id: RecordId(id),
        version: 2,
        schema: "article@1".into(),
        data: serde_json::json!({"text": text, "headline": "Hi"}),
        data_hash: "ab".repeat(32),
        captured_at: t0(),
        extractor: ExtractorInfo {
            version: "test-1".into(),
            method: ExtractionMethod::Llm,
            model: Some(vc_core::ids::ModelId::new("gemini-3.1-flash-lite")),
            prompt_hash: Some("cd".repeat(32)),
            run_id: None,
        },
        licence_class: class,
        use_policy: UsePolicy::ALLOW_ALL,
        valid_from: t0(),
        superseded_at: None,
    }
}

fn assert_item_shape(it: &Value, ctx: &str) {
    let lic = &it["licence"];
    assert!(lic.is_object(), "{ctx}: licence");
    assert!(lic["class"].is_string(), "{ctx}: licence.class");
    assert!(lic["attribution"].is_string(), "{ctx}: licence.attribution");
    let prov = it["provenance"].as_array().expect("provenance");
    assert!(!prov.is_empty(), "{ctx}: provenance non-empty");
    for p in prov {
        assert!(p["source_id"].is_string(), "{ctx}");
        assert!(p["url"].is_string(), "{ctx}");
        assert!(p["captured_at"].is_string(), "{ctx}");
        assert!(p["extractor"]["method"].is_string(), "{ctx}");
    }
    assert!(
        it["schema"].as_str().expect("schema").contains('@'),
        "{ctx}"
    );
    assert!(it["conflict"].is_boolean(), "{ctx}: conflict");
}

#[tokio::test]
async fn licence_and_provenance_on_every_item() {
    let h = harness().await;
    for (i, class) in ALL.iter().enumerate() {
        h.index
            .add_item(item(&format!("it-{i}"), "attraction", *class));
        h.index
            .add_item(item(&format!("ar-{i}"), "article", *class));
    }
    let eid = EntityId::new();
    let mut ent = item(&eid.to_string(), "attraction", LicenceClass::Open);
    ent.provenance
        .push(provenance("other-src", "https://other.example/x"));
    h.index.add_entity(
        eid,
        EntityView {
            item: ent,
            claims: vec![ClaimView {
                attribute: "name".into(),
                value: serde_json::json!("Tower of London"),
                record_id: RecordId(1),
                version: 1,
                confidence: 0.9,
            }],
        },
    );

    let mut n = 0;
    for path in [
        "/v1/search?q=tower",
        "/v1/entities",
        "/v1/entities?kind=attraction&place=2643743",
        "/v1/articles",
    ] {
        let body = h.json(path).await;
        let items = body["items"].as_array().expect("items");
        assert!(!items.is_empty(), "{path}");
        for it in items {
            assert_item_shape(it, path);
            n += 1;
        }
    }
    let body = h.json(&format!("/v1/entities/{eid}")).await;
    assert_item_shape(&body["item"], "entity");
    assert_eq!(body["item"]["provenance"].as_array().expect("p").len(), 2);
    assert_eq!(body["claims"][0]["record_id"], 1);
    assert!(n >= 16);

    // Record versions carry the licence class and their extractor (provenance).
    h.index.add_versions(
        RecordId(3),
        vec![version(3, LicenceClass::Open, "short".into())],
    );
    let body = h.json("/v1/records/3/versions").await;
    let v = &body["items"][0];
    assert_eq!(v["licence_class"], "open");
    assert_eq!(v["extractor"]["method"], "llm");
    assert!(v["captured_at"].is_string());

    // Observations carry their licence.
    h.index.add_observation(ObservationView {
        series_key: "fx:USD:EUR".into(),
        observed_at: t0(),
        value: serde_json::json!(0.93),
        source_id: SourceId("ecb".into()),
        licence: licence(LicenceClass::Open),
    });
    let body = h
        .json(
            "/v1/observations?series=fx:USD:EUR&from=2026-10-01T00:00:00Z&to=2026-12-01T00:00:00Z",
        )
        .await;
    assert_eq!(body["items"][0]["licence"]["class"], "open");
    assert_eq!(body["items"][0]["source_id"], "ecb");
}

#[tokio::test]
async fn redirected_entity_301() {
    let h = harness().await;
    let old = EntityId::new();
    let new = EntityId::new();
    h.index.add_redirect(old, new);
    h.index.add_entity(
        new,
        EntityView {
            item: item(&new.to_string(), "place", LicenceClass::Open),
            claims: vec![],
        },
    );
    let r = h
        .get(&format!("/v1/entities/{old}"))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::MOVED_PERMANENTLY);
    let loc = r
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("location")
        .to_string();
    assert_eq!(loc, format!("/v1/entities/{new}"));
    let body: Value = r.json().await.expect("json");
    assert_eq!(body["redirect_to"], new.to_string());
    // Following it lands on the entity.
    let body = h.json(&loc).await;
    assert_eq!(body["item"]["id"], new.to_string());
    // Unknown → 404, malformed → 400.
    let r = h
        .get(&format!("/v1/entities/{}", EntityId::new()))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = h.get("/v1/entities/not-an-id").send().await.expect("send");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn licence_classes_filtered_per_client() {
    let h = harness_with(
        ApiConfig::default(),
        vec![client("open-only", "pw", &[LicenceClass::Open], 10_000)],
        "open-only.pw",
    )
    .await;
    h.index
        .add_item(item("a", "attraction", LicenceClass::Open));
    h.index
        .add_item(item("b", "attraction", LicenceClass::ShareAlike));
    h.index
        .add_item(item("c", "attraction", LicenceClass::FactsOnly));
    let body = h.json("/v1/search?q=x").await;
    let ids: Vec<&str> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        ids,
        vec!["a"],
        "the API filters even if the reader does not"
    );
    let q = h.index.last_query.lock().expect("lock").clone().expect("q");
    assert_eq!(q.licence_classes, vec![LicenceClass::Open]);

    let eid = EntityId::new();
    h.index.add_entity(
        eid,
        EntityView {
            item: item(&eid.to_string(), "place", LicenceClass::ShareAlike),
            claims: vec![],
        },
    );
    let r = h
        .get(&format!("/v1/entities/{eid}"))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn use_policy_restricts_served_fields() {
    let h = harness().await;
    let mut link = item("link", "article", LicenceClass::Open);
    link.use_policy.serve = ServeUse::LinkOnly;
    h.index.add_item(link);
    let mut facts = item("facts", "article", LicenceClass::Open);
    facts.use_policy.serve = ServeUse::FactsOnly;
    h.index.add_item(facts);
    let mut hidden = item("hidden", "article", LicenceClass::Open);
    hidden.use_policy.index = IndexUse::None;
    h.index.add_item(hidden);
    let body = h.json("/v1/search").await;
    let items = body["items"].as_array().expect("items");
    let get = |id: &str| items.iter().find(|i| i["id"] == id).cloned();
    let link = get("link").expect("link");
    assert!(link["excerpt"].is_null());
    assert_eq!(link["data"], serde_json::json!({}));
    assert!(link["provenance"][0]["url"].is_string());
    let facts = get("facts").expect("facts");
    assert!(facts["excerpt"].is_null());
    assert!(facts["data"]["name"].is_string());
    assert!(get("hidden").is_none());
}

#[tokio::test]
async fn rate_limited_per_client_429() {
    let h = harness_with(
        ApiConfig::default(),
        vec![
            client("slow", "pw", ALL, 3),
            client("fast", "pw", ALL, 10_000),
        ],
        "slow.pw",
    )
    .await;
    for _ in 0..3 {
        assert_eq!(
            h.get("/v1/sources").send().await.expect("send").status(),
            StatusCode::OK
        );
    }
    let r = h.get("/v1/sources").send().await.expect("send");
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = r
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("retry-after");
    assert!((1..=60).contains(&retry));
    // Another client is unaffected.
    let r = h
        .http
        .get(format!("{}/v1/sources", h.base))
        .bearer_auth("fast.pw")
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn metrics_not_on_public_listener() {
    let h = harness().await;
    let r = h.get("/metrics").send().await.expect("send");
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn places_resolve_and_limits() {
    let h = harness().await;
    let body = h.json("/v1/places/resolve?name=London&country=GB").await;
    assert_eq!(body["items"][0]["geoname_id"], 2_643_743);
    let r = h.get("/v1/places/resolve").send().await.expect("send");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // limit is clamped to max_page.
    h.json("/v1/search?limit=100000").await;
    let q = h.index.last_query.lock().expect("lock").clone().expect("q");
    assert_eq!(q.limit, ApiConfig::default().max_page);
    let r = h.get("/v1/search?limit=abc").send().await.expect("send");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let body: Value = r.json().await.expect("json");
    assert_eq!(body["error"], "bad_request");
    // Radius search needs lat, lon and radius together.
    let r = h.get("/v1/search?lat=1").send().await.expect("send");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    h.json("/v1/search?lat=51.5&lon=-0.1&radius_km=5").await;
    let q = h.index.last_query.lock().expect("lock").clone().expect("q");
    assert_eq!(q.near.map(|n| n.1), Some(5.0));
    // Unknown /v1 routes are 404 for authenticated callers.
    let r = h.get("/v1/nope").send().await.expect("send");
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn metrics_localhost_only() {
    let state = ApiState::new(
        ApiConfig::default(),
        Arc::new(MemIndex::new()),
        Arc::new(MemPlaces::default()),
        Arc::new(MemClientStore::new(vec![client(
            "voyager", "s3cret", ALL, 10_000,
        )])),
        Arc::new(MemCatalog::default()),
    );
    let api = start(state.clone()).await;
    let http = reqwest::Client::new();
    http.get(format!("{api}/v1/sources"))
        .bearer_auth("voyager.s3cret")
        .send()
        .await
        .expect("send");
    http.get(format!("{api}/v1/sources"))
        .send()
        .await
        .expect("send");

    // Loopback peer: served.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = l.local_addr().expect("addr");
    let app = vc_api::metrics_router(&state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await.expect("serve") });
    let r = http
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::OK);
    let text = r.text().await.expect("text");
    assert!(text.contains(r#"vc_api_requests_total{route="/v1/sources",status="200"} 1"#));
    assert!(text.contains(r#"vc_api_requests_total{route="/v1/sources",status="401"} 1"#));
    assert!(text.contains("vc_api_auth_failures_total 1"));
    assert!(!text.contains("s3cret"));

    // Without a known loopback peer: refused.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = l.local_addr().expect("addr");
    let app = vc_api::metrics_router(&state);
    tokio::spawn(async move { axum::serve(l, app).await.expect("serve") });
    let r = http
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn sources_stats_and_probes() {
    let h = harness().await;
    let body = h.json("/v1/stats").await;
    assert_eq!(body["sources_enabled"], 0);
    assert!(body["generated_at"].is_string());
    let body = h.json("/v1/sources").await;
    assert_eq!(body["items"], serde_json::json!([]));
    let r = h
        .http
        .get(format!("{}/healthz", h.base))
        .send()
        .await
        .expect("send");
    let body: Value = r.json().await.expect("json");
    assert_eq!(body["status"], "ok");
    // Observations: from must precede to.
    let r = h
        .get("/v1/observations?series=x&from=2026-12-01T00:00:00Z&to=2026-11-01T00:00:00Z")
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = h.get("/v1/observations").send().await.expect("send");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "series is required");
    let r = h
        .get("/v1/records/abc/versions")
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn healthz_follows_the_host_process_health_probe() {
    let healthy = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let probe = healthy.clone();
    let state = ApiState::new(
        ApiConfig::default(),
        Arc::new(MemIndex::new()),
        Arc::new(MemPlaces { matches: vec![] }),
        Arc::new(MemClientStore::new(vec![])),
        Arc::new(MemCatalog::default()),
    )
    .with_health(Arc::new(move || {
        probe.load(std::sync::atomic::Ordering::Relaxed)
    }));
    let base = start(state).await;
    let http = reqwest::Client::new();
    let r = http
        .get(format!("{base}/healthz"))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::OK);
    healthy.store(false, std::sync::atomic::Ordering::Relaxed);
    let r = http
        .get(format!("{base}/healthz"))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = r.json().await.expect("json");
    assert_eq!(body["status"], "unhealthy");
}

// ---------------------------------------------------------------------------------------------
// /v1/carbon (2026-11-19): emission factors and grid intensity for the trip carbon footprint.
// ---------------------------------------------------------------------------------------------

fn factor(id: &str, class: LicenceClass, data: Value, source: &str) -> vc_core::index::Item {
    let mut it = item(id, "emission_factor", class);
    it.data = data;
    it.provenance = vec![provenance(source, &format!("https://example.org/{id}"))];
    it
}

fn grid_point(series: &str, year: i32, g: f64, class: LicenceClass) -> ObservationView {
    ObservationView {
        series_key: series.into(),
        observed_at: chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, year, 1, 1, 0, 0, 0)
            .single()
            .expect("date"),
        value: serde_json::json!({"intensity_g_per_kwh": g, "country_code": &series[8..10]}),
        source_id: SourceId("carbon-owid-grapher-ci".into()),
        licence: licence(class),
    }
}

async fn carbon_harness(classes: &[LicenceClass]) -> Harness {
    let cat = MemCatalog {
        factors: vec![
            factor(
                "1",
                LicenceClass::Open,
                serde_json::json!({"activity": "flights_long_haul_to_from_uk_economy_class_with_rf",
                    "category": "flights", "country_code": "GB", "gas_basis": "co2e_with_rf",
                    "year": 2026, "value": 0.117, "unit": "kg CO2e per passenger.km"}),
                "od-desnz-ghg-factors",
            ),
            factor(
                "2",
                LicenceClass::Open,
                serde_json::json!({"activity": "flights_long_haul_to_from_uk_economy_class_with_rf",
                    "category": "flights", "country_code": "GB", "gas_basis": "co2e_with_rf",
                    "year": 2025, "value": 0.12}),
                "od-desnz-ghg-factors",
            ),
            factor(
                "3",
                LicenceClass::Open,
                serde_json::json!({"activity": "electricity_generation_total_output_emission_rate_wecc_california",
                    "category": "electricity", "country_code": "US", "region_code": "CAMX",
                    "gas_basis": "co2e", "year": 2023, "value": 429.983}),
                "od-epa-egrid",
            ),
            factor(
                "4",
                LicenceClass::ShareAlike,
                serde_json::json!({"activity": "beef", "category": "food", "country_code": "FR",
                    "gas_basis": "co2e", "year": 2025, "value": 30.0}),
                "od-concito-big-climate-db",
            ),
            // `serve: link_only` is never served by the factors read (the SQL excludes it).
            {
                let mut it = factor(
                    "5",
                    LicenceClass::Open,
                    serde_json::json!({"activity": "beef", "category": "food", "country_code": "FR",
                        "gas_basis": "co2e", "year": 2025, "value": 31.0}),
                    "od-concito-big-climate-db",
                );
                it.use_policy.serve = vc_core::usepolicy::ServeUse::LinkOnly;
                it
            },
        ],
        grid: vec![
            grid_point(
                "grid_ci:FR:annual:lifecycle",
                2023,
                56.0,
                LicenceClass::Open,
            ),
            grid_point(
                "grid_ci:FR:annual:lifecycle",
                2024,
                40.48,
                LicenceClass::Open,
            ),
            grid_point(
                "grid_ci:FR:monthly:lifecycle",
                2025,
                35.0,
                LicenceClass::Open,
            ),
            grid_point(
                "grid_ci:DE:annual:lifecycle",
                2024,
                330.0,
                LicenceClass::Open,
            ),
        ],
        ..MemCatalog::default()
    };
    let state = ApiState::new(
        ApiConfig::default(),
        Arc::new(MemIndex::new()),
        Arc::new(MemPlaces::default()),
        Arc::new(MemClientStore::new(vec![client(
            "voyager", "s3cret", classes, 10_000,
        )])),
        Arc::new(cat),
    );
    let base = start(state).await;
    Harness {
        base,
        http: reqwest::Client::new(),
        index: Arc::new(MemIndex::new()),
        token: "voyager.s3cret".into(),
    }
}

fn ids(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["id"].as_str().expect("id").to_string())
        .collect()
}

#[tokio::test]
async fn carbon_factors_filter_by_category_country_region_year_source() {
    let h = carbon_harness(ALL).await;
    // Newest factor-set year first.
    let all = h.json("/v1/carbon/factors").await;
    assert_eq!(ids(&all), ["1", "2", "4", "3"]);
    assert_eq!(all["items"][0]["kind"], "emission_factor");
    assert_eq!(
        ids(&h
            .json("/v1/carbon/factors?category=flights&country=gb")
            .await),
        ["1", "2"]
    );
    assert_eq!(
        ids(&h
            .json("/v1/carbon/factors?category=flights&year=2025")
            .await),
        ["2"]
    );
    assert_eq!(ids(&h.json("/v1/carbon/factors?region=camx").await), ["3"]);
    assert_eq!(
        ids(&h
            .json("/v1/carbon/factors?source=od-epa-egrid&category=electricity")
            .await),
        ["3"]
    );
    assert_eq!(
        ids(&h
            .json("/v1/carbon/factors?activity=long_haul&gas_basis=co2e_with_rf")
            .await),
        ["1", "2"]
    );
    assert!(ids(&h.json("/v1/carbon/factors?category=rail").await).is_empty());
    // Paging.
    let p1 = h.json("/v1/carbon/factors?limit=2").await;
    assert_eq!(ids(&p1), ["1", "2"]);
    let next = p1["next_cursor"].as_str().expect("cursor").to_string();
    let p2 = h
        .json(&format!("/v1/carbon/factors?limit=2&cursor={next}"))
        .await;
    assert_eq!(ids(&p2), ["4", "3"]);
    assert!(p2["next_cursor"].is_null());
    // Bad filters are 400, never a wildcard.
    for bad in [
        "/v1/carbon/factors?category=planes",
        "/v1/carbon/factors?country=G%25",
        "/v1/carbon/factors?activity=Long%20Haul",
        "/v1/carbon/factors?gas_basis=ch4",
        "/v1/carbon/factors?cursor=abc",
        "/v1/carbon/factors?year=x",
    ] {
        let r = h.get(bad).send().await.expect("send");
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn carbon_factors_respect_the_client_licence_classes() {
    let h = carbon_harness(&[LicenceClass::Open]).await;
    let v = h.json("/v1/carbon/factors").await;
    assert_eq!(
        ids(&v),
        ["1", "2", "3"],
        "share_alike CONCITO row not served"
    );
}

#[tokio::test]
async fn carbon_grid_is_one_country_newest_first() {
    let h = carbon_harness(ALL).await;
    let v = h.json("/v1/carbon/grid?country=fr").await;
    let keys: Vec<(String, String)> = v["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|o| {
            (
                o["series_key"].as_str().unwrap().to_string(),
                o["observed_at"].as_str().unwrap()[..4].to_string(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        [
            (
                "grid_ci:FR:monthly:lifecycle".to_string(),
                "2025".to_string()
            ),
            (
                "grid_ci:FR:annual:lifecycle".to_string(),
                "2024".to_string()
            ),
            (
                "grid_ci:FR:annual:lifecycle".to_string(),
                "2023".to_string()
            ),
        ]
    );
    let annual = h
        .json("/v1/carbon/grid?country=FR&period=annual&basis=lifecycle&limit=1")
        .await;
    assert_eq!(annual["items"].as_array().unwrap().len(), 1);
    assert_eq!(annual["items"][0]["value"]["intensity_g_per_kwh"], 40.48);
    let y = h
        .json("/v1/carbon/grid?country=FR&from=2023-06-01T00:00:00Z&to=2025-01-01T00:00:00Z")
        .await;
    assert_eq!(y["items"].as_array().unwrap().len(), 1);
    for bad in [
        "/v1/carbon/grid",
        "/v1/carbon/grid?country=FRA",
        "/v1/carbon/grid?country=F%25",
        "/v1/carbon/grid?country=FR&period=weekly",
        "/v1/carbon/grid?country=FR&basis=x",
        "/v1/carbon/grid?country=FR&from=2025-01-01T00:00:00Z&to=2024-01-01T00:00:00Z",
    ] {
        let r = h.get(bad).send().await.expect("send");
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

// ---------------------------------------------------------------------------------------------
// /v1/carbon/occupancy (2026-11-19): persons per vehicle, mirroring /v1/carbon/factors.
// ---------------------------------------------------------------------------------------------

fn occ(id: &str, class: LicenceClass, data: Value, source: &str) -> vc_core::index::Item {
    let mut it = item(id, "occupancy", class);
    it.data = data;
    it.provenance = vec![provenance(source, &format!("https://example.org/{id}"))];
    it
}

async fn occupancy_harness(classes: &[LicenceClass]) -> Harness {
    let car = |purpose: &str, year: i32, v: f64| {
        serde_json::json!({"mode": "car", "trip_purpose": purpose, "country_code": "GB",
            "basis": "per_vehicle_km", "year": year, "value": v,
            "unit": "persons per vehicle", "includes_driver": true})
    };
    let cat = MemCatalog {
        occupancy: vec![
            occ(
                "1",
                LicenceClass::Open,
                car("all", 2023, 1.61),
                "od-dft-tag-databook",
            ),
            occ(
                "2",
                LicenceClass::Open,
                car("work", 2023, 1.12),
                "od-dft-tag-databook",
            ),
            occ(
                "3",
                LicenceClass::Open,
                serde_json::json!({"mode": "bus_coach", "trip_purpose": "all",
                    "country_code": "GB", "basis": "per_vehicle_km", "year": 2023,
                    "value": 13.8}),
                "od-dft-tag-databook",
            ),
            occ(
                "4",
                LicenceClass::Open,
                serde_json::json!({"mode": "car", "trip_purpose": "all", "country_code": "US",
                    "basis": "per_trip", "year": 2022, "value": 1.4}),
                "od-other-occupancy",
            ),
            occ(
                "5",
                LicenceClass::ShareAlike,
                car("all", 2024, 1.5),
                "od-sa-occupancy",
            ),
            // `serve: link_only` is never served (the SQL excludes it).
            {
                let mut it = occ("6", LicenceClass::Open, car("all", 2025, 1.7), "od-x");
                it.use_policy.serve = vc_core::usepolicy::ServeUse::LinkOnly;
                it
            },
        ],
        // An emission factor is never an occupancy.
        factors: vec![factor(
            "9",
            LicenceClass::Open,
            serde_json::json!({"activity": "car", "category": "car", "country_code": "GB",
                "gas_basis": "co2e", "year": 2026, "value": 0.17}),
            "od-desnz-ghg-factors",
        )],
        ..MemCatalog::default()
    };
    let state = ApiState::new(
        ApiConfig::default(),
        Arc::new(MemIndex::new()),
        Arc::new(MemPlaces::default()),
        Arc::new(MemClientStore::new(vec![client(
            "voyager", "s3cret", classes, 10_000,
        )])),
        Arc::new(cat),
    );
    let base = start(state).await;
    Harness {
        base,
        http: reqwest::Client::new(),
        index: Arc::new(MemIndex::new()),
        token: "voyager.s3cret".into(),
    }
}

#[tokio::test]
async fn carbon_occupancy_filters_by_mode_country_year_purpose_basis_source() {
    let h = occupancy_harness(ALL).await;
    // Newest year first, then by id; link_only and emission factors never served.
    let all = h.json("/v1/carbon/occupancy").await;
    assert_eq!(ids(&all), ["5", "1", "2", "3", "4"]);
    assert_eq!(all["items"][1]["kind"], "occupancy");
    assert_eq!(all["items"][1]["data"]["value"], 1.61);
    assert_eq!(
        ids(&h.json("/v1/carbon/occupancy?mode=car&country=gb").await),
        ["5", "1", "2"]
    );
    assert_eq!(
        ids(&h.json("/v1/carbon/occupancy?mode=car&year=2023").await),
        ["1", "2"]
    );
    assert_eq!(
        ids(&h.json("/v1/carbon/occupancy?trip_purpose=work").await),
        ["2"]
    );
    assert_eq!(
        ids(&h.json("/v1/carbon/occupancy?mode=bus_coach").await),
        ["3"]
    );
    assert_eq!(
        ids(&h.json("/v1/carbon/occupancy?basis=per_trip").await),
        ["4"]
    );
    assert_eq!(
        ids(&h
            .json("/v1/carbon/occupancy?source=od-dft-tag-databook&trip_purpose=all")
            .await),
        ["1", "3"]
    );
    assert!(ids(&h.json("/v1/carbon/occupancy?mode=rail").await).is_empty());
    // Paging.
    let p1 = h.json("/v1/carbon/occupancy?limit=3").await;
    assert_eq!(ids(&p1), ["5", "1", "2"]);
    let next = p1["next_cursor"].as_str().expect("cursor").to_string();
    let p2 = h
        .json(&format!("/v1/carbon/occupancy?limit=3&cursor={next}"))
        .await;
    assert_eq!(ids(&p2), ["3", "4"]);
    assert!(p2["next_cursor"].is_null());
    // Bad filters are 400, never a wildcard.
    for bad in [
        "/v1/carbon/occupancy?mode=plane",
        "/v1/carbon/occupancy?country=G%25",
        "/v1/carbon/occupancy?trip_purpose=holiday",
        "/v1/carbon/occupancy?basis=per_seat",
        "/v1/carbon/occupancy?cursor=abc",
        "/v1/carbon/occupancy?year=x",
    ] {
        let r = h.get(bad).send().await.expect("send");
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn carbon_occupancy_respects_the_client_licence_classes() {
    let h = occupancy_harness(&[LicenceClass::Open]).await;
    let v = h.json("/v1/carbon/occupancy").await;
    assert_eq!(ids(&v), ["1", "2", "3", "4"], "share_alike row not served");
}

#[tokio::test]
async fn carbon_occupancy_needs_a_token() {
    let h = occupancy_harness(ALL).await;
    let r = h
        .http
        .get(format!("{}/v1/carbon/occupancy", h.base))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------------------------
// /v1/facts (2026-11-20): place facts; Wikipedia hooks served only once resolved to a Wikidata
// place through its sitelinks.
// ---------------------------------------------------------------------------------------------

fn fact(id: &str, class: LicenceClass, data: Value, source: &str) -> vc_core::index::Item {
    let mut it = item(id, "place_fact", class);
    it.data = data;
    it.place_ids = vec![];
    it.provenance = vec![provenance(source, &format!("https://example.org/{id}"))];
    it
}

fn berlin_place() -> Value {
    serde_json::json!({"geonames_id": 2950159, "wikidata_qid": "Q64", "admin_level": "admin1",
        "country": "DE", "name": "Berlin", "name_lang": "en",
        "sitelinks": ["dewiki:Berlin", "simplewiki:Berlin"]})
}

async fn facts_harness(classes: &[LicenceClass]) -> Harness {
    let wd = |text: &str, ty: &str, extra: Value| {
        let mut d = serde_json::json!({"place": berlin_place(), "fact_type": ty, "text": text,
            "lang": "en", "text_origin": "templated", "llm_ok": true, "confidence": "structured",
            "source_url": "https://www.wikidata.org/wiki/Q64#P571", "as_of": "2026-11-20",
            "source": {"name": "Wikidata", "publisher": "Wikidata contributors", "sourced": "yes"}});
        for (k, v) in extra.as_object().unwrap() {
            d[k] = v.clone();
        }
        d
    };
    let hook = |text: &str, cands: Value| {
        serde_json::json!({"place": {"link_candidates": cands}, "fact_type": "fun",
            "text": text, "lang": "en", "text_origin": "source_hook", "llm_ok": false,
            "confidence": "extracted",
            "source_url": "https://simple.wikipedia.org/wiki/Wikipedia:Did_you_know"})
    };
    let cat = MemCatalog {
        sources: vec![vc_api::dto::SourceDto {
            source_id: "wikidata-facts".into(),
            name: "Wikidata EntityData".into(),
            category: "facts".into(),
            source_class: "reference".into(),
            licence: vc_api::dto::LicenceDto::from_core(&licence(LicenceClass::Open)).unwrap(),
            method: "api".into(),
            enabled: true,
        }],
        facts: vec![
            fact(
                "1",
                LicenceClass::Open,
                wd(
                    "Berlin was founded in 1244.",
                    "historical",
                    serde_json::json!({"date_ref": "1244"}),
                ),
                "wikidata-facts",
            ),
            fact(
                "2",
                LicenceClass::Open,
                wd(
                    "Berlin had a population of 3,700,577 in 2025.",
                    "popular",
                    serde_json::json!({"quantity": {"value": 3700577.0, "unit": "people"}}),
                ),
                "wikidata-facts",
            ),
            // A hook: resolved through `simplewiki:Berlin` (the second candidate).
            fact(
                "3",
                LicenceClass::ShareAlike,
                hook(
                    "... that Berlin has more bridges than Venice?",
                    serde_json::json!(["simplewiki:Venice", "simplewiki:Berlin"]),
                ),
                "wiki-facts-simple",
            ),
            // A hook nothing resolves: never served.
            fact(
                "4",
                LicenceClass::ShareAlike,
                hook(
                    "... that Atlantis is not on any map at all?",
                    serde_json::json!(["simplewiki:Atlantis"]),
                ),
                "wiki-facts-simple",
            ),
            // A country fact (Factbook).
            fact(
                "5",
                LicenceClass::Open,
                serde_json::json!({"place": {"country": "DE", "admin_level": "country", "name": "Germany"},
                    "fact_type": "unbelievable",
                    "text": "The highest point of Germany is Zugspitze (2,963 m).",
                    "text_origin": "templated", "llm_ok": true, "confidence": "structured",
                    "quantity": {"value": 2963.0, "unit": "m", "rank_scope": "highest point of Germany"},
                    "as_of": "2026-02-04"}),
                "od-factbook-json",
            ),
            // Link-only: never served here.
            {
                let mut it = fact(
                    "6",
                    LicenceClass::Open,
                    wd(
                        "Berlin is nicknamed Spree-Athen.",
                        "popular",
                        serde_json::json!({}),
                    ),
                    "wikidata-facts",
                );
                it.use_policy.serve = ServeUse::LinkOnly;
                it
            },
            // An AI-reserved editorial fact: served, but never marked for a model.
            fact(
                "7",
                LicenceClass::FactsOnly,
                serde_json::json!({"place": {"geonames_id": 2950159, "country": "DE"},
                    "fact_type": "little_known", "text": "A rewritten sentence about Berlin.",
                    "text_origin": "rewritten", "llm_ok": true, "ai_bot_blocked": true,
                    "confidence": "extracted"}),
                "fx-example",
            ),
        ],
        ..MemCatalog::default()
    };
    let state = ApiState::new(
        ApiConfig::default(),
        Arc::new(MemIndex::new()),
        Arc::new(MemPlaces::default()),
        Arc::new(MemClientStore::new(vec![client(
            "voyager", "s3cret", classes, 10_000,
        )])),
        Arc::new(cat),
    );
    let base = start(state).await;
    Harness {
        base,
        http: reqwest::Client::new(),
        index: Arc::new(MemIndex::new()),
        token: "voyager.s3cret".into(),
    }
}

#[tokio::test]
async fn facts_filter_by_place_country_and_type_and_resolve_hooks() {
    let h = facts_harness(ALL).await;
    let all = h.json("/v1/facts").await;
    assert_eq!(
        ids(&all),
        ["1", "2", "3", "5", "7"],
        "unresolved hooks and link-only never served"
    );
    let f1 = &all["items"][0];
    assert_eq!(f1["place"]["wikidata_qid"], "Q64");
    assert_eq!(f1["place"]["geonames_id"], 2950159);
    assert_eq!(f1["fact_type"], "historical");
    assert_eq!(f1["text"], "Berlin was founded in 1244.");
    assert_eq!(f1["date_ref"], "1244");
    assert_eq!(f1["llm_ok"], true);
    assert_eq!(f1["confidence"], "structured");
    assert_eq!(f1["as_of"], "2026-11-20");
    assert_eq!(f1["source"]["source_id"], "wikidata-facts");
    assert_eq!(
        f1["source"]["url"],
        "https://www.wikidata.org/wiki/Q64#P571"
    );
    assert_eq!(f1["source"]["name"], "Wikidata");
    assert_eq!(f1["source"]["publisher"], "Wikidata contributors");
    assert_eq!(
        f1["source"]["source_class"], "reference",
        "from the source registry"
    );
    assert_eq!(f1["source"]["sourced"], "yes");
    assert_eq!(f1["licence"]["class"], "open");
    assert!(f1.get("sitelinks").is_none() && f1["place"].get("sitelinks").is_none());
    // The hook takes Berlin's place; it is share-alike and never for a model.
    let hook = &all["items"][2];
    assert_eq!(hook["place"]["wikidata_qid"], "Q64");
    assert_eq!(hook["place"]["country"], "DE");
    assert_eq!(hook["text_origin"], "source_hook");
    assert_eq!(hook["llm_ok"], false);
    assert_eq!(hook["licence"]["class"], "share_alike");
    assert_eq!(
        hook["as_of"], "2026-11-01",
        "the capture date (t0) when the data has none"
    );
    assert_eq!(
        all["items"][3]["quantity"]["rank_scope"],
        "highest point of Germany"
    );
    assert_eq!(all["items"][4]["llm_ok"], false, "AI-reserved source");
    assert_eq!(all["items"][4]["ai_bot_blocked"], true);

    assert_eq!(
        ids(&h.json("/v1/facts?wikidata_qid=Q64").await),
        ["1", "2", "3"]
    );
    assert_eq!(
        ids(&h.json("/v1/facts?wikidata_qid=q64").await),
        ["1", "2", "3"]
    );
    assert_eq!(
        ids(&h.json("/v1/facts?geonames_id=2950159").await),
        ["1", "2", "3", "7"]
    );
    assert_eq!(
        ids(&h.json("/v1/facts?country=de&fact_type=unbelievable").await),
        ["5"]
    );
    assert_eq!(ids(&h.json("/v1/facts?fact_type=fun").await), ["3"]);
    assert!(ids(&h.json("/v1/facts?country=FR").await).is_empty());
    // Paging.
    let p1 = h.json("/v1/facts?limit=2").await;
    assert_eq!(ids(&p1), ["1", "2"]);
    let next = p1["next_cursor"].as_str().expect("cursor").to_string();
    let p2 = h.json(&format!("/v1/facts?limit=2&cursor={next}")).await;
    assert_eq!(ids(&p2), ["3", "5"]);
    let p3 = h
        .json(&format!(
            "/v1/facts?limit=2&cursor={}",
            p2["next_cursor"].as_str().unwrap()
        ))
        .await;
    assert_eq!(ids(&p3), ["7"]);
    assert!(p3["next_cursor"].is_null());
    for bad in [
        "/v1/facts?fact_type=trivia",
        "/v1/facts?wikidata_qid=P31",
        "/v1/facts?wikidata_qid=Q0",
        "/v1/facts?country=D%25",
        "/v1/facts?geonames_id=-1",
        "/v1/facts?geonames_id=x",
        "/v1/facts?cursor=abc",
    ] {
        let r = h.get(bad).send().await.expect("send");
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn facts_respect_the_client_licence_classes() {
    let h = facts_harness(&[LicenceClass::Open]).await;
    assert_eq!(
        ids(&h.json("/v1/facts").await),
        ["1", "2", "5"],
        "no share-alike hooks, no facts-only"
    );
}

#[tokio::test]
async fn facts_need_a_token() {
    let h = facts_harness(ALL).await;
    let r = h
        .http
        .get(format!("{}/v1/facts", h.base))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}
