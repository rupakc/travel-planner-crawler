//! Durable held reference records (`held_records`, migration 1901): shared by processor
//! instances, kept across restarts, bounded and expiring.

#[path = "processor_support.rs"]
mod support;

use std::time::Duration;

use serde_json::json;
use support::*;
use vc_core::RecordKey;
use vc_core::records::{ExtractedRecord, Provenance};
use vcrawl::data::peers::PendingPeers;
use vcrawl::data::processor::Outcome;

fn numbers() -> serde_json::Value {
    json!({"records": [{"country": "ES", "numbers": [
        {"service": "general", "number": "112"},
        {"service": "police", "number": "091"}
    ]}]})
}

fn llm() -> RigOpts {
    RigOpts {
        llm: true,
        ..RigOpts::default()
    }
}

fn counts(o: &Outcome) -> (u32, usize, usize) {
    match o {
        Outcome::Processed {
            report,
            held,
            promoted,
            ..
        } => (report.inserted, *held, *promoted),
        Outcome::Skipped(r) => panic!("skipped {r:?}"),
    }
}

/// Holds wiki-a's numbers on `r` (one instance).
async fn hold_a(r: &Rig) {
    r.llm.push_json(numbers());
    let a = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://wiki-a.example.org/spain",
            "wiki-a",
            &fixture("emergency_a.html"),
            at(0),
        ),
    )
    .await;
    let o = r.proc.process(&a).await.unwrap();
    assert_eq!(counts(&o), (0, 1, 0));
}

/// Processes wiki-b's agreeing numbers on `r`.
async fn agree_b(r: &Rig) -> Outcome {
    r.llm.push_json(numbers());
    let b = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://wiki-b.example.org/spain",
            "wiki-b",
            &fixture("emergency_b.html"),
            at(5),
        ),
    )
    .await;
    r.proc.process(&b).await.unwrap()
}

#[tokio::test]
async fn held_record_promoted_by_another_instance_after_restart() {
    let pool = testkit::pg().await;
    {
        let first = rig_on(pool.clone(), &["wiki-a", "wiki-b"], llm()).await;
        hold_a(&first).await;
        assert_eq!(first.proc.pending().len().await.unwrap(), 1);
        // `first` is dropped here: the process "restarts".
    }
    assert_eq!(count(&pool, "SELECT count(*) FROM held_records").await, 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM records").await, 0);

    let second = rig_on(pool.clone(), &["wiki-a", "wiki-b"], llm()).await;
    assert_eq!(
        second.proc.pending().len().await.unwrap(),
        1,
        "the new instance sees the held record"
    );
    let o = agree_b(&second).await;
    assert_eq!(counts(&o), (1, 0, 1), "B accepted, A promoted");
    assert!(second.proc.pending().is_empty().await.unwrap());
    assert_eq!(count(&pool, "SELECT count(*) FROM held_records").await, 0);
    let srcs: Vec<(String,)> = sqlx::query_as(
        "SELECT source_id FROM records WHERE kind = 'emergency_numbers' AND state = 'active'
         ORDER BY source_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(srcs, vec![("wiki-a".into(),), ("wiki-b".into(),)]);
    // A was promoted with its own provenance (its own URL and capture).
    let a_url: String =
        sqlx::query_scalar("SELECT url FROM records WHERE source_id = 'wiki-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(a_url, "https://wiki-a.example.org/spain");
}

#[tokio::test]
async fn expired_held_record_is_not_a_peer() {
    let r = rig(&["wiki-a", "wiki-b"], llm()).await;
    hold_a(&r).await;
    sqlx::query("UPDATE held_records SET held_at = now() - interval '8 days'")
        .execute(&r.pool)
        .await
        .unwrap();
    assert!(r.proc.pending().is_empty().await.unwrap(), "expired");
    let o = agree_b(&r).await;
    assert_eq!(
        counts(&o),
        (0, 1, 0),
        "no agreement from an expired record: B is held, nothing promoted"
    );
    assert_eq!(count(&r.pool, "SELECT count(*) FROM records").await, 0);
    // The expired row was deleted when B was held; only B remains.
    let left: Vec<(String,)> = sqlx::query_as("SELECT source_id FROM held_records")
        .fetch_all(&r.pool)
        .await
        .unwrap();
    assert_eq!(left, vec![("wiki-b".into(),)]);
}

async fn held_row(pool: &sqlx::PgPool) -> (ExtractedRecord, Provenance) {
    let (rec, prov): (serde_json::Value, serde_json::Value) =
        sqlx::query_as("SELECT record, provenance FROM held_records LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
    (
        serde_json::from_value(rec).unwrap(),
        serde_json::from_value(prov).unwrap(),
    )
}

#[tokio::test]
async fn held_set_is_bounded_oldest_first() {
    let r = rig(&["wiki-a", "wiki-b"], llm()).await;
    hold_a(&r).await;
    let (rec, prov) = held_row(&r.pool).await;
    let p = PendingPeers::with_limits(r.pool.clone(), Duration::from_secs(3600), 2);
    for k in ["k1", "k2", "k3"] {
        let mut x = rec.clone();
        x.record_key = RecordKey(k.into());
        p.hold(x, prov.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(p.len().await.unwrap(), 2);
    let keys: Vec<(String,)> =
        sqlx::query_as("SELECT record_key FROM held_records ORDER BY record_key")
            .fetch_all(&r.pool)
            .await
            .unwrap();
    assert_eq!(keys, vec![("k2".into(),), ("k3".into(),)]);
}

#[tokio::test]
async fn older_capture_never_replaces_a_newer_held_record() {
    let r = rig(&["wiki-a", "wiki-b"], llm()).await;
    hold_a(&r).await;
    let (rec, prov) = held_row(&r.pool).await;
    let p = r.proc.pending();
    let mut older = prov.clone();
    older.capture.captured_at -= chrono::Duration::days(1);
    let mut stale = rec.clone();
    stale.data = json!({"country": "ES", "numbers": [{"service": "general", "number": "999"}]});
    p.hold(stale, older).await.unwrap();
    let (now_rec, _) = held_row(&r.pool).await;
    assert_eq!(now_rec.data, rec.data, "the newer capture's record stays");
    // A newer capture does replace it.
    let mut newer = prov.clone();
    newer.capture.captured_at += chrono::Duration::days(1);
    let mut fresh = rec.clone();
    fresh.data = json!({"country": "ES", "numbers": [{"service": "general", "number": "112"}]});
    p.hold(fresh.clone(), newer).await.unwrap();
    let (now_rec, _) = held_row(&r.pool).await;
    assert_eq!(now_rec.data, fresh.data);
    assert_eq!(p.len().await.unwrap(), 1);
    // Release forgets it.
    p.release(&rec.kind, &prov.source_id, &rec.record_key)
        .await
        .unwrap();
    assert!(p.is_empty().await.unwrap());
}
