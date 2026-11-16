//! Politeness defects from the final review (fetcher side): adapter and robots.txt 429/503
//! back the key off with `Retry-After`, adapter 5xx count toward the 5xx park, a 200 ends a
//! 5xx run, robots `last_good` is kept, and a detected challenge parks as a challenge.

mod crawl_path_support;

use std::sync::Arc;
use std::time::Duration;

use crawl_path_support::*;
use testkit::MemBlobStore;
use testkit::fake_web::{FakeWeb, FakeWebServer, Reply, Site};
use vc_core::frontier::{KeySignal, NewUrl, ParkReason, UrlResult};
use vcrawl::roles::fetcher::{ClaimReport, Pass};
use vcrawl::roles::scheduler::Scheduler;

fn claimed(p: Pass) -> ClaimReport {
    match p {
        Pass::Claimed(r) => r,
        other => panic!("expected a claim, got {other:?}"),
    }
}

async fn start(sites: Vec<Site>) -> (Arc<FakeWeb>, FakeWebServer, u16) {
    let web = Arc::new(FakeWeb::with_sites(sites));
    let srv = web.clone().start().await;
    let port = srv.addr().port();
    (web, srv, port)
}

fn paths(web: &FakeWeb) -> Vec<String> {
    web.log()
        .entries()
        .into_iter()
        .map(|e| {
            let u = url::Url::parse(&e.url).expect("logged url");
            format!("{}{}", u.host_str().unwrap_or(""), u.path())
        })
        .collect()
}

