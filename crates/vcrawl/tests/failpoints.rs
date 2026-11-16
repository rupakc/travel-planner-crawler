//! Review Focus 3: a process killed at any step of §5.5 — spool append/fsync, between the fsync
//! and `fetched_pending`, before the claim completes, at seal, before/after the segment upload,
//! before/after `vc_commit_segment` — loses no capture, duplicates none, and never produces a
//! 304 without a stored body, after a restart (spool replay) and a recrawl.
//!
//! The crash is a `fail` failpoint returning an error: the claim is abandoned without
//! completion (the permit is dropped) or the roller stops mid-publish, and the fetcher is
//! dropped; a new fetcher on the same spool directory plays the restarted process. Failpoints
//! are process-global, so the test runs the steps one after another.

mod crawl_path_support;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use crawl_path_support::*;
use testkit::{FakeServer, MemBlobStore, assert_raw_first};
use vc_core::frontier::NewUrl;
use vc_core::store::BlobStore;
use vc_store::{Spool, SpoolOptions};
use vcrawl::roles::fetcher::Pass;

const ETAG: &str = "\"v1\"";

/// An origin that honours `If-None-Match` (fake-web replies are unconditional).
#[derive(Default)]
struct Origin {
    /// One entry per page request: whether it was conditional (and answered 304).
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

async fn origin() -> (Arc<Origin>, FakeServer) {
    let o = Arc::new(Origin::default());
    let app = Router::new()
        .route("/page", get(page))
        .route("/robots.txt", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(o.clone());
    (o, FakeServer::spawn(app).await)
}

/// The capture ids a crashed process had acknowledged (fsynced frames on disk).
fn acknowledged(dir: &std::path::Path) -> HashSet<String> {
    let clock: Arc<dyn vc_core::Clock> = Arc::new(vc_core::SystemClock);
    let spool = Spool::open(dir, SpoolOptions::default(), clock).expect("reopen spool");
    let mut ids: HashSet<String> = spool
        .open_entries()
        .expect("open entries")
        .into_iter()
        .map(|e| e.meta.capture_id.to_string())
        .collect();
    for s in spool.sealed().expect("sealed") {
        for e in s.read_entries().expect("sealed entries") {
            ids.insert(e.meta.capture_id.to_string());
        }
    }
    ids
}

const FETCH_STEPS: &[&str] = &[
    "spool::before_fsync",
    "fetcher::after_spool",
    "fetcher::before_complete",
];
const ROLLER_STEPS: &[&str] = &[
    "spool::before_seal",
    "segment::before_upload",
    "segment::after_upload",
    "segment::before_commit",
    "segment::after_commit",
];

#[tokio::test]
async fn no_lost_no_duplicate_no_orphan_304() {
    let scenario = fail::FailScenario::setup();
    for fp in FETCH_STEPS.iter().chain(ROLLER_STEPS) {
        crash_and_restart(fp).await;
    }
    scenario.teardown();
}

async fn crash_and_restart(fp: &str) {
    let (origin, srv) = origin().await;
    let port = srv.addr().port();
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    let blobs: Arc<dyn BlobStore> = store.clone();
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg();
    let hosts = ["origin.test"];
    let page_url = url("origin.test", port, "/page");

    // ---- First life: crawl, crash at `fp`.
    {
        let f = fetcher(&cfg, &pool, &hosts, vec![], dir.path(), blobs.clone());
        f.parts()
            .frontier
            .enqueue(&[NewUrl::new(page_url.clone())])
            .await
            .unwrap();
        if FETCH_STEPS.contains(&fp) {
            fail::cfg(fp, "return").unwrap();
            assert!(f.pass().await.is_err(), "{fp}: the claim must crash");
        } else {
            assert!(matches!(f.pass().await.unwrap(), Pass::Claimed(_)), "{fp}");
            fail::cfg(fp, "return").unwrap();
            assert!(f.flush().await.is_err(), "{fp}: the roller must crash");
        }
        fail::remove(fp);
    } // the process dies: fetcher, spool handles and the permit are dropped
    let acked = acknowledged(dir.path());
    if fp != "spool::before_fsync" {
        assert!(
            !acked.is_empty(),
            "{fp}: something was fsynced before the crash"
        );
    }

    // ---- Restart: replay the spool, then crawl again (twice, so a 304 path is exercised).
    let f = fetcher(&cfg, &pool, &hosts, vec![], dir.path(), blobs.clone());
    f.tick_roller().await.unwrap();
    f.flush().await.unwrap();
    for round in 0..2 {
        fast_forward(&pool).await;
        let p = f.pass().await.unwrap();
        assert!(matches!(p, Pass::Claimed(_)), "{fp}: round {round}: {p:?}");
        f.flush().await.unwrap();
    }

    // ---- No lost capture: every acknowledged record is committed.
    let caps = captures(&pool).await;
    let committed: HashSet<String> = caps.iter().map(|c| c.0.clone()).collect();
    let lost: Vec<_> = acked.difference(&committed).collect();
    assert!(lost.is_empty(), "{fp}: lost captures {lost:?}");

    // ---- No duplicate: each capture once; at most one primary capture per epoch and URL.
    assert_eq!(committed.len(), caps.len(), "{fp}: duplicate capture rows");
    let dup_epochs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT epoch, url_key FROM captures WHERE role = 'primary'
                               GROUP BY 1, 2 HAVING count(*) > 1) d",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        dup_epochs, 0,
        "{fp}: duplicate primary captures in one epoch"
    );
    let segs: Vec<(String,)> = sqlx::query_as("SELECT path FROM segments")
        .fetch_all(&pool)
        .await
        .unwrap();
    let mut seen = HashSet::new();
    for (path,) in &segs {
        for r in vc_store::read_segment(blobs.as_ref(), path).await.unwrap() {
            if let Some(id) = r.capture_id()
                && matches!(
                    r.warc_type(),
                    Some("response" | "revisit" | "resource" | "metadata")
                )
            {
                assert!(
                    seen.insert(id.to_string()),
                    "{fp}: capture {id} stored twice"
                );
            }
        }
    }

