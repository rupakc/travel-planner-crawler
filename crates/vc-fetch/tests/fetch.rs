//! `ReqwestPermitClient` against wiremock (Task 4). Names follow the plan.

use std::io::Write as _;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use futures::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode};
use testkit::vc_fetch_fixtures::{StubPolicy, robots_ok};
use testkit::{FakeClock, MemFrontier};
use vc_config::FetchConfig;
use vc_core::fetch::{FetchError, FetchRequest, PermitClient, RequestPurpose};
use vc_core::frontier::{Claim, Frontier, NewUrl, Validators};
use vc_core::{CanonicalUrl, Clock, USER_AGENT};
use vc_fetch::{ReqwestPermitClient, StaticResolver, parse_retry_after, retry_after};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LOOPBACK: [u8; 4] = [127, 0, 0, 1];

struct Env {
    server: MockServer,
    clock: FakeClock,
    frontier: Arc<MemFrontier>,
    policy: Arc<StubPolicy>,
    client: ReqwestPermitClient,
}

impl Env {
    async fn new() -> Env {
        Env::with(
            Duration::from_secs(240),
            Duration::ZERO,
            FetchConfig::default(),
        )
        .await
    }

    async fn with(budget: Duration, delay: Duration, config: FetchConfig) -> Env {
        let server = MockServer::start().await;
        let clock = FakeClock::default();
        let frontier = Arc::new(
            MemFrontier::new(Arc::new(clock.clone()))
                .with_per_url_budget(budget)
                .with_delay(delay),
        );
        let policy = Arc::new(StubPolicy::loopback_ok());
        let resolver = StaticResolver::new()
            .with("site.test", IpAddr::from(LOOPBACK))
            .with("www.site.test", IpAddr::from(LOOPBACK))
            .with("other.test", IpAddr::from(LOOPBACK))
            .with("internal.site.test", IpAddr::from([10, 0, 0, 5]))
            // Dual-stack: AAAA first (hickory's default order), the server listens on IPv4.
            .with("dual.site.test", IpAddr::from(std::net::Ipv6Addr::LOCALHOST))
            .with("dual.site.test", IpAddr::from(LOOPBACK));
        policy.allow_ip(IpAddr::from(std::net::Ipv6Addr::LOCALHOST));
        let client = ReqwestPermitClient::new(config, frontier.clone(), policy.clone())
            .with_clock(Arc::new(clock.clone()))
            .with_resolver(Arc::new(resolver));
        Env {
            server,
            clock,
            frontier,
            policy,
            client,
        }
    }

    fn url(&self, host: &str, p: &str) -> CanonicalUrl {
        let port = self.server.address().port();
        CanonicalUrl::parse(&format!("http://{host}:{port}{p}")).unwrap()
    }

    /// Enqueues and claims `url`, storing `robots` for the given hosts' origins.
    async fn claim(&self, url: &CanonicalUrl, robots: &str, hosts: &[&str]) -> Claim {
        self.frontier
            .enqueue(&[NewUrl::new(url.clone())])
            .await
            .unwrap();
        let claim = self
            .frontier
            .claim(&[0], 1)
            .await
            .unwrap()
            .expect("a claim");
        for h in hosts {
            let origin = self.url(h, "/").origin();
            self.frontier
                .store_robots(&claim.permit, robots_ok(origin, robots, self.clock.now()))
                .await
                .unwrap();
        }
        claim
    }

    async fn requests_to(&self, p: &str) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == p)
            .count()
    }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn page(url: CanonicalUrl) -> FetchRequest {
    FetchRequest::get(url, RequestPurpose::Page)
}

/// A dual-stack host whose resolver lists the AAAA address first is pinned to its IPv4
/// address: the production VM has no IPv6 route (GCE default network) and BOT.md publishes
/// one IPv4 egress address, so a pinned IPv6 address fails every request on the host.
#[tokio::test]
async fn dual_stack_host_pins_ipv4() {
    let env = Env::new().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&env.server)
        .await;
    let u = env.url("dual.site.test", "/robots.txt");
    let claim = env.claim(&u, "", &["dual.site.test"]).await;
    let ex = env
        .client
        .fetch(&claim.permit, FetchRequest::get(u, RequestPurpose::Robots))
        .await
        .expect("an IPv4-reachable dual-stack host is fetched");
    assert_eq!(ex.status, StatusCode::OK);
    assert_eq!(ex.peer_ip, Some(IpAddr::from(LOOPBACK)));
}

