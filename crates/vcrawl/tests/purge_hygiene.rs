//! Purge and expiry hygiene (final review B4, B8, B9, B12): removed captures release their
//! URL's validators, a crash between a rewrite's commit and its object deletion is cleaned up
//! later (and never resurrected by `rebuild-captures`), purged URLs stop being crawled, and a
//! stale rewrite generation is stepped over and deleted.
//!
//! Failpoints are process-global, so the tests that use one take `SERIAL`.

mod admin_common;
mod crawl_path_support;

use std::sync::{Arc, Mutex};

use admin_common::{at, commit_segment, count, entry, meta};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use testkit::{FakeServer, MemBlobStore};
use vc_core::frontier::NewUrl;
use vc_core::index::{PurgeSelector, TombstoneReason};
use vc_core::store::BlobStore;
use vc_core::{CanonicalUrl, SegmentId};
use vcrawl::admin::purge::{process_pending, process_purge, request_purge};
use vcrawl::admin::rebuild::{RebuildArgs, rebuild_captures};
use vcrawl::roles::fetcher::Pass;
use vcrawl::roles::maintenance::Maintenance;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const ETAG: &str = "\"v1\"";

/// An origin that honours `If-None-Match`; records whether each page request was conditional.
#[derive(Default)]
struct Origin {
    requests: Mutex<Vec<bool>>,
}

async fn page(State(o): State<Arc<Origin>>, h: HeaderMap) -> Response {
    let cond = h
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == ETAG);
    o.requests.lock().unwrap().push(cond);
    if cond {
        (StatusCode::NOT_MODIFIED, [(header::ETAG, ETAG)]).into_response()
    } else {
        (
            StatusCode::OK,
            [
                (header::ETAG, ETAG),
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            ],
            "<html><p>the body</p></html>",
        )
            .into_response()
    }
}

fn maint(pool: &sqlx::PgPool, store: Arc<dyn BlobStore>) -> Maintenance {
    Maintenance {
        pool: pool.clone(),
        store,
        metrics: None,
        partitions_ahead_months: 2,
        agentic: None,
    }
}