    // ---- No orphan 304: every 304 revisits a committed 2xx capture with a body; the origin's
    // 304s are exactly our revisits (we never sent validators we had no body for).
    let primary: Vec<_> = caps.iter().filter(|c| c.1 == "primary").collect();
    let revisits: Vec<_> = primary.iter().filter(|c| c.2 == Some(304)).collect();
    for r in &revisits {
        // Follow the revisit chain to the capture that stored the body.
        let mut of = r.3.clone().expect("a 304 names the capture it repeats");
        let target = loop {
            let t = caps
                .iter()
                .find(|c| c.0 == of)
                .unwrap_or_else(|| panic!("{fp}: 304 revisits an uncommitted capture {of}"));
            match (t.2, &t.3) {
                (Some(304), Some(next)) => of = next.clone(),
                _ => break t,
            }
        };
        assert_eq!(
            target.2,
            Some(200),
            "{fp}: the chain must end in a 2xx body"
        );
        let hash: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT payload_hash FROM captures WHERE capture_id = $1")
                .bind(&target.0)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(hash.is_some(), "{fp}: revisit of a capture without a body");
    }
    let conditional = origin
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|c| **c)
        .count();
    assert_eq!(
        conditional,
        revisits.len(),
        "{fp}: every 304 is recorded as a revisit"
    );
    assert!(
        conditional >= 1,
        "{fp}: the recrawl used the committed validators"
    );

    // ---- The URL is back in the crawl with validators of a committed capture.
    let (state, etag, last, hash) = url_state(&pool, &page_url).await;
    assert_eq!(state, "due", "{fp}: never stuck in fetched_pending/leased");
    assert_eq!(etag.as_deref(), Some(ETAG));
    assert!(hash.is_some());
    assert!(
        committed.contains(last.as_deref().unwrap_or_default()),
        "{fp}"
    );
    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .all(|e| !e.file_name().to_string_lossy().ends_with(".sealed")),
        "{fp}: sealed spool files are deleted after the commit"
    );
    assert_raw_first(&pool, store.as_ref()).await;
}
