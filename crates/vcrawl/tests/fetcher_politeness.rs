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

/// The stub policy, plus a robots `Crawl-delay` in the summary.
struct CrawlDelayPolicy {
    inner: testkit::vc_fetch_fixtures::StubPolicy,
    delay: Duration,
}

impl vc_core::policy::Policy for CrawlDelayPolicy {
    fn robots_verdict(
        &self,
        r: &vc_core::policy::RobotsEntry,
        url: &vc_core::CanonicalUrl,
    ) -> vc_core::policy::RobotsVerdict {
        self.inner.robots_verdict(r, url)
    }
    fn robots_summary(&self, r: &vc_core::policy::RobotsEntry) -> vc_core::policy::RobotsSummary {
        let mut s = self.inner.robots_summary(r);
        s.crawl_delay = Some(self.delay);
        s
    }
    fn newly_disallowed(
        &self,
        old: &vc_core::policy::RobotsEntry,
        new: &vc_core::policy::RobotsEntry,
        urls: &[vc_core::CanonicalUrl],
    ) -> Vec<vc_core::CanonicalUrl> {
        self.inner.newly_disallowed(old, new, urls)
    }
    fn use_policy(&self, s: &vc_core::usepolicy::UseSignals) -> vc_core::usepolicy::UsePolicy {
        self.inner.use_policy(s)
    }
    fn denied(&self, url: &vc_core::CanonicalUrl) -> Option<vc_core::policy::DenyReason> {
        self.inner.denied(url)
    }
    fn subresource_host_allowed(&self, host: &str) -> bool {
        self.inner.subresource_host_allowed(host)
    }
    fn ssrf_check(
        &self,
        host: &str,
        ip: std::net::IpAddr,
        port: u16,
    ) -> Result<(), vc_core::policy::SsrfBlock> {
        self.inner.ssrf_check(host, ip, port)
    }
    fn is_challenge(&self, ex: &vc_core::fetch::Exchange) -> bool {
        self.inner.is_challenge(ex)
    }
}

