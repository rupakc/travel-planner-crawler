//! Graceful shutdown of the fetcher (`vcrawl run`, SIGTERM): a claim in progress stops before
//! its next request, is completed (its lease released, the rest of its URLs `due` again), and
//! every spool frame fsynced before the stop is committed by the final flush — nothing is
//! lost, and the stop does not wait out the key's pacing.

mod crawl_path_support;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use crawl_path_support::*;
use testkit::{FakeServer, MemBlobStore};
use tokio::sync::watch;
use vc_core::frontier::NewUrl;
use vc_core::store::BlobStore;
use vc_store::{Spool, SpoolOptions};

#[derive(Default)]
struct Origin {
    pages: Mutex<Vec<String>>,
}

async fn page(State(o): State<Arc<Origin>>, Path(p): Path<String>) -> impl IntoResponse {
    o.pages.lock().unwrap().push(p.clone());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        format!("<html><p>page {p}</p></html>"),
    )
}

/// Capture ids still in the spool (open or sealed, i.e. fsynced but not yet published).
fn in_spool(dir: &std::path::Path) -> HashSet<String> {
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

#[tokio::test]
async fn shutdown_mid_claim_releases_the_lease_and_loses_no_spool_frame() {
    let o = Arc::new(Origin::default());
    let app = Router::new()
        .route("/p/{n}", get(page))
        .route("/robots.txt", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(o.clone());
    let srv = FakeServer::spawn(app).await;
    let port = srv.addr().port();
    let pool = testkit::pg().await;
    let blobs: Arc<dyn BlobStore> = Arc::new(MemBlobStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = cfg();
    // 4 s between requests on the key: five pages would take 20 s.
    cfg.frontier.min_delay_s = 4;
    cfg.frontier.max_urls_per_claim = 5;
    let f = Arc::new(fetcher(
        &cfg,
        &pool,
        &["origin.test"],
        vec![],
        dir.path(),
        blobs,
    ));
    let urls: Vec<_> = (0..5)
        .map(|i| url("origin.test", port, &format!("/p/{i}")))
        .collect();
    f.parts()
        .frontier
        .enqueue(&urls.iter().cloned().map(NewUrl::new).collect::<Vec<_>>())
        .await
        .unwrap();

    let (tx, rx) = watch::channel(false);
    let run = {
        let f = f.clone();
        tokio::spawn(async move { f.run_until(rx).await })
    };
    // Wait for the first page (robots.txt came first, then one key delay).
    let t0 = Instant::now();
    while o.pages.lock().unwrap().is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(30), "no page fetched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Mid-claim: the worker is now waiting out the 4 s delay before page 2.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stop_at = Instant::now();
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(25), run)
        .await
        .expect("run_until returns within the grace period")
        .expect("task")
        .expect("clean shutdown");
    let took = stop_at.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "the stop waited out the key's pacing: {took:?}"
    );

    let fetched: Vec<String> = o.pages.lock().unwrap().clone();
    assert_eq!(fetched.len(), 1, "no request after the stop: {fetched:?}");

    // Every frame that was fsynced is committed; nothing is left behind in the spool.
    assert!(
        in_spool(dir.path()).is_empty(),
        "spool not flushed on shutdown"
    );
    let caps = captures(&pool).await;
    let pages: Vec<_> = caps
        .iter()
        .filter(|c| c.1 == "primary" && c.4.contains("/p/"))
        .collect();
    assert_eq!(pages.len(), 1, "the fetched page is committed: {caps:?}");

    // The claim was completed (lease released), the other URLs are due again.
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM claims WHERE completed_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(open, 0, "a claim was left open");
    let leased: i64 =
        sqlx::query_scalar("SELECT count(*) FROM url_state WHERE state IN ('leased')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(leased, 0, "URLs left leased");
    for u in &urls[1..] {
        let (state, ..) = url_state(&pool, u).await;
        assert_eq!(state, "due", "{u}");
    }
}

#[tokio::test]
async fn stop_before_any_request_makes_none() {
    let o = Arc::new(Origin::default());
    let app = Router::new()
        .route("/p/{n}", get(page))
        .route("/robots.txt", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(o.clone());
    let srv = FakeServer::spawn(app).await;
    let port = srv.addr().port();
    let pool = testkit::pg().await;
    let blobs: Arc<dyn BlobStore> = Arc::new(MemBlobStore::new());
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(&cfg(), &pool, &["origin.test"], vec![], dir.path(), blobs);
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(url("origin.test", port, "/p/0"))])
        .await
        .unwrap();
    f.request_stop();
    // The claim is taken, nothing is requested, and it is completed at once.
    let p = f.pass().await.unwrap();
    assert!(
        matches!(p, vcrawl::roles::fetcher::Pass::Claimed(ref r) if r.completed),
        "{p:?}"
    );
    assert!(o.pages.lock().unwrap().is_empty());
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM claims WHERE completed_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(open, 0);
}
