//! The crawl path end to end against testkit fake-web, fake-gcs and Postgres: scheduler →
//! frontier → fetcher (robots first, permit-gated fetch, spool fsync, `fetched_pending`) →
//! segment roller → `vc_commit_segment`.

mod crawl_path_support;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use crawl_path_support::*;
use testkit::fake_web::{FakeWeb, FakeWebServer, Reply, Site};
use testkit::{MemBlobStore, assert_raw_first};
use vc_core::frontier::{KeySignal, NewUrl, ParkReason, UrlResult};
use vc_core::media::FetchMode;
use vc_core::store::BlobStore;
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

/// Scheduler seeds a source; the fetcher fetches robots.txt first, then the page; nothing is
/// visible before the segment commit (raw first); after the commit the URL is `due` again with
/// the server's validators, and `capture_ready` is queued. Blob store: fake-gcs.
#[tokio::test]
async fn crawl_path_seeds_fetches_robots_first_and_commits_raw_first() {
    let (web, _srv, port) = start(vec![
        Site::new("blog", "blog.test")
            .route(
                "/robots.txt",
                Reply::text("User-agent: *\nDisallow: /private\n"),
            )
            .route(
                "/guide",
                Reply::html("<html><h1>Lisbon guide</h1></html>").header("etag", "\"v1\""),
            ),
    ])
    .await;
    let pool = testkit::pg().await;
    let gcs = testkit::fake_gcs().await;
    let store: Arc<dyn BlobStore> = Arc::new(gcs.clone());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg();
    let page = url("blog.test", port, "/guide");
    let s = spec(&format!(
        "id: blog-test\nmethod: html\ncadence: daily\nmin_delay_s: 7\ndaily_budget: 500\nseeds: [\"{page}\"]\n"
    ));
    let f = fetcher(
        &cfg,
        &pool,
        &["blog.test"],
        vec![s.clone()],
        dir.path(),
        store.clone(),
    );

    // Scheduler: source row, seed URL, source limits on the key.
    let sched = Scheduler::new(
        Arc::new(cfg.clone()),
        pool.clone(),
        f.parts().frontier.clone(),
    );
    let rep = sched.pass(std::slice::from_ref(&s)).await.unwrap();
    assert_eq!((rep.sources.inserted, rep.enqueued), (1, 1));
    assert_eq!(rep.keys_limited, 1);
    let (min_delay, budget): (i32, i32) =
        sqlx::query_as("SELECT min_delay_ms, budget_day FROM politeness_keys WHERE key = $1")
            .bind(page.politeness_key().as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (min_delay, budget),
        (7000, 500),
        "sources only raise delay / lower budget"
    );
    assert_eq!(sched.pass(&[s]).await.unwrap().enqueued, 0, "idempotent");

    let r = claimed(f.pass().await.unwrap());
    assert!(r.completed);
    assert_eq!(r.urls.len(), 1);
    assert_eq!(r.urls[0].result, UrlResult::Fetched);
    assert_eq!(r.spooled.len(), 2, "robots.txt + page");
    assert_eq!(paths(&web), ["blog.test/robots.txt", "blog.test/guide"]);
    assert!(
        web.log()
            .entries()
            .iter()
            .all(|e| e.ua == vc_core::USER_AGENT)
    );

    // Raw first: spooled and fsynced, but nothing derived is visible yet.
    assert!(captures(&pool).await.is_empty());
    let (state, etag, ..) = url_state(&pool, &page).await;
    assert_eq!((state.as_str(), etag), ("fetched_pending", None));

    assert_eq!(f.flush().await.unwrap(), 2);
    let caps = captures(&pool).await;
    let roles: Vec<&str> = caps.iter().map(|c| c.1.as_str()).collect();
    assert!(
        roles.contains(&"robots") && roles.contains(&"primary"),
        "{roles:?}"
    );
    let (state, etag, last, hash) = url_state(&pool, &page).await;
    assert_eq!(state, "due");
    assert_eq!(etag.as_deref(), Some("\"v1\""));
    assert!(last.is_some() && hash.is_some());
    let ready: i64 =
        sqlx::query_scalar("SELECT count(*) FROM work_items WHERE kind = 'capture_ready'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        ready, 1,
        "only the primary capture is queued for processing"
    );
    let robots_state: String =
        sqlx::query_scalar("SELECT robots_state FROM origins WHERE origin = $1")
            .bind(&page.origin().0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(robots_state, "ok");
    assert!(!gcs.list("segments/").await.unwrap().is_empty());
    assert_raw_first(&pool, store.as_ref()).await;
}

/// A committed capture as [`captures`] lists it.
type Cap = (String, String, Option<i16>, Option<String>, String);

/// `(last_captured_at, body_captured_at)` of a URL.
async fn fetch_times(
    pool: &sqlx::PgPool,
    u: &vc_core::CanonicalUrl,
) -> (Option<chrono::DateTime<Utc>>, Option<chrono::DateTime<Utc>>) {
    sqlx::query_as("SELECT last_captured_at, body_captured_at FROM url_state WHERE url_key = $1")
        .bind(u.key().as_bytes().as_slice())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A 304 is spooled as a `revisit` of the last **committed** capture: url_state keeps the
/// stored body's hash; the revisit points at a capture that exists.
/// B5 (migration 1103): `url_state.last_capture_id` only moves for captures that hold a body.
/// After C0 200, R1 304, R2 304 (and an identical-payload 200, a dedupe revisit) the URL still
/// points at C0, every revisit refers to C0 directly (one hop), the conditional headers are
/// still sent, and `last_captured_at` keeps tracking the last fetch. A changed body moves it.
#[tokio::test]
async fn not_modified_is_a_revisit_of_the_committed_capture() {
    let (web, _srv, port) = start(vec![Site::new("blog", "blog.test").route_cycles(
        "/p",
        vec![
            Some(Reply::html("<p>v1</p>").header("etag", "\"v1\"")),
            Some(Reply::status(304).header("etag", "\"v1\"")),
            Some(Reply::status(304).header("etag", "\"v1\"")),
            Some(Reply::html("<p>v1</p>").header("etag", "\"v1b\"")),
            Some(Reply::html("<p>v2</p>").header("etag", "\"v2\"")),
        ],
    )])
    .await;
    let pool = testkit::pg().await;
    let store: Arc<dyn BlobStore> = Arc::new(MemBlobStore::new());
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg(),
        &pool,
        &["blog.test"],
        vec![],
        dir.path(),
        store.clone(),
    );
    let page = url("blog.test", port, "/p");
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(page.clone())])
        .await
        .unwrap();

    claimed(f.pass().await.unwrap());
    f.flush().await.unwrap();
    let (_, _, first, hash1) = url_state(&pool, &page).await;
    let first = first.expect("C0 is the body capture");

    // The primary captures of /p as (capture_id, status, revisit_of_id), oldest first.
    let page_caps = |caps: Vec<Cap>| {
        caps.into_iter()
            .filter(|c| c.1 == "primary" && c.4.ends_with("/p"))
            .map(|c| (c.0, c.2, c.3))
            .collect::<Vec<_>>()
    };
    for cycle in 1..=3 {
        fast_forward(&pool).await;
        web.set_cycle(cycle);
        let r = claimed(f.pass().await.unwrap());
        assert_eq!(r.urls[0].result, UrlResult::Fetched, "cycle {cycle}");
        f.flush().await.unwrap();
        let caps = page_caps(captures(&pool).await);
        assert_eq!(caps.len(), cycle + 1);
        let latest = caps.last().unwrap();
        let want_status = if cycle == 3 { 200 } else { 304 };
        assert_eq!(latest.1, Some(want_status), "cycle {cycle}");
        assert_eq!(
            latest.2.as_deref(),
            Some(first.as_str()),
            "cycle {cycle}: the revisit refers to C0's body directly"
        );
        let (state, etag, last, hash) = url_state(&pool, &page).await;
        assert_eq!(state, "due");
        assert_eq!(hash, hash1, "the stored body's hash is kept");
        assert_eq!(
            last.as_deref(),
            Some(first.as_str()),
            "cycle {cycle}: url_state still points at C0's body"
        );
        let want_etag = if cycle == 3 { "\"v1b\"" } else { "\"v1\"" };
        assert_eq!(etag.as_deref(), Some(want_etag), "validators still refresh");
        let (fetched, body) = fetch_times(&pool, &page).await;
        assert!(
            fetched > body,
            "last_captured_at is the last fetch, not the body"
        );
    }

    // R2 (the second 304) was a conditional request, built from the validators that survive.
    let (path, offset, length): (String, i64, i64) = sqlx::query_as(
        "SELECT s.path, c.warc_offset, c.warc_length FROM captures c
         JOIN segments s ON s.segment_id = c.segment_id
         WHERE c.capture_id = $1",
    )
    .bind(&page_caps(captures(&pool).await)[2].0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let recs = vc_store::read_capture(store.as_ref(), &path, offset as u64, length as u64)
        .await
        .unwrap();
    let req = recs
        .iter()
        .find(|r| r.warc_type() == Some("request"))
        .expect("R2's request record");
    let req = String::from_utf8_lossy(&req.block).to_ascii_lowercase();
    assert!(
        req.contains("if-none-match: \"v1\""),
        "R2 sent If-None-Match: {req}"
    );

    // A changed body is a new body capture: the pointer moves.
    fast_forward(&pool).await;
    web.set_cycle(4);
    claimed(f.pass().await.unwrap());
    f.flush().await.unwrap();
    let caps = page_caps(captures(&pool).await);
    let c4 = caps.last().unwrap();
    assert_eq!((c4.1, c4.2.as_deref()), (Some(200), None));
    let (_, _, last, hash) = url_state(&pool, &page).await;
    assert_eq!(last.as_deref(), Some(c4.0.as_str()));
    assert_ne!(hash, hash1);
    let (fetched, body) = fetch_times(&pool, &page).await;
    assert_eq!(fetched, body);
    assert_raw_first(&pool, store.as_ref()).await;
}

