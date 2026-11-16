//! Wiring of the crawl path: `vcrawl health`, the cross-process daily raw-bytes cap, and the
//! sub-resource gap of the permit client.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use testkit::fake_web::{FakeWeb, Reply, Site};
use testkit::vc_fetch_fixtures::StubPolicy;
use testkit::{FakeClock, MemFrontier};
use vc_core::fetch::{FetchRequest, RequestPurpose};
use vc_core::frontier::{Frontier, NewUrl};
use vc_core::policy::{RobotsEntry, RobotsStatus};
use vc_core::{CanonicalUrl, Clock, SystemClock};
use vc_fetch::StaticResolver;
use vcrawl::Ctx;
use vcrawl::wiring::rawcap::{PgRawBytes, RawBytesBudget};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn cfg_with_metrics(port: u16) -> vc_config::Config {
    let mut c = vc_config::Config::default();
    c.obs.metrics_addr = format!("0.0.0.0:{port}");
    c.obs.progress_window_s = 60;
    c.vcrawl.health_timeout_s = 3;
    c
}

#[tokio::test]
async fn health_ok_when_db_and_role_are_up() {
    let pool = testkit::pg().await;
    let ctx = Ctx::new(cfg_with_metrics(free_port())).with_pool(pool.clone());
    let _obs = vcrawl::wiring::obs::start(&ctx, "fetcher", pool)
        .await
        .expect("obs");
    vcrawl::health::check(&ctx).await.expect("healthy");
}

#[tokio::test]
async fn health_fails_when_the_role_does_not_serve() {
    let pool = testkit::pg().await;
    let ctx = Ctx::new(cfg_with_metrics(free_port())).with_pool(pool);
    let e = vcrawl::health::check(&ctx).await.expect_err("no /healthz");
    assert!(e.0.contains("not serving"), "{e}");
}

#[tokio::test]
async fn health_fails_when_the_role_is_stalled() {
    let pool = testkit::pg().await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let ctx = Ctx::with_clock(cfg_with_metrics(free_port()), clock.clone()).with_pool(pool.clone());
    let _obs = vcrawl::wiring::obs::start(&ctx, "fetcher", pool)
        .await
        .expect("obs");
    ctx.tracker.set_work_due(true);
    clock.advance(Duration::from_secs(11 * 60));
    let e = vcrawl::health::check(&ctx).await.expect_err("stalled");
    assert!(e.0.contains("503"), "{e}");
    ctx.tracker.record_progress(1);
    vcrawl::health::check(&ctx).await.expect("progress resumes");
}

#[tokio::test]
async fn health_fails_when_postgres_is_unreachable() {
    let mut cfg = cfg_with_metrics(free_port());
    cfg.obs.metrics_addr = String::new();
    cfg.database.url = format!("postgres://x:y@127.0.0.1:{}/x", free_port());
    let ctx = Ctx::new(cfg);
    let e = vcrawl::health::check(&ctx).await.expect_err("no db");
    assert!(e.0.contains("postgres"), "{e}");
}

