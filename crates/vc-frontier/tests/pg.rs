//! `PgFrontier` against a real Postgres 17 (testkit::pg()). Every concurrency test runs on real
//! Postgres with several pools, because the politeness guarantees live in SQL.

#![allow(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use vc_config::FrontierConfig;
use vc_core::frontier::{
    Frontier, KeyOutcome, KeySignal, LeaseCheck, NewUrl, ParkReason, UrlOutcome, UrlResult,
};
use vc_core::media::{FetchMode, MediaType};
use vc_core::policy::{RobotsEntry, RobotsStatus};
use vc_core::{CanonicalUrl, Error, LeaseLostReason, OriginKey, PolitenessKey, SourceId, UrlKey};
use vc_frontier::PgFrontier;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn cfg() -> FrontierConfig {
    FrontierConfig {
        min_delay_s: 0,
        holder: Some("test".into()),
        ..FrontierConfig::default()
    }
}

fn url(s: &str) -> CanonicalUrl {
    CanonicalUrl::parse(s).expect("valid url")
}

fn key(s: &str) -> PolitenessKey {
    PolitenessKey(s.into())
}

async fn seed(f: &PgFrontier, host: &str, n: usize) -> Vec<UrlKey> {
    let batch: Vec<NewUrl> = (0..n)
        .map(|i| NewUrl::new(url(&format!("https://{host}/p/{i}"))))
        .collect();
    f.enqueue(&batch).await.expect("enqueue");
    batch.iter().map(|u| u.url.key()).collect()
}

/// Makes the current claim of `k` look expired and the key ready again (DB-time travel).
async fn expire(pool: &PgPool, k: &str) {
    sqlx::query("UPDATE claims SET deadline = now() - interval '1 second' WHERE key = $1")
        .bind(k)
        .execute(pool)
        .await
        .expect("expire claim");
    make_ready(pool, k).await;
}

async fn make_ready(pool: &PgPool, k: &str) {
    sqlx::query(
        "UPDATE politeness_keys SET next_allowed_at = now() - interval '1 second' WHERE key = $1",
    )
    .bind(k)
    .execute(pool)
    .await
    .expect("make ready");
}

async fn state_of(pool: &PgPool, u: &UrlKey) -> (String, Option<i64>) {
    sqlx::query_as("SELECT state, lease_epoch FROM url_state WHERE url_key = $1")
        .bind(u.as_bytes().as_slice())
        .fetch_one(pool)
        .await
        .expect("url_state row")
}

async fn secs_until(pool: &PgPool, sql: &'static str, k: &str) -> f64 {
    let (v,): (f64,) = sqlx::query_as(sql)
        .bind(k)
        .fetch_one(pool)
        .await
        .expect("seconds");
    v
}

async fn second_pool(opts: &sqlx::postgres::PgConnectOptions) -> PgPool {
    PgPoolOptions::new()
        .max_connections(8)
        .connect_with(opts.clone())
        .await
        .expect("second pool")
}

fn not_attempted(urls: &[UrlKey]) -> KeyOutcome {
    KeyOutcome {
        urls: urls
            .iter()
            .map(|u| UrlOutcome {
                url_key: *u,
                result: UrlResult::NotAttempted,
            })
            .collect(),
        ..KeyOutcome::normal()
    }
}

// ---------------------------------------------------------------------------------------------
// plan tests (Task 2)
// ---------------------------------------------------------------------------------------------

/// Two claimers on two pools, 500 claims each, never hold the same key at the same time; epochs
/// are unique.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_claimers_never_share_a_key() {
    let (pool_a, opts) = testkit::pg_with_url().await;
    let pool_b = second_pool(&opts).await;
    let fa = Arc::new(PgFrontier::new(pool_a.clone(), cfg()));
    let fb = Arc::new(PgFrontier::new(pool_b, cfg()));
    for i in 0..8 {
        seed(&fa, &format!("www.site{i}.org"), 3).await;
    }

    let held: Arc<Mutex<HashMap<PolitenessKey, usize>>> = Arc::default();
    let epochs: Arc<Mutex<Vec<i64>>> = Arc::default();
    let violations: Arc<Mutex<Vec<String>>> = Arc::default();

    let mut tasks = Vec::new();
    for (who, f) in [(0usize, fa.clone()), (1usize, fb.clone())] {
        let held = held.clone();
        let epochs = epochs.clone();
        let violations = violations.clone();
        tasks.push(tokio::spawn(async move {
            let mut done = 0;
            let mut idle = 0;
            while done < 500 {
                let Some(claim) = f.claim(&[0], 5).await.expect("claim") else {
                    idle += 1;
                    assert!(idle < 100_000, "starved");
                    tokio::task::yield_now().await;
                    continue;
                };
                let k = claim.permit.key().clone();
                {
                    let mut h = held.lock().unwrap();
                    if let Some(other) = h.insert(k.clone(), who) {
                        violations
                            .lock()
                            .unwrap()
                            .push(format!("{k} held by {other} and {who}"));
                    }
                }
                epochs.lock().unwrap().push(claim.permit.epoch().0);
                f.check(&claim.permit).await.expect("fresh claim is live");
                tokio::task::yield_now().await;
                let urls: Vec<UrlKey> = claim.urls.iter().map(|u| u.url_key).collect();
                // Release in the map *before* the DB completion, so a later claim by the other
                // task can never be a false positive.
                held.lock().unwrap().remove(&k);
                f.complete(claim.permit, not_attempted(&urls))
                    .await
                    .expect("complete");
                done += 1;
            }
        }));
    }
    for t in tasks {
        t.await.expect("task");
    }
    assert!(
        violations.lock().unwrap().is_empty(),
        "{:?}",
        violations.lock().unwrap()
    );
    let e = epochs.lock().unwrap();
    assert_eq!(e.len(), 1000);
    let uniq: HashSet<i64> = e.iter().copied().collect();
    assert_eq!(uniq.len(), 1000, "epochs are unique");
}

