//! `PgWorkQueue` against a migrated Postgres (testcontainers).

use std::collections::HashSet;
use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use vc_config::QueueConfig;
use vc_core::WorkId;
use vc_core::queue::{MAX_ATTEMPTS, WorkKind, WorkQueue};
use vc_queue::PgWorkQueue;

/// No back-off, so retries are immediately ready.
fn fast() -> QueueConfig {
    QueueConfig {
        backoff_base_s: 0,
        backoff_max_s: 0,
        ..QueueConfig::default()
    }
}

async fn state(pool: &PgPool, id: WorkId) -> String {
    sqlx::query_scalar("SELECT state FROM work_items WHERE work_id = $1")
        .bind(id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

const LONG: Duration = Duration::from_secs(300);

#[tokio::test]
async fn queue_at_least_once_redelivery() {
    let pool = testkit::pg().await;
    let q = PgWorkQueue::new(pool.clone(), fast());
    let id = q
        .enqueue(WorkKind::Extract, json!({"capture_id": "c1"}), None)
        .await
        .unwrap()
        .expect("new item");

    // A worker leases it and dies without ack: nothing else sees it while the lease holds.
    let first = q
        .take(WorkKind::Extract, 10, Duration::from_millis(200))
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, id);
    assert_eq!(first[0].attempts, 1);
    assert_eq!(first[0].payload, json!({"capture_id": "c1"}));
    assert!(
        q.take(WorkKind::Extract, 10, LONG)
            .await
            .unwrap()
            .is_empty()
    );

    // Once the lease expires the item is delivered again, with the attempt counted.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let again = q.take(WorkKind::Extract, 10, LONG).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, id);
    assert_eq!(again[0].attempts, 2);

    // A nack makes it ready again; an ack finishes it for good.
    q.nack(id, Duration::ZERO).await.unwrap();
    let third = q.take(WorkKind::Extract, 10, LONG).await.unwrap();
    assert_eq!(third.len(), 1);
    assert_eq!(third[0].attempts, 3);
    q.ack(id).await.unwrap();
    assert_eq!(state(&pool, id).await, "done");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        q.take(WorkKind::Extract, 10, LONG)
            .await
            .unwrap()
            .is_empty()
    );
    // ack is idempotent.
    q.ack(id).await.unwrap();
}

#[tokio::test]
async fn dead_letter_after_8() {
    assert_eq!(MAX_ATTEMPTS, 8);
    let pool = testkit::pg().await;
    let q = PgWorkQueue::new(pool.clone(), fast());
    let id = q
        .enqueue(WorkKind::Embed, json!({"record_id": 7}), None)
        .await
        .unwrap()
        .unwrap();
    for attempt in 1..=8u32 {
        let items = q.take(WorkKind::Embed, 1, LONG).await.unwrap();
        assert_eq!(items.len(), 1, "attempt {attempt} delivered");
        assert_eq!(items[0].attempts, attempt);
        q.nack(id, Duration::ZERO).await.unwrap();
        let expected = if attempt < 8 { "ready" } else { "dead" };
        assert_eq!(state(&pool, id).await, expected, "after attempt {attempt}");
    }
    assert!(q.take(WorkKind::Embed, 1, LONG).await.unwrap().is_empty());
    let dead = q.dead_letters(WorkKind::Embed, 10).await.unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].id, id);
    assert_eq!(dead[0].attempts, 8);
}

#[tokio::test]
async fn dead_letter_after_8_expired_leases() {
    // A poison item that kills its worker never gets a nack: expired leases count too.
    let pool = testkit::pg().await;
    let q = PgWorkQueue::new(pool.clone(), fast());
    let id = q
        .enqueue(WorkKind::Audit, json!({}), None)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..8 {
        let items = q
            .take(WorkKind::Audit, 1, Duration::from_millis(1))
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(q.take(WorkKind::Audit, 1, LONG).await.unwrap().is_empty());
    assert_eq!(state(&pool, id).await, "dead");
}

#[tokio::test]
async fn dedupe_key() {
    let pool = testkit::pg().await;
    let q = PgWorkQueue::new(pool.clone(), fast());
    let a = q
        .enqueue(WorkKind::CaptureReady, json!({"capture_id": "x"}), Some("x"))
        .await
        .unwrap();
    assert!(a.is_some());
    // Same key while pending (ready) → None.
    let b = q
        .enqueue(WorkKind::CaptureReady, json!({"capture_id": "x"}), Some("x"))
        .await
        .unwrap();
    assert_eq!(b, None);
    // Another kind with the same key is independent.
    assert!(
        q.enqueue(WorkKind::Extract, json!({}), Some("x"))
            .await
            .unwrap()
            .is_some()
    );
    // Still pending while leased.
    let items = q.take(WorkKind::CaptureReady, 10, LONG).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].dedupe_key.as_deref(), Some("x"));
    assert_eq!(
        q.enqueue(WorkKind::CaptureReady, json!({}), Some("x"))
            .await
            .unwrap(),
        None
    );
    // After ack the key is free again.
    q.ack(a.unwrap()).await.unwrap();
    let c = q
        .enqueue(WorkKind::CaptureReady, json!({"capture_id": "x"}), Some("x"))
        .await
        .unwrap();
    assert!(c.is_some());
    assert_ne!(c, a);
    // Items without a key never dedupe.
    let n1 = q.enqueue(WorkKind::Triage, json!({}), None).await.unwrap();
    let n2 = q.enqueue(WorkKind::Triage, json!({}), None).await.unwrap();
    assert!(n1.is_some() && n2.is_some() && n1 != n2);
}