#[tokio::test]
async fn daily_raw_bytes_cap_holds_across_processes() {
    let pool = testkit::pg().await;
    // Two fetcher processes share the counter through Postgres (each its own pool).
    let pool_b = sqlx::PgPool::connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let a = PgRawBytes::new(pool.clone(), 100);
    let b = PgRawBytes::new(pool_b.clone(), 100);
    assert!(!a.exhausted().await.unwrap());
    assert_eq!(a.add(60).await.unwrap(), 60);
    assert!(!b.exhausted().await.unwrap(), "60 < 100");
    assert_eq!(b.add(50).await.unwrap(), 110);
    assert!(a.exhausted().await.unwrap(), "a sees b's bytes");
    assert!(b.exhausted().await.unwrap());
    // Concurrent adds from both processes never lose an increment.
    let mut tasks = Vec::new();
    for i in 0..40u64 {
        let c = if i % 2 == 0 { a.clone() } else { b.clone() };
        tasks.push(tokio::spawn(async move { c.add(1).await.unwrap() }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(a.today().await.unwrap(), 150);
    // No cap configured: never exhausted.
    assert!(!PgRawBytes::new(pool.clone(), 0).exhausted().await.unwrap());
    // The counter has its own table: the frontier's rate_bucket only holds its global row.
    let (day_rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM raw_bytes_daily")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(day_rows, 1);
    let names: Vec<(String,)> = sqlx::query_as("SELECT name FROM rate_bucket ORDER BY name")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(names, vec![("global".to_string(),)]);
    // Earlier days (and legacy rate_bucket rows of earlier days) are pruned; today stays.
    sqlx::query("INSERT INTO raw_bytes_daily (day, bytes) VALUES ('2000-01-01', 5)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO rate_bucket (name, tokens, capacity, refill_per_s)
         VALUES ('raw_bytes:2000-01-02', 5, 100, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(a.prune().await.unwrap(), 2);
    assert_eq!(a.today().await.unwrap(), 150);
    let names: Vec<(String,)> = sqlx::query_as("SELECT name FROM rate_bucket ORDER BY name")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(names, vec![("global".to_string(),)]);
}

/// Migration 1801 carries a legacy `rate_bucket` counter of today over to `raw_bytes_daily`.
#[tokio::test]
async fn raw_bytes_migration_carries_legacy_counter_over() {
    let pool = testkit::pg().await;
    sqlx::query(
        "INSERT INTO rate_bucket (name, tokens, capacity, refill_per_s)
         VALUES ('raw_bytes:' || to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD'), 42, 100, 1),
                ('raw_bytes:garbage', 7, 100, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM raw_bytes_daily")
        .execute(&pool)
        .await
        .unwrap();
    let sql = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../migrations/1801_raw_bytes_daily.sql"),
    )
    .unwrap();
    let insert = sql[sql.find("INSERT INTO raw_bytes_daily").unwrap()..].to_string();
    sqlx::raw_sql(sqlx::AssertSqlSafe(insert))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        PgRawBytes::new(pool.clone(), 100).today().await.unwrap(),
        42
    );
}

/// Render sub-requests are spaced by the 250 ms sub-resource gap, not by the page delay; the
/// page delay still applies to the next page (spec §5.4, §17.1).
#[tokio::test]
async fn subresource_requests_use_the_gap_not_the_page_delay() {
    let site = Site::new("s", "site.test")
        .route(
            "/page",
            Reply::html("<html><script src=/a.js></script></html>"),
        )
        .route("/a.js", Reply::js("var a=1;"))
        .route("/b.js", Reply::js("var b=1;"))
        .route("/c.css", Reply::css("p{}"))
        .route("/next", Reply::html("<p>next</p>"))
        .subresources(&["/a.js", "/b.js", "/c.css"]);
    let web = Arc::new(FakeWeb::with_sites(vec![site]));
    let srv = web.clone().start().await;
    let port = srv.addr().port();
    let url = |p: &str| CanonicalUrl::parse(&format!("http://site.test:{port}{p}")).unwrap();

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let page_delay = Duration::from_secs(2);
    let frontier = Arc::new(MemFrontier::new(clock.clone()).with_delay(page_delay));
    frontier
        .enqueue(&[NewUrl::new(url("/page"))])
        .await
        .unwrap();
    let claim = frontier.claim(&[0], 5).await.unwrap().expect("claim");
    let p = claim.permit;
    let now = Utc::now();
    frontier
        .store_robots(
            &p,
            RobotsEntry {
                origin: url("/page").origin(),
                status: RobotsStatus::NotFound,
                http_status: Some(404),
                body: None,
                last_good: None,
                fetched_at: now,
                expires_at: now + chrono::Duration::days(1),
            },
        )
        .await
        .unwrap();
    let policy = Arc::new(StubPolicy::loopback_ok());
    let resolver =
        Arc::new(StaticResolver::new().with("site.test", IpAddr::V4(Ipv4Addr::LOCALHOST)));
    let cfg = vc_config::Config::default();
    let client =
        vcrawl::wiring::fetch::permit_client(&cfg, frontier.clone(), policy, clock, Some(resolver));
    let gap = Duration::from_millis(cfg.render.subresource_gap_ms);
    assert_eq!(gap, Duration::from_millis(250));

    let get = |path: &str, purpose| FetchRequest::get(url(path), purpose);
    let ex = client
        .fetch(&p, get("/page", RequestPurpose::Page))
        .await
        .unwrap();
    assert_eq!(ex.status.as_u16(), 200);
    let t0 = Instant::now();
    for path in ["/a.js", "/b.js", "/c.css"] {
        let ex = client
            .fetch(&p, get(path, RequestPurpose::SubResource))
            .await
            .unwrap();
        assert_eq!(ex.status.as_u16(), 200, "{path}");
    }
    let subs = t0.elapsed();
    assert!(
        subs >= 3 * gap - Duration::from_millis(30),
        "sub-requests are still spaced by the gap: {subs:?}"
    );
    assert!(
        subs < page_delay,
        "three sub-requests must not wait for the page delay: {subs:?}"
    );
    let t1 = Instant::now();
    client
        .fetch(&p, get("/next", RequestPurpose::Page))
        .await
        .unwrap();
    assert!(
        t1.elapsed() >= page_delay - Duration::from_millis(50),
        "the next page waits for the page delay: {:?}",
        t1.elapsed()
    );
    // Five requests, all with the one UA, through the permit (lease check + token each).
    assert_eq!(frontier.rate_tokens_taken(), 5);
    // KeyOutcome.requests comes from the permit: one count per request, none inflated.
    assert_eq!(p.requests_made(), 5);
    let log = web.log();
    assert_eq!(log.entries().len(), 5);
    assert!(log.entries().iter().all(|e| e.ua == vc_core::USER_AGENT));
}