/// Many claimers racing over many keys without completing: each key is claimed exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_claimers_take_each_key_once() {
    let (pool, opts) = testkit::pg_with_url().await;
    let pool_b = second_pool(&opts).await;
    let fa = Arc::new(PgFrontier::new(pool.clone(), cfg()));
    let fb = Arc::new(PgFrontier::new(pool_b, cfg()));
    for i in 0..40 {
        seed(&fa, &format!("www.k{i}.org"), 2).await;
    }
    // A claimer may see `None` while the remaining keys are row-locked by others (SKIP LOCKED),
    // so every task keeps trying until all keys are taken or a time limit passes.
    let taken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let start = Instant::now();
    let mut tasks = Vec::new();
    for t in 0..8 {
        let f = if t % 2 == 0 { fa.clone() } else { fb.clone() };
        let taken = taken.clone();
        tasks.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            while taken.load(std::sync::atomic::Ordering::SeqCst) < 40
                && start.elapsed() < Duration::from_secs(30)
            {
                match f.claim(&[0], 5).await.expect("claim") {
                    Some(c) => {
                        taken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        mine.push(c);
                    }
                    None => tokio::task::yield_now().await,
                }
            }
            mine
        }));
    }
    let mut keys = Vec::new();
    for t in tasks {
        for c in t.await.expect("task") {
            keys.push(c.permit.key().clone());
        }
    }
    assert_eq!(keys.len(), 40);
    let uniq: HashSet<_> = keys.into_iter().collect();
    assert_eq!(uniq.len(), 40);
    assert!(
        fa.claim(&[0], 5).await.unwrap().is_none(),
        "every key is held"
    );
}

#[tokio::test]
async fn stale_epoch_completion_rejected() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "stale.example.com", 2).await;
    let old = f.claim(&[0], 5).await.unwrap().expect("first claim");
    expire(&pool, "example.com").await;
    let new = f.claim(&[0], 5).await.unwrap().expect("second claim");
    assert!(new.permit.epoch() > old.permit.epoch());

    let e = f.check(&old.permit).await.unwrap_err();
    assert_eq!(e.reason, LeaseLostReason::Superseded);
    assert!(f.take_rate_token(&old.permit).await.is_err());
    assert!(
        f.extend(&old.permit, Duration::from_secs(60))
            .await
            .is_err()
    );
    assert!(
        f.mark_fetched(&old.permit, &urls[0], Utc::now())
            .await
            .is_err()
    );
    let e = f
        .complete(old.permit, not_attempted(&urls))
        .await
        .unwrap_err();
    assert_eq!(e.reason, LeaseLostReason::Superseded);

    // The stale completion changed nothing: the URLs are still leased by the new epoch.
    for u in &urls {
        assert_eq!(
            state_of(&pool, u).await,
            ("leased".into(), Some(new.permit.epoch().0))
        );
    }
    f.complete(new.permit, not_attempted(&urls))
        .await
        .expect("current completes");
    for u in &urls {
        assert_eq!(state_of(&pool, u).await, ("due".into(), None));
    }
}

#[tokio::test]
async fn lease_expiry_returns_leased_urls_to_due() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "expiry.example.com", 3).await;
    let first = f.claim(&[0], 5).await.unwrap().expect("claim");
    assert_eq!(first.urls.len(), 3);
    // While the lease is live nobody else gets the key.
    assert!(f.claim(&[0], 5).await.unwrap().is_none());
    expire(&pool, "example.com").await;
    let e = f.check(&first.permit).await.unwrap_err();
    assert_eq!(e.reason, LeaseLostReason::Expired);
    let second = f
        .claim(&[0], 5)
        .await
        .unwrap()
        .expect("re-claim after expiry");
    let got: HashSet<UrlKey> = second.urls.iter().map(|u| u.url_key).collect();
    assert_eq!(got, urls.iter().copied().collect());
    for u in &urls {
        assert_eq!(
            state_of(&pool, u).await,
            ("leased".into(), Some(second.permit.epoch().0))
        );
    }
}

#[tokio::test]
async fn fetched_pending_never_released() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "pending.example.com", 2).await;
    let first = f.claim(&[0], 5).await.unwrap().expect("claim");
    let e1 = first.permit.epoch().0;
    let next = Utc::now() + chrono::Duration::days(1);
    f.mark_fetched(&first.permit, &urls[0], next)
        .await
        .expect("mark");
    // Idempotent for the same epoch.
    f.mark_fetched(&first.permit, &urls[0], next)
        .await
        .expect("mark again");
    assert_eq!(
        state_of(&pool, &urls[0]).await,
        ("fetched_pending".into(), Some(e1))
    );

    // The holder dies; the next claim gets only the other URL.
    expire(&pool, "example.com").await;
    let second = f.claim(&[0], 5).await.unwrap().expect("claim");
    let got: Vec<UrlKey> = second.urls.iter().map(|u| u.url_key).collect();
    assert_eq!(got, vec![urls[1]]);
    // Completing the new claim (even naming the pending URL) never releases it.
    f.complete(second.permit, not_attempted(&urls))
        .await
        .unwrap();
    assert_eq!(
        state_of(&pool, &urls[0]).await,
        ("fetched_pending".into(), Some(e1))
    );
    // Nor does a later claim, once the key is ready.
    make_ready(&pool, "example.com").await;
    let third = f.claim(&[0], 5).await.unwrap().expect("claim");
    assert!(third.urls.iter().all(|u| u.url_key != urls[0]));
    f.complete(third.permit, KeyOutcome::normal())
        .await
        .unwrap();

    // Only the segment commit (spool replay, same epoch) returns it to due.
    let manifest = serde_json::json!({
        "segment_id": "01JSEGMENT0000000000000001",
        "path": "segments/x.warc.zst", "cdxj_path": "cdxj/x.cdxj", "bytes": 1, "crc32c": 1,
        "record_count": 1, "created_at": Utc::now(),
        "captures": [{
            "capture_id": "01JCAPTURE0000000000000001", "captured_at": Utc::now(),
            "offset": 0, "length": 1, "warc_record_id": "<urn:uuid:x>",
            "url_key": urls[0].to_hex(), "url": "https://pending.example.com/p/0",
            "key": "example.com", "role": "primary", "fetch_mode": "http", "epoch": e1,
            "status": 200, "media_type": "html", "use_policy_fetch": {},
            "validators": {"etag": "\"v1\""}
        }]
    });
    let (ok,): (bool,) = sqlx::query_as("SELECT vc_commit_segment($1)")
        .bind(manifest)
        .fetch_one(&pool)
        .await
        .expect("commit");
    assert!(ok);
    assert_eq!(state_of(&pool, &urls[0]).await, ("due".into(), None));
}

