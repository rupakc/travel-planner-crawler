//! Integration tests against the pinned `chromedp/headless-shell` container (same digest as
//! `deploy/compose.yaml`, started with `deploy/chromium/run.sh`, sandbox on). Chromium gets no
//! network: every request is paused and answered from the fake web through the renderer.
//!
//! `#[ignore]`d unless `cfg(vc_render_it)` (set by build.rs when `CI` or `VC_RENDER_IT` is set):
//! `VC_RENDER_IT=1 cargo test -p vc-render --test chromium`.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use support::Env;
use vc_config::RenderConfig;
use vc_core::fetch::{FetchError, RequestPurpose};
use vc_core::policy::SsrfBlock;
use vc_core::render::{BlockReason, RenderBudget, RenderOutcome, Renderer};
use vc_core::{CanonicalUrl, USER_AGENT};
use vc_render::ChromiumBrowser;

const PAGE: &str = "https://spa.example.com/";

const HTML: &str = r#"<!doctype html>
<html><head><title>SPA</title>
<link rel="stylesheet" href="/style.css">
<script src="/app.js"></script>
<script src="https://cdn.allowed.test/lib.js"></script>
<script src="https://evil.test/x.js"></script>
<script src="/private/secret.js"></script>
<script src="https://internal.spa.example.com/i.js"></script>
</head><body><img src="/logo.png"><ul id="list"></ul><div id="ua"></div><div id="cdn"></div>
<div id="evil"></div><div id="secret"></div></body></html>"#;

const APP_JS: &str = r#"
document.addEventListener('DOMContentLoaded', () => {
  document.getElementById('ua').textContent = navigator.userAgent;
  if (window.cdnLoaded) document.getElementById('cdn').textContent = 'cdn-ok';
  fetch('/api/items.json').then(r => r.json()).then(items => {
    for (const i of items) {
      const li = document.createElement('li');
      li.textContent = i;
      document.getElementById('list').appendChild(li);
    }
  });
  fetch('https://10.0.0.5/secret').catch(() => {});
  fetch('http://169.254.169.254/latest/meta-data/').catch(() => {});
});
"#;

fn u(s: &str) -> CanonicalUrl {
    CanonicalUrl::parse(s).unwrap()
}

fn site(env: &Env) {
    env.frontier
        .robots_txt(PAGE, "User-agent: *\nDisallow: /private\n");
    env.frontier.allow_all("https://cdn.allowed.test/");
    env.frontier.allow_all("https://evil.test/");
    env.frontier.allow_all("https://internal.spa.example.com/");
    env.web.page(PAGE, "text/html; charset=utf-8", HTML);
    env.web.page(
        "https://spa.example.com/style.css",
        "text/css",
        "body{color:#111}",
    );
    env.web.page(
        "https://spa.example.com/app.js",
        "application/javascript",
        APP_JS,
    );
    env.web.page(
        "https://cdn.allowed.test/lib.js",
        "application/javascript",
        "window.cdnLoaded = true;",
    );
    env.web.page(
        "https://evil.test/x.js",
        "application/javascript",
        "document.addEventListener('DOMContentLoaded',()=>{document.getElementById('evil').textContent='EVIL'});",
    );
    env.web.page(
        "https://spa.example.com/private/secret.js",
        "application/javascript",
        "document.addEventListener('DOMContentLoaded',()=>{document.getElementById('secret').textContent='SECRET'});",
    );
    env.web.page(
        "https://spa.example.com/api/items.json",
        "application/json",
        r#"["Louvre","Orsay"]"#,
    );
    env.web
        .page("https://spa.example.com/logo.png", "image/png", "png");
    env.web.error(
        "https://internal.spa.example.com/i.js",
        FetchError::Ssrf(SsrfBlock {
            host: "internal.spa.example.com".into(),
            ip: "10.1.2.3".parse().unwrap(),
            port: 443,
            rule: "rfc1918".into(),
        }),
    );
}

fn budget() -> RenderBudget {
    RenderBudget {
        total: Duration::from_secs(30),
        network_idle: Duration::from_millis(500),
        subresource_gap: Duration::from_millis(10),
        ..RenderBudget::default()
    }
}

async fn render(env: &Env, budget: RenderBudget) -> RenderOutcome {
    let browser = Arc::new(ChromiumBrowser::new(
        testkit::vc_render_fixtures::chromium_cdp_url(),
        1000,
        Duration::from_secs(60),
    ));
    let r = env.renderer(RenderConfig::default(), browser);
    let p = env.frontier.permit(PAGE).await;
    r.render(&p, &u(PAGE), budget).await.expect("render")
}

fn reason(out: &RenderOutcome, url: &str) -> Option<BlockReason> {
    out.blocked
        .iter()
        .find(|(b, _)| *b == u(url))
        .map(|(_, r)| r.clone())
}

