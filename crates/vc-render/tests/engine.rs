//! Decision logic of the renderer, driven through a mock CDP (no browser, no Docker).

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use support::{Env, MockBrowser, Script, fast_budget, req};
use vc_config::RenderConfig;
use vc_core::fetch::{FetchError, RequestPurpose};
use vc_core::frontier::Frontier;
use vc_core::policy::SsrfBlock;
use vc_core::render::{BlockReason, RenderBudget, Renderer};
use vc_core::{CanonicalUrl, Error, USER_AGENT};
use vc_render::cdp::ResourceKind as K;

const PAGE: &str = "https://spa.example.com/";

fn u(s: &str) -> CanonicalUrl {
    CanonicalUrl::parse(s).unwrap()
}

fn blocked_reason(out: &vc_core::render::RenderOutcome, url: &str) -> Option<BlockReason> {
    out.blocked
        .iter()
        .find(|(b, _)| *b == u(url))
        .map(|(_, r)| r.clone())
}

/// A page whose app.js loads /api/items.json and renders it.
fn spa(env: &Env) -> Script {
    env.frontier.allow_all(PAGE);
    env.web.page(
        PAGE,
        "text/html",
        "<html><body><div id=app></div></body></html>",
    );
    env.web.page(
        "https://spa.example.com/app.js",
        "application/javascript",
        "fetch('/api/items.json')",
    );
    env.web.page(
        "https://spa.example.com/api/items.json",
        "application/json",
        r#"["Louvre","Orsay"]"#,
    );
    Script::default()
        .load(PAGE, vec![req("https://spa.example.com/app.js", K::Script)])
        .load(
            "https://spa.example.com/app.js",
            vec![req("https://spa.example.com/api/items.json", K::Xhr)],
        )
}

async fn render_with(
    env: &Env,
    script: Script,
    budget: RenderBudget,
) -> (
    Result<vc_core::render::RenderOutcome, Error>,
    Arc<MockBrowser>,
) {
    let browser = MockBrowser::new(script);
    let r = env.renderer(RenderConfig::default(), browser.clone());
    let p = env.frontier.permit(PAGE).await;
    let out = r.render(&p, &u(PAGE), budget).await;
    (out, browser)
}

#[tokio::test]
async fn spa_page_rendered() {
    let env = Env::new();
    let script = spa(&env);
    let (out, browser) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    let dom = String::from_utf8_lossy(&out.dom_html);
    assert!(dom.contains("id=app"), "{dom}");
    assert!(dom.contains("Louvre"), "{dom}");
    assert_eq!(out.exchanges.len(), 3);
    assert_eq!(out.exchanges[0].url, u(PAGE));
    assert_eq!(out.exchanges[0].purpose, RequestPurpose::Page);
    assert!(
        out.exchanges[1..]
            .iter()
            .all(|e| e.purpose == RequestPurpose::SubResource)
    );
    assert!(out.blocked.is_empty(), "{:?}", out.blocked);
    let log = browser.log();
    assert_eq!(log.tabs_opened, 1);
    assert_eq!(log.tabs_closed, 1);
}

#[tokio::test]
async fn subrequest_to_private_ip_blocked() {
    let env = Env::new();
    let script = spa(&env).load(
        PAGE,
        vec![
            req("https://spa.example.com/app.js", K::Script),
            req("http://10.0.0.5/secret.js", K::Script),
            req("http://169.254.169.254/latest/meta-data", K::Xhr),
            req("https://internal.spa.example.com/x.js", K::Script),
        ],
    );
    // A same-key host that resolves to a private IP: vc-fetch's pinned-IP check refuses it.
    env.web.error(
        "https://internal.spa.example.com/x.js",
        FetchError::Ssrf(SsrfBlock {
            host: "internal.spa.example.com".into(),
            ip: "10.1.2.3".parse().unwrap(),
            port: 443,
            rule: "rfc1918".into(),
        }),
    );
    env.frontier.allow_all("https://internal.spa.example.com/");
    let (out, _) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    assert_eq!(
        blocked_reason(&out, "http://10.0.0.5/secret.js"),
        Some(BlockReason::Ssrf)
    );
    assert_eq!(
        blocked_reason(&out, "http://169.254.169.254/latest/meta-data"),
        Some(BlockReason::Ssrf)
    );
    assert_eq!(
        blocked_reason(&out, "https://internal.spa.example.com/x.js"),
        Some(BlockReason::Ssrf)
    );
    // IP literals never reach the network layer at all.
    assert!(
        env.web
            .urls()
            .iter()
            .all(|x| !x.contains("10.0.0.5") && !x.contains("169.254"))
    );
    // The page itself still rendered.
    assert!(String::from_utf8_lossy(&out.dom_html).contains("Louvre"));
}

