//! Migration 1104 (`policy/key_splits.yaml`, europa.eu): rows written under the old key
//! `europa.eu` move to their host keys; the park of 2026-11-17 stays only on the host that
//! caused it; a second run changes nothing; the frontier then claims each EU host on its own.

#![allow(missing_docs)]

use sqlx::PgPool;
use vc_config::FrontierConfig;
use vc_core::frontier::{Frontier, NewUrl};
use vc_core::{CanonicalUrl, PolitenessKey};
use vc_frontier::PgFrontier;

const MIGRATION: &str = include_str!("../../../migrations/1104_key_split_europa_eu.sql");

const TAXUD: &str = "taxation-customs.ec.europa.eu";
const ECB: &str = "www.ecb.europa.eu";
const HOME: &str = "home-affairs.ec.europa.eu";
const ROBOTS_WALL: &str = "food.ec.europa.eu";

const URLS: &[&str] = &[
    "https://taxation-customs.ec.europa.eu/travelling_en",
    "https://taxation-customs.ec.europa.eu/sitemap.xml",
    "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml",
    "https://home-affairs.ec.europa.eu/policies/schengen/visa-policy_en",
    "https://food.ec.europa.eu/animals/movement-pets_en",
];

fn cfg() -> FrontierConfig {
    FrontierConfig {
        min_delay_s: 0,
        holder: Some("test".into()),
        ..FrontierConfig::default()
    }
}

async fn run_migration(pool: &PgPool) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(MIGRATION))
        .execute(pool)
        .await
        .expect("migration 1104 runs");
}

/// The state the previous image left: every EU URL, origin and the parked key under
/// `europa.eu` (parked by the 403 from taxation-customs), one stale completed claim, and a
/// robots challenge on food.ec.europa.eu, plus an unrelated key that must not move.
async fn legacy_state(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO politeness_keys (key, min_delay_ms, delay_ms, budget_day, used_today,
                                      parked_until, park_level, park_reason)
         VALUES ('europa.eu', 7000, 9000, 1200, 17, now() + interval '20 hours', 1,
                 '{\"reason\":\"challenge\"}'),
                ('example.co.uk', 5000, 5000, 3000, 0, NULL, 0, NULL)",
    )
    .execute(pool)
    .await
    .expect("keys");
    sqlx::query(
        "INSERT INTO claims (key, holder, epoch, deadline, completed_at)
         VALUES ('europa.eu', 'old', nextval('epoch_seq'), now() - interval '1 hour',
                 now() - interval '1 hour')",
    )
    .execute(pool)
    .await
    .expect("claim");
    let all: Vec<&str> = URLS
        .iter()
        .copied()
        .chain(["https://www.example.co.uk/a"])
        .collect();
    for u in all {
        let c = CanonicalUrl::parse(u).expect("url");
        let k = if c.host().ends_with("europa.eu") {
            "europa.eu"
        } else {
            "example.co.uk"
        };
        sqlx::query("INSERT INTO urls (url_key, url, key) VALUES ($1, $2, $3)")
            .bind(c.key().0.to_vec())
            .bind(c.as_str())
            .bind(k)
            .execute(pool)
            .await
            .expect("urls");
        sqlx::query("INSERT INTO url_state (url_key, key) VALUES ($1, $2)")
            .bind(c.key().0.to_vec())
            .bind(k)
            .execute(pool)
            .await
            .expect("url_state");
        sqlx::query(
            "INSERT INTO origins (origin, key, robots_state) VALUES ($1, $2, $3)
             ON CONFLICT (origin) DO NOTHING",
        )
        .bind(c.origin().0)
        .bind(k)
        .bind(if c.host() == ROBOTS_WALL {
            "challenge"
        } else {
            "ok"
        })
        .execute(pool)
        .await
        .expect("origins");
    }
}

type KeyRow = (bool, bool, i16, Option<String>, i32, i32, i32);

async fn key_row(pool: &PgPool, k: &str) -> Option<KeyRow> {
    sqlx::query_as(
        "SELECT enabled, coalesce(parked_until > now(), false), park_level, park_reason,
                min_delay_ms, budget_day, used_today
         FROM politeness_keys WHERE key = $1",
    )
    .bind(k)
    .fetch_optional(pool)
    .await
    .expect("key row")
}

async fn snapshot(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT 'k:' || key, concat_ws('|', enabled, parked_until, park_level, park_reason,
                                     min_delay_ms, delay_ms, budget_day, used_today)
           FROM politeness_keys
         UNION ALL SELECT 'u:' || url, key FROM urls
         UNION ALL SELECT 's:' || encode(url_key, 'hex'), key FROM url_state
         UNION ALL SELECT 'o:' || origin, key FROM origins
         ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .expect("snapshot")
}