/// B4: a noarchive capture expires (or is purged) → the validators it vouched for go with it,
/// so the next fetch is unconditional and stores a body (never a 304 revisit of bytes that no
/// longer exist).
#[tokio::test]
async fn expiry_releases_validators_and_the_next_fetch_stores_a_body() {
    use crawl_path_support as cp;
    let o = Arc::new(Origin::default());
    let app = Router::new()
        .route("/page", get(page))
        .route("/robots.txt", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(o.clone());
    let srv = FakeServer::spawn(app).await;
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    let blobs: Arc<dyn BlobStore> = store.clone();
    let dir = tempfile::tempdir().unwrap();
    let cfg = cp::cfg();
    let hosts = ["origin.test"];
    let u = cp::url("origin.test", srv.addr().port(), "/page");
    let f = cp::fetcher(&cfg, &pool, &hosts, vec![], dir.path(), blobs.clone());
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(u.clone())])
        .await
        .unwrap();
    for _ in 0..2 {
        cp::fast_forward(&pool).await;
        assert!(matches!(f.pass().await.unwrap(), Pass::Claimed(_)));
        f.flush().await.unwrap();
    }
    assert_eq!(
        *o.requests.lock().unwrap(),
        vec![false, true],
        "200 then 304"
    );
    let st = cp::url_state(&pool, &u).await;
    assert!(st.1.is_some() && st.2.is_some() && st.3.is_some(), "{st:?}");

    // The page turns out to be noarchive: its raw bytes expire.
    sqlx::query(
        "UPDATE captures SET raw_expires_at = now() - interval '1 day' WHERE role = 'primary'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (caps, _) = maint(&pool, blobs.clone())
        .expire_ephemeral(chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(caps, 2);
    let st = cp::url_state(&pool, &u).await;
    assert_eq!(
        (st.1.as_deref(), st.2.as_deref(), st.3.as_deref()),
        (None, None, None),
        "validators of removed captures must be cleared"
    );

    cp::fast_forward(&pool).await;
    assert!(matches!(f.pass().await.unwrap(), Pass::Claimed(_)));
    f.flush().await.unwrap();
    assert_eq!(
        o.requests.lock().unwrap().last(),
        Some(&false),
        "the next fetch is unconditional"
    );
    let caps: Vec<_> = cp::captures(&pool)
        .await
        .into_iter()
        .filter(|c| c.1 == "primary")
        .collect();
    assert_eq!(caps.len(), 1, "{caps:?}");
    assert_eq!(
        (caps[0].2, caps[0].3.as_deref()),
        (Some(200), None),
        "a body"
    );
    let (path, off, len): (String, i64, i64) = sqlx::query_as(
        "SELECT s.path, c.warc_offset, c.warc_length FROM captures c
         JOIN segments s ON s.segment_id = c.segment_id WHERE c.role = 'primary'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let recs = vc_store::read_capture(blobs.as_ref(), &path, off as u64, len as u64)
        .await
        .unwrap();
    assert!(recs.iter().any(|r| r.warc_type() == Some("response")));
}

/// B4 (SQL level): only the URLs whose stored body is gone lose their validators.
#[tokio::test]
async fn removing_a_capture_clears_only_the_validators_it_backed() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let a = CanonicalUrl::parse("https://v.example/a").unwrap();
    let b = CanonicalUrl::parse("https://v.example/b").unwrap();
    for u in [&a, &b] {
        sqlx::query("INSERT INTO urls (url_key, url, key) VALUES ($1, $2, $3)")
            .bind(u.key().as_bytes().as_slice())
            .bind(u.as_str())
            .bind(u.politeness_key().as_str())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO url_state (url_key, key) VALUES ($1, $2)")
            .bind(u.key().as_bytes().as_slice())
            .bind(u.politeness_key().as_str())
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut ma = meta(a.as_str(), at(1, 1), None);
    ma.validators.etag = Some("\"a\"".into());
    ma.payload_hash = Some(vc_core::PayloadHash([1; 32]));
    let mut mb = meta(b.as_str(), at(1, 1), None);
    mb.validators.etag = Some("\"b\"".into());
    mb.payload_hash = Some(vc_core::PayloadHash([2; 32]));
    let doomed = ma.capture_ref();
    let seg = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(1, 1)),
        false,
        &[entry(ma, "a"), entry(mb, "b")],
    )
    .await;
    admin_common::purge_from_segment(&pool, &store, &seg, &[doomed.capture_id], true).await;
    let etags: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT x.url, u.etag, u.last_capture_id FROM url_state u
         JOIN urls x ON x.url_key = u.url_key ORDER BY x.url",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(etags[0].0, a.as_str());
    assert_eq!((etags[0].1.as_deref(), etags[0].2.as_deref()), (None, None));
    assert_eq!(
        etags[1].1.as_deref(),
        Some("\"b\""),
        "b keeps its validators"
    );
    assert!(etags[1].2.is_some());
}