#[tokio::test]
async fn conditional_get_304() {
    let env = Env::new().await;
    Mock::given(method("GET"))
        .and(path("/a"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304).insert_header("etag", "\"v1\""))
        .expect(1)
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/a");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let mut req = page(u.clone());
    req.validators = Validators {
        etag: Some("\"v1\"".into()),
        last_modified: Some("Wed, 21 Oct 2025 07:28:00 GMT".into()),
    };
    let ex = env.client.fetch(&claim.permit, req).await.unwrap();
    assert_eq!(ex.status, StatusCode::NOT_MODIFIED);
    assert!(ex.body.is_empty());
    assert_eq!(ex.validators().etag.as_deref(), Some("\"v1\""));
    assert_eq!(ex.request_headers.get("if-none-match").unwrap(), "\"v1\"");
    assert_eq!(ex.peer_ip, Some(IpAddr::from(LOOPBACK)));
    // (wiremock's header matcher splits on commas, so the date is checked here.)
    let reqs = env.server.received_requests().await.unwrap();
    assert_eq!(
        reqs[0].headers.get("if-modified-since").unwrap(),
        "Wed, 21 Oct 2025 07:28:00 GMT"
    );
}

#[tokio::test]
async fn gzip_bomb_capped() {
    let env = Env::new().await;
    let bomb = gzip(&vec![b'a'; 20 * 1024 * 1024]);
    assert!(bomb.len() < 100 * 1024);
    Mock::given(path("/bomb"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .insert_header("content-encoding", "gzip")
                .set_body_bytes(bomb),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/bomb");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    assert!(
        matches!(err, FetchError::BodyTooLarge { limit } if limit == 5 * 1024 * 1024),
        "{err:?}"
    );
}

#[tokio::test]
async fn gzip_decoded_and_encoding_recorded() {
    let env = Env::new().await;
    Mock::given(path("/z"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .insert_header("content-encoding", "gzip")
                .set_body_bytes(gzip(b"<html>hello</html>")),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/z");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let ex = env.client.fetch(&claim.permit, page(u)).await.unwrap();
    assert_eq!(&ex.body[..], b"<html>hello</html>");
    assert_eq!(ex.original_encoding.as_deref(), Some("gzip"));
    assert!(ex.response_headers.get("content-encoding").is_none());
    assert!(!ex.truncated);
}

#[tokio::test]
async fn nested_gz_capped() {
    let env = Env::new().await;
    // A .gz dump (application/gzip) that is itself served gzip-encoded: two layers.
    let inner = gzip(&vec![b'x'; 30 * 1024 * 1024]);
    let outer = gzip(&inner);
    Mock::given(path("/dump.xml.gz"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/gzip")
                .insert_header("content-encoding", "gzip")
                .set_body_bytes(outer),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/dump.xml.gz");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    assert!(
        matches!(err, FetchError::BodyTooLarge { limit } if limit == 10 * 1024 * 1024),
        "{err:?}"
    );
}

#[tokio::test]
async fn nested_gz_small_is_kept_as_payload() {
    let env = Env::new().await;
    let inner = gzip(b"<urlset></urlset>");
    Mock::given(path("/sitemap.xml.gz"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/gzip")
                .set_body_bytes(inner.clone()),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/sitemap.xml.gz");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let ex = env.client.fetch(&claim.permit, page(u)).await.unwrap();
    assert_eq!(&ex.body[..], &inner[..], "payload stays the .gz file");
    assert_eq!(ex.original_encoding, None);
}

#[tokio::test]
async fn redirect_cross_key_enqueued() {
    let env = Env::new().await;
    let target = env.url("other.test", "/b");
    Mock::given(path("/a"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", target.as_str()))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(path("/b"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/a");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    let FetchError::CrossKeyRedirect(to) = err else {
        panic!("expected CrossKeyRedirect, got {err:?}");
    };
    assert_eq!(to, target);
    // The caller enqueues the target under its own key.
    assert_eq!(
        env.frontier
            .enqueue(&[NewUrl::new(to.clone())])
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        env.frontier.url_state(&to.key()),
        Some(testkit::UrlState::Due)
    );
    assert_eq!(env.requests_to("/b").await, 0);
}

#[tokio::test]
async fn redirect_same_key_followed() {
    let env = Env::new().await;
    let target = env.url("www.site.test", "/b");
    Mock::given(path("/a"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", target.as_str()))
        .mount(&env.server)
        .await;
    Mock::given(path("/b"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("ok"),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/a");
    let claim = env.claim(&u, "", &["site.test", "www.site.test"]).await;
    let tokens_before = env.frontier.rate_tokens_taken();
    let ex = env
        .client
        .fetch(&claim.permit, page(u.clone()))
        .await
        .unwrap();
    assert_eq!(ex.url, target);
    assert_eq!(ex.redirects.len(), 1);
    assert_eq!(ex.redirects[0].url, u);
    assert_eq!(ex.redirects[0].status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(&ex.body[..], b"ok");
    // One rate token per request, hops included.
    assert_eq!(env.frontier.rate_tokens_taken() - tokens_before, 2);
    assert_eq!(claim.permit.requests_made(), 2);
}

#[tokio::test]
async fn redirect_to_private_ip_blocked() {
    let env = Env::new().await;
    // An IP literal (the metadata server) and a same-key host that resolves into RFC 1918.
    let cases = [
        (
            "/meta",
            "http://169.254.169.254/computeMetadata/v1/".to_string(),
            "metadata",
        ),
        (
            "/int",
            env.url("internal.site.test", "/x").to_string(),
            "rfc1918",
        ),
    ];
    for (from, location, rule) in cases {
        Mock::given(path(from))
            .respond_with(ResponseTemplate::new(302).insert_header("location", location.as_str()))
            .mount(&env.server)
            .await;
        let u = env.url("site.test", from);
        let claim = env
            .claim(&u, "", &["site.test", "internal.site.test"])
            .await;
        let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
        let FetchError::Ssrf(block) = err else {
            panic!("expected Ssrf for {location}, got {err:?}");
        };
        assert_eq!(block.rule, rule);
        env.frontier
            .complete(claim.permit, vc_core::frontier::KeyOutcome::normal())
            .await
            .unwrap();
        env.clock.advance(Duration::from_secs(1));
    }
    // Only the two original requests reached the network.
    assert_eq!(env.server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn first_hop_private_ip_blocked_before_connect() {
    let env = Env::new().await;
    let u = env.url("internal.site.test", "/x");
    let claim = env.claim(&u, "", &["internal.site.test"]).await;
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    assert!(matches!(err, FetchError::Ssrf(_)), "{err:?}");
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn redirect_disallowed_path_stopped() {
    let env = Env::new().await;
    let target = env.url("site.test", "/private/x");
    Mock::given(path("/a"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/private/x"))
        .mount(&env.server)
        .await;
    Mock::given(path("/private/x"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/a");
    let claim = env
        .claim(&u, "User-agent: *\nDisallow: /private\n", &["site.test"])
        .await;
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    assert!(
        matches!(&err, FetchError::RobotsDisallowed(x) if *x == target),
        "{err:?}"
    );
}

#[tokio::test]
async fn too_many_redirects() {
    let env = Env::new().await;
    for i in 0..10 {
        Mock::given(path(format!("/r{i}")))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", format!("/r{}", i + 1)),
            )
            .mount(&env.server)
            .await;
    }
    let u = env.url("site.test", "/r0");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    assert!(matches!(err, FetchError::TooManyRedirects), "{err:?}");
    // The original request plus 5 followed hops; the 6th redirect is not followed.
    assert_eq!(env.server.received_requests().await.unwrap().len(), 6);
}

#[tokio::test]
async fn timeout_bounded_by_deadline() {
    // Lease of 1 s; own timeout 30 s; the server answers after 10 s.
    let env = Env::with(
        Duration::from_secs(1),
        Duration::ZERO,
        FetchConfig::default(),
    )
    .await;
    Mock::given(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/slow");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let t0 = Instant::now();
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    let took = t0.elapsed();
    assert!(matches!(err, FetchError::Timeout), "{err:?}");
    assert!(took < Duration::from_secs(3), "took {took:?}");
    assert!(took >= Duration::from_millis(900), "took {took:?}");
}

#[tokio::test]
async fn deadline_passed_is_not_a_request() {
    let env = Env::with(
        Duration::from_secs(1),
        Duration::ZERO,
        FetchConfig::default(),
    )
    .await;
    let u = env.url("site.test", "/x");
    let claim = env.claim(&u, "", &["site.test"]).await;
    env.clock.advance(Duration::from_secs(1));
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    assert!(
        matches!(err, FetchError::Lease(_) | FetchError::DeadlineExceeded),
        "{err:?}"
    );
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn ua_is_exact_constant() {
    let env = Env::new().await;
    Mock::given(path("/ua"))
        .and(header("user-agent", USER_AGENT))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/html"))
        .expect(1)
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/ua");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let mut req = page(u);
    let mut h = HeaderMap::new();
    h.insert(
        "user-agent",
        HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0)"),
    );
    h.insert("cookie", HeaderValue::from_static("session=1"));
    h.insert("x-api-key", HeaderValue::from_static("k"));
    req.headers = h;
    let ex = env.client.fetch(&claim.permit, req).await.unwrap();
    assert_eq!(ex.status, StatusCode::OK);
    assert_eq!(ex.request_headers.get("user-agent").unwrap(), USER_AGENT);
    let reqs = env.server.received_requests().await.unwrap();
    let r = &reqs[0];
    let uas: Vec<_> = r.headers.get_all("user-agent").iter().collect();
    assert_eq!(uas, vec![USER_AGENT]);
    assert!(r.headers.get("cookie").is_none());
    assert_eq!(r.headers.get("x-api-key").unwrap(), "k");
}

#[tokio::test]
async fn lost_lease_aborts_before_request() {
    let env = Env::new().await;
    Mock::given(path("/a"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/a");
    let claim = env.claim(&u, "", &["site.test"]).await;
    // The lease expires (DB time passes the deadline).
    env.clock.advance(Duration::from_secs(241));
    let tokens = env.frontier.rate_tokens_taken();
    let err = env
        .client
        .fetch(&claim.permit, page(u.clone()))
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::Lease(_)), "{err:?}");
    assert_eq!(env.frontier.rate_tokens_taken(), tokens);
    let err = env.client.stream(&claim.permit, page(u)).await.unwrap_err();
    assert!(matches!(err, FetchError::Lease(_)), "{err:?}");
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn retry_after_parsed() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .to_utc();
    assert_eq!(
        parse_retry_after("120", now),
        Some(Duration::from_secs(120))
    );
    assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
    let later = httpdate::fmt_http_date((now + chrono::Duration::seconds(90)).into());
    assert_eq!(
        parse_retry_after(&later, now),
        Some(Duration::from_secs(90))
    );
    let past = httpdate::fmt_http_date((now - chrono::Duration::seconds(90)).into());
    assert_eq!(parse_retry_after(&past, now), Some(Duration::ZERO));
    assert_eq!(parse_retry_after("soon", now), None);
    assert_eq!(parse_retry_after("-5", now), None);

    // A 429 is an Ok exchange; the caller reads Retry-After from it.
    let env = Env::new().await;
    Mock::given(path("/busy"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/busy");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let ex = env.client.fetch(&claim.permit, page(u)).await.unwrap();
    assert_eq!(ex.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        retry_after(&ex.response_headers, env.clock.now()),
        Some(Duration::from_secs(30))
    );
}

#[tokio::test]
async fn challenge_and_403_detected() {
    let env = Env::new().await;
    Mock::given(path("/forbidden"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&env.server)
        .await;
    Mock::given(path("/unauth"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&env.server)
        .await;
    Mock::given(path("/cf"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("content-type", "text/html")
                .insert_header("cf-mitigated", "challenge"),
        )
        .mount(&env.server)
        .await;
    Mock::given(path("/body"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("<div id=cf-challenge>"),
        )
        .mount(&env.server)
        .await;
    for (p, status) in [
        ("/forbidden", 403),
        ("/unauth", 401),
        ("/cf", 503),
        ("/body", 200),
    ] {
        let u = env.url("site.test", p);
        let claim = env.claim(&u, "", &["site.test"]).await;
        let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
        assert!(
            matches!(err, FetchError::Challenge { status: s } if s == status),
            "{p}: {err:?}"
        );
        env.frontier
            .complete(claim.permit, vc_core::frontier::KeyOutcome::normal())
            .await
            .unwrap();
        env.clock.advance(Duration::from_secs(1));
    }
    // Detect → park: exactly one request each, never retried.
    assert_eq!(env.server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn content_type_not_allowed() {
    let env = Env::new().await;
    Mock::given(path("/img"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(vec![0u8; 16]),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/img");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    assert!(
        matches!(&err, FetchError::ContentTypeNotAllowed(t) if t == "image/png"),
        "{err:?}"
    );
}

#[tokio::test]
async fn error_statuses_are_ok_exchanges() {
    let env = Env::new().await;
    Mock::given(path("/gone"))
        .respond_with(ResponseTemplate::new(404).insert_header("content-type", "image/png"))
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/gone");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let ex = env.client.fetch(&claim.permit, page(u)).await.unwrap();
    assert_eq!(ex.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn robots_missing_fails_closed() {
    let env = Env::new().await;
    let u = env.url("site.test", "/a");
    env.frontier
        .enqueue(&[NewUrl::new(u.clone())])
        .await
        .unwrap();
    let claim = env.frontier.claim(&[0], 1).await.unwrap().unwrap();
    let err = env
        .client
        .fetch(&claim.permit, page(u.clone()))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, FetchError::RobotsUnavailable(o) if *o == u.origin()),
        "{err:?}"
    );
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn robots_purpose_skips_robots_and_truncates() {
    let env = Env::new().await;
    let big = "User-agent: *\n".to_string() + &"# padding\n".repeat(80_000);
    Mock::given(path("/robots.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_string(big),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/a");
    env.frontier
        .enqueue(&[NewUrl::new(u.clone())])
        .await
        .unwrap();
    let claim = env.frontier.claim(&[0], 1).await.unwrap().unwrap();
    let req = FetchRequest::get(u.robots_url(), RequestPurpose::Robots);
    let ex = env.client.fetch(&claim.permit, req).await.unwrap();
    assert_eq!(ex.body.len(), 500 * 1024);
    assert!(ex.truncated);
}

#[tokio::test]
async fn out_of_scope_and_denylisted() {
    let env = Env::new().await;
    let u = env.url("site.test", "/a");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let other = env.url("other.test", "/a");
    let err = env
        .client
        .fetch(&claim.permit, page(other.clone()))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, FetchError::OutOfScope(x) if *x == other),
        "{err:?}"
    );
    env.policy.deny_host("site.test");
    let err = env.client.fetch(&claim.permit, page(u)).await.unwrap_err();
    assert!(matches!(err, FetchError::Denied(_)), "{err:?}");
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn subresource_on_allowlisted_host() {
    let env = Env::new().await;
    Mock::given(path("/app.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/javascript")
                .set_body_string("1"),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/a");
    let claim = env.claim(&u, "", &["site.test", "other.test"]).await;
    let js = env.url("other.test", "/app.js");
    let req = FetchRequest::get(js.clone(), RequestPurpose::SubResource);
    let err = env
        .client
        .fetch(&claim.permit, req.clone())
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::OutOfScope(_)), "{err:?}");
    env.policy.allow_subresource_host("other.test");
    let ex = env.client.fetch(&claim.permit, req).await.unwrap();
    assert_eq!(&ex.body[..], b"1");
}

#[tokio::test]
async fn paced_by_permit_delay() {
    let env = Env::with(
        Duration::from_secs(240),
        Duration::from_millis(400),
        FetchConfig::default(),
    )
    .await;
    Mock::given(path("/p"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/html"))
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/p");
    let claim = env.claim(&u, "", &["site.test"]).await;
    env.client
        .fetch(&claim.permit, page(u.clone()))
        .await
        .unwrap();
    let t0 = Instant::now();
    env.client.fetch(&claim.permit, page(u)).await.unwrap();
    assert!(
        t0.elapsed() >= Duration::from_millis(350),
        "{:?}",
        t0.elapsed()
    );
}

/// Spec §17.1: render sub-requests are spaced by the sub-resource gap (250 ms), not the page
/// delay; the page delay still applies to the next page.
#[tokio::test]
async fn subresources_paced_by_gap_not_page_delay() {
    let delay = Duration::from_secs(3);
    let env = Env::with(Duration::from_secs(240), delay, FetchConfig::default()).await;
    Mock::given(path("/p"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/html"))
        .mount(&env.server)
        .await;
    Mock::given(path("/s.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/javascript")
                .set_body_string("1"),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/p");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let p = &claim.permit;
    env.client.fetch(p, page(u.clone())).await.unwrap();
    let t0 = Instant::now();
    for _ in 0..3 {
        let req = FetchRequest::get(env.url("site.test", "/s.js"), RequestPurpose::SubResource);
        env.client.fetch(p, req).await.unwrap();
    }
    let subs = t0.elapsed();
    assert!(
        subs >= Duration::from_millis(3 * 250 - 30),
        "sub-requests keep the gap: {subs:?}"
    );
    assert!(
        subs < Duration::from_millis(1500),
        "not the page delay: {subs:?}"
    );
    // Every request is counted once.
    assert_eq!(p.requests_made(), 4);
    let t1 = Instant::now();
    env.client.fetch(p, page(u)).await.unwrap();
    assert!(
        t1.elapsed() >= delay - Duration::from_millis(50),
        "the next page still waits for the page delay: {:?}",
        t1.elapsed()
    );
    assert_eq!(p.requests_made(), 5);
}

#[tokio::test]
async fn subresource_gap_is_configurable_and_never_exceeds_delay() {
    let env = Env::with(
        Duration::from_secs(240),
        Duration::from_millis(300),
        FetchConfig::default(),
    )
    .await;
    let client = env
        .client
        .clone()
        .with_subresource_gap(Duration::from_secs(5));
    Mock::given(path("/s.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/javascript")
                .set_body_string("1"),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/s.js");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let sub = || FetchRequest::get(u.clone(), RequestPurpose::SubResource);
    client.fetch(&claim.permit, sub()).await.unwrap();
    let t0 = Instant::now();
    client.fetch(&claim.permit, sub()).await.unwrap();
    let e = t0.elapsed();
    // A gap longer than the page delay is capped at the delay.
    assert!(e >= Duration::from_millis(250), "{e:?}");
    assert!(e < Duration::from_secs(2), "{e:?}");
}

#[tokio::test]
async fn stream_yields_decoded_and_caps() {
    let env = Env::new().await;
    Mock::given(path("/dump"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/csv")
                .insert_header("content-encoding", "gzip")
                .set_body_bytes(gzip(&vec![b'c'; 3 * 1024 * 1024])),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/dump");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let s = env
        .client
        .stream(&claim.permit, page(u.clone()))
        .await
        .unwrap();
    assert_eq!(s.head.status, StatusCode::OK);
    assert_eq!(s.head.original_encoding.as_deref(), Some("gzip"));
    let mut body = s.body;
    let mut n = 0usize;
    while let Some(chunk) = body.next().await {
        n += chunk.unwrap().len();
    }
    assert_eq!(n, 3 * 1024 * 1024);

    let mut req = page(u);
    req.max_body_bytes = 1024 * 1024;
    let s = env.client.stream(&claim.permit, req).await.unwrap();
    let mut body = s.body;
    let mut err = None;
    let mut n = 0usize;
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(c) => n += c.len(),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert!(n <= 1024 * 1024);
    assert!(
        matches!(err, Some(FetchError::BodyTooLarge { limit: 1_048_576 })),
        "{err:?}"
    );
}

#[tokio::test]
async fn stream_sees_extend_after_open() {
    // Lease of 10 s; the stream is opened, then the lease is extended and the clock passes the
    // original deadline: the stream keeps reading (it shares the permit's deadline).
    let env = Env::with(
        Duration::from_secs(10),
        Duration::ZERO,
        FetchConfig::default(),
    )
    .await;
    Mock::given(path("/dump"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/csv")
                .set_body_bytes(vec![b'c'; 64 * 1024]),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/dump");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let s = env
        .client
        .stream(&claim.permit, page(u.clone()))
        .await
        .unwrap();
    env.frontier
        .extend(&claim.permit, Duration::from_secs(60))
        .await
        .unwrap();
    env.clock.advance(Duration::from_secs(30));
    let mut body = s.body;
    let mut n = 0usize;
    while let Some(chunk) = body.next().await {
        n += chunk.expect("extended lease keeps the stream alive").len();
    }
    assert_eq!(n, 64 * 1024);

    // Without an extension, passing the deadline still fails closed.
    let s = env.client.stream(&claim.permit, page(u)).await.unwrap();
    env.clock.advance(Duration::from_secs(41));
    let mut body = s.body;
    let first = body.next().await.expect("an item");
    assert!(matches!(first, Err(FetchError::Lease(_))), "{first:?}");
    assert!(body.next().await.is_none());
}

#[tokio::test]
async fn stream_records_body_end_time() {
    let env = Env::new().await;
    Mock::given(path("/dump"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/csv")
                .set_body_bytes(vec![b'c'; 16 * 1024]),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/dump");
    let claim = env.claim(&u, "", &["site.test"]).await;
    let s = env.client.stream(&claim.permit, page(u)).await.unwrap();
    let opened = claim.permit.last_request_end().expect("head recorded");
    let requests = claim.permit.requests_made();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut body = s.body;
    while let Some(chunk) = body.next().await {
        chunk.unwrap();
    }
    let ended = claim.permit.last_request_end().expect("body end recorded");
    assert!(
        ended >= opened + Duration::from_millis(50),
        "body end {ended:?} vs head {opened:?}"
    );
    assert_eq!(claim.permit.requests_made(), requests, "no extra request");
}

// ---- Final-review fixes: credentials across authorities, mixed public/private DNS ----

fn with_auth(mut req: FetchRequest) -> FetchRequest {
    req.headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer s3cret"),
    );
    req
}

#[tokio::test]
async fn authorization_dropped_on_port_change() {
    let env = Env::new().await;
    let other = MockServer::start().await;
    let target =
        CanonicalUrl::parse(&format!("http://site.test:{}/b", other.address().port())).unwrap();
    Mock::given(path("/a"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", target.as_str()))
        .mount(&env.server)
        .await;
    Mock::given(path("/b"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string("{}"),
        )
        .mount(&other)
        .await;
    let u = env.url("site.test", "/a");
    let claim = env.claim(&u, "", &["site.test"]).await;
    env.frontier
        .store_robots(
            &claim.permit,
            robots_ok(target.origin(), "", env.clock.now()),
        )
        .await
        .unwrap();
    let ex = env
        .client
        .fetch(
            &claim.permit,
            with_auth(FetchRequest::get(u, RequestPurpose::Api)),
        )
        .await
        .unwrap();
    assert_eq!(ex.url, target);
    let first = env.server.received_requests().await.unwrap();
    assert_eq!(
        first[0].headers.get("authorization").unwrap(),
        "Bearer s3cret"
    );
    let second = other.received_requests().await.unwrap();
    assert_eq!(second.len(), 1);
    assert!(
        second[0].headers.get("authorization").is_none(),
        "credentials leaked to another port"
    );
    assert!(ex.request_headers.get("authorization").is_none());
}

#[tokio::test]
async fn authorization_kept_on_same_origin_redirect() {
    let env = Env::new().await;
    let target = env.url("site.test", "/b");
    Mock::given(path("/a"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", target.as_str()))
        .mount(&env.server)
        .await;
    Mock::given(path("/b"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string("{}"),
        )
        .mount(&env.server)
        .await;
    let u = env.url("site.test", "/a");
    let claim = env.claim(&u, "", &["site.test"]).await;
    env.client
        .fetch(
            &claim.permit,
            with_auth(FetchRequest::get(u, RequestPurpose::Api)),
        )
        .await
        .unwrap();
    let reqs = env.server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    for r in &reqs {
        assert_eq!(r.headers.get("authorization").unwrap(), "Bearer s3cret");
    }
}

#[tokio::test]
async fn host_with_public_and_private_addresses_refused() {
    // The first address is allowed (wiremock on loopback), the second is RFC 1918: the host
    // is refused before any connection, whichever address would have been pinned.
    let server = MockServer::start().await;
    let clock = FakeClock::default();
    let frontier = Arc::new(MemFrontier::new(Arc::new(clock.clone())));
    let policy = Arc::new(StubPolicy::loopback_ok());
    let resolver = StaticResolver::new()
        .with("mixed.test", IpAddr::from(LOOPBACK))
        .with("mixed.test", IpAddr::from([10, 0, 0, 5]));
    let client = ReqwestPermitClient::new(FetchConfig::default(), frontier.clone(), policy)
        .with_clock(Arc::new(clock.clone()))
        .with_resolver(Arc::new(resolver));
    Mock::given(path("/x"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let u =
        CanonicalUrl::parse(&format!("http://mixed.test:{}/x", server.address().port())).unwrap();
    frontier.enqueue(&[NewUrl::new(u.clone())]).await.unwrap();
    let claim = frontier.claim(&[0], 1).await.unwrap().unwrap();
    frontier
        .store_robots(&claim.permit, robots_ok(u.origin(), "", clock.now()))
        .await
        .unwrap();
    let err = client.fetch(&claim.permit, page(u)).await.unwrap_err();
    let FetchError::Ssrf(block) = err else {
        panic!("expected Ssrf, got {err:?}");
    };
    assert_eq!(block.ip, IpAddr::from([10, 0, 0, 5]));
    assert!(server.received_requests().await.unwrap().is_empty());
}