/// Review Focus 2: a redirect to another politeness key is enqueued, never followed inline.
#[tokio::test]
async fn cross_key_redirect_is_enqueued_not_followed() {
    let (web, _srv, port) = start(vec![
        Site::new("old", "old.test").route("/moved", Reply::redirect(301, "PLACEHOLDER")),
        Site::new("new", "new.test").route("/here", Reply::html("<p>here</p>")),
    ])
    .await;
    let target = url("new.test", port, "/here");
    web.add_site(
        Site::new("old", "old.test").route("/moved", Reply::redirect(301, target.as_str())),
    );
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg(),
        &pool,
        &["old.test", "new.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let from = url("old.test", port, "/moved");
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(from.clone())])
        .await
        .unwrap();
    let r = claimed(f.pass().await.unwrap());
    assert_eq!(r.enqueued, vec![target.clone()]);
    assert!(
        !paths(&web).iter().any(|p| p.starts_with("new.test")),
        "never followed inline: {:?}",
        paths(&web)
    );
    let (state, ..) = url_state(&pool, &target).await;
    assert_eq!(state, "due", "the target waits for its own key's claim");
    let (key, from_key): (String, Option<Vec<u8>>) =
        sqlx::query_as("SELECT key, discovered_from FROM urls WHERE url_key = $1")
            .bind(target.key().as_bytes().as_slice())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(key, target.politeness_key().0);
    assert_eq!(from_key.as_deref(), Some(from.key().as_bytes().as_slice()));
}