/// Item 3: a `Crawl-delay` learned from the robots.txt fetched at the start of a claim paces
/// the rest of that same claim (it used to apply only from the next claim on).
#[tokio::test]
async fn crawl_delay_learned_in_a_claim_paces_that_claim() {
    let (web, _srv, port) = start(vec![
        Site::new("slow", "slow.test")
            .route(
                "/robots.txt",
                Reply::text("User-agent: *\nCrawl-delay: 3\n"),
            )
            .route("/a", Reply::html("<p>a</p>"))
            .route("/b", Reply::html("<p>b</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let mut parts = crawl_path_support::parts(
        &cfg(),
        &pool,
        &["slow.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    parts.policy = Arc::new(CrawlDelayPolicy {
        inner: testkit::vc_fetch_fixtures::StubPolicy::loopback_ok(),
        delay: Duration::from_secs(3),
    });
    let f = vcrawl::roles::fetcher::Fetcher::new(parts);
    let mut a = NewUrl::new(url("slow.test", port, "/a"));
    a.priority = 0.9;
    let b = NewUrl::new(url("slow.test", port, "/b"));
    f.parts().frontier.enqueue(&[a, b]).await.unwrap();
    let r = claimed(f.pass().await.unwrap());
    assert!(
        r.urls.iter().all(|o| o.result == UrlResult::Fetched),
        "{r:?}"
    );
    let e = web.log().entries();
    assert_eq!(e.len(), 3);
    for w in e.windows(2) {
        let gap = (w[1].started_at - w[0].ended_at).num_milliseconds();
        assert!(
            gap >= 2900,
            "{} → {}: {gap} ms < Crawl-delay",
            w[0].url,
            w[1].url
        );
    }
}

/// Item 5: a same-key redirect (apex → www) to an origin whose robots.txt is not cached is no
/// longer an endless 10-minute retry: that origin's robots.txt is fetched under the same
/// permit and the URL is fetched in the same claim.
#[tokio::test]
async fn same_key_redirect_fetches_the_target_robots_first() {
    let (web, srv, port) = start(vec![]).await;
    web.add_site(
        Site::new("apex", "apex.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route(
                "/p",
                Reply::redirect(301, &format!("http://www.apex.test:{port}/p")),
            ),
    );
    web.add_site(
        Site::new("www", "www.apex.test")
            .route(
                "/robots.txt",
                Reply::text("User-agent: *\nDisallow: /private\n"),
            )
            .route("/p", Reply::html("<p>www</p>")),
    );
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg(),
        &pool,
        &["apex.test", "www.apex.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(url("apex.test", port, "/p"))])
        .await
        .unwrap();
    let r = claimed(f.pass().await.unwrap());
    assert_eq!(r.urls[0].result, UrlResult::Fetched, "{r:?}");
    assert_eq!(
        paths(&web),
        [
            "apex.test/robots.txt",
            "apex.test/p",
            "www.apex.test/robots.txt",
            "apex.test/p",
            "www.apex.test/p",
        ]
    );
    let st: String = sqlx::query_scalar("SELECT robots_state FROM origins WHERE origin = $1")
        .bind(format!("http://www.apex.test:{port}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "ok");
    drop(srv);
}

/// A renderer stand-in: the document through the permit client, no sub-requests.
struct DocOnlyRenderer {
    client: Arc<dyn vc_core::fetch::PermitClient>,
}

#[async_trait::async_trait]
impl vc_core::render::Renderer for DocOnlyRenderer {
    async fn render(
        &self,
        p: &vc_core::frontier::ClaimPermit,
        url: &vc_core::CanonicalUrl,
        _budget: vc_core::render::RenderBudget,
    ) -> vc_core::Result<vc_core::render::RenderOutcome> {
        let ex = self
            .client
            .fetch(
                p,
                vc_core::fetch::FetchRequest::get(
                    url.clone(),
                    vc_core::fetch::RequestPurpose::Page,
                ),
            )
            .await
            .map_err(vc_core::Error::Fetch)?;
        Ok(vc_core::render::RenderOutcome {
            dom_html: ex.body.clone(),
            exchanges: vec![ex],
            blocked: Vec::new(),
        })
    }
}

async fn render_claim(reply: Reply) -> (sqlx::PgPool, ClaimReport, tempfile::TempDir) {
    let (_web, srv, port) = start(vec![
        Site::new("spa", "spa.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route("/p", reply)
            .route("/q", Reply::html("<p>q</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let mut parts = crawl_path_support::parts(
        &cfg(),
        &pool,
        &["spa.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    parts.renderer = Some(Arc::new(DocOnlyRenderer {
        client: parts.client.clone(),
    }));
    let f = vcrawl::roles::fetcher::Fetcher::new(parts);
    let mut p = NewUrl::new(url("spa.test", port, "/p"));
    p.fetch_mode = vc_core::media::FetchMode::Render;
    p.priority = 0.9;
    let mut q = NewUrl::new(url("spa.test", port, "/q"));
    q.fetch_mode = vc_core::media::FetchMode::Render;
    f.parts().frontier.enqueue(&[p, q]).await.unwrap();
    let r = claimed(f.pass().await.unwrap());
    drop(srv);
    (pool, r, dir)
}

/// Item 2: a rendered page answered 503 + `Retry-After` backs the key off and stops the claim
/// (it used to be stored as a fetched page).
#[tokio::test]
async fn render_page_503_rate_limits_the_key() {
    let (_pool, r, _dir) = render_claim(Reply::status(503).header("retry-after", "900")).await;
    assert_eq!(
        r.signal,
        Some(KeySignal::RateLimited {
            retry_after: Some(Duration::from_secs(900))
        }),
        "{r:?}"
    );
    assert!(matches!(r.urls[0].result, UrlResult::Retry { .. }), "{r:?}");
    assert_eq!(r.urls[1].result, UrlResult::NotAttempted, "claim stopped");
}

/// Item 2: a rendered page's 404 is `Gone`, its 5xx counts toward the 5xx park.
#[tokio::test]
async fn render_page_404_and_5xx_follow_the_status_ladder() {
    let (_pool, r, _dir) = render_claim(Reply::status(404)).await;
    assert_eq!(r.urls[0].result, UrlResult::Gone, "{r:?}");
    assert_eq!(r.urls[1].result, UrlResult::Fetched);
    let (_pool, r, _dir) = render_claim(Reply::status(500)).await;
    assert!(
        matches!(r.urls[0].result, UrlResult::Failed { .. }),
        "{r:?}"
    );
    assert_eq!(r.urls[1].result, UrlResult::Fetched);
    assert_eq!(
        r.signal,
        Some(KeySignal::Normal),
        "a later 200 ends the run"
    );
}

/// A renderer stand-in: the document, then one third-party sub-resource, both through the
/// renderer's permit client, the sub-resource behind the renderer's third-party hook exactly
/// as `vc_render`'s engine calls it (borrow, fetch, drop the guard); a refused sub-request is
/// skipped.
struct DocAndCdnRenderer {
    client: Arc<dyn vc_core::fetch::PermitClient>,
    borrow: Arc<dyn vc_render::ThirdPartyBorrow>,
    sub: vc_core::CanonicalUrl,
}

#[async_trait::async_trait]
impl vc_core::render::Renderer for DocAndCdnRenderer {
    async fn render(
        &self,
        p: &vc_core::frontier::ClaimPermit,
        url: &vc_core::CanonicalUrl,
        _budget: vc_core::render::RenderBudget,
    ) -> vc_core::Result<vc_core::render::RenderOutcome> {
        use vc_core::fetch::{FetchRequest, RequestPurpose};
        let doc = self
            .client
            .fetch(p, FetchRequest::get(url.clone(), RequestPurpose::Page))
            .await
            .map_err(vc_core::Error::Fetch)?;
        let mut out = vc_core::render::RenderOutcome {
            dom_html: doc.body.clone(),
            exchanges: vec![doc],
            blocked: Vec::new(),
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let sub = match self.borrow.borrow(p, &self.sub, deadline).await {
            Ok(guard) => {
                let r = self
                    .client
                    .fetch(
                        p,
                        FetchRequest::get(self.sub.clone(), RequestPurpose::SubResource),
                    )
                    .await;
                drop(guard);
                r
            }
            Err(e) => Err(e),
        };
        match sub {
            Ok(ex) => out.exchanges.push(ex),
            Err(_) => out
                .blocked
                .push((self.sub.clone(), vc_core::render::BlockReason::FetchFailed)),
        }
        Ok(out)
    }
}

/// 8': a render sub-request to a third-party key (an allowlisted CDN) honours that key's own
/// state: right after a claim on the CDN fetched from it, the sub-request is skipped instead
/// of hitting the CDN again within its delay.
#[tokio::test]
async fn render_subrequest_to_a_third_party_key_honours_that_key() {
    let (web, _srv, port) = start(vec![
        Site::new("spa", "spa.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route("/p", Reply::html("<link rel=stylesheet href=//cdn>")),
        Site::new("cdn", "cdn.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route("/css/site.css", Reply::css("p{}"))
            .route("/x", Reply::html("<p>cdn</p>")),
    ])
    .await;
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg();
    c.frontier.min_delay_s = 5;
    let mut parts = crawl_path_support::parts(
        &c,
        &pool,
        &["spa.test", "cdn.test"],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let policy = Arc::new(testkit::vc_fetch_fixtures::StubPolicy::loopback_ok());
    policy.allow_subresource_host("cdn.test");
    let resolver = vc_fetch::StaticResolver::new()
        .with(
            "spa.test",
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        )
        .with(
            "cdn.test",
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        );
    let client = vcrawl::wiring::fetch::permit_client(
        &c,
        parts.frontier.clone(),
        policy.clone(),
        parts.clock.clone(),
        Some(Arc::new(resolver)),
    );
    let pg = Arc::new(vc_frontier::PgFrontier::new(
        pool.clone(),
        c.frontier.clone(),
    ));
    let css = url("cdn.test", port, "/css/site.css");
    parts.client = client.clone();
    parts.policy = policy;
    parts.renderer = Some(Arc::new(DocAndCdnRenderer {
        client,
        borrow: vcrawl::wiring::fetch::third_party_borrow(pg),
        sub: css.clone(),
    }));
    let f = vcrawl::roles::fetcher::Fetcher::new(parts);
    // A claim on the CDN itself: robots.txt + a page.
    f.parts()
        .frontier
        .enqueue(&[NewUrl::new(url("cdn.test", port, "/x"))])
        .await
        .unwrap();
    assert_eq!(
        claimed(f.pass().await.unwrap()).urls[0].result,
        UrlResult::Fetched
    );
    // The CDN key is not ready again for a minute (its own Crawl-delay, say).
    sqlx::query(
        "UPDATE politeness_keys SET next_allowed_at = now() + interval '60 seconds'
         WHERE key = 'cdn.test'",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Meanwhile a page on another key renders and wants a stylesheet from the CDN.
    let mut p = NewUrl::new(url("spa.test", port, "/p"));
    p.fetch_mode = vc_core::media::FetchMode::Render;
    f.parts().frontier.enqueue(&[p]).await.unwrap();
    let r = claimed(f.pass().await.unwrap());
    assert_eq!(r.urls[0].result, UrlResult::Fetched, "{r:?}");
    let css_hits = |web: &FakeWeb| {
        paths(web)
            .iter()
            .filter(|p| p.as_str() == "cdn.test/css/site.css")
            .count()
    };
    assert_eq!(
        css_hits(&web),
        0,
        "the CDN key's delay was honoured: {:?}",
        paths(&web)
    );
    // Once the CDN key is ready again, the sub-resource is fetched (and holds the key).
    fast_forward(&pool).await;
    sqlx::query("UPDATE url_state SET state = 'due', lease_epoch = NULL, next_fetch_at = now() WHERE key = 'spa.test'")
        .execute(&pool)
        .await
        .unwrap();
    claimed(f.pass().await.unwrap());
    assert_eq!(css_hits(&web), 1, "{:?}", paths(&web));
}

/// A scripted [`KeyBorrow`]: answers `grant` (or an error) and records every call.
struct ScriptedBorrow {
    grant: Option<bool>,
    delay: Duration,
    calls: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl vcrawl::wiring::fetch::KeyBorrow for ScriptedBorrow {
    async fn borrow(&self, key: &vc_core::PolitenessKey) -> vc_core::Result<bool> {
        self.calls.lock().unwrap().push(format!("borrow {key}"));
        tokio::time::sleep(self.delay).await;
        self.grant
            .ok_or_else(|| vc_core::Error::Db("connection reset".into()))
    }

    async fn done(&self, key: &vc_core::PolitenessKey) -> vc_core::Result<()> {
        self.calls.lock().unwrap().push(format!("done {key}"));
        Ok(())
    }
}

/// The production third-party hook: a granted borrow is released (key held for its delay)
/// when the request's guard drops; a refused or failed borrow refuses only that sub-request
/// (a `Transport` error, nothing to release); the permit's own key never borrows; a borrow
/// still pending at the render deadline ends as `DeadlineExceeded`.
#[tokio::test]
async fn third_party_borrow_hook_holds_and_releases_the_other_key() {
    use vc_core::fetch::FetchError;
    use vc_core::frontier::Frontier as _;
    let clock = testkit::FakeClock::default();
    let frontier = testkit::MemFrontier::new(Arc::new(clock.clone()));
    let page = vc_core::CanonicalUrl::parse("https://spa.example.com/p").unwrap();
    frontier
        .enqueue(&[NewUrl::new(page.clone())])
        .await
        .unwrap();
    let claim = frontier.claim(&[0], 1).await.unwrap().expect("a claim");
    let p = &claim.permit;
    let cdn = vc_core::CanonicalUrl::parse("https://cdn.example.net/lib.js").unwrap();
    let soon = || tokio::time::Instant::now() + Duration::from_secs(5);

    let hook = |grant: Option<bool>, delay: Duration| {
        let b = Arc::new(ScriptedBorrow {
            grant,
            delay,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        (b.clone(), vcrawl::wiring::fetch::third_party_borrow(b))
    };

    // Granted: held while the request runs, released when the guard drops.
    let (b, h) = hook(Some(true), Duration::ZERO);
    let guard = h.borrow(p, &cdn, soon()).await.expect("granted");
    assert_eq!(*b.calls.lock().unwrap(), vec!["borrow example.net"]);
    drop(guard);
    for _ in 0..100 {
        if b.calls.lock().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *b.calls.lock().unwrap(),
        vec!["borrow example.net", "done example.net"]
    );

    // Not ready, or unverifiable: that sub-request only is refused; nothing to release.
    for grant in [Some(false), None] {
        let (b, h) = hook(grant, Duration::ZERO);
        let e = h.borrow(p, &cdn, soon()).await.err().expect("refused");
        assert!(matches!(e, FetchError::Transport(_)), "{e:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*b.calls.lock().unwrap(), vec!["borrow example.net"]);
    }

    // The permit's own key is covered by the permit: no borrow.
    let (b, h) = hook(Some(false), Duration::ZERO);
    let own = vc_core::CanonicalUrl::parse("https://spa.example.com/app.js").unwrap();
    drop(h.borrow(p, &own, soon()).await.expect("own key"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(b.calls.lock().unwrap().is_empty());

    // A borrow that cannot finish before the render deadline.
    let (_, h) = hook(Some(true), Duration::from_secs(5));
    let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
    let e = h.borrow(p, &cdn, deadline).await.err().expect("deadline");
    assert!(matches!(e, FetchError::DeadlineExceeded), "{e:?}");
}
