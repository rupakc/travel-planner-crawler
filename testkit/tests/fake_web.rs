//! fake-web: the seven scripted sites, cycles, request logging, admin endpoints, and the
//! politeness oracle end to end over real HTTP.

use std::sync::Arc;
use std::time::Duration;

use testkit::fake_web::sites::{self, SiteOptions};
use testkit::fake_web::{FakeWeb, Reply, Site};
use testkit::oracle::{ViolationKind, check_polite};
use testkit::{RequestLog, assert_polite};
use vc_core::USER_AGENT;

fn fast() -> SiteOptions {
    SiteOptions {
        slow_latency: Duration::from_millis(50),
        slow_crawl_delay_s: 1,
        ..SiteOptions::default()
    }
}

async fn get(c: &reqwest::Client, url: &str) -> (u16, String, reqwest::header::HeaderMap) {
    let r = c.get(url).send().await.unwrap();
    let s = r.status().as_u16();
    let h = r.headers().clone();
    (s, r.text().await.unwrap(), h)
}

#[test]
fn all_seven_sites_have_distinct_hosts_and_robots() {
    let all = sites::all(&SiteOptions::default());
    assert_eq!(all.len(), 7);
    let hosts: Vec<_> = all.iter().map(|s| s.host.as_str()).collect();
    assert_eq!(hosts, sites::HOSTS.to_vec());
    for s in &all {
        assert!(
            s.reply("/robots.txt", 0).is_some(),
            "{} has robots.txt",
            s.host
        );
    }
    // The official site's key differs from its host.
    assert_eq!(sites::visa(&fast()).key(), "immigration.test");
    assert_eq!(sites::blog(&fast()).key(), "blog.test");
}