#[tokio::test]
async fn disallowed_subresource_failed() {
    let env = Env::new();
    let script = spa(&env).load(
        "https://spa.example.com/app.js",
        vec![
            req("https://spa.example.com/api/items.json", K::Xhr),
            req("https://spa.example.com/private/data.json", K::Xhr),
        ],
    );
    env.frontier
        .robots_txt(PAGE, "User-agent: *\nDisallow: /private\n");
    env.web.page(
        "https://spa.example.com/private/data.json",
        "application/json",
        "{}",
    );
    let (out, _) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    assert_eq!(
        blocked_reason(&out, "https://spa.example.com/private/data.json"),
        Some(BlockReason::RobotsDisallowed)
    );
    assert_eq!(
        env.web.count("https://spa.example.com/private/data.json"),
        0
    );
}

#[tokio::test]
async fn third_party_not_allowlisted_failed() {
    let env = Env::new();
    let script = spa(&env).load(
        PAGE,
        vec![
            req("https://spa.example.com/app.js", K::Script),
            req("https://evil.test/x.js", K::Script),
            req("https://cdn.allowed.test/lib.js", K::Script),
        ],
    );
    env.frontier.allow_all("https://evil.test/");
    env.frontier.allow_all("https://cdn.allowed.test/");
    env.web
        .page("https://evil.test/x.js", "application/javascript", "1");
    env.web.page(
        "https://cdn.allowed.test/lib.js",
        "application/javascript",
        "2",
    );
    let (out, _) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    assert_eq!(
        blocked_reason(&out, "https://evil.test/x.js"),
        Some(BlockReason::OutOfScope)
    );
    assert_eq!(env.web.count("https://evil.test/x.js"), 0);
    assert_eq!(env.web.count("https://cdn.allowed.test/lib.js"), 1);
    assert!(
        out.exchanges
            .iter()
            .any(|e| e.url == u("https://cdn.allowed.test/lib.js"))
    );
}

#[tokio::test]
async fn render_budget_enforced() {
    // Count budget: five scripts, three allowed.
    let env = Env::new();
    let scripts: Vec<String> = (1..=5)
        .map(|i| format!("https://spa.example.com/s{i}.js"))
        .collect();
    for s in &scripts {
        env.web.page(s, "application/javascript", "x");
    }
    let script = spa(&env).load(PAGE, scripts.iter().map(|s| req(s, K::Script)).collect());
    let budget = RenderBudget {
        max_subrequests: 3,
        ..fast_budget()
    };
    let (out, _) = render_with(&env, script, budget).await;
    let out = out.unwrap();
    let over: Vec<_> = out
        .blocked
        .iter()
        .filter(|(_, r)| *r == BlockReason::Budget)
        .collect();
    assert_eq!(over.len(), 2, "{:?}", out.blocked);
    assert_eq!(
        env.web
            .requests()
            .iter()
            .filter(|l| l.purpose == RequestPurpose::SubResource)
            .count(),
        3
    );

    // Time budget: a page that polls forever never goes idle; the render still ends on time.
    let env = Env::new();
    let mut script = spa(&env).load(PAGE, vec![req("https://spa.example.com/api/poll", K::Xhr)]);
    script.poll.insert(
        "https://spa.example.com/api/poll".into(),
        Duration::from_millis(20),
    );
    env.web
        .page("https://spa.example.com/api/poll", "application/json", "{}");
    let budget = RenderBudget {
        total: Duration::from_secs(2),
        max_subrequests: 1000,
        // Much longer than the polling interval, even on a loaded machine.
        network_idle: Duration::from_millis(500),
        ..fast_budget()
    };
    let t0 = Instant::now();
    let (out, browser) = render_with(&env, script, budget).await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1900),
        "{elapsed:?} {} {:?}",
        out.as_ref().map(|o| o.exchanges.len()).unwrap_or(0),
        out.as_ref().map(|o| o.blocked.clone())
    );
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    let out = out.unwrap();
    assert!(!out.exchanges.is_empty());
    assert_eq!(browser.log().tabs_closed, 1);

    // Bytes budget: the remaining byte allowance is passed down as the body cap.
    let env = Env::new();
    let script = spa(&env);
    let budget = RenderBudget {
        max_bytes: 1000,
        ..fast_budget()
    };
    let (out, _) = render_with(&env, script, budget).await;
    out.unwrap();
    let caps: Vec<u64> = env
        .web
        .requests()
        .iter()
        .filter(|l| l.purpose == RequestPurpose::SubResource)
        .map(|l| l.max_body_bytes)
        .collect();
    assert_eq!(caps.len(), 2);
    assert!(caps.iter().all(|c| *c > 0 && *c <= 1000), "{caps:?}");
    assert!(caps[1] < caps[0], "{caps:?}");
}