/// Two pools (two "processes"), 8 tasks hammering the global bucket: the measured rate stays
/// at 4/s ± 10 %.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_token_global_across_pools() {
    let (pool, opts) = testkit::pg_with_url().await;
    let pool_b = second_pool(&opts).await;
    let fa = Arc::new(PgFrontier::new(pool.clone(), cfg()));
    let fb = Arc::new(PgFrontier::new(pool_b, cfg()));
    seed(&fa, "a.example.com", 1).await;
    seed(&fa, "b.example.net", 1).await;
    let ca = fa.claim(&[0], 5).await.unwrap().expect("claim a");
    let cb = fb.claim(&[0], 5).await.unwrap().expect("claim b");
    let pa = Arc::new(ca.permit);
    let pb = Arc::new(cb.permit);
    // Empty bucket: measure the steady-state refill rate, not the burst.
    sqlx::query("UPDATE rate_bucket SET tokens = 0, updated_at = clock_timestamp()")
        .execute(&pool)
        .await
        .unwrap();
    let start = Instant::now();
    let window = Duration::from_secs(6);
    let mut tasks = Vec::new();
    for t in 0..8 {
        let (f, p) = if t % 2 == 0 {
            (fa.clone(), pa.clone())
        } else {
            (fb.clone(), pb.clone())
        };
        tasks.push(tokio::spawn(async move {
            let mut n = 0u32;
            while start.elapsed() < window {
                match f.take_rate_token(&p).await {
                    Ok(()) => {}
                    // A transient DB error fails closed (no token); on a loaded machine that
                    // can happen, and it is not what this test measures.
                    Err(e) if e.reason == LeaseLostReason::Unverifiable => continue,
                    Err(e) => panic!("token: {e:?}"),
                }
                if start.elapsed() <= window {
                    n += 1;
                }
            }
            n
        }));
    }
    let mut total = 0;
    for t in tasks {
        total += t.await.unwrap();
    }
    let rate = f64::from(total) / window.as_secs_f64();
    assert!(
        (3.6..=4.4).contains(&rate),
        "rate {rate}/s ({total} tokens)"
    );
    // Every token was counted against the keys' daily budgets.
    let (used,): (i64,) = sqlx::query_as("SELECT sum(used_today)::bigint FROM politeness_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(used >= i64::from(total));
}