#[tokio::test]
async fn take_respects_kind_limit_and_backoff() {
    let pool = testkit::pg().await;
    let q = PgWorkQueue::new(pool.clone(), QueueConfig::default());
    for i in 0..5 {
        q.enqueue(WorkKind::Extract, json!({"i": i}), None)
            .await
            .unwrap();
    }
    q.enqueue(WorkKind::Embed, json!({}), None).await.unwrap();
    let two = q.take(WorkKind::Extract, 2, LONG).await.unwrap();
    assert_eq!(two.len(), 2);
    assert!(two.iter().all(|i| i.kind == WorkKind::Extract));
    // FIFO among ready items.
    assert_eq!(two[0].payload, json!({"i": 0}));
    assert_eq!(two[1].payload, json!({"i": 1}));
    assert_eq!(q.take(WorkKind::Extract, 10, LONG).await.unwrap().len(), 3);
    // With the default back-off (30 s base) a nack with retry_in 0 is not ready at once.
    q.nack(two[0].id, Duration::ZERO).await.unwrap();
    assert!(
        q.take(WorkKind::Extract, 10, LONG)
            .await
            .unwrap()
            .is_empty()
    );
    let delay: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM available_at - now())::float8 FROM work_items WHERE work_id = $1",
    )
    .bind(two[0].id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!((25.0..=31.0).contains(&delay), "back-off {delay}");
}

#[tokio::test]
async fn nack_unknown_and_ack_unknown_are_not_found() {
    let pool = testkit::pg().await;
    let q = PgWorkQueue::new(pool, fast());
    assert!(matches!(
        q.ack(WorkId(999_999)).await,
        Err(vc_core::Error::NotFound(_))
    ));
    assert!(matches!(
        q.nack(WorkId(999_999), Duration::ZERO).await,
        Err(vc_core::Error::NotFound(_))
    ));
}

#[tokio::test]
async fn concurrent_takers_never_share_an_item() {
    let pool = testkit::pg().await;
    let q = PgWorkQueue::new(pool, fast());
    for i in 0..200 {
        q.enqueue(WorkKind::Extract, json!({"i": i}), None)
            .await
            .unwrap();
    }
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let q = q.clone();
        tasks.push(tokio::spawn(async move {
            let mut got = Vec::new();
            loop {
                let items = q.take(WorkKind::Extract, 7, LONG).await.unwrap();
                if items.is_empty() {
                    break;
                }
                got.extend(items.into_iter().map(|i| i.id));
            }
            got
        }));
    }
    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    let unique: HashSet<_> = all.iter().copied().collect();
    assert_eq!(all.len(), 200);
    assert_eq!(unique.len(), 200);
}

#[tokio::test]
async fn sql_enqueue_from_vc_commit_segment_is_taken() {
    // Items enqueued inside other transactions via vc_enqueue are seen by the queue.
    let pool = testkit::pg().await;
    let q = PgWorkQueue::new(pool.clone(), fast());
    let id: Option<i64> = sqlx::query_scalar("SELECT vc_enqueue('capture_ready', $1, 'cap-1')")
        .bind(json!({"capture_id": "cap-1"}))
        .fetch_one(&pool)
        .await
        .unwrap();
    let items = q.take(WorkKind::CaptureReady, 5, LONG).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, WorkId(id.unwrap()));
}

#[tokio::test]
async fn prune_and_stats_and_requeue_dead() {
    let pool = testkit::pg().await;
    let q = PgWorkQueue::new(
        pool.clone(),
        QueueConfig {
            max_attempts: 1,
            ..fast()
        },
    );
    let a = q
        .enqueue(WorkKind::Purge, json!({}), None)
        .await
        .unwrap()
        .unwrap();
    let b = q
        .enqueue(WorkKind::Purge, json!({}), None)
        .await
        .unwrap()
        .unwrap();
    q.enqueue(WorkKind::Purge, json!({}), None).await.unwrap();
    let items = q.take(WorkKind::Purge, 2, LONG).await.unwrap();
    assert_eq!(items.len(), 2);
    q.ack(a).await.unwrap();
    q.nack(b, Duration::ZERO).await.unwrap(); // max_attempts 1 → dead
    let stats = q.stats().await.unwrap();
    let get = |s: &str| {
        stats
            .iter()
            .find(|r| r.kind == WorkKind::Purge && r.state == s)
            .map(|r| r.count)
            .unwrap_or(0)
    };
    assert_eq!((get("ready"), get("done"), get("dead")), (1, 1, 1));

    // Done items older than the retention are deleted.
    sqlx::query("UPDATE work_items SET updated_at = now() - interval '8 days' WHERE work_id = $1")
        .bind(a.0)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(q.prune_done().await.unwrap(), 1);

    // A dead item can be re-queued by an operator, with its attempts reset.
    assert!(q.requeue_dead(b).await.unwrap());
    assert!(!q.requeue_dead(b).await.unwrap());
    let again = q.take(WorkKind::Purge, 5, LONG).await.unwrap();
    assert!(again.iter().any(|i| i.id == b && i.attempts == 1));
}

#[test]
fn backoff_is_exponential_and_capped() {
    let cfg = QueueConfig {
        backoff_base_s: 30,
        backoff_max_s: 600,
        ..QueueConfig::default()
    };
    let b = |n| PgWorkQueue::backoff(&cfg, n).as_secs();
    assert_eq!(b(1), 30);
    assert_eq!(b(2), 60);
    assert_eq!(b(3), 120);
    assert_eq!(b(5), 480);
    assert_eq!(b(6), 600);
    assert_eq!(b(60), 600);
    assert_eq!(b(0), 30);
}