/// B8: a crash after `vc_rewrite_segment` commits but before the old objects are deleted leaves
/// them listed in `store_garbage`; the next run deletes them, and `rebuild-captures` in between
/// never resurrects the purged capture.
#[tokio::test]
async fn crash_between_rewrite_commit_and_delete_is_collected_and_never_resurrected() {
    let _g = SERIAL.lock().await;
    let scenario = fail::FailScenario::setup();
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let doomed = meta("https://optout.example/x", at(2, 1), None);
    let doomed_id = doomed.capture_id;
    let seg_whole = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(2, 1)),
        false,
        &[entry(doomed, "secret")],
    )
    .await;
    let other = meta("https://optout.example/y", at(3, 1), None);
    let keep = meta("https://keep.example/z", at(3, 2), None);
    let keep_id = keep.capture_id;
    let seg_part = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(3, 1)),
        false,
        &[entry(other, "secret too"), entry(keep, "kept")],
    )
    .await;

    let id = request_purge(
        &pool,
        &PurgeSelector::Host("optout.example".into()),
        TombstoneReason::OptOut,
        "t",
    )
    .await
    .unwrap();
    fail::cfg("purge::after_commit_rewrite", "return").unwrap();
    assert!(process_purge(&pool, &store, id, at(4, 0)).await.is_err());
    fail::remove("purge::after_commit_rewrite");
    assert!(
        store.contains(&seg_whole.path),
        "crashed before the old objects were deleted"
    );
    assert!(count(&pool, "SELECT count(*) FROM store_garbage").await >= 2);

    // Disaster recovery in between: nothing purged comes back.
    rebuild_captures(&pool, &store, &RebuildArgs::default())
        .await
        .unwrap();
    let caps: Vec<String> = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(
        !caps.contains(&doomed_id.to_string()),
        "rebuild resurrected a purged segment"
    );

    // The maintenance run finishes the request and deletes the objects.
    process_pending(&pool, &store, at(4, 1)).await.unwrap();
    for p in [
        &seg_whole.path,
        &seg_whole.cdxj_path,
        &seg_part.path,
        &seg_part.cdxj_path,
    ] {
        assert!(!store.contains(p), "{p} left behind");
    }
    assert_eq!(count(&pool, "SELECT count(*) FROM store_garbage").await, 0);
    let live: String = sqlx::query_scalar("SELECT path FROM segments WHERE purged_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(store.contains(&live), "the live generation is kept");
    let state: String = sqlx::query_scalar("SELECT state FROM purge_requests WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "raw_done");
    let caps: Vec<String> = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(caps, vec![keep_id.to_string()]);
    scenario.teardown();
}

/// B8: after a restore that lost the purge's rows, `rebuild-captures` still honours the
/// standing exclusions (`tombstones`).
#[tokio::test]
async fn rebuild_honours_tombstones() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let gone = meta("https://optout.example/x", at(2, 1), None);
    let keep = meta("https://keep.example/z", at(2, 2), None);
    let keep_id = keep.capture_id;
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(2, 1)),
        false,
        &[entry(gone, "x"), entry(keep, "z")],
    )
    .await;
    sqlx::query("DELETE FROM captures")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM segments")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tombstones (selector_type, selector, reason)
         VALUES ('host', 'optout.example', 'opt_out')",
    )
    .execute(&pool)
    .await
    .unwrap();
    rebuild_captures(&pool, &store, &RebuildArgs::default())
        .await
        .unwrap();
    let caps: Vec<String> = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(caps, vec![keep_id.to_string()]);
}

async fn add_url(pool: &sqlx::PgPool, url: &str, state: &str, source: Option<&str>) {
    let u = CanonicalUrl::parse(url).unwrap();
    sqlx::query("INSERT INTO urls (url_key, url, key, source_id) VALUES ($1, $2, $3, $4)")
        .bind(u.key().as_bytes().as_slice())
        .bind(u.as_str())
        .bind(u.politeness_key().as_str())
        .bind(source)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO url_state (url_key, key, state, lease_epoch, source_id)
         VALUES ($1, $2, $3, CASE WHEN $3 IN ('leased', 'fetched_pending') THEN 9 END, $4)",
    )
    .bind(u.key().as_bytes().as_slice())
    .bind(u.politeness_key().as_str())
    .bind(state)
    .bind(source)
    .execute(pool)
    .await
    .unwrap();
}

async fn states(pool: &sqlx::PgPool) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT x.url, u.state FROM url_state u JOIN urls x ON x.url_key = u.url_key
         ORDER BY x.url",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// B9: a URL / host / prefix / source purge stops crawling the matching URLs (terminal