#[tokio::test]
async fn bytes_budget_exhausted_blocks() {
    let env = Env::new();
    env.web.page(
        "https://spa.example.com/big.js",
        "application/javascript",
        &"x".repeat(600),
    );
    env.web.page(
        "https://spa.example.com/big2.js",
        "application/javascript",
        &"y".repeat(600),
    );
    let script = spa(&env).load(
        PAGE,
        vec![
            req("https://spa.example.com/big.js", K::Script),
            req("https://spa.example.com/big2.js", K::Script),
        ],
    );
    let budget = RenderBudget {
        max_bytes: 1000,
        ..fast_budget()
    };
    let (out, _) = render_with(&env, script, budget).await;
    let out = out.unwrap();
    assert_eq!(
        blocked_reason(&out, "https://spa.example.com/big2.js"),
        Some(BlockReason::Budget)
    );
}

#[tokio::test]
async fn all_requests_recorded_as_exchanges() {
    let env = Env::new();
    let script = spa(&env).load(
        PAGE,
        vec![
            req("https://spa.example.com/app.js", K::Script),
            req("https://spa.example.com/style.css", K::Stylesheet),
            req("https://spa.example.com/missing.js", K::Script),
            req("https://spa.example.com/logo.png", K::Image),
        ],
    );
    env.web
        .page("https://spa.example.com/style.css", "text/css", "body{}");
    let (out, browser) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    // Every network request is an exchange, and every exchange was a network request.
    let mut fetched = env.web.urls();
    let mut recorded: Vec<String> = out
        .exchanges
        .iter()
        .map(|e| e.url.as_str().to_string())
        .collect();
    fetched.sort();
    recorded.sort();
    assert_eq!(fetched, recorded);
    // Every paused request was answered: fulfilled or failed.
    let log = browser.log();
    assert_eq!(log.paused.len(), log.fulfilled.len() + log.failed.len());
    assert_eq!(log.failed.len(), out.blocked.len());
    // A 404 is an exchange, fulfilled as a 404.
    assert!(
        log.fulfilled
            .contains(&("https://spa.example.com/missing.js".into(), 404))
    );
    // Exchanges carry the one UA.
    for e in &out.exchanges {
        assert_eq!(
            e.request_headers.get(http::header::USER_AGENT).unwrap(),
            USER_AGENT
        );
    }
}

#[tokio::test]
async fn blocked_resource_types_failed() {
    let env = Env::new();
    let kinds: [(&str, K); 4] = [
        ("https://spa.example.com/a.png", K::Image),
        ("https://spa.example.com/a.woff2", K::Font),
        ("https://spa.example.com/a.mp4", K::Media),
        ("https://spa.example.com/ping", K::Other),
    ];
    let script = spa(&env).load(PAGE, kinds.iter().map(|(x, k)| req(x, *k)).collect());
    let (out, _) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    for (x, _) in kinds {
        assert_eq!(
            blocked_reason(&out, x),
            Some(BlockReason::ResourceType),
            "{x}"
        );
        assert_eq!(env.web.count(x), 0);
    }
}