#[tokio::test]
async fn park_ladder_doubles_to_30d() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    seed(&f, "parked.example.com", 1).await;
    let k = key("example.com");
    let sql = "SELECT extract(epoch FROM parked_until - now())::float8 FROM politeness_keys WHERE key = $1";
    let mut got = Vec::new();
    for _ in 0..7 {
        f.park(&k, ParkReason::Challenge).await.unwrap();
        let s = secs_until(&pool, sql, "example.com").await;
        got.push((s / 3600.0).round() as i64);
        // Let the park run out, then trip again.
        sqlx::query("UPDATE politeness_keys SET parked_until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
    }
    assert_eq!(got, vec![24, 48, 96, 192, 384, 720, 720]);

    // A park while already parked does not climb the ladder twice.
    f.park(&k, ParkReason::Unauthorized { status: 403 })
        .await
        .unwrap();
    f.park(&k, ParkReason::Challenge).await.unwrap();
    let s = secs_until(&pool, sql, "example.com").await;
    assert_eq!((s / 3600.0).round() as i64, 720);

    // 5xx parks are 1 h and do not touch the ladder.
    sqlx::query("UPDATE politeness_keys SET parked_until = NULL, park_level = 2")
        .execute(&pool)
        .await
        .unwrap();
    f.park(&k, ParkReason::ServerErrors).await.unwrap();
    let s = secs_until(&pool, sql, "example.com").await;
    assert_eq!((s / 60.0).round() as i64, 60);
    let (lvl,): (i16,) = sqlx::query_as("SELECT park_level FROM politeness_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lvl, 2);
}

#[tokio::test]
async fn delay_respects_crawl_delay() {
    let pool = testkit::pg().await;
    let config = FrontierConfig {
        min_delay_s: 5,
        ..cfg()
    };
    let f = PgFrontier::new(pool.clone(), config);
    let urls = seed(&f, "slow.example.com", 2).await;
    let c = f.claim(&[0], 1).await.unwrap().expect("claim");
    assert_eq!(c.permit.delay(), Duration::from_secs(5));
    let outcome = KeyOutcome {
        crawl_delay: Some(Duration::from_secs(30)),
        mean_latency: Some(Duration::from_millis(200)),
        requests: 1,
        ..not_attempted(&urls[..1])
    };
    f.complete(c.permit, outcome).await.unwrap();
    let s = secs_until(
        &pool,
        "SELECT extract(epoch FROM next_allowed_at - now())::float8 FROM politeness_keys WHERE key = $1",
        "example.com",
    )
    .await;
    assert!((29.0..=31.0).contains(&s), "next_allowed_at in {s}s");
    // Not claimable before the delay has passed.
    assert!(f.claim(&[0], 5).await.unwrap().is_none());
    make_ready(&pool, "example.com").await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    assert_eq!(c.permit.delay(), Duration::from_secs(30));

    // 10 × EWMA latency wins when it is larger; the 1 h cap always holds.
    let outcome = KeyOutcome {
        crawl_delay: Some(Duration::from_secs(7200)),
        ..KeyOutcome::normal()
    };
    f.complete(c.permit, outcome).await.unwrap();
    let (d,): (i32,) = sqlx::query_as("SELECT delay_ms FROM politeness_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(d, 3_600_000);
}

#[tokio::test]
async fn budget_exhaustion_blocks_key() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    seed(&f, "budget.example.com", 3).await;
    sqlx::query("UPDATE politeness_keys SET budget_day = 2")
        .execute(&pool)
        .await
        .unwrap();
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    f.take_rate_token(&c.permit).await.unwrap();
    f.take_rate_token(&c.permit).await.unwrap();
    f.complete(c.permit, KeyOutcome::normal()).await.unwrap();
    make_ready(&pool, "example.com").await;
    assert!(f.claim(&[0], 5).await.unwrap().is_none(), "budget spent");
    // A new UTC day resets the budget.
    sqlx::query("UPDATE politeness_keys SET budget_date = budget_date - 1")
        .execute(&pool)
        .await
        .unwrap();
    assert!(f.claim(&[0], 5).await.unwrap().is_some());
}

#[tokio::test]
async fn enqueue_dedupes() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let mut batch: Vec<NewUrl> = (0..2500)
        .map(|i| NewUrl::new(url(&format!("https://www.dedupe.example/{i}"))))
        .collect();
    // Duplicates inside one batch.
    batch.push(NewUrl::new(url("https://www.dedupe.example/7")));
    batch.push(NewUrl::new(url("https://www.dedupe.example/7#frag")));
    assert_eq!(f.enqueue(&batch).await.unwrap(), 2500);
    assert_eq!(f.enqueue(&batch).await.unwrap(), 0);
    batch.push(NewUrl::new(url("https://www.dedupe.example/new")));
    assert_eq!(f.enqueue(&batch).await.unwrap(), 1);
    let (n_urls, n_state, n_keys, n_origins): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM urls), (SELECT count(*) FROM url_state), \
         (SELECT count(*) FROM politeness_keys), (SELECT count(*) FROM origins)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((n_urls, n_state, n_keys, n_origins), (2501, 2501, 1, 1));
    assert_eq!(f.enqueue(&[]).await.unwrap(), 0);
}

/// Concurrent enqueues of overlapping batches from two pools insert each URL exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_enqueue_inserts_once() {
    let (pool, opts) = testkit::pg_with_url().await;
    let pool_b = second_pool(&opts).await;
    let fa = Arc::new(PgFrontier::new(pool.clone(), cfg()));
    let fb = Arc::new(PgFrontier::new(pool_b, cfg()));
    let mut tasks = Vec::new();
    for t in 0..6 {
        let f = if t % 2 == 0 { fa.clone() } else { fb.clone() };
        tasks.push(tokio::spawn(async move {
            // Overlapping, differently ordered batches over several keys.
            let mut b: Vec<NewUrl> = (0..600)
                .map(|i| {
                    NewUrl::new(url(&format!(
                        "https://h{}.race{}.example/{i}",
                        i % 3,
                        i % 5
                    )))
                })
                .collect();
            if t % 3 == 1 {
                b.reverse();
            }
            f.enqueue(&b).await.expect("enqueue")
        }));
    }
    let mut total = 0;
    for t in tasks {
        total += t.await.unwrap();
    }
    assert_eq!(total, 600);
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM url_state")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 600);
}

// ---------------------------------------------------------------------------------------------
// further behaviour
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn claim_leases_by_priority_and_reports_fields() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let mut batch = Vec::new();
    for (i, p) in [0.1f32, 0.9, 0.5, 0.7].iter().enumerate() {
        let mut n = NewUrl::new(url(&format!("https://prio.example.com/{i}")));
        n.priority = *p;
        n.depth = 3;
        n.source_id = Some(SourceId("src".into()));
        n.expect = Some(MediaType::Rss);
        n.fetch_mode = FetchMode::Adapter("nager".into());
        batch.push(n);
    }
    let mut later = NewUrl::new(url("https://prio.example.com/later"));
    later.priority = 1.0;
    later.not_before = Some(Utc::now() + chrono::Duration::hours(1));
    batch.push(later);
    f.enqueue(&batch).await.unwrap();

    assert!(matches!(f.claim(&[0], 0).await, Err(Error::Invalid(_))));
    assert!(matches!(f.claim(&[0], 6).await, Err(Error::Invalid(_))));
    // Wrong shard: nothing.
    assert!(f.claim(&[3], 5).await.unwrap().is_none());

    let c = f
        .claim(&[], 2)
        .await
        .unwrap()
        .expect("claim (config shards)");
    let prios: Vec<f32> = c.urls.iter().map(|u| u.priority).collect();
    assert_eq!(prios, vec![0.9, 0.7]);
    let u = &c.urls[0];
    assert_eq!(u.depth, 3);
    assert_eq!(u.source_id, Some(SourceId("src".into())));
    assert_eq!(u.expect, Some(MediaType::Rss));
    assert_eq!(u.fetch_mode, FetchMode::Adapter("nager".into()));
    assert!(u.validators.is_empty());
    assert!(u.last_capture.is_none());
    assert_eq!(
        c.robots_due,
        vec![OriginKey("https://prio.example.com".into())]
    );
    let remaining = c.permit.remaining(Utc::now());
    assert!(remaining > Duration::from_secs(470) && remaining <= Duration::from_secs(480));
    assert_eq!(c.permit.scope().key, key("example.com"));
}