#[tokio::test]
async fn serves_sites_by_host_and_logs_every_request() {
    let web = Arc::new(FakeWeb::with_sites(sites::all(&fast())));
    let srv = web.clone().start().await;
    let c = srv.client();

    let (s, body, _) = get(&c, &srv.url(sites::BLOG, "/robots.txt")).await;
    assert_eq!(s, 200);
    assert!(body.contains("Sitemap: http://blog.test/sitemap.xml"));
    let (s, body, _) = get(&c, &srv.url(sites::BLOG, "/feed.xml")).await;
    assert_eq!(s, 200);
    assert!(body.contains("<rss"));
    let (s, body, _) = get(&c, &srv.url(sites::EVENTS, "/events/jazz-night")).await;
    assert_eq!(s, 200);
    assert!(body.contains("application/ld+json") && body.contains("\"@type\":\"Event\""));
    let (s, body, _) = get(&c, &srv.url(sites::SPA, "/")).await;
    assert_eq!(s, 200);
    assert!(body.contains(r#"<div id="root"></div>"#));
    let (s, body, _) = get(&c, &srv.url(sites::CDN, "/js/app.js")).await;
    assert_eq!(s, 200);
    assert!(body.contains(sites::METADATA_IP) && body.contains("/api/attractions.json"));
    let (s, _, _) = get(&c, &srv.url(sites::VISA, "/nope")).await;
    assert_eq!(s, 404);
    // Unknown hosts are answered (404) and logged too.
    let (s, _, _) = get(&c, &srv.url(sites::DENIED, "/deal")).await;
    assert_eq!(s, 404);
    // Admin traffic is not logged.
    let r = reqwest::get(format!("{}/__admin/health", srv.base_url()))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let log = srv.log();
    assert_eq!(log.len(), 7);
    let e = &log.entries()[0];
    assert_eq!(e.url, "http://blog.test/robots.txt");
    assert_eq!(e.key, "blog.test");
    assert_eq!(e.ua, USER_AGENT);
    assert_eq!(e.status, 200);
    assert!(e.ended_at >= e.started_at);
    let keys: Vec<String> = log.entries().iter().map(|e| e.key.clone()).collect();
    assert!(keys.contains(&"immigration.test".to_string()));
    assert!(keys.contains(&"denied.test".to_string()));
}

#[tokio::test]
async fn cycles_change_content_robots_and_status() {
    let web = Arc::new(FakeWeb::with_sites(sites::all(&fast())));
    let srv = web.clone().start().await;
    let c = srv.client();
    let hello = srv.url(sites::BLOG, "/posts/hello");
    let robots = srv.url(sites::BLOG, "/robots.txt");
    let third = srv.url(sites::BLOG, "/posts/third");

    let (_, v0, _) = get(&c, &hello).await;
    let (_, r0, _) = get(&c, &robots).await;
    assert_eq!(get(&c, &third).await.0, 404);
    assert!(!r0.contains("Disallow: /private/"));

    assert_eq!(srv.next_cycle(), 1);
    let (_, v1, _) = get(&c, &hello).await;
    assert_ne!(v0, v1, "content changes in cycle 1");
    assert_eq!(get(&c, &third).await.0, 200, "a new post appears");
    assert!(
        get(&c, &srv.url(sites::BLOG, "/feed.xml"))
            .await
            .1
            .contains("/posts/third")
    );

    // Admin endpoint moves the cycle.
    let r: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/__admin/cycle", srv.base_url()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["cycle"], 2);
    let (_, r2, _) = get(&c, &robots).await;
    assert!(
        r2.contains("Disallow: /private/"),
        "robots changes in cycle 2"
    );
    let (_, v2, _) = get(&c, &hello).await;
    assert_eq!(v1, v2, "the last scripted reply repeats");

    // Set an explicit cycle.
    let r: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/__admin/cycle", srv.base_url()))
        .json(&serde_json::json!({"cycle": 0}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["cycle"], 0);
    assert_eq!(get(&c, &hello).await.1, v0);

    // The snapshot remembers both robots bodies, in order.
    let snap = srv.policy_snapshot();
    let blog: Vec<_> = snap
        .robots
        .iter()
        .filter(|r| r.origin == "http://blog.test")
        .collect();
    assert_eq!(blog.len(), 2);
    assert!(!blog[0].body.contains("/private/"));
    assert!(blog[1].body.contains("/private/"));
}

#[tokio::test]
async fn redirects_challenges_and_retry_after() {
    let web = Arc::new(FakeWeb::with_sites(sites::all(&fast())));
    let srv = web.clone().start().await;
    let c = srv.client();

    let (s, _, h) = get(&c, &srv.url(sites::BLOG, "/go/events")).await;
    assert_eq!(s, 301);
    assert_eq!(h["location"], "http://events.test/");
    let (s, _, h) = get(&c, &srv.url(sites::BLOG, "/go/internal")).await;
    assert_eq!(s, 302);
    assert_eq!(h["location"], format!("http://{}/admin", sites::PRIVATE_IP));
    assert_eq!(get(&c, &srv.url(sites::BLOG, "/gone")).await.0, 410);

    let (s, body, h) = get(&c, &srv.url(sites::HOSTILE, "/anything")).await;
    assert_eq!(s, 403);
    assert_eq!(h["cf-mitigated"], "challenge");
    assert!(body.contains("Just a moment..."));
    let (s, _, h) = get(&c, &srv.url(sites::SLOW, "/busy")).await;
    assert_eq!(s, 503);
    assert_eq!(h["retry-after"], "120");
    let (_, _, h) = get(&c, &srv.url(sites::VISA, "/notice")).await;
    assert_eq!(h["x-robots-tag"], "noarchive");

    let snap = srv.policy_snapshot();
    assert!(
        snap.backoffs
            .iter()
            .any(|b| b.key == "hostile.test" && b.reason == "challenge")
    );
    let ra = snap
        .backoffs
        .iter()
        .find(|b| b.key == "slow.test" && b.reason == "retry_after")
        .expect("retry-after window");
    assert_eq!((ra.until - ra.from).num_seconds(), 120);
    assert!(snap.denylist.contains(&sites::DENIED.to_string()));
    assert_eq!(snap.keys["slow.test"].min_delay_ms, Some(1_000));
    assert_eq!(
        snap.keys["attractions.test"].subresource_paths,
        vec!["/api/"]
    );
}

#[tokio::test]
async fn slow_site_adds_latency() {
    let web = Arc::new(FakeWeb::with_sites(vec![sites::slow(&SiteOptions {
        slow_latency: Duration::from_millis(300),
        ..fast()
    })]));
    let srv = web.clone().start().await;
    let t0 = std::time::Instant::now();
    assert_eq!(get(&srv.client(), &srv.url(sites::SLOW, "/")).await.0, 200);
    assert!(t0.elapsed() >= Duration::from_millis(300));
    let e = &srv.log().entries()[0];
    assert!((e.ended_at - e.started_at).num_milliseconds() >= 300);
}

/// `Reply::no_response()` never answers; once the client gives up, the request is logged with
/// status 0 and the snapshot records a robots.txt with no response.
#[tokio::test]
async fn no_response_hangs_and_is_recorded_when_the_client_gives_up() {
    let web = Arc::new(FakeWeb::with_sites(vec![
        Site::new("dead", "dead.test").route("/robots.txt", Reply::no_response()),
    ]));
    let srv = web.clone().start().await;
    let c = srv.client();
    let err = c
        .get(srv.url("dead.test", "/robots.txt"))
        .timeout(Duration::from_millis(300))
        .send()
        .await
        .unwrap_err();
    assert!(err.is_timeout(), "{err:?}");
    let mut entries = Vec::new();
    for _ in 0..100 {
        entries = srv.log().entries();
        if !entries.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, 0);
    assert!((entries[0].ended_at - entries[0].started_at).num_milliseconds() >= 250);
    let snap = srv.policy_snapshot();
    assert_eq!(snap.robots.len(), 1);
    assert!(snap.robots[0].no_response);
    assert_eq!(snap.robots[0].origin, "http://dead.test");
}

#[tokio::test]
async fn admin_log_and_snapshot_round_trip_for_e2e() {
    let web = Arc::new(FakeWeb::with_default_sites());
    let srv = web.clone().start().await;
    let c = srv.client();
    get(&c, &srv.url(sites::EVENTS, "/robots.txt")).await;
    get(&c, &srv.url(sites::EVENTS, "/")).await;

    let jsonl = reqwest::get(format!("{}/__admin/log", srv.base_url()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let log = RequestLog::from_jsonl(&jsonl).unwrap();
    assert_eq!(log.entries(), srv.log().entries());
    let snap: testkit::PolicySnapshot =
        reqwest::get(format!("{}/__admin/snapshot", srv.base_url()))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(snap, srv.policy_snapshot());

    let r = reqwest::Client::new()
        .post(format!("{}/__admin/reset", srv.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(srv.log().is_empty());
    assert!(srv.policy_snapshot().robots.is_empty());
}

#[tokio::test]
async fn runtime_routes_and_custom_sites() {
    let web = Arc::new(FakeWeb::with_sites(vec![
        Site::new("x", "x.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route("/a?p=1", Reply::text("query")),
    ]));
    let srv = web.clone().start().await;
    let c = srv.client();
    assert_eq!(get(&c, &srv.url("x.test", "/a?p=1")).await.1, "query");
    assert_eq!(get(&c, &srv.url("x.test", "/a")).await.0, 404);
    web.set_route("x.test", "/a", vec![None, Some(Reply::text("now"))]);
    assert_eq!(get(&c, &srv.url("x.test", "/a")).await.0, 404);
    web.set_cycle(5);
    assert_eq!(get(&c, &srv.url("x.test", "/a")).await.1, "now");
}

/// A tiny polite crawler: robots first, then pages with the key delay, never a disallowed
/// path, and stop at a challenge. The oracle must pass it; a hasty variant must fail.
async fn crawl(
    srv: &testkit::FakeWebServer,
    host: &str,
    paths: &[&str],
    gap: Duration,
    polite: bool,
) {
    let c = srv.client();
    let robots = c
        .get(srv.url(host, "/robots.txt"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let robot = texting_robots::Robot::new(vc_core::ROBOTS_TOKEN, robots.as_bytes()).unwrap();
    for p in paths {
        tokio::time::sleep(gap).await;
        if polite && !robot.allowed(p) {
            continue;
        }
        let r = c.get(srv.url(host, p)).send().await.unwrap();
        if polite && r.status() == 403 {
            break;
        }
    }
}

#[tokio::test]
async fn oracle_passes_a_polite_crawl_and_catches_a_hasty_one() {
    let o = SiteOptions {
        robots_change_cycle: 0, // /private/ disallowed from the start
        ..fast()
    };
    let web = Arc::new(FakeWeb::with_sites(sites::all(&o)));
    let srv = web.clone().start().await;
    let gap = Duration::from_millis(120);
    let paths = ["/", "/posts/hello", "/private/draft", "/posts/second"];
    crawl(&srv, sites::BLOG, &paths, gap, true).await;
    crawl(&srv, sites::HOSTILE, &["/", "/again"], gap, true).await;
    let mut snap = srv.policy_snapshot();
    snap.default_min_delay_ms = 100;
    snap.keys.clear();
    assert_polite(&srv.log(), &snap);

    // The same crawl without manners.
    web.reset();
    crawl(&srv, sites::BLOG, &paths, Duration::ZERO, false).await;
    crawl(&srv, sites::HOSTILE, &["/", "/again"], gap, false).await;
    let mut snap = srv.policy_snapshot();
    snap.default_min_delay_ms = 100;
    snap.keys.clear();
    let kinds: Vec<ViolationKind> = check_polite(&srv.log(), &snap)
        .into_iter()
        .map(|v| v.kind)
        .collect();
    assert!(kinds.contains(&ViolationKind::ShortGap), "{kinds:?}");
    assert!(kinds.contains(&ViolationKind::Disallowed), "{kinds:?}");
    assert!(kinds.contains(&ViolationKind::AfterPark), "{kinds:?}");
}
