//! The permit client with the production `VoyagerPolicy` (real policy files, real SSRF rules):
//! private, loopback, metadata and mixed resolutions are refused before any connection.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use testkit::vc_fetch_fixtures::robots_ok;
use testkit::vc_policy_fixtures as pfx;
use testkit::{FakeClock, MemFrontier};
use vc_config::{FetchConfig, PolicyConfig};
use vc_core::fetch::{FetchError, FetchRequest, PermitClient, RequestPurpose};
use vc_core::frontier::{Frontier, NewUrl};
use vc_core::{CanonicalUrl, Clock};
use vc_fetch::{ReqwestPermitClient, StaticResolver};
use vc_policy::VoyagerPolicy;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[tokio::test]
async fn voyager_policy_refuses_private_resolutions_end_to_end() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let port = server.address().port();
    // The mock's port is allowed, so only the address rules can refuse these.
    let policy = VoyagerPolicy::new(PolicyConfig {
        denylist_path: pfx::denylist_path().display().to_string(),
        subresource_allowlist_path: pfx::subresource_allowlist_path().display().to_string(),
        allowed_ports: vec![80, 443, port],
        ..PolicyConfig::default()
    })
    .unwrap();
    let resolver = StaticResolver::new()
        .with("loop.test", ip("127.0.0.1"))
        .with("lan.test", ip("10.0.0.5"))
        .with("meta.test", ip("169.254.169.254"))
        .with("cgnat.test", ip("100.64.0.1"))
        .with("v6loop.test", ip("::1"))
        .with("mapped.test", ip("::ffff:127.0.0.1"))
        .with("mixed.test", ip("93.184.216.34"))
        .with("mixed.test", ip("127.0.0.1"))
        .with("metadata.google.internal", ip("93.184.216.34"));
    let policy = Arc::new(policy);
    let cases = [
        ("loop.test", "loopback"),
        ("lan.test", "rfc1918"),
        ("meta.test", "metadata"),
        ("cgnat.test", "cgnat"),
        ("v6loop.test", "loopback"),
        ("mapped.test", "ipv4_mapped"),
        ("mixed.test", "loopback"),
        ("127.0.0.1", "loopback"),
        ("metadata.google.internal", "metadata"),
    ];
    for (host, rule) in cases {
        // A fresh frontier per host, so each claim is that host's key.
        let clock = FakeClock::default();
        let frontier = Arc::new(MemFrontier::new(Arc::new(clock.clone())));
        let client =
            ReqwestPermitClient::new(FetchConfig::default(), frontier.clone(), policy.clone())
                .with_clock(Arc::new(clock.clone()))
                .with_resolver(Arc::new(resolver.clone()));
        let u = CanonicalUrl::parse(&format!("http://{host}:{port}/x")).unwrap();
        frontier.enqueue(&[NewUrl::new(u.clone())]).await.unwrap();
        let claim = frontier.claim(&[0], 1).await.unwrap().expect("a claim");
        frontier
            .store_robots(&claim.permit, robots_ok(u.origin(), "", clock.now()))
            .await
            .unwrap();
        let mut req = FetchRequest::get(u, RequestPurpose::Page);
        req.timeout = Some(Duration::from_secs(2));
        let err = client.fetch(&claim.permit, req).await.unwrap_err();
        let FetchError::Ssrf(block) = err else {
            panic!("{host}: expected Ssrf, got {err:?}");
        };
        assert_eq!(block.rule, rule, "{host}");
    }
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a refused host reached the network"
    );
}

#[tokio::test]
async fn voyager_policy_refuses_disallowed_port() {
    let policy = VoyagerPolicy::new(PolicyConfig {
        denylist_path: pfx::denylist_path().display().to_string(),
        subresource_allowlist_path: pfx::subresource_allowlist_path().display().to_string(),
        ..PolicyConfig::default()
    })
    .unwrap();
    let resolver = StaticResolver::new().with("public.test", ip("93.184.216.34"));
    let clock = FakeClock::default();
    let frontier = Arc::new(MemFrontier::new(Arc::new(clock.clone())));
    let client =
        ReqwestPermitClient::new(FetchConfig::default(), frontier.clone(), Arc::new(policy))
            .with_clock(Arc::new(clock.clone()))
            .with_resolver(Arc::new(resolver));
    let u = CanonicalUrl::parse("http://public.test:6379/").unwrap();
    frontier.enqueue(&[NewUrl::new(u.clone())]).await.unwrap();
    let claim = frontier.claim(&[0], 1).await.unwrap().expect("a claim");
    frontier
        .store_robots(&claim.permit, robots_ok(u.origin(), "", clock.now()))
        .await
        .unwrap();
    let err = client
        .fetch(&claim.permit, FetchRequest::get(u, RequestPurpose::Page))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, FetchError::Ssrf(b) if b.port == 6379),
        "{err:?}"
    );
}
