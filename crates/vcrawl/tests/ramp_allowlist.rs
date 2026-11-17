//! The go-live ramp (`vcrawl.source_allowlist`): the scheduler seeds only allowlisted sources,
//! and the fetcher never requests anything for a source outside the list, releasing its URLs.

mod crawl_path_support;

use std::sync::Arc;

use chrono::Utc;
use crawl_path_support::*;
use testkit::MemBlobStore;
use testkit::fake_web::{FakeWeb, Reply, Site};
use vc_core::SourceId;
use vc_core::frontier::{NewUrl, UrlResult};
use vc_core::media::FetchMode;
use vcrawl::roles::fetcher::Pass;
use vcrawl::roles::scheduler::Scheduler;

fn ramp_cfg(dir: &std::path::Path, ids: &str) -> vc_config::Config {
    std::fs::write(dir.join("ramp.txt"), ids).unwrap();
    let mut c = cfg();
    c.vcrawl.source_allowlist = Some(dir.join("ramp.txt").display().to_string());
    c
}

async fn due_urls(pool: &sqlx::PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT source_id FROM url_state ORDER BY source_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn scheduler_seeds_only_allowlisted_sources() {
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = ramp_cfg(dir.path(), "# ramp\nin-ramp  # why\n");
    let a = spec(
        "id: in-ramp\nmethod: html\ncadence: daily\nmin_delay_s: 9\nseeds: [\"http://a.test/p\"]\n",
    );
    let b = spec(
        "id: not-ramp\nmethod: html\ncadence: daily\nmin_delay_s: 9\nseeds: [\"http://b.test/q\"]\n",
    );
    let f = fetcher(
        &cfg,
        &pool,
        &[],
        vec![a.clone(), b.clone()],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let sched = Scheduler::new(
        Arc::new(cfg.clone()),
        pool.clone(),
        f.parts().frontier.clone(),
    );
    let rep = sched.pass(&[a.clone(), b.clone()]).await.unwrap();
    assert_eq!(rep.sources.inserted, 2, "the catalogue stays complete");
    assert_eq!((rep.enqueued, rep.held_by_allowlist), (1, 1));
    assert_eq!(due_urls(&pool).await, ["in-ramp"]);
    let limited: Vec<String> =
        sqlx::query_scalar("SELECT key FROM politeness_keys WHERE min_delay_ms = 9000")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(limited, ["a.test"], "no key limits for held sources");

    // The operator removes the allowlist: the next pass seeds the rest.
    let mut open = cfg.clone();
    open.vcrawl.source_allowlist = None;
    let sched = Scheduler::new(Arc::new(open), pool.clone(), f.parts().frontier.clone());
    let rep = sched.pass(&[a, b]).await.unwrap();
    assert_eq!((rep.enqueued, rep.held_by_allowlist), (1, 0));
    assert_eq!(due_urls(&pool).await, ["in-ramp", "not-ramp"]);
}

#[tokio::test]
async fn scheduler_fails_closed_on_a_missing_allowlist() {
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = cfg();
    cfg.vcrawl.source_allowlist = Some(dir.path().join("nope.txt").display().to_string());
    let a = spec("id: in-ramp\nmethod: html\ncadence: daily\nseeds: [\"http://a.test/p\"]\n");
    let f = fetcher(
        &cfg,
        &pool,
        &[],
        vec![a.clone()],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    let sched = Scheduler::new(Arc::new(cfg), pool.clone(), f.parts().frontier.clone());
    assert!(sched.pass(&[a]).await.is_err());
    assert!(due_urls(&pool).await.is_empty(), "nothing written");
}

#[tokio::test]
async fn fetcher_skips_and_releases_claims_of_unlisted_sources() {
    let web = Arc::new(FakeWeb::with_sites(vec![
        Site::new("a", "a.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route("/p", Reply::html("<p>in</p>"))
            .route("/q", Reply::html("<p>out</p>")),
    ]));
    let srv = web.clone().start().await;
    let port = srv.addr().port();
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = ramp_cfg(dir.path(), "in-ramp\n");
    let p = url("a.test", port, "/p");
    let q = url("a.test", port, "/q");
    let a = spec(&format!(
        "id: in-ramp\nmethod: html\ncadence: daily\nseeds: [\"{p}\"]\n"
    ));
    let b = spec(&format!(
        "id: not-ramp\nmethod: html\ncadence: daily\nseeds: [\"{q}\"]\n"
    ));
    let spool = tempfile::tempdir().unwrap();
    let f = fetcher(
        &cfg,
        &pool,
        &["a.test"],
        vec![a, b],
        spool.path(),
        Arc::new(MemBlobStore::new()),
    );
    // Rows of an unlisted source already in the frontier (e.g. enqueued before the ramp).
    let row = |u: &vc_core::CanonicalUrl, src: &str| {
        let mut n = NewUrl::new(u.clone());
        n.source_id = Some(SourceId(src.into()));
        n.fetch_mode = FetchMode::Http;
        n
    };
    f.parts()
        .frontier
        .enqueue(&[row(&q, "not-ramp")])
        .await
        .unwrap();

    // Only the unlisted URL: no request at all, released for an hour.
    let r = match f.pass().await.unwrap() {
        Pass::Claimed(r) => r,
        other => panic!("expected a claim, got {other:?}"),
    };
    assert!(r.completed);
    assert!(web.log().entries().is_empty(), "not even robots.txt");
    let UrlResult::Retry { at } = r.urls[0].result else {
        panic!("{:?}", r.urls[0].result)
    };
    assert!(at > Utc::now() + chrono::Duration::minutes(50));
    assert_eq!(url_state(&pool, &q).await.0, "due");

    // Mixed claim: the listed URL is fetched (robots first), the unlisted one is not.
    f.parts()
        .frontier
        .enqueue(&[row(&p, "in-ramp")])
        .await
        .unwrap();
    fast_forward(&pool).await;
    let r = match f.pass().await.unwrap() {
        Pass::Claimed(r) => r,
        other => panic!("expected a claim, got {other:?}"),
    };
    let result_of = |k| {
        r.urls
            .iter()
            .find(|o| o.url_key == k)
            .map(|o| o.result.clone())
    };
    assert_eq!(result_of(p.key()), Some(UrlResult::Fetched));
    assert!(matches!(result_of(q.key()), Some(UrlResult::Retry { .. })));
    let fetched: Vec<String> = web.log().entries().into_iter().map(|e| e.url).collect();
    assert!(fetched.iter().all(|u| !u.ends_with("/q")), "{fetched:?}");
    assert!(fetched.iter().any(|u| u.ends_with("/p")), "{fetched:?}");
}