fn dom(out: &RenderOutcome) -> String {
    String::from_utf8_lossy(&out.dom_html).to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(vc_render_it),
    ignore = "needs Docker: VC_RENDER_IT=1 (CI sets CI)"
)]
async fn spa_page_rendered() {
    let env = Env::new();
    site(&env);
    let out = render(&env, budget()).await;
    let d = dom(&out);
    assert!(d.contains("<li>Louvre</li><li>Orsay</li>"), "{d}");
    // The one UA, also inside Chromium.
    assert!(d.contains(USER_AGENT), "{d}");
    assert!(d.contains("cdn-ok"), "{d}");
    assert_eq!(out.exchanges[0].url, u(PAGE));
    assert_eq!(out.exchanges[0].purpose, RequestPurpose::Page);
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(vc_render_it),
    ignore = "needs Docker: VC_RENDER_IT=1 (CI sets CI)"
)]
async fn subrequest_to_private_ip_blocked() {
    let env = Env::new();
    site(&env);
    let out = render(&env, budget()).await;
    assert_eq!(
        reason(&out, "https://internal.spa.example.com/i.js"),
        Some(BlockReason::Ssrf)
    );
    // Chromium may refuse private-network requests itself; either way nothing reaches the
    // network, and if it paused them they were failed by the SSRF guard.
    for ip in [
        "https://10.0.0.5/secret",
        "http://169.254.169.254/latest/meta-data/",
    ] {
        if let Some(r) = reason(&out, ip) {
            assert_eq!(r, BlockReason::Ssrf, "{ip}");
        }
    }
    assert!(
        env.web
            .urls()
            .iter()
            .all(|x| !x.contains("10.0.0.5") && !x.contains("169.254"))
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(vc_render_it),
    ignore = "needs Docker: VC_RENDER_IT=1 (CI sets CI)"
)]
async fn disallowed_subresource_failed() {
    let env = Env::new();
    site(&env);
    let out = render(&env, budget()).await;
    assert_eq!(
        reason(&out, "https://spa.example.com/private/secret.js"),
        Some(BlockReason::RobotsDisallowed)
    );
    assert_eq!(
        env.web.count("https://spa.example.com/private/secret.js"),
        0
    );
    assert!(!dom(&out).contains("SECRET"));
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(vc_render_it),
    ignore = "needs Docker: VC_RENDER_IT=1 (CI sets CI)"
)]
async fn third_party_not_allowlisted_failed() {
    let env = Env::new();
    site(&env);
    let out = render(&env, budget()).await;
    assert_eq!(
        reason(&out, "https://evil.test/x.js"),
        Some(BlockReason::OutOfScope)
    );
    assert_eq!(env.web.count("https://evil.test/x.js"), 0);
    assert!(!dom(&out).contains("EVIL"));
    // The allowlisted CDN was fetched (under the page's permit) and ran.
    assert_eq!(env.web.count("https://cdn.allowed.test/lib.js"), 1);
    assert!(dom(&out).contains("cdn-ok"));
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(vc_render_it),
    ignore = "needs Docker: VC_RENDER_IT=1 (CI sets CI)"
)]
async fn render_budget_enforced() {
    let env = Env::new();
    env.frontier.allow_all(PAGE);
    env.web.page(
        PAGE,
        "text/html",
        "<!doctype html><html><body><div id=n>0</div><script>\
         let n = 0; setInterval(() => fetch('/api/poll?n=' + (n++)).then(() => {\
         document.getElementById('n').textContent = String(n); }), 50);\
         </script></body></html>",
    );
    for i in 0..400 {
        env.web.page(
            &format!("https://spa.example.com/api/poll?n={i}"),
            "application/json",
            "{}",
        );
    }
    let budget = RenderBudget {
        total: Duration::from_secs(4),
        max_subrequests: 5,
        ..budget()
    };
    let total = budget.total;
    let t0 = Instant::now();
    let out = render(&env, budget).await;
    let elapsed = t0.elapsed();
    // Liveness bound only (catches a render that ignores its budget and hangs). It includes
    // the CDP connect and context set-up, which take several seconds on a loaded CI runner;
    // the budget itself is asserted below via the sub-request cap and the Budget block.
    assert!(elapsed < total + Duration::from_secs(26), "{elapsed:?}");
    let sub = env
        .web
        .requests()
        .iter()
        .filter(|l| l.purpose == RequestPurpose::SubResource)
        .count();
    assert_eq!(sub, 5);
    assert!(
        out.blocked.iter().any(|(_, r)| *r == BlockReason::Budget),
        "{:?}",
        out.blocked
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(vc_render_it),
    ignore = "needs Docker: VC_RENDER_IT=1 (CI sets CI)"
)]
async fn all_requests_recorded_as_exchanges() {
    let env = Env::new();
    site(&env);
    let out = render(&env, budget()).await;
    // The SSRF-refused host never got a request (vc-fetch refuses it on the pinned IP before
    // connecting); the fake web logs it only because it answers with that error.
    let mut fetched: Vec<String> = env
        .web
        .urls()
        .into_iter()
        .filter(|x| !x.contains("internal.spa.example.com"))
        .collect();
    let mut recorded: Vec<String> = out
        .exchanges
        .iter()
        .map(|e| e.url.as_str().to_string())
        .collect();
    fetched.sort();
    recorded.sort();
    assert_eq!(fetched, recorded);
    for want in [
        PAGE,
        "https://spa.example.com/style.css",
        "https://spa.example.com/app.js",
        "https://cdn.allowed.test/lib.js",
        "https://spa.example.com/api/items.json",
    ] {
        assert!(recorded.contains(&want.to_string()), "{want}: {recorded:?}");
    }
    assert_eq!(
        reason(&out, "https://spa.example.com/logo.png"),
        Some(BlockReason::ResourceType)
    );
    for e in &out.exchanges {
        assert_eq!(
            e.request_headers.get(http::header::USER_AGENT).unwrap(),
            USER_AGENT
        );
    }
}

/// The fixture's image is the one `deploy/compose.yaml` runs.
#[test]
fn fixture_image_matches_compose() {
    let compose = include_str!("../../../deploy/compose.yaml");
    let pinned = format!(
        "{}:{}",
        testkit::vc_render_fixtures::CHROMIUM_IMAGE,
        testkit::vc_render_fixtures::CHROMIUM_TAG
    );
    let line = compose
        .lines()
        .find(|l| l.trim_start().starts_with("image: chromedp/headless-shell"))
        .expect("chromium service in deploy/compose.yaml");
    assert_eq!(line.trim().trim_start_matches("image:").trim(), pinned);
    assert!(pinned.contains("@sha256:"), "pinned by digest");
}
