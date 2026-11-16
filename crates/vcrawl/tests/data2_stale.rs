//! Stale records (migration 1902): a record missing from a newer capture of its page is marked
//! stale (never deleted) with provenance and a `changes` row; it is re-activated when it
//! reappears.

#[path = "processor_support.rs"]
mod support;

use support::*;
use vc_core::CaptureRef;
use vcrawl::data::processor::Outcome;

const URL: &str = "https://events.example.org/listing";

async fn cap(r: &Rig, file: &str, minutes: i64) -> CaptureRef {
    commit(
        &r.pool,
        &r.blobs,
        Cap::html(URL, "events-test", &fixture(file), at(minutes)),
    )
    .await
}

fn stale_counts(o: &Outcome) -> (u64, u64) {
    match o {
        Outcome::Processed {
            staled, unstaled, ..
        } => (*staled, *unstaled),
        Outcome::Skipped(s) => panic!("skipped {s:?}"),
    }
}

/// `(record_key, state, stale_capture_id)` ordered by key.
async fn states(r: &Rig) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as(
        "SELECT record_key, state, stale_capture_id FROM records WHERE kind = 'event'
         ORDER BY record_key",
    )
    .fetch_all(&r.pool)
    .await
    .unwrap()
}

async fn changes(r: &Rig) -> i64 {
    count(
        &r.pool,
        "SELECT count(*) FROM changes WHERE owner_type = 'record'",
    )
    .await
}

#[tokio::test]
async fn record_missing_from_new_capture_is_marked_stale() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    let c1 = cap(&r, "listing_v1.html", 0).await;
    assert_eq!(stale_counts(&r.proc.process(&c1).await.unwrap()), (0, 0));
    let s = states(&r).await;
    assert_eq!(s.len(), 2, "{s:?}");
    assert!(s.iter().all(|x| x.1 == "active"));
    let before = changes(&r).await;

    // The fado evening is gone from the page.
    let c2 = cap(&r, "listing_v2.html", 60).await;
    assert_eq!(stale_counts(&r.proc.process(&c2).await.unwrap()), (1, 0));
    let s = states(&r).await;
    assert_eq!(s.len(), 2, "never deleted");
    let stale: Vec<_> = s.iter().filter(|x| x.1 == "stale").collect();
    assert_eq!(stale.len(), 1);
    assert_eq!(
        stale[0].2.as_deref(),
        Some(c2.capture_id.to_string().as_str()),
        "provenance: the capture that found it missing"
    );
    let (since, at): (
        Option<chrono::DateTime<chrono::Utc>>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT stale_since, stale_captured_at FROM records WHERE state = 'stale'")
        .fetch_one(&r.pool)
        .await
        .unwrap();
    assert!(since.is_some());
    assert_eq!(at, c2.captured_at);
    assert_eq!(
        changes(&r).await,
        before + 1,
        "one change row for the stale record"
    );
    // Its versions are untouched; the jazz night stays active.
    assert_eq!(
        count(&r.pool, "SELECT count(*) FROM record_versions").await,
        2
    );
    assert_eq!(
        s.iter().filter(|x| x.1 == "active").count(),
        1,
        "the record still on the page stays active"
    );

    // Redelivery of the same capture changes nothing more.
    assert_eq!(stale_counts(&r.proc.process(&c2).await.unwrap()), (0, 0));
    assert_eq!(changes(&r).await, before + 1);
}

#[tokio::test]
async fn stale_record_reactivated_when_it_reappears() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    let c1 = cap(&r, "listing_v1.html", 0).await;
    r.proc.process(&c1).await.unwrap();
    let c2 = cap(&r, "listing_v2.html", 60).await;
    r.proc.process(&c2).await.unwrap();
    let before = changes(&r).await;

    let c3 = cap(&r, "listing_v1.html", 120).await;
    assert_eq!(stale_counts(&r.proc.process(&c3).await.unwrap()), (0, 1));
    let s = states(&r).await;
    assert!(
        s.iter().all(|x| x.1 == "active" && x.2.is_none()),
        "re-activated, provenance cleared: {s:?}"
    );
    assert_eq!(
        count(
            &r.pool,
            "SELECT count(*) FROM records WHERE stale_since IS NOT NULL OR stale_captured_at IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(changes(&r).await, before + 1);
    assert_eq!(
        count(&r.pool, "SELECT count(*) FROM record_versions").await,
        2,
        "unchanged data writes no new version"
    );
}

#[tokio::test]
async fn older_capture_never_marks_stale() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    let c1 = cap(&r, "listing_v1.html", 0).await;
    let late = cap(&r, "listing_v2.html", 60).await;
    let c3 = cap(&r, "listing_v1.html", 120).await;
    r.proc.process(&c1).await.unwrap();
    r.proc.process(&c3).await.unwrap();
    // The capture from minute 60 is processed last: a newer capture saw both records.
    assert_eq!(stale_counts(&r.proc.process(&late).await.unwrap()), (0, 0));
    assert!(states(&r).await.iter().all(|x| x.1 == "active"));
}

#[tokio::test]
async fn an_older_capture_never_reactivates() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    let c1 = cap(&r, "listing_v1.html", 0).await;
    let c2 = cap(&r, "listing_v2.html", 120).await;
    let old = cap(&r, "listing_v1.html", 60).await;
    r.proc.process(&c1).await.unwrap();
    r.proc.process(&c2).await.unwrap();
    // A capture older than the one that found the record missing does not bring it back.
    assert_eq!(stale_counts(&r.proc.process(&old).await.unwrap()), (0, 0));
    assert_eq!(
        states(&r).await.iter().filter(|x| x.1 == "stale").count(),
        1
    );
}