#[tokio::test]
async fn trackers_and_denylist_failed() {
    let env = Env::new();
    let script = spa(&env).load(
        PAGE,
        vec![
            req("https://www.googletagmanager.com/gtm.js", K::Script),
            req("https://denied.example.org/x.js", K::Script),
        ],
    );
    let (out, _) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    assert_eq!(
        blocked_reason(&out, "https://www.googletagmanager.com/gtm.js"),
        Some(BlockReason::Tracker)
    );
    assert_eq!(
        blocked_reason(&out, "https://denied.example.org/x.js"),
        Some(BlockReason::Denylisted)
    );
    assert_eq!(env.web.urls().len(), 1);
}

#[tokio::test]
async fn non_get_failed() {
    let env = Env::new();
    let mut post = req("https://spa.example.com/api/track", K::Xhr);
    post.method = "POST".into();
    let script = spa(&env).load(PAGE, vec![post]);
    let (out, _) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    assert_eq!(
        blocked_reason(&out, "https://spa.example.com/api/track"),
        Some(BlockReason::Method)
    );
}

#[tokio::test]
async fn missing_robots_fails_and_enqueues_refresh() {
    let env = Env::new();
    let script = spa(&env).load(
        PAGE,
        vec![
            req("https://cdn.allowed.test/a.js", K::Script),
            req("https://cdn.allowed.test/b.js", K::Script),
            req("https://spa.example.com/app.js", K::Script),
        ],
    );
    let (out, _) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    assert_eq!(
        blocked_reason(&out, "https://cdn.allowed.test/a.js"),
        Some(BlockReason::RobotsUnavailable)
    );
    assert_eq!(
        blocked_reason(&out, "https://cdn.allowed.test/b.js"),
        Some(BlockReason::RobotsUnavailable)
    );
    assert_eq!(env.web.count("https://cdn.allowed.test/a.js"), 0);
    // One refresh per origin; the renderer itself never fetches robots.txt.
    assert_eq!(
        env.frontier.enqueued(),
        vec!["https://cdn.allowed.test/robots.txt".to_string()]
    );
    assert!(env.web.urls().iter().all(|x| !x.ends_with("robots.txt")));

    // Stale counts as missing.
    let env = Env::new();
    let script = spa(&env).load(PAGE, vec![req("https://cdn.allowed.test/a.js", K::Script)]);
    env.frontier.stale_robots("https://cdn.allowed.test/");
    let (out, _) = render_with(&env, script, fast_budget()).await;
    assert_eq!(
        blocked_reason(&out.unwrap(), "https://cdn.allowed.test/a.js"),
        Some(BlockReason::RobotsUnavailable)
    );
}

#[tokio::test]
async fn subresource_cache_reused_within_ttl_and_expires() {
    let env = Env::new();
    let script = spa(&env);
    let browser = MockBrowser::new(script);
    let r = env.renderer(RenderConfig::default(), browser.clone());
    let lib = "https://spa.example.com/app.js";

    let p = env.frontier.permit(PAGE).await;
    let first = r.render(&p, &u(PAGE), fast_budget()).await.unwrap();
    assert_eq!(env.web.count(lib), 1);
    env.frontier
        .inner
        .complete(p, vc_core::frontier::KeyOutcome::normal())
        .await
        .unwrap();

    // One hour later: the script comes from the cache, the XHR is fetched again.
    env.clock.advance(Duration::from_secs(3600));
    let p = env.frontier.permit(PAGE).await;
    let second = r.render(&p, &u(PAGE), fast_budget()).await.unwrap();
    assert_eq!(env.web.count(lib), 1);
    assert_eq!(env.web.count("https://spa.example.com/api/items.json"), 2);
    // The cached script is still part of the render's record.
    assert!(second.exchanges.iter().any(|e| e.url == u(lib)));
    assert_eq!(first.exchanges.len(), second.exchanges.len());
    env.frontier
        .inner
        .complete(p, vc_core::frontier::KeyOutcome::normal())
        .await
        .unwrap();

    // 25 hours later: expired, fetched again.
    env.clock.advance(Duration::from_secs(25 * 3600));
    let p = env.frontier.permit(PAGE).await;
    r.render(&p, &u(PAGE), fast_budget()).await.unwrap();
    assert_eq!(env.web.count(lib), 2);
}