/// `tombstoned` state, kept when the URL is discovered again); others are untouched.
#[tokio::test]
async fn url_host_prefix_and_source_purges_stop_crawling() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    add_url(&pool, "https://a.example/one", "due", None).await;
    add_url(&pool, "https://a.example/two", "due", None).await;
    add_url(&pool, "https://b.example/x", "leased", None).await;
    add_url(&pool, "https://b.example/y", "due", None).await;
    add_url(&pool, "https://c.example/blog/1", "due", None).await;
    add_url(&pool, "https://c.example/shop/1", "due", None).await;
    add_url(&pool, "https://d.example/feed", "due", Some("dsrc")).await;
    add_url(&pool, "https://e.example/", "due", None).await;
    for sel in [
        PurgeSelector::Url(CanonicalUrl::parse("https://a.example/one").unwrap().key()),
        PurgeSelector::Host("b.example".into()),
        PurgeSelector::UrlPrefix("https://c.example/blog/".into()),
        PurgeSelector::Source(vc_core::SourceId("dsrc".into())),
    ] {
        let id = request_purge(&pool, &sel, TombstoneReason::OptOut, "t")
            .await
            .unwrap();
        process_purge(&pool, &store, id, at(5, 0)).await.unwrap();
    }
    let s = states(&pool).await;
    let get = |u: &str| s.iter().find(|x| x.0 == u).map(|x| x.1.as_str()).unwrap();
    assert_eq!(get("https://a.example/one"), "tombstoned");
    assert_eq!(get("https://a.example/two"), "due");
    assert_eq!(get("https://b.example/x"), "tombstoned", "a leased URL too");
    assert_eq!(get("https://b.example/y"), "tombstoned");
    assert_eq!(get("https://c.example/blog/1"), "tombstoned");
    assert_eq!(get("https://c.example/shop/1"), "due");
    assert_eq!(get("https://d.example/feed"), "tombstoned");
    assert_eq!(get("https://e.example/"), "due");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM url_state WHERE state = 'tombstoned' AND lease_epoch IS NOT NULL"
        )
        .await,
        0
    );

    // Re-discovery does not revive a purged URL.
    let fr = vc_frontier::PgFrontier::new(pool.clone(), vc_config::FrontierConfig::default());
    use vc_core::frontier::Frontier as _;
    fr.enqueue(&[NewUrl::new(
        CanonicalUrl::parse("https://b.example/y").unwrap(),
    )])
    .await
    .unwrap();
    assert_eq!(
        states(&pool)
            .await
            .into_iter()
            .find(|x| x.0 == "https://b.example/y")
            .unwrap()
            .1,
        "tombstoned"
    );
}

/// B12 end to end: a purge that crashed after uploading its `.r1` (never committed) does not
/// stall the next purge of the same segment, and the stale generation is deleted.
#[tokio::test]
async fn stale_rewrite_generation_is_stepped_over_and_deleted() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let x = meta("https://s.example/x", at(6, 1), None);
    let y = meta("https://s.example/y", at(6, 2), None);
    let z = meta("https://t.example/z", at(6, 3), None);
    let (xid, zid) = (x.capture_id, z.capture_id);
    let seg = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(6, 1)),
        false,
        &[entry(x, "x"), entry(y, "y"), entry(z, "z")],
    )
    .await;
    // A first purge (of x only) crashed after its upload.
    let stale = vc_store::rewrite_segment_without(&store, &seg.path, &seg.cdxj_path, &[xid].into())
        .await
        .unwrap();
    let stale_path = stale.new_path.unwrap();

    let id = request_purge(
        &pool,
        &PurgeSelector::Host("s.example".into()),
        TombstoneReason::OptOut,
        "t",
    )
    .await
    .unwrap();
    let out = process_purge(&pool, &store, id, at(7, 0)).await.unwrap();
    assert_eq!((out.state.as_str(), out.captures_removed), ("raw_done", 2));
    let path: String = sqlx::query_scalar("SELECT path FROM segments")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(path.contains(".r2."), "{path}");
    assert!(!store.contains(&stale_path), "stale generation deleted");
    assert!(!store.contains(&seg.path));
    let caps: Vec<String> = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(caps, vec![zid.to_string()]);
}
