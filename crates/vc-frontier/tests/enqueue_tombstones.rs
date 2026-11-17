//! `Frontier::enqueue` honours the standing exclusions in `tombstones` (data deletion): a URL
//! first discovered under a purged host, url_prefix, URL or source after the purge is never
//! inserted, so it is never fetched; other URLs in the same batch are.

#![allow(missing_docs)]

use sqlx::PgPool;
use vc_config::FrontierConfig;
use vc_core::frontier::{Frontier, NewUrl};
use vc_core::{CanonicalUrl, SourceId};
use vc_frontier::PgFrontier;

fn cfg() -> FrontierConfig {
    FrontierConfig {
        min_delay_s: 0,
        holder: Some("test".into()),
        ..FrontierConfig::default()
    }
}

fn new_url(s: &str) -> NewUrl {
    NewUrl::new(CanonicalUrl::parse(s).expect("valid url"))
}

async fn tombstone(pool: &PgPool, selector_type: &str, selector: &str) {
    sqlx::query(
        "INSERT INTO tombstones (selector_type, selector, reason) VALUES ($1, $2, 'opt_out')",
    )
    .bind(selector_type)
    .bind(selector)
    .execute(pool)
    .await
    .expect("tombstone");
}

async fn known(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT x.url FROM urls x JOIN url_state s ON s.url_key = x.url_key ORDER BY x.url",
    )
    .fetch_all(pool)
    .await
    .expect("urls")
}

#[tokio::test]
async fn enqueue_skips_urls_under_a_purged_host_or_prefix() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    tombstone(&pool, "host", "purged.example").await;
    tombstone(&pool, "url_prefix", "https://mixed.example/private/").await;
    let one = new_url("https://exact.example/gone");
    tombstone(&pool, "url", &one.url.key().to_hex()).await;
    tombstone(&pool, "source", "purged-source").await;
    // A url-type tombstone is a hex key; an author tombstone never selects URLs.
    tombstone(&pool, "author", "sha256:00").await;

    let mut from_source = new_url("https://ok.example/from-purged-source");
    from_source.source_id = Some(SourceId("purged-source".into()));
    let batch = vec![
        new_url("https://purged.example/new-page"),
        new_url("https://PURGED.example./other"),
        new_url("https://sub.purged.example/kept"),
        new_url("https://mixed.example/private/a"),
        new_url("https://mixed.example/public/b"),
        one,
        new_url("https://exact.example/kept"),
        from_source,
        new_url("https://ok.example/plain"),
    ];
    let added = f.enqueue(&batch).await.expect("enqueue");
    let want = vec![
        "https://exact.example/kept".to_string(),
        "https://mixed.example/public/b".to_string(),
        "https://ok.example/plain".to_string(),
        "https://sub.purged.example/kept".to_string(),
    ];
    assert_eq!(known(&pool).await, want);
    assert_eq!(added, want.len() as u64);

    // Nothing is created for a batch that is entirely excluded (no origin row either).
    let only = f
        .enqueue(&[new_url("https://purged.example/again")])
        .await
        .expect("enqueue");
    assert_eq!(only, 0);
    let origins: Vec<String> = sqlx::query_scalar("SELECT origin FROM origins ORDER BY origin")
        .fetch_all(&pool)
        .await
        .expect("origins");
    assert!(
        !origins.iter().any(|o| o.contains("//purged.example")),
        "{origins:?}"
    );
}

#[tokio::test]
async fn enqueue_without_tombstones_is_unchanged() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let batch: Vec<NewUrl> = (0..5)
        .map(|i| new_url(&format!("https://a.example/p/{i}")))
        .collect();
    assert_eq!(f.enqueue(&batch).await.expect("enqueue"), 5);
    assert_eq!(f.enqueue(&batch).await.expect("enqueue"), 0, "idempotent");
}