#[tokio::test]
async fn document_challenge_aborts() {
    let env = Env::new();
    let script = spa(&env);
    env.web.error(PAGE, FetchError::Challenge { status: 403 });
    let (out, browser) = render_with(&env, script, fast_budget()).await;
    let err = out.unwrap_err();
    assert!(
        matches!(err, Error::Fetch(FetchError::Challenge { status: 403 })),
        "{err:?}"
    );
    assert_eq!(browser.log().tabs_closed, 1);
    assert_eq!(env.web.urls().len(), 1);
}

#[tokio::test]
async fn subrequest_challenge_in_scope_aborts() {
    let env = Env::new();
    let script = spa(&env);
    env.web.respond(
        "https://spa.example.com/api/items.json",
        403,
        &[("cf-mitigated", "challenge")],
        "checking your browser",
    );
    let (out, _) = render_with(&env, script, fast_budget()).await;
    let err = out.unwrap_err();
    assert!(
        matches!(err, Error::Fetch(FetchError::Challenge { status: 403 })),
        "{err:?}"
    );
    // No retry, no further requests after the challenge.
    assert_eq!(env.web.urls().len(), 3);
}

#[tokio::test]
async fn third_party_403_is_not_a_park() {
    let env = Env::new();
    let script = spa(&env).load(
        PAGE,
        vec![
            req("https://cdn.allowed.test/lib.js", K::Script),
            req("https://spa.example.com/app.js", K::Script),
        ],
    );
    env.frontier.allow_all("https://cdn.allowed.test/");
    env.web
        .respond("https://cdn.allowed.test/lib.js", 403, &[], "no");
    let (out, _) = render_with(&env, script, fast_budget()).await;
    let out = out.unwrap();
    assert_eq!(
        blocked_reason(&out, "https://cdn.allowed.test/lib.js"),
        Some(BlockReason::FetchFailed)
    );
    assert!(String::from_utf8_lossy(&out.dom_html).contains("Louvre"));
}

#[tokio::test]
async fn lost_lease_aborts_before_any_request() {
    let env = Env::new();
    let script = spa(&env);
    let browser = MockBrowser::new(script);
    let r = env.renderer(RenderConfig::default(), browser.clone());
    let p = env.frontier.permit(PAGE).await;
    // The lease (240 s per URL in MemFrontier) runs out.
    env.clock.advance(Duration::from_secs(600));
    let err = r.render(&p, &u(PAGE), fast_budget()).await.unwrap_err();
    assert_eq!(err.class(), vc_core::ErrorClass::Lease, "{err:?}");
    assert!(env.web.urls().is_empty());
}

#[tokio::test]
async fn document_robots_disallowed_is_an_error() {
    let env = Env::new();
    let script = spa(&env);
    env.frontier
        .robots_txt(PAGE, "User-agent: *\nDisallow: /\n");
    let (out, browser) = render_with(&env, script, fast_budget()).await;
    let err = out.unwrap_err();
    assert!(
        matches!(err, Error::Fetch(FetchError::RobotsDisallowed(_))),
        "{err:?}"
    );
    assert!(env.web.urls().is_empty());
    assert_eq!(browser.log().tabs_closed, 1);
}

#[tokio::test]
async fn cross_key_redirect_of_document_returned() {
    let env = Env::new();
    let script = spa(&env);
    env.web.error(
        PAGE,
        FetchError::CrossKeyRedirect(u("https://elsewhere.example.net/")),
    );
    let (out, _) = render_with(&env, script, fast_budget()).await;
    assert!(matches!(
        out.unwrap_err(),
        Error::Fetch(FetchError::CrossKeyRedirect(_))
    ));
}

#[tokio::test]
async fn same_key_redirect_fetched_once() {
    let env = Env::new();
    let old = "https://spa.example.com/old";
    let new = "https://spa.example.com/new";
    env.frontier.allow_all(PAGE);
    env.web.redirect(old, new, "<html>moved here</html>");
    let browser = MockBrowser::new(Script::default());
    let r = env.renderer(RenderConfig::default(), browser.clone());
    let p = env.frontier.permit(old).await;
    let out = r.render(&p, &u(old), fast_budget()).await.unwrap();
    assert!(String::from_utf8_lossy(&out.dom_html).contains("moved here"));
    assert_eq!(env.web.urls(), vec![old.to_string()]);
    assert_eq!(out.exchanges.len(), 1);
    assert_eq!(out.exchanges[0].url, u(new));
    // Chromium saw a redirect, then the final document.
    let log = browser.log();
    assert_eq!(log.fulfilled, vec![(old.into(), 302), (new.into(), 200)]);
}