#[tokio::test]
async fn europa_eu_rows_move_to_host_keys_and_the_park_stays_on_its_host() {
    let pool = testkit::pg().await;
    legacy_state(&pool).await;
    run_migration(&pool).await;

    // Every URL, state row and origin is keyed by its host (= PolitenessKey::for_host now).
    for u in URLS {
        let c = CanonicalUrl::parse(u).expect("url");
        let want = PolitenessKey::for_host(c.host()).0;
        assert_eq!(want, c.host(), "for_host splits europa.eu");
        let (uk, sk): (String, String) = sqlx::query_as(
            "SELECT x.key, s.key FROM urls x JOIN url_state s USING (url_key) WHERE x.url = $1",
        )
        .bind(c.as_str())
        .fetch_one(&pool)
        .await
        .expect("row");
        assert_eq!(
            (uk.as_str(), sk.as_str()),
            (want.as_str(), want.as_str()),
            "{u}"
        );
        let (ok,): (String,) = sqlx::query_as("SELECT key FROM origins WHERE origin = $1")
            .bind(c.origin().0)
            .fetch_one(&pool)
            .await
            .expect("origin");
        assert_eq!(ok, want, "{u}");
    }

    // The park of 2026-11-17 stays on taxation-customs only (level and reason kept); the
    // robots challenge on food.ec.europa.eu keeps that host parked too.
    for host in [TAXUD, ROBOTS_WALL] {
        let (enabled, parked, level, reason, min_delay, budget, used) =
            key_row(&pool, host).await.expect("host key");
        assert!(enabled && parked, "{host} stays parked");
        assert_eq!(level, 1, "{host}");
        assert_eq!(
            reason.as_deref(),
            Some("{\"reason\":\"challenge\"}"),
            "{host}"
        );
        assert_eq!(
            (min_delay, budget, used),
            (7000, 1200, 17),
            "{host} limits copied"
        );
    }
    // The ECB and home-affairs start unparked, with the old key's limits.
    for host in [ECB, HOME] {
        let (enabled, parked, level, reason, min_delay, budget, used) =
            key_row(&pool, host).await.expect("host key");
        assert!(enabled && !parked, "{host} is not parked");
        assert_eq!((level, reason), (0, None), "{host}");
        assert_eq!(
            (min_delay, budget, used),
            (7000, 1200, 17),
            "{host} limits copied"
        );
    }
    // The old key is never claimed again; its park record stays.
    let (enabled, parked, ..) = key_row(&pool, "europa.eu").await.expect("old key");
    assert!(!enabled && parked);
    let (n,): (i64,) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM url_state WHERE key = 'europa.eu')
              + (SELECT count(*) FROM urls WHERE key = 'europa.eu')
              + (SELECT count(*) FROM origins WHERE key = 'europa.eu')",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(n, 0, "nothing left under europa.eu");
    // Other keys are untouched.
    let (enabled, parked, ..) = key_row(&pool, "example.co.uk").await.expect("other key");
    assert!(enabled && !parked);
    let (k,): (String,) = sqlx::query_as("SELECT key FROM urls WHERE url LIKE '%example.co.uk%'")
        .fetch_one(&pool)
        .await
        .expect("other url");
    assert_eq!(k, "example.co.uk");

    // Idempotent: a second run changes nothing.
    let before = snapshot(&pool).await;
    run_migration(&pool).await;
    assert_eq!(snapshot(&pool).await, before);
}

/// After the split the frontier claims the ECB while taxation-customs stays parked, and new
/// EU URLs are enqueued under their host keys.
#[tokio::test]
async fn after_the_split_the_ecb_is_claimable_and_taxud_is_not() {
    let pool = testkit::pg().await;
    legacy_state(&pool).await;
    sqlx::query("DELETE FROM url_state WHERE key = 'example.co.uk'")
        .execute(&pool)
        .await
        .expect("clear other");
    run_migration(&pool).await;
    sqlx::query("UPDATE politeness_keys SET next_allowed_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .expect("ready");

    let f = PgFrontier::new(pool.clone(), cfg());
    let mut claimed = Vec::new();
    while let Some(c) = f.claim(&[0], 5).await.expect("claim") {
        claimed.push(c.permit.key().0.clone());
    }
    claimed.sort();
    assert_eq!(claimed, vec![HOME.to_string(), ECB.to_string()]);

    let nu =
        NewUrl::new(CanonicalUrl::parse("https://travel-europe.europa.eu/en/etias").expect("url"));
    assert_eq!(nu.key.0, "travel-europe.europa.eu");
    f.enqueue(&[nu]).await.expect("enqueue");
    let (enabled, parked, ..) = key_row(&pool, "travel-europe.europa.eu")
        .await
        .expect("new key");
    assert!(enabled && !parked);
}

/// On a fresh database (no europa.eu rows) the migration is a no-op apart from its function.
#[tokio::test]
async fn migration_is_a_noop_without_old_rows() {
    let pool = testkit::pg().await;
    let before = snapshot(&pool).await;
    run_migration(&pool).await;
    assert_eq!(snapshot(&pool).await, before);
}