async fn secs_until_allowed(pool: &sqlx::PgPool, key: &str) -> f64 {
    sqlx::query_scalar(
        "SELECT extract(epoch FROM next_allowed_at - now())::float8 FROM politeness_keys WHERE key = $1",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A MET Norway adapter row against `reply`, claimed once.
async fn adapter_claim(reply: Reply) -> (sqlx::PgPool, ClaimReport, String, tempfile::TempDir) {
    let (_web, srv, port) = start(vec![
        Site::new("met", "api.met.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route("/weatherapi/locationforecast/2.0/compact", reply),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let row = url(
        "api.met.test",
        port,
        "/weatherapi/locationforecast/2.0/compact?lat=59.91&lon=10.75",
    );
    let s = spec(&format!(
        "id: met-test\nmethod: api\nadapter: met_norway\ncadence: hourly\nseeds: [\"{row}\"]\n"
    ));
    let cfg = cfg();
    let f = fetcher(
        &cfg,
        &pool,
        &["api.met.test"],
        vec![s.clone()],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let sched = Scheduler::new(
        Arc::new(cfg.clone()),
        pool.clone(),
        f.parts().frontier.clone(),
    );
    sched.pass(std::slice::from_ref(&s)).await.unwrap();
    let r = claimed(f.pass().await.unwrap());
    drop(srv);
    (pool, r, row.politeness_key().as_str().to_string(), dir)
}

/// Item 1: an adapter's 429 + `Retry-After` is a key-level rate limit, not a per-URL
/// transient failure.
#[tokio::test]
async fn adapter_429_rate_limits_the_key_with_retry_after() {
    let (pool, r, key, _dir) =
        adapter_claim(Reply::status(429).header("retry-after", "1800")).await;
    assert_eq!(
        r.signal,
        Some(KeySignal::RateLimited {
            retry_after: Some(Duration::from_secs(1800))
        }),
        "{r:?}"
    );
    assert!(matches!(r.urls[0].result, UrlResult::Retry { .. }), "{r:?}");
    let s = secs_until_allowed(&pool, &key).await;
    assert!(s > 1700.0, "Retry-After holds the key: {s}");
}

/// Item 1: an adapter's 5xx counts toward the consecutive-5xx park.
#[tokio::test]
async fn adapter_5xx_counts_toward_server_errors() {
    let (pool, r, key, _dir) = adapter_claim(Reply::status(500)).await;
    assert_eq!(
        r.signal,
        Some(KeySignal::ServerErrors { consecutive: 1 }),
        "{r:?}"
    );
    let n: i16 = sqlx::query_scalar("SELECT consecutive_5xx FROM politeness_keys WHERE key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

/// Item 9: robots.txt answered 429 → the key backs off with `Retry-After`; no page is asked.
#[tokio::test]
async fn robots_429_rate_limits_the_key() {
    let (web, _srv, port) = start(vec![
        Site::new("busy", "busy.test")
            .route(
                "/robots.txt",
                Reply::status(429).header("retry-after", "1200"),
            )
            .route("/p", Reply::html("<p>p</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg(),
        &pool,
        &["busy.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let p = url("busy.test", port, "/p");
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(p.clone())])
        .await
        .unwrap();
    let r = claimed(f.pass().await.unwrap());
    assert_eq!(
        r.signal,
        Some(KeySignal::RateLimited {
            retry_after: Some(Duration::from_secs(1200))
        })
    );
    assert_eq!(paths(&web), ["busy.test/robots.txt"]);
    assert!(secs_until_allowed(&pool, p.politeness_key().as_str()).await > 1100.0);
}

/// Item 10: a good robots.txt is kept as `last_good` (the 30-day rule needs it).
#[tokio::test]
async fn robots_ok_is_stored_as_last_good() {
    let (_web, _srv, port) = start(vec![
        Site::new("blog", "blog.test")
            .route("/robots.txt", Reply::text("User-agent: *\nDisallow: /x\n"))
            .route("/p", Reply::html("<p>p</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg(),
        &pool,
        &["blog.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let p = url("blog.test", port, "/p");
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(p.clone())])
        .await
        .unwrap();
    claimed(f.pass().await.unwrap());
    let (good, at): (Option<Vec<u8>>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT robots_last_good, robots_last_good_at FROM origins WHERE origin = $1",
    )
    .bind(&p.origin().0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(good.as_deref(), Some(&b"User-agent: *\nDisallow: /x\n"[..]));
    assert!(at.is_some());
}

/// Item 11: a 200 after a 5xx ends the run: the claim reports `Normal`, so parks do not
/// accumulate across claims.
#[tokio::test]
async fn success_after_5xx_resets_the_signal() {
    let (_web, _srv, port) = start(vec![
        Site::new("flaky", "flaky.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route("/a", Reply::status(500))
            .route("/b", Reply::html("<p>b</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg(),
        &pool,
        &["flaky.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let mut a = NewUrl::new(url("flaky.test", port, "/a"));
    a.priority = 0.9;
    let b = NewUrl::new(url("flaky.test", port, "/b"));
    f.parts().frontier.enqueue(&[a, b]).await.unwrap();
    let r = claimed(f.pass().await.unwrap());
    assert_eq!(r.urls.len(), 2);
    assert_eq!(r.signal, Some(KeySignal::Normal), "{r:?}");
}

/// Item 15: a detected challenge parks as `Challenge`, not as a plain 401/403.
#[tokio::test]
async fn detected_challenge_parks_as_challenge() {
    let (_web, _srv, port) = start(vec![
        Site::new("hostile", "hostile.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route("/a", Reply::challenge()),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg(),
        &pool,
        &["hostile.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(url("hostile.test", port, "/a"))])
        .await
        .unwrap();
    let r = claimed(f.pass().await.unwrap());
    assert_eq!(r.signal, Some(KeySignal::Park(ParkReason::Challenge)));
}

/// B1: a fetcher that died (a deploy changed its instance name) left acknowledged frames in
/// its spool; the next fetcher adopts that spool, publishes it, and the URL is released. A
/// spool still locked by a live fetcher is never touched.
#[tokio::test]
async fn orphaned_spool_is_adopted_and_committed() {
    let (_web, _srv, port) = start(vec![
        Site::new("blog", "blog.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route("/p", Reply::html("<p>p</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let root = tempfile::tempdir().unwrap();
    let store: Arc<dyn vc_core::store::BlobStore> = Arc::new(MemBlobStore::new());
    let p = url("blog.test", port, "/p");
    {
        let old = fetcher(
            &cfg(),
            &pool,
            &["blog.test"],
            vec![],
            &root.path().join("fetcher-old0"),
            store.clone(),
        );
        old.parts()
            .frontier
            .enqueue(&[NewUrl::new(p.clone())])
            .await
            .unwrap();
        let r = claimed(old.pass().await.unwrap());
        assert_eq!(r.urls[0].result, UrlResult::Fetched);
        // The process dies here: nothing sealed or uploaded.
    }
    let (state, ..) = url_state(&pool, &p).await;
    assert_eq!(state, "fetched_pending");
    // A live fetcher's spool (locked) must stay put.
    let live = fetcher(
        &cfg(),
        &pool,
        &["blog.test"],
        vec![],
        &root.path().join("fetcher-live"),
        store.clone(),
    );
    let live_dir = live.parts().spool.dir().to_path_buf();
    std::fs::write(live_dir.join("marker"), b"x").unwrap();

    let mut tight = cfg();
    tight.store.spool_backpressure_bytes = 1;
    let new = fetcher(
        &tight,
        &pool,
        &["blog.test"],
        vec![],
        &root.path().join("fetcher-new0"),
        store.clone(),
    );
    assert_eq!(
        new.pass().await.unwrap(),
        Pass::Paused,
        "an orphaned backlog counts toward backpressure until published"
    );
    let n = new.tick_roller().await.unwrap();
    assert_eq!(n, 2, "robots + page from the adopted spool");
    let (state, ..) = url_state(&pool, &p).await;
    assert_eq!(state, "due");
    assert!(
        !root.path().join("fetcher-old0").exists(),
        "a published orphan spool is removed"
    );
    assert!(
        live_dir.join("marker").exists(),
        "a live spool is not adopted"
    );
    assert_eq!(
        new.pass().await.unwrap(),
        Pass::Idle,
        "backlog gone: no pause"
    );
    drop(live);
}
