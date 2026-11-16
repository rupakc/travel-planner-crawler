//! A render sub-request to a third-party key borrows that key (spec §17.1; E2E finding 8'):
//! honoured across processes through Postgres, refused while the key is not ready.

use chrono::Utc;
use sqlx::PgPool;
use vc_config::FrontierConfig;
use vc_core::frontier::{Frontier, KeyOutcome, NewUrl};
use vc_core::{CanonicalUrl, PolitenessKey};
use vc_frontier::PgFrontier;

fn cfg() -> FrontierConfig {
    FrontierConfig {
        min_delay_s: 2,
        holder: Some("test".into()),
        ..FrontierConfig::default()
    }
}

async fn secs_until_allowed(pool: &PgPool, key: &str) -> f64 {
    sqlx::query_scalar(
        "SELECT extract(epoch FROM next_allowed_at - now())::float8 FROM politeness_keys WHERE key = $1",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn borrow_honours_and_advances_the_third_party_key() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let cdn = PolitenessKey("cdn.test".into());
    // A key nobody enqueued yet: created, borrowed, and its delay starts now.
    assert!(f.borrow_subresource(&cdn).await.unwrap());
    let s = secs_until_allowed(&pool, "cdn.test").await;
    assert!((1.5..=5.5).contains(&s), "next_allowed_at advanced: {s}");
    // The request ended later: the key is held for its delay from the end.
    sqlx::query("UPDATE politeness_keys SET next_allowed_at = now() + interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    f.subresource_done(&cdn).await.unwrap();
    let s = secs_until_allowed(&pool, "cdn.test").await;
    assert!((1.5..=5.5).contains(&s), "held from the end: {s}");
    // A second sub-request right away (another render, another process): refused.
    let g = PgFrontier::new(pool.clone(), cfg());
    assert!(!g.borrow_subresource(&cdn).await.unwrap());

    // A key with an in-flight claim is refused even when its next_allowed_at has passed.
    f.enqueue(&[NewUrl::new(
        CanonicalUrl::parse("https://cdn.test/robots-page").unwrap(),
    )])
    .await
    .unwrap();
    sqlx::query("UPDATE politeness_keys SET next_allowed_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    sqlx::query("UPDATE politeness_keys SET next_allowed_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !g.borrow_subresource(&cdn).await.unwrap(),
        "in-flight claim"
    );
    f.complete(c.permit, KeyOutcome::normal()).await.unwrap();

    // A parked key is refused.
    sqlx::query(
        "UPDATE politeness_keys SET next_allowed_at = now() - interval '1 second',
                                    parked_until = $1",
    )
    .bind(Utc::now() + chrono::Duration::hours(1))
    .execute(&pool)
    .await
    .unwrap();
    assert!(!g.borrow_subresource(&cdn).await.unwrap(), "parked");
    // Ready again: granted.
    sqlx::query("UPDATE politeness_keys SET parked_until = NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert!(g.borrow_subresource(&cdn).await.unwrap());
}