#[tokio::test]
async fn extend_moves_deadline_and_holds_off_next_claim() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    seed(&f, "ext.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    let before = c.permit.deadline();
    f.extend(&c.permit, Duration::from_secs(600)).await.unwrap();
    let after = c.permit.deadline();
    assert_eq!((after.0 - before.0).num_seconds(), 600);
    let (db,): (DateTime<Utc>,) = sqlx::query_as("SELECT deadline FROM claims")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(db, after.0);
    let (na,): (DateTime<Utc>,) = sqlx::query_as("SELECT next_allowed_at FROM politeness_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(na >= after.0);
    // An expired lease cannot be extended.
    sqlx::query("UPDATE claims SET deadline = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let e = f
        .extend(&c.permit, Duration::from_secs(60))
        .await
        .unwrap_err();
    assert_eq!(e.reason, LeaseLostReason::Expired);
    let e = f.take_rate_token(&c.permit).await.unwrap_err();
    assert_eq!(e.reason, LeaseLostReason::Expired);
    // An expired but not superseded claim may still complete.
    f.complete(c.permit, KeyOutcome::normal()).await.unwrap();
}

#[tokio::test]
async fn completed_claim_releases_lease_and_waits_for_delay() {
    let pool = testkit::pg().await;
    let config = FrontierConfig {
        min_delay_s: 5,
        ..cfg()
    };
    let f = PgFrontier::new(pool.clone(), config);
    let urls = seed(&f, "rel.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    // Before completion the key waits for the whole deadline plus the delay.
    let s = secs_until(
        &pool,
        "SELECT extract(epoch FROM next_allowed_at - now())::float8 FROM politeness_keys WHERE key = $1",
        "example.com",
    )
    .await;
    assert!(s > 240.0, "held for the lease: {s}");
    f.complete(c.permit, not_attempted(&urls)).await.unwrap();
    let (done, live): (bool, bool) = sqlx::query_as(
        "SELECT completed_at IS NOT NULL, deadline > now() FROM claims WHERE key = 'example.com'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(done && !live);
    let s = secs_until(
        &pool,
        "SELECT extract(epoch FROM next_allowed_at - now())::float8 FROM politeness_keys WHERE key = $1",
        "example.com",
    )
    .await;
    assert!((4.0..=5.5).contains(&s), "released with the delay: {s}");
}

#[tokio::test]
async fn url_results_are_applied() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "res.example.com", 5).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    let retry_at = Utc::now() + chrono::Duration::hours(2);
    f.mark_fetched(&c.permit, &urls[0], retry_at).await.unwrap();
    let outcome = KeyOutcome {
        urls: vec![
            UrlOutcome {
                url_key: urls[0],
                result: UrlResult::Fetched,
            },
            UrlOutcome {
                url_key: urls[1],
                result: UrlResult::Retry { at: retry_at },
            },
            UrlOutcome {
                url_key: urls[2],
                result: UrlResult::Gone,
            },
            UrlOutcome {
                url_key: urls[3],
                result: UrlResult::Disallowed,
            },
            UrlOutcome {
                url_key: urls[4],
                result: UrlResult::Failed {
                    class: vc_core::ErrorClass::Transient,
                    retry_at,
                },
            },
        ],
        ..KeyOutcome::normal()
    };
    f.complete(c.permit, outcome).await.unwrap();
    assert_eq!(state_of(&pool, &urls[0]).await.0, "fetched_pending");
    assert_eq!(state_of(&pool, &urls[1]).await, ("due".into(), None));
    assert_eq!(state_of(&pool, &urls[2]).await, ("due".into(), None));
    assert_eq!(state_of(&pool, &urls[3]).await, ("disabled".into(), None));
    assert_eq!(state_of(&pool, &urls[4]).await, ("due".into(), None));
    let (nf,): (DateTime<Utc>,) =
        sqlx::query_as("SELECT next_fetch_at FROM url_state WHERE url_key = $1")
            .bind(urls[1].as_bytes().as_slice())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(nf.timestamp_micros(), retry_at.timestamp_micros());

    // A second 404 in a row marks the URL gone.
    sqlx::query("UPDATE url_state SET next_fetch_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    make_ready(&pool, "example.com").await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    assert!(c.urls.iter().any(|u| u.url_key == urls[2]));
    let outcome = KeyOutcome {
        urls: vec![UrlOutcome {
            url_key: urls[2],
            result: UrlResult::Gone,
        }],
        ..KeyOutcome::normal()
    };
    f.complete(c.permit, outcome).await.unwrap();
    assert_eq!(state_of(&pool, &urls[2]).await, ("gone".into(), None));
}

#[tokio::test]
async fn rate_limited_backs_off() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    seed(&f, "rl.example.com", 1).await;
    let sql = "SELECT extract(epoch FROM next_allowed_at - now())::float8 FROM politeness_keys WHERE key = $1";

    let c = f.claim(&[0], 5).await.unwrap().unwrap();
    let o = KeyOutcome {
        signal: KeySignal::RateLimited {
            retry_after: Some(Duration::from_secs(120)),
        },
        ..KeyOutcome::normal()
    };
    f.complete(c.permit, o).await.unwrap();
    let s = secs_until(&pool, sql, "example.com").await;
    assert!((119.0..=121.0).contains(&s), "Retry-After honoured: {s}");

    // Without Retry-After the back-off doubles.
    make_ready(&pool, "example.com").await;
    let c = f.claim(&[0], 5).await.unwrap().unwrap();
    let o = KeyOutcome {
        signal: KeySignal::RateLimited { retry_after: None },
        ..KeyOutcome::normal()
    };
    f.complete(c.permit, o).await.unwrap();
    let s = secs_until(&pool, sql, "example.com").await;
    assert!((239.0..=241.0).contains(&s), "doubled: {s}");

    // A huge Retry-After is capped at 24 h.
    make_ready(&pool, "example.com").await;
    let c = f.claim(&[0], 5).await.unwrap().unwrap();
    let o = KeyOutcome {
        signal: KeySignal::RateLimited {
            retry_after: Some(Duration::from_secs(10 * 86_400)),
        },
        ..KeyOutcome::normal()
    };
    f.complete(c.permit, o).await.unwrap();
    let s = secs_until(&pool, sql, "example.com").await;
    assert!((86_399.0..=86_401.0).contains(&s), "capped: {s}");

    // Normal completions decay the penalty again.
    make_ready(&pool, "example.com").await;
    let c = f.claim(&[0], 5).await.unwrap().unwrap();
    f.complete(c.permit, KeyOutcome::normal()).await.unwrap();
    let (p,): (i32,) = sqlx::query_as("SELECT penalty_ms FROM politeness_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(p, 43_200_000);
}

#[tokio::test]
async fn server_errors_park_one_hour_after_threshold() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    seed(&f, "five.example.com", 1).await;
    for consecutive in [3u32, 2] {
        make_ready(&pool, "example.com").await;
        let c = f.claim(&[0], 5).await.unwrap().expect("claim");
        let o = KeyOutcome {
            signal: KeySignal::ServerErrors { consecutive },
            ..KeyOutcome::normal()
        };
        f.complete(c.permit, o).await.unwrap();
    }
    let s = secs_until(
        &pool,
        "SELECT extract(epoch FROM parked_until - now())::float8 FROM politeness_keys WHERE key = $1",
        "example.com",
    )
    .await;
    assert!((3590.0..=3610.0).contains(&s), "parked 1h: {s}");
    make_ready(&pool, "example.com").await;
    assert!(
        f.claim(&[0], 5).await.unwrap().is_none(),
        "parked keys are not claimed"
    );
}

#[tokio::test]
async fn park_signal_and_park_revoke_live_lease() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    seed(&f, "blocked.example.com", 1).await;
    seed(&f, "other.example.net", 1).await;

    // Park via the completion signal.
    let c = f.claim(&[0], 5).await.unwrap().unwrap();
    let k = c.permit.key().clone();
    let o = KeyOutcome {
        signal: KeySignal::Park(ParkReason::Unauthorized { status: 403 }),
        ..KeyOutcome::normal()
    };
    f.complete(c.permit, o).await.unwrap();
    let (reason,): (Option<String>,) =
        sqlx::query_as("SELECT park_reason FROM politeness_keys WHERE key = $1")
            .bind(k.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(reason.unwrap().contains("unauthorized"));

    // A direct park revokes the other key's live lease immediately (detect → park → stop).
    let c = f.claim(&[0], 5).await.unwrap().expect("other key");
    assert_ne!(c.permit.key(), &k);
    f.park(c.permit.key(), ParkReason::Manual { note: "ops".into() })
        .await
        .unwrap();
    assert!(f.check(&c.permit).await.is_err());
    assert!(f.take_rate_token(&c.permit).await.is_err());
    make_ready(&pool, "example.net").await;
    make_ready(&pool, k.as_str()).await;
    assert!(f.claim(&[0], 5).await.unwrap().is_none());

    // Parking an unknown key creates it parked.
    f.park(&key("new.example"), ParkReason::RobotsBlocked)
        .await
        .unwrap();
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM politeness_keys WHERE key = 'new.example' AND parked_until > now()",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn robots_roundtrip_is_lease_checked() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    seed(&f, "robots.example.com", 1).await;
    let origin = OriginKey("https://robots.example.com".into());
    assert!(f.robots(&origin).await.unwrap().is_none());
    let c = f.claim(&[0], 5).await.unwrap().unwrap();
    assert_eq!(c.robots_due, vec![origin.clone()]);

    let now = Utc::now();
    let entry = RobotsEntry {
        origin: origin.clone(),
        status: RobotsStatus::Ok,
        http_status: Some(200),
        body: Some(b"User-agent: *\nDisallow: /x\n".to_vec().into()),
        last_good: Some((
            b"User-agent: *\n".to_vec().into(),
            now - chrono::Duration::days(1),
        )),
        fetched_at: now,
        expires_at: now + chrono::Duration::hours(24),
    };
    f.store_robots(&c.permit, entry.clone()).await.unwrap();
    let got = f.robots(&origin).await.unwrap().expect("stored");
    assert_eq!(got.status, RobotsStatus::Ok);
    assert_eq!(got.http_status, Some(200));
    assert_eq!(got.body, entry.body);
    assert_eq!(
        got.last_good.as_ref().map(|x| x.0.clone()),
        entry.last_good.as_ref().map(|x| x.0.clone())
    );
    assert_eq!(got.fetched_at.timestamp_micros(), now.timestamp_micros());

    // Other statuses round-trip too.
    for st in [
        RobotsStatus::NotFound,
        RobotsStatus::Unauthorized,
        RobotsStatus::Challenge,
        RobotsStatus::Unavailable,
        RobotsStatus::NoResponse,
    ] {
        let e = RobotsEntry {
            status: st,
            body: None,
            last_good: None,
            ..entry.clone()
        };
        f.store_robots(&c.permit, e).await.unwrap();
        assert_eq!(f.robots(&origin).await.unwrap().unwrap().status, st);
    }

    // An origin of another key is out of scope.
    let foreign = RobotsEntry {
        origin: OriginKey("https://evil.example.org".into()),
        ..entry.clone()
    };
    assert!(matches!(
        f.store_robots(&c.permit, foreign).await,
        Err(Error::Policy(_))
    ));
    let bad = RobotsEntry {
        origin: OriginKey("not a url".into()),
        ..entry.clone()
    };
    assert!(f.store_robots(&c.permit, bad).await.is_err());

    // Fresh robots: the next claim does not ask for it again.
    let urls: Vec<UrlKey> = c.urls.iter().map(|u| u.url_key).collect();
    f.store_robots(&c.permit, entry.clone()).await.unwrap();
    f.complete(c.permit, not_attempted(&urls)).await.unwrap();
    make_ready(&pool, "example.com").await;
    let c2 = f.claim(&[0], 5).await.unwrap().unwrap();
    assert!(c2.robots_due.is_empty());

    // A stale permit cannot store robots.
    expire(&pool, "example.com").await;
    let _c3 = f.claim(&[0], 5).await.unwrap().unwrap();
    assert!(matches!(
        f.store_robots(&c2.permit, entry).await,
        Err(Error::LeaseLost(_))
    ));
}

#[tokio::test]
async fn take_rate_token_waits_for_refill() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    seed(&f, "wait.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().unwrap();
    // The bucket row is recreated if missing.
    sqlx::query("DELETE FROM rate_bucket")
        .execute(&pool)
        .await
        .unwrap();
    let t = Instant::now();
    for _ in 0..6 {
        f.take_rate_token(&c.permit).await.unwrap();
    }
    // Starting from an empty recreated bucket, 6 tokens at 4/s need ≥ ~1.5 s.
    assert!(
        t.elapsed() >= Duration::from_millis(1400),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn estimate_next_fetch_uses_change_history() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "est.example.com", 2).await;
    let bounds = (Duration::from_secs(3600), Duration::from_secs(30 * 86_400));
    // No history: the lower bound.
    let at = f
        .estimate_next_fetch(&urls[0], bounds.0, bounds.1)
        .await
        .unwrap();
    let secs = (at - Utc::now()).num_seconds();
    assert!((3590..=3600).contains(&secs), "{secs}");
    // Never changed in 20 fetches: back off towards the upper bound.
    sqlx::query("UPDATE url_state SET fetch_count = 20, change_count = 0, est_interval_s = 86400")
        .execute(&pool)
        .await
        .unwrap();
    let at = f
        .estimate_next_fetch(&urls[0], bounds.0, bounds.1)
        .await
        .unwrap();
    assert!((at - Utc::now()).num_seconds() > 86_400);
    // Changed every time: the lower bound.
    sqlx::query("UPDATE url_state SET fetch_count = 20, change_count = 19, est_interval_s = 7200")
        .execute(&pool)
        .await
        .unwrap();
    let at = f
        .estimate_next_fetch(&urls[1], bounds.0, bounds.1)
        .await
        .unwrap();
    assert!((at - Utc::now()).num_seconds() <= 3600);
    let missing = UrlKey::of("https://nowhere.example/");
    assert!(matches!(
        f.estimate_next_fetch(&missing, bounds.0, bounds.1).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn database_failure_fails_closed() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    seed(&f, "closed.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().unwrap();
    pool.close().await;
    let e = f.check(&c.permit).await.unwrap_err();
    assert_eq!(e.reason, LeaseLostReason::Unverifiable);
    assert_eq!(
        f.take_rate_token(&c.permit).await.unwrap_err().reason,
        LeaseLostReason::Unverifiable
    );
    assert!(f.extend(&c.permit, Duration::from_secs(1)).await.is_err());
    assert!(
        f.mark_fetched(&c.permit, &UrlKey::of("x"), Utc::now())
            .await
            .is_err()
    );
    assert!(matches!(f.claim(&[0], 5).await, Err(Error::Db(_))));
    assert!(
        f.enqueue(&[NewUrl::new(url("https://a.example/"))])
            .await
            .is_err()
    );
    assert!(
        f.park(&key("example.com"), ParkReason::Challenge)
            .await
            .is_err()
    );
    assert!(
        f.robots(&OriginKey("https://a.example".into()))
            .await
            .is_err()
    );
    assert!(f.complete(c.permit, KeyOutcome::normal()).await.is_err());
}

// ---------------------------------------------------------------------------------------------
// reschedule (controller addition)
// ---------------------------------------------------------------------------------------------

async fn schedule_of(pool: &PgPool, u: &UrlKey) -> (DateTime<Utc>, Option<DateTime<Utc>>) {
    sqlx::query_as("SELECT next_fetch_at, refetch_at FROM url_state WHERE url_key = $1")
        .bind(u.as_bytes().as_slice())
        .fetch_one(pool)
        .await
        .expect("url_state row")
}

fn one(u: UrlKey, result: UrlResult) -> KeyOutcome {
    KeyOutcome {
        urls: vec![UrlOutcome { url_key: u, result }],
        ..KeyOutcome::normal()
    }
}

fn close(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    (a - b).num_milliseconds().abs() < 5
}

#[tokio::test]
async fn reschedule_due_url_becomes_claimable_at() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "resched.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    let later = Utc::now() + chrono::Duration::days(1);
    f.complete(c.permit, one(urls[0], UrlResult::Retry { at: later }))
        .await
        .unwrap();
    make_ready(&pool, "example.com").await;
    assert!(f.claim(&[0], 5).await.unwrap().is_none(), "not due yet");

    let at = Utc::now() - chrono::Duration::seconds(1);
    assert!(f.reschedule(&urls[0], at).await.unwrap());
    let (next, pending) = schedule_of(&pool, &urls[0]).await;
    assert!(close(next, at), "{next} vs {at}");
    assert_eq!(pending, None);
    let c = f.claim(&[0], 5).await.unwrap().expect("due again");
    assert_eq!(c.urls[0].url_key, urls[0]);
    f.complete(c.permit, KeyOutcome::normal()).await.unwrap();

    // A due URL can also be pushed later.
    let far = Utc::now() + chrono::Duration::days(3);
    assert!(f.reschedule(&urls[0], far).await.unwrap());
    assert!(close(schedule_of(&pool, &urls[0]).await.0, far));
}

#[tokio::test]
async fn reschedule_never_bypasses_key_politeness() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "polite.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    f.complete(
        c.permit,
        one(
            urls[0],
            UrlResult::Retry {
                at: Utc::now() + chrono::Duration::days(1),
            },
        ),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE politeness_keys SET next_allowed_at = now() + interval '1 hour' WHERE key = $1",
    )
    .bind("example.com")
    .execute(&pool)
    .await
    .unwrap();
    assert!(f.reschedule(&urls[0], Utc::now()).await.unwrap());
    assert!(
        f.claim(&[0], 5).await.unwrap().is_none(),
        "key delay still applies"
    );
    let left = secs_until(
        &pool,
        "SELECT extract(epoch FROM next_allowed_at - now())::float8 FROM politeness_keys
         WHERE key = $1",
        "example.com",
    )
    .await;
    assert!(left > 3500.0, "key schedule untouched: {left}");
    f.park(&key("example.com"), ParkReason::Challenge)
        .await
        .unwrap();
    make_ready(&pool, "example.com").await;
    assert!(
        f.claim(&[0], 5).await.unwrap().is_none(),
        "parked key stays parked"
    );
}

#[tokio::test]
async fn reschedule_leased_url_applies_when_it_returns_to_due() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "leased.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    let e = c.permit.epoch().0;
    let at = Utc::now() + chrono::Duration::minutes(5);
    assert!(f.reschedule(&urls[0], at).await.unwrap());
    // The running lease is undisturbed.
    assert_eq!(state_of(&pool, &urls[0]).await, ("leased".into(), Some(e)));
    f.check(&c.permit).await.expect("lease still live");
    // A later reschedule never moves the pending refetch later.
    assert!(
        f.reschedule(&urls[0], at + chrono::Duration::hours(1))
            .await
            .unwrap()
    );
    // Completion schedules a retry in a day; the forced refetch wins.
    let later = Utc::now() + chrono::Duration::days(1);
    f.complete(c.permit, one(urls[0], UrlResult::Retry { at: later }))
        .await
        .unwrap();
    assert_eq!(state_of(&pool, &urls[0]).await, ("due".into(), None));
    let (next, pending) = schedule_of(&pool, &urls[0]).await;
    assert!(close(next, at), "{next} vs {at}");
    assert_eq!(pending, None);
}

#[tokio::test]
async fn reschedule_fetched_pending_applies_at_commit() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "pending2.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    let e1 = c.permit.epoch().0;
    let day = Utc::now() + chrono::Duration::days(1);
    f.mark_fetched(&c.permit, &urls[0], day).await.unwrap();
    let at = Utc::now() + chrono::Duration::minutes(10);
    assert!(f.reschedule(&urls[0], at).await.unwrap());
    assert_eq!(
        state_of(&pool, &urls[0]).await,
        ("fetched_pending".into(), Some(e1))
    );
    f.complete(c.permit, one(urls[0], UrlResult::Fetched))
        .await
        .unwrap();
    assert_eq!(
        state_of(&pool, &urls[0]).await,
        ("fetched_pending".into(), Some(e1))
    );
    let manifest = serde_json::json!({
        "segment_id": "01JSEGMENT0000000000000002",
        "path": "segments/y.warc.zst", "cdxj_path": "cdxj/y.cdxj", "bytes": 1, "crc32c": 1,
        "record_count": 1, "created_at": Utc::now(),
        "captures": [{
            "capture_id": "01JCAPTURE0000000000000002", "captured_at": Utc::now(),
            "offset": 0, "length": 1, "warc_record_id": "<urn:uuid:y>",
            "url_key": urls[0].to_hex(), "url": "https://pending2.example.com/p/0",
            "key": "example.com", "role": "primary", "fetch_mode": "http", "epoch": e1,
            "status": 200, "media_type": "html", "use_policy_fetch": {},
            "next_fetch_at": day,
            "validators": {"etag": "\"v1\""}
        }]
    });
    let (ok,): (bool,) = sqlx::query_as("SELECT vc_commit_segment($1)")
        .bind(manifest)
        .fetch_one(&pool)
        .await
        .expect("commit");
    assert!(ok);
    assert_eq!(state_of(&pool, &urls[0]).await, ("due".into(), None));
    let (next, pending) = schedule_of(&pool, &urls[0]).await;
    assert!(close(next, at), "{next} vs {at}");
    assert_eq!(pending, None);
}

#[tokio::test]
async fn reschedule_unknown_or_terminal_is_noop() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let unknown = url("https://nowhere.example.org/x").key();
    assert!(!f.reschedule(&unknown, Utc::now()).await.unwrap());
    let urls = seed(&f, "terminal.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    f.complete(c.permit, one(urls[0], UrlResult::Disallowed))
        .await
        .unwrap();
    let before = schedule_of(&pool, &urls[0]).await;
    assert!(!f.reschedule(&urls[0], Utc::now()).await.unwrap());
    assert_eq!(state_of(&pool, &urls[0]).await, ("disabled".into(), None));
    assert_eq!(schedule_of(&pool, &urls[0]).await, before);
}