/// Detect → park: a challenge page parks the key; nothing is retried.
#[tokio::test]
async fn challenge_parks_the_key_and_stops() {
    let (web, _srv, port) = start(vec![
        Site::new("hostile", "hostile.test")
            .route("/a", Reply::challenge())
            .route("/b", Reply::html("<p>b</p>")),
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
    let mut a = NewUrl::new(url("hostile.test", port, "/a"));
    a.priority = 0.9;
    let b = NewUrl::new(url("hostile.test", port, "/b"));
    f.parts().frontier.enqueue(&[a, b]).await.unwrap();
    let r = claimed(f.pass().await.unwrap());
    assert_eq!(r.signal, Some(KeySignal::Park(ParkReason::Challenge)));
    assert!(r.urls.iter().all(|o| o.result == UrlResult::NotAttempted));
    assert_eq!(
        paths(&web),
        ["hostile.test/robots.txt", "hostile.test/a"],
        "stop after the challenge"
    );
    let parked: bool = sqlx::query_scalar(
        "SELECT parked_until > now() + interval '23 hours' FROM politeness_keys WHERE key = $1",
    )
    .bind(url("hostile.test", port, "/a").politeness_key().as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(parked, "24 h park");
    fast_forward(&pool).await;
    assert_eq!(
        f.pass().await.unwrap(),
        Pass::Idle,
        "a parked key is not claimed"
    );
}

/// robots.txt answered 401 on one host → that origin is do-not-crawl: a WARC metadata record
/// notes it, none of its pages is requested and its URL ends `Disallowed`. Another host of the
/// same key (its own robots.txt is 404) is still crawled and the key is not parked: robots.txt
/// speaks for its own origin only (RFC 9309 §2.3).
#[tokio::test]
async fn robots_401_blocks_only_its_origin() {
    let (web, _srv, port) = start(vec![
        Site::new("locked", "www.locked.test")
            .route("/robots.txt", Reply::status(401))
            .route("/x", Reply::html("<p>x</p>")),
        Site::new("api", "api.locked.test").route("/y", Reply::html("<p>y</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let store: Arc<dyn BlobStore> = Arc::new(MemBlobStore::new());
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg(),
        &pool,
        &["www.locked.test", "api.locked.test"],
        vec![],
        dir.path(),
        store.clone(),
    );
    let x = url("www.locked.test", port, "/x");
    let y = url("api.locked.test", port, "/y");
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(x.clone()), NewUrl::new(y.clone())])
        .await
        .unwrap();
    let r = claimed(f.pass().await.unwrap());
    assert_eq!(r.signal, Some(KeySignal::Normal), "the key is not parked: {r:?}");
    let result = |u: &vc_core::CanonicalUrl| {
        r.urls
            .iter()
            .find(|o| o.url_key == u.key())
            .map(|o| o.result.clone())
    };
    assert_eq!(result(&x), Some(UrlResult::Disallowed), "{r:?}");
    assert_eq!(result(&y), Some(UrlResult::Fetched), "{r:?}");
    let mut got = paths(&web);
    got.sort();
    assert_eq!(
        got,
        [
            "api.locked.test/robots.txt",
            "api.locked.test/y",
            "www.locked.test/robots.txt"
        ],
        "nothing but robots.txt is asked of the refusing host"
    );
    f.flush().await.unwrap();
    let caps = captures(&pool).await;
    assert!(
        caps.iter()
            .any(|c| c.1.as_str() == "robots" && c.2 == Some(401)),
        "{caps:?}"
    );
    let seg: (String, i64) = sqlx::query_as(
        "SELECT s.path, c.warc_offset FROM captures c JOIN segments s USING (segment_id)
         WHERE c.status = 401",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let recs = vc_store::read_segment(store.as_ref(), &seg.0)
        .await
        .unwrap();
    assert!(
        recs.iter().any(|r| r.warc_type() == Some("metadata")),
        "a WARC metadata record"
    );
    let states: Vec<(String, String)> =
        sqlx::query_as("SELECT origin, robots_state FROM origins ORDER BY origin")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(states[0].1, "not_found", "{states:?}");
    assert_eq!(states[1].1, "unauthorized", "{states:?}");
    let parked: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT parked_until FROM politeness_keys WHERE key = 'locked.test'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        parked.is_none_or(|t| t <= Utc::now()),
        "not parked: {parked:?}"
    );
}

/// Review Focus 1: robots.txt changes mid-crawl to disallow a path → no further request to it,
/// and it is tombstoned.
#[tokio::test]
async fn robots_change_tombstones_newly_disallowed_paths() {
    let (web, _srv, port) = start(vec![
        Site::new("site", "site.test")
            .route_cycles(
                "/robots.txt",
                vec![
                    Some(Reply::text("User-agent: *\nDisallow: /nothing\n")),
                    Some(Reply::text("User-agent: *\nDisallow: /private\n")),
                ],
            )
            .route("/private/a", Reply::html("<p>a</p>"))
            .route("/public/b", Reply::html("<p>b</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg(),
        &pool,
        &["site.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let a = url("site.test", port, "/private/a");
    let b = url("site.test", port, "/public/b");
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(a.clone()), NewUrl::new(b.clone())])
        .await
        .unwrap();
    claimed(f.pass().await.unwrap());
    f.flush().await.unwrap();
    assert_eq!(web.log().entries().len(), 3);

    // The cached robots.txt expires; the site now disallows /private.
    sqlx::query("UPDATE origins SET robots_expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    fast_forward(&pool).await;
    web.set_cycle(1);
    let before = web.log().entries().len();
    let r = claimed(f.pass().await.unwrap());
    assert!(r.tombstoned >= 1, "{r:?}");
    let after: Vec<String> = paths(&web)[before..].to_vec();
    assert_eq!(
        after.first().map(String::as_str),
        Some("site.test/robots.txt")
    );
    assert!(
        !after.iter().any(|p| p.ends_with("/private/a")),
        "{after:?}"
    );
    let t: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tombstones WHERE selector_type = 'url' AND reason = 'robots'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(t, 1);
    let (state, ..) = url_state(&pool, &a).await;
    assert!(state == "tombstoned" || state == "disabled", "{state}");
}

/// The overall daily raw-bytes cap pauses every fetcher once reached.
#[tokio::test]
async fn daily_raw_bytes_cap_pauses_fetching() {
    let (web, _srv, port) = start(vec![
        Site::new("big", "big.test").route("/1", Reply::html("x".repeat(500))),
        Site::new("other", "other.test").route("/2", Reply::html("y".repeat(500))),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg();
    c.fetch.daily_raw_bytes_cap = 100;
    let f = fetcher(
        &c,
        &pool,
        &["big.test", "other.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    f.parts()
        .frontier
        .enqueue(&[
            NewUrl::new(url("big.test", port, "/1")),
            NewUrl::new(url("other.test", port, "/2")),
        ])
        .await
        .unwrap();
    claimed(f.pass().await.unwrap());
    let n = web.log().entries().len();
    assert_eq!(f.pass().await.unwrap(), Pass::Paused);
    assert_eq!(web.log().entries().len(), n, "no request after the cap");
}

/// A 429 with `Retry-After` delays the **key**, so a later `Frontier::reschedule` of the URL
/// (e.g. from a change feed) cannot bring the request forward.
#[tokio::test]
async fn rate_limit_is_not_bypassed_by_reschedule() {
    let (web, _srv, port) =
        start(vec![Site::new("busy", "busy.test").route(
            "/p",
            Reply::status(429).header("retry-after", "3600"),
        )])
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
            retry_after: Some(Duration::from_secs(3600))
        })
    );
    assert!(
        f.parts()
            .frontier
            .reschedule(&p.key(), Utc::now())
            .await
            .unwrap()
    );
    let wait_ok: bool = sqlx::query_scalar(
        "SELECT next_allowed_at > now() + interval '55 minutes' FROM politeness_keys WHERE key = $1",
    )
    .bind(p.politeness_key().as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(wait_ok, "Retry-After holds the key");
    let n = web.log().entries().len();
    assert_eq!(f.pass().await.unwrap(), Pass::Idle);
    assert_eq!(web.log().entries().len(), n);
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../testkit/fixtures/vc-sources/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture")
}

/// MET Norway: the response's `Expires` is a lower bound for the row's next fetch (their
/// terms), even with an hourly cadence; the scheduler never moves the row earlier.
#[tokio::test]
async fn met_norway_expires_bounds_the_next_fetch() {
    let expires = Utc::now() + chrono::Duration::hours(5);
    let (_web, _srv, port) = start(vec![
        Site::new("met", "api.met.test").route(
            "/weatherapi/locationforecast/2.0/compact",
            Reply::json(&serde_json::from_str(&fixture("met_norway_compact.json")).unwrap())
                .header("expires", &httpdate::fmt_http_date(expires.into())),
        ),
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
    assert_eq!(r.urls[0].result, UrlResult::Fetched, "{r:?}");
    f.flush().await.unwrap();
    let next = || async {
        let (state, at): (String, chrono::DateTime<Utc>) =
            sqlx::query_as("SELECT state, next_fetch_at FROM url_state WHERE url_key = $1")
                .bind(row.key().as_bytes().as_slice())
                .fetch_one(&pool)
                .await
                .unwrap();
        (state, at)
    };
    let (state, at) = next().await;
    assert_eq!(state, "due");
    assert!(
        at >= expires - chrono::Duration::seconds(1),
        "{at} < Expires {expires}"
    );
    // Robots expire; the scheduler's robots refresh never pulls an adapter row forward.
    sqlx::query("UPDATE origins SET robots_expires_at = now() - interval '1 day'")
        .execute(&pool)
        .await
        .unwrap();
    let rep = sched.pass(&[s]).await.unwrap();
    assert_eq!(rep.robots_refresh, 0);
    assert_eq!(next().await.1, at);
    let fm: String = sqlx::query_scalar("SELECT fetch_mode FROM url_state WHERE url_key = $1")
        .bind(row.key().as_bytes().as_slice())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(fm, FetchMode::Adapter("met_norway".into()).to_string());
}

/// The FCDO change feed reschedules the countries whose entry is newer than their last
/// capture (and only those).
#[tokio::test]
async fn fcdo_change_feed_reschedules_changed_countries() {
    let (_web, _srv, port) = start(vec![Site::new("fcdo", "www.gov.test").route(
        "/foreign-travel-advice.atom",
        Reply::new(200, "application/atom+xml", fixture("fcdo_changes.atom")),
    )])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let base = |p: &str| url("www.gov.test", port, p);
    let index = base("/api/content/foreign-travel-advice");
    let s = spec(&format!(
        "id: fcdo-test\nmethod: api\nadapter: fcdo_content_api\ncadence: daily\nseeds: [\"{index}\"]\n"
    ));
    let f = fetcher(
        &cfg(),
        &pool,
        &["www.gov.test"],
        vec![s.clone()],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let feed = base("/foreign-travel-advice.atom");
    let usa = base("/api/content/foreign-travel-advice/usa");
    let georgia = base("/api/content/foreign-travel-advice/georgia");
    let row = |u: &vc_core::CanonicalUrl| {
        let mut n = NewUrl::new(u.clone());
        n.source_id = Some(s.id.clone());
        n.fetch_mode = FetchMode::Adapter("fcdo_content_api".into());
        n
    };
    f.parts()
        .frontier
        .enqueue(&[row(&feed), row(&usa), row(&georgia)])
        .await
        .unwrap();
    // usa last captured before its feed entry (2026-11-14), georgia after its (2026-11-12).
    // Postgres keeps microseconds.
    let far = chrono::DateTime::from_timestamp_micros(
        (Utc::now() + chrono::Duration::days(30)).timestamp_micros(),
    )
    .unwrap();
    for (u, captured) in [
        (&usa, "2026-11-10T00:00:00Z"),
        (&georgia, "2026-11-13T00:00:00Z"),
    ] {
        sqlx::query(
            "UPDATE url_state SET next_fetch_at = $2, last_captured_at = $3::timestamptz
             WHERE url_key = $1",
        )
        .bind(u.key().as_bytes().as_slice())
        .bind(far)
        .bind(captured)
        .execute(&pool)
        .await
        .unwrap();
    }
    let r = claimed(f.pass().await.unwrap());
    assert_eq!(r.urls.len(), 1, "only the feed row is due");
    assert_eq!(r.urls[0].result, UrlResult::Fetched, "{r:?}");
    let at = |u: &vc_core::CanonicalUrl| {
        let pool = pool.clone();
        let k = u.key().as_bytes().to_vec();
        async move {
            sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
                "SELECT next_fetch_at FROM url_state WHERE url_key = $1",
            )
            .bind(k)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert!(at(&usa).await <= Utc::now(), "usa changed: due now");
    assert_eq!(at(&georgia).await, far, "georgia unchanged: schedule kept");
}

/// Scheduler hygiene: an origin whose robots.txt expired (and whose key has nothing due soon)
/// gets one page row made due, so robots.txt is refetched first; rows of a disabled source
/// leave the crawl.
#[tokio::test]
async fn scheduler_refreshes_stale_robots_and_retires_disabled_sources() {
    let (_web, _srv, port) = start(vec![
        Site::new("a", "a.test").route("/p", Reply::html("<p>p</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg();
    let p = url("a.test", port, "/p");
    let q = url("b.test", port, "/q");
    let live = spec(&format!(
        "id: live-src\nmethod: html\ncadence: weekly\nseeds: [\"{p}\"]\n"
    ));
    let gone = spec(&format!(
        "id: gone-src\nmethod: html\ncadence: weekly\nseeds: [\"{q}\"]\n"
    ));
    let f = fetcher(
        &cfg,
        &pool,
        &["a.test"],
        vec![live.clone()],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let sched = Scheduler::new(
        Arc::new(cfg.clone()),
        pool.clone(),
        f.parts().frontier.clone(),
    );
    sched.pass(&[live.clone(), gone]).await.unwrap();
    claimed(f.pass().await.unwrap());
    f.flush().await.unwrap();
    let next = || async {
        sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "SELECT next_fetch_at FROM url_state WHERE url_key = $1",
        )
        .bind(p.key().as_bytes().as_slice())
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(
        next().await > Utc::now() + chrono::Duration::minutes(30),
        "recrawl later"
    );

    // `gone-src` is no longer in the directory: disabled, its due row retired.
    let rep = sched.pass(std::slice::from_ref(&live)).await.unwrap();
    assert_eq!(rep.sources.disabled, 1);
    assert_eq!(rep.retired, 1);
    assert_eq!(url_state(&pool, &q).await.0, "disabled");
    assert_eq!(rep.robots_refresh, 0, "robots still fresh");

    // Days later: the page is not due for a while, its origin's robots.txt has expired.
    sqlx::query(
        "UPDATE url_state SET next_fetch_at = now() + interval '3 days' WHERE state = 'due'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE origins SET robots_expires_at = now() - interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();
    let rep = sched.pass(std::slice::from_ref(&live)).await.unwrap();
    assert_eq!(rep.robots_refresh, 1);
    assert!(next().await <= Utc::now());
    let rep = sched.pass(std::slice::from_ref(&live)).await.unwrap();
    assert_eq!(
        rep.robots_refresh, 0,
        "something is due now: no second refresh"
    );
}

/// An adapter that stops at its page cap leaves a cursor in `adapter_cursors`; a new fetcher
/// (a restart) continues the stream from it instead of starting over, and the row is due
/// again soon. A complete run deletes the cursor; the scheduler prunes cursors of rows that
/// left the crawl.
#[tokio::test]
async fn adapter_cursor_survives_a_restart() {
    use vc_sources::adapters::ResumableAdapter;
    use vc_sources::adapters::wikidata_sparql::WikidataSparql;
    let two_rows = serde_json::json!({
        "head": { "vars": ["x"] },
        "results": { "bindings": [
            { "x": { "type": "literal", "value": "1" } },
            { "x": { "type": "literal", "value": "2" } }
        ] }
    });
    let (web, _srv, port) = start(vec![
        Site::new("wd", "query.wd.test").route("/sparql", Reply::json(&two_rows)),
    ])
    .await;
    let pool = testkit::pg().await;
    let row = url("query.wd.test", port, "/sparql");
    let s = spec(&format!(
        "id: wd-test\nmethod: api\nadapter: wikidata_sparql\ncadence: monthly\nseeds: [\"{row}\"]\n"
    ));
    let cfg = cfg();
    let fetcher = |dir: &std::path::Path| {
        let mut p = parts(
            &cfg,
            &pool,
            &["query.wd.test"],
            vec![s.clone()],
            dir,
            Arc::new(MemBlobStore::new()),
        );
        let small: Arc<dyn ResumableAdapter> = Arc::new(WikidataSparql {
            page_size: 2,
            max_pages: 1,
        });
        p.resumable = Arc::new([("wikidata_sparql", small)].into_iter().collect());
        vcrawl::roles::fetcher::Fetcher::new(p)
    };
    let cursor = || async {
        sqlx::query_scalar::<_, String>("SELECT cursor FROM adapter_cursors WHERE url_key = $1")
            .bind(row.key().as_bytes().as_slice())
            .fetch_optional(&pool)
            .await
            .unwrap()
    };
    let offsets = || -> Vec<String> {
        web.log()
            .entries()
            .into_iter()
            .filter(|e| e.url.contains("/sparql"))
            .map(|e| {
                let u = url::Url::parse(&e.url).unwrap();
                let q: String = u
                    .query_pairs()
                    .find(|(k, _)| k == "query")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                q.split("OFFSET").nth(1).unwrap_or("").trim().to_string()
            })
            .collect()
    };

    let dir1 = tempfile::tempdir().unwrap();
    let f1 = fetcher(dir1.path());
    let sched = Scheduler::new(
        Arc::new(cfg.clone()),
        pool.clone(),
        f1.parts().frontier.clone(),
    );
    sched.pass(std::slice::from_ref(&s)).await.unwrap();
    let r = claimed(f1.pass().await.unwrap());
    assert_eq!(r.urls[0].result, UrlResult::Fetched, "{r:?}");
    f1.flush().await.unwrap();
    assert_eq!(cursor().await.as_deref(), Some("emergency_numbers@2"));
    assert_eq!(offsets(), vec!["0"]);
    // Due again within RESUME_AFTER, not a month later.
    let at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT next_fetch_at FROM url_state WHERE url_key = $1")
            .bind(row.key().as_bytes().as_slice())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(at <= Utc::now() + chrono::Duration::seconds(65), "{at}");
    drop(f1);

    // "Restart": a brand-new fetcher, its own spool, the same database.
    fast_forward(&pool).await;
    let dir2 = tempfile::tempdir().unwrap();
    let f2 = fetcher(dir2.path());
    let r = claimed(f2.pass().await.unwrap());
    assert_eq!(r.urls[0].result, UrlResult::Fetched, "{r:?}");
    f2.flush().await.unwrap();
    assert_eq!(offsets(), vec!["0", "2"], "continued, not restarted");
    assert_eq!(cursor().await.as_deref(), Some("emergency_numbers@4"));

    // A complete run (no page cap) deletes the cursor. Jump to the last query so the run
    // fits in one claim: it fetches only that query, then completes.
    let last = vc_sources::adapters::wikidata_sparql::QUERIES
        .last()
        .unwrap()
        .name;
    vc_sources::cursors::PgAdapterCursors::new(pool.clone())
        .save(&row.key(), "wikidata_sparql", &format!("{last}@0"))
        .await
        .unwrap();
    fast_forward(&pool).await;
    let dir3 = tempfile::tempdir().unwrap();
    let mut p = parts(
        &cfg,
        &pool,
        &["query.wd.test"],
        vec![s.clone()],
        dir3.path(),
        Arc::new(MemBlobStore::new()),
    );
    let big: Arc<dyn ResumableAdapter> = Arc::new(WikidataSparql {
        page_size: 5,
        max_pages: 3,
    });
    p.resumable = Arc::new([("wikidata_sparql", big)].into_iter().collect());
    let f3 = vcrawl::roles::fetcher::Fetcher::new(p);
    let r = claimed(f3.pass().await.unwrap());
    assert_eq!(r.urls[0].result, UrlResult::Fetched, "{r:?}");
    f3.flush().await.unwrap();
    assert_eq!(offsets().len(), 3, "one request: resumed at the last query");
    assert_eq!(cursor().await, None);

    // The scheduler prunes cursors of rows that left the crawl.
    vc_sources::cursors::PgAdapterCursors::new(pool.clone())
        .save(&row.key(), "wikidata_sparql", "emergency_numbers@4")
        .await
        .unwrap();
    sqlx::query("UPDATE url_state SET state = 'disabled' WHERE url_key = $1")
        .bind(row.key().as_bytes().as_slice())
        .execute(&pool)
        .await
        .unwrap();
    let rep = sched.pass(std::slice::from_ref(&s)).await.unwrap();
    assert_eq!(rep.cursors_pruned, 1);
    assert_eq!(cursor().await, None);
}