#[tokio::test]
async fn expand_selectors_clicked_once() {
    let env = Env::new();
    let mut script = spa(&env);
    script.on_click.insert(
        "button.more".into(),
        vec![req("https://spa.example.com/api/more.json", K::Fetch)],
    );
    env.web.page(
        "https://spa.example.com/api/more.json",
        "application/json",
        r#"["Rodin"]"#,
    );
    let budget = RenderBudget {
        expand_selectors: vec!["button.more".into(), "a[data-x=\"y\"]".into()],
        ..fast_budget()
    };
    let (out, browser) = render_with(&env, script, budget).await;
    let out = out.unwrap();
    assert!(String::from_utf8_lossy(&out.dom_html).contains("Rodin"));
    assert_eq!(
        browser.log().clicks,
        vec!["button.more".to_string(), "a[data-x=\"y\"]".to_string()]
    );
}

#[tokio::test]
async fn auto_scroll_loads_lazy_content() {
    let env = Env::new();
    let mut script = spa(&env);
    script.on_scroll = vec![req("https://spa.example.com/api/page2.json", K::Xhr)];
    env.web.page(
        "https://spa.example.com/api/page2.json",
        "application/json",
        r#"["Pompidou"]"#,
    );
    let (out, browser) = render_with(&env, script.clone(), fast_budget()).await;
    let out = out.unwrap();
    assert!(String::from_utf8_lossy(&out.dom_html).contains("Pompidou"));
    let scrolls = browser.log().scrolls;
    assert!((2..=5).contains(&scrolls), "{scrolls}");

    // Off: no scrolling.
    let env = Env::new();
    let script = spa(&env);
    let budget = RenderBudget {
        auto_scroll: false,
        ..fast_budget()
    };
    let (_, browser) = render_with(&env, script, budget).await;
    assert_eq!(browser.log().scrolls, 0);
}

#[tokio::test]
async fn user_agent_is_the_constant() {
    let env = Env::new();
    let script = spa(&env);
    let (_, browser) = render_with(&env, script, fast_budget()).await;
    assert_eq!(browser.log().user_agents, vec![USER_AGENT.to_string()]);
}

#[tokio::test]
async fn subresource_gap_enforced_per_key() {
    let env = Env::new();
    let script = spa(&env).load(
        PAGE,
        vec![
            req("https://spa.example.com/a.js", K::Script),
            req("https://spa.example.com/b.js", K::Script),
            req("https://spa.example.com/c.js", K::Script),
        ],
    );
    let budget = RenderBudget {
        subresource_gap: Duration::from_millis(150),
        ..fast_budget()
    };
    let (out, _) = render_with(&env, script, budget).await;
    out.unwrap();
    let times: Vec<Instant> = env.web.requests().iter().map(|l| l.at).collect();
    assert_eq!(times.len(), 4);
    for w in times.windows(2) {
        assert!(
            w[1].duration_since(w[0]) >= Duration::from_millis(145),
            "{:?}",
            w[1].duration_since(w[0])
        );
    }
}

#[tokio::test]
async fn render_bounded_by_lease_deadline() {
    // The lease has 3 s left; the 90 s render budget must shrink to it.
    let env = Env::new();
    let mut script = spa(&env).load(PAGE, vec![req("https://spa.example.com/api/poll", K::Xhr)]);
    script.poll.insert(
        "https://spa.example.com/api/poll".into(),
        Duration::from_millis(20),
    );
    env.web
        .page("https://spa.example.com/api/poll", "application/json", "{}");
    let browser = MockBrowser::new(script);
    let r = env.renderer(RenderConfig::default(), browser);
    let p = env.frontier.permit(PAGE).await;
    env.clock.advance(Duration::from_secs(237));
    let budget = RenderBudget {
        total: Duration::from_secs(90),
        max_subrequests: 10_000,
        ..fast_budget()
    };
    let t0 = Instant::now();
    let _ = r.render(&p, &u(PAGE), budget).await;
    assert!(t0.elapsed() < Duration::from_secs(6), "{:?}", t0.elapsed());
}
