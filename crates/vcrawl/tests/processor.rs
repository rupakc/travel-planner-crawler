//! The `processor` data path (plan Task 15): committed captures → records, against a migrated
//! Postgres, the real kind registry and parsers, and the fake LLM.

#[path = "processor_support.rs"]
mod support;

use serde_json::json;
use support::*;
use vc_core::queue::{WorkKind, WorkQueue};
use vc_core::store::CaptureRole;
use vcrawl::data::processor::{Outcome, SkipReason};

const EVENT_URL: &str = "https://events.example.org/e/jazz-night";

fn processed(o: &Outcome) -> (&vc_core::index::UpsertReport, usize, usize, u64, bool) {
    match o {
        Outcome::Processed {
            report,
            held,
            promoted,
            links,
            payload_changed,
            ..
        } => (report, *held, *promoted, *links, *payload_changed),
        Outcome::Skipped(r) => panic!("expected processed, got skipped {r:?}"),
    }
}

#[tokio::test]
async fn version_bump_on_content_change() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    let q = vc_queue::PgWorkQueue {
        pool: r.pool.clone(),
        config: vc_config::QueueConfig::default(),
    };

    // v1 → one record, version 1.
    commit(
        &r.pool,
        &r.blobs,
        Cap::html(EVENT_URL, "events-test", &fixture("event_v1.html"), at(0)),
    )
    .await;
    assert_eq!(r.proc.tick(8).await.unwrap(), 1);
    let v: Vec<(i64, i32)> =
        sqlx::query_as("SELECT record_id, current_version FROM records WHERE kind = 'event'")
            .fetch_all(&r.pool)
            .await
            .unwrap();
    assert_eq!(v.len(), 1);
    let (record_id, version) = v[0];
    assert_eq!(version, 1);

    // The same content again (a recrawl): no new version.
    commit(
        &r.pool,
        &r.blobs,
        Cap::html(EVENT_URL, "events-test", &fixture("event_v1.html"), at(60)),
    )
    .await;
    assert_eq!(r.proc.tick(8).await.unwrap(), 1);
    assert_eq!(
        count(&r.pool, "SELECT count(*) FROM record_versions").await,
        1,
        "unchanged content never writes a version"
    );

    // The price changes: version 2, the old one superseded, a change row written.
    commit(
        &r.pool,
        &r.blobs,
        Cap::html(EVENT_URL, "events-test", &fixture("event_v2.html"), at(120)),
    )
    .await;
    assert_eq!(r.proc.tick(8).await.unwrap(), 1);
    let rows: Vec<(i32, serde_json::Value, bool)> = sqlx::query_as(
        "SELECT version, data, superseded_at IS NULL FROM record_versions
         WHERE record_id = $1 ORDER BY version",
    )
    .bind(record_id)
    .fetch_all(&r.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, 1);
    assert!(!rows[0].2, "v1 superseded");
    assert_eq!(rows[1].0, 2);
    assert!(rows[1].2, "v2 current");
    assert_ne!(rows[0].1, rows[1].1);
    assert!(
        rows[1].1.to_string().contains("22"),
        "v2 carries the new price"
    );
    assert_eq!(
        count(&r.pool, "SELECT current_version::bigint FROM records").await,
        2
    );
    assert!(
        count(
            &r.pool,
            "SELECT count(*) FROM changes WHERE owner_type = 'record' AND op = 'upsert'"
        )
        .await
            >= 2
    );
    // Every capture_ready item was acked.
    assert!(
        q.take(
            WorkKind::CaptureReady,
            10,
            std::time::Duration::from_secs(60)
        )
        .await
        .unwrap()
        .is_empty()
    );
    // The final use policy was recorded on the captures.
    assert_eq!(
        count(
            &r.pool,
            "SELECT count(*) FROM captures WHERE use_policy_final IS NULL"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn processor_idempotent_on_redelivery() {
    let r = rig(
        &["events-test"],
        RigOpts {
            frontier: true,
            ..RigOpts::default()
        },
    )
    .await;
    let c = commit(
        &r.pool,
        &r.blobs,
        Cap::html(EVENT_URL, "events-test", &fixture("event_v1.html"), at(0)),
    )
    .await;
    let q = vc_queue::PgWorkQueue {
        pool: r.pool.clone(),
        config: vc_config::QueueConfig::default(),
    };
    let items = q
        .take(
            WorkKind::CaptureReady,
            10,
            std::time::Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(items.len(), 1);

    let first = r.proc.process_item(&items[0]).await.unwrap();
    let (rep, _, _, links, changed) = processed(&first);
    assert_eq!(rep.inserted, 1);
    assert!(changed);
    assert!(links > 0);
    let snapshot = |pool: sqlx::PgPool| async move {
        (
            count(&pool, "SELECT count(*) FROM records").await,
            count(&pool, "SELECT count(*) FROM record_versions").await,
            count(&pool, "SELECT count(*) FROM entities").await,
            count(&pool, "SELECT count(*) FROM changes").await,
            count(&pool, "SELECT count(*) FROM urls").await,
        )
    };
    let before = snapshot(r.pool.clone()).await;

    // Redelivery (lease expired before the ack): same item again, then the same capture.
    let again = r.proc.process_item(&items[0]).await.unwrap();
    let (rep, _, _, links, _) = processed(&again);
    assert_eq!((rep.inserted, rep.new_versions), (0, 0));
    assert_eq!(links, 0, "links already enqueued");
    r.proc.process(&c).await.unwrap();
    assert_eq!(
        snapshot(r.pool.clone()).await,
        before,
        "no duplicate writes"
    );
}

#[tokio::test]
async fn links_in_scope_are_enqueued() {
    let r = rig(
        &["events-test"],
        RigOpts {
            frontier: true,
            ..RigOpts::default()
        },
    )
    .await;
    commit(
        &r.pool,
        &r.blobs,
        Cap::html(EVENT_URL, "events-test", &fixture("event_v1.html"), at(0)),
    )
    .await;
    r.proc.tick(8).await.unwrap();
    let urls: Vec<(String, i16, Option<String>)> =
        sqlx::query_as("SELECT url, depth, source_id FROM urls ORDER BY url")
            .fetch_all(&r.pool)
            .await
            .unwrap();
    let names: Vec<&str> = urls.iter().map(|u| u.0.as_str()).collect();
    assert_eq!(
        names,
        vec!["https://events.example.org/e/fado-evening"],
        "{urls:?}"
    );
    assert_eq!(urls[0].1, 1);
    assert_eq!(urls[0].2.as_deref(), Some("events-test"));
}

#[tokio::test]
async fn non_processable_captures_are_skipped() {
    let r = rig(&["events-test", "blog-test"], RigOpts::default()).await;
    let page = fixture("event_v1.html");
    let mut c404 = Cap::html(
        "https://events.example.org/gone",
        "events-test",
        &page,
        at(0),
    );
    c404.status = Some(404);
    let c404 = commit(&r.pool, &r.blobs, c404).await;
    assert_eq!(
        r.proc.process(&c404).await.unwrap(),
        Outcome::Skipped(SkipReason::Status)
    );

    let mut sub = Cap::html(
        "https://events.example.org/app.js",
        "events-test",
        &page,
        at(1),
    );
    sub.role = CaptureRole::Subresource;
    let sub = commit(&r.pool, &r.blobs, sub).await;
    assert_eq!(
        r.proc.process(&sub).await.unwrap(),
        Outcome::Skipped(SkipReason::NotPrimary)
    );

    let unknown = commit(
        &r.pool,
        &r.blobs,
        Cap::html("https://x.example.org/", "nope-src", &page, at(2)),
    )
    .await;
    assert_eq!(
        r.proc.process(&unknown).await.unwrap(),
        Outcome::Skipped(SkipReason::NoSource)
    );

    sqlx::query("UPDATE sources SET enabled = false WHERE source_id = 'events-test'")
        .execute(&r.pool)
        .await
        .unwrap();
    let off = commit(
        &r.pool,
        &r.blobs,
        Cap::html(EVENT_URL, "events-test", &page, at(3)),
    )
    .await;
    assert_eq!(
        r.proc.process(&off).await.unwrap(),
        Outcome::Skipped(SkipReason::SourceDisabled)
    );

    let missing = vc_core::CaptureRef {
        capture_id: vc_core::CaptureId::new(),
        captured_at: at(4),
    };
    assert_eq!(
        r.proc.process(&missing).await.unwrap(),
        Outcome::Skipped(SkipReason::Missing)
    );
    assert_eq!(count(&r.pool, "SELECT count(*) FROM records").await, 0);
    // Skipped items are acked, not retried.
    r.proc.tick(16).await.unwrap();
    let q = vc_queue::PgWorkQueue {
        pool: r.pool.clone(),
        config: vc_config::QueueConfig::default(),
    };
    assert!(
        q.take(
            WorkKind::CaptureReady,
            10,
            std::time::Duration::from_secs(60)
        )
        .await
        .unwrap()
        .is_empty()
    );
}

#[tokio::test]
async fn high_stakes_kinds_refused_from_editorial_sources() {
    let r = rig(
        &["blog-test"],
        RigOpts {
            llm: true,
            ..RigOpts::default()
        },
    )
    .await;
    let c = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://blog.example.org/vietnam-visa",
            "blog-test",
            &fixture("visa_blog.html"),
            at(0),
        ),
    )
    .await;
    let o = r.proc.process(&c).await.unwrap();
    let (rep, held, _, _, _) = processed(&o);
    assert_eq!((rep.inserted, held), (0, 0));
    assert_eq!(r.llm.calls(), 0, "refused before any model call");
    assert_eq!(r.ledger.open_reservations(), 0);
    assert_eq!(count(&r.pool, "SELECT count(*) FROM records").await, 0);
}

fn numbers() -> serde_json::Value {
    json!({"records": [{"country": "ES", "numbers": [
        {"service": "general", "number": "112"},
        {"service": "police", "number": "091"}
    ]}]})
}

#[tokio::test]
async fn reference_numbers_held_until_a_second_source_agrees() {
    let r = rig(
        &["wiki-a", "wiki-b"],
        RigOpts {
            llm: true,
            ..RigOpts::default()
        },
    )
    .await;

    // One reference source: extracted, but held (not servable).
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
    let (rep, held, promoted, _, _) = {
        let o = r.proc.process(&a).await.unwrap();
        let p = processed(&o);
        (*p.0, p.1, p.2, p.3, p.4)
    };
    assert_eq!((rep.inserted, held, promoted), (0, 1, 0));
    assert_eq!(r.llm.calls(), 1);
    assert_eq!(r.proc.pending().len().await.unwrap(), 1);
    assert_eq!(count(&r.pool, "SELECT count(*) FROM records").await, 0);

    // Redelivery of the same capture keeps one held record.
    r.llm.push_json(numbers());
    r.proc.process(&a).await.unwrap();
    assert_eq!(r.proc.pending().len().await.unwrap(), 1);

    // A second, independent reference source agrees: it is accepted and the held one promoted.
    r.llm.push_json(numbers());
    let b = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://wiki-b.example.net/spain",
            "wiki-b",
            &fixture("emergency_b.html"),
            at(5),
        ),
    )
    .await;
    let o = r.proc.process(&b).await.unwrap();
    let (rep, held, promoted, _, _) = processed(&o);
    assert_eq!((rep.inserted, held, promoted), (1, 0, 1));
    assert!(r.proc.pending().is_empty().await.unwrap());
    let srcs: Vec<(String, bool)> = sqlx::query_as(
        "SELECT source_id, conflict FROM records WHERE kind = 'emergency_numbers' ORDER BY source_id",
    )
    .fetch_all(&r.pool)
    .await
    .unwrap();
    assert_eq!(
        srcs,
        vec![("wiki-a".to_string(), false), ("wiki-b".to_string(), false)]
    );
}

#[tokio::test]
async fn disagreeing_reference_sources_stay_held() {
    let r = rig(
        &["wiki-a", "wiki-b"],
        RigOpts {
            llm: true,
            ..RigOpts::default()
        },
    )
    .await;
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
    r.proc.process(&a).await.unwrap();
    // B only carries 112 → the verbatim sets differ: no agreement, both held.
    r.llm.push_json(json!({"records": [{"country": "ES", "numbers": [{"service": "general", "number": "112"}]}]}));
    let b = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://wiki-b.example.net/spain",
            "wiki-b",
            &fixture("emergency_b.html"),
            at(5),
        ),
    )
    .await;
    let o = r.proc.process(&b).await.unwrap();
    let (rep, held, promoted, _, _) = processed(&o);
    assert_eq!((rep.inserted, held, promoted), (0, 1, 0));
    assert_eq!(r.proc.pending().len().await.unwrap(), 2);
    assert_eq!(count(&r.pool, "SELECT count(*) FROM records").await, 0);
}

#[tokio::test]
async fn llm_disabled_makes_no_model_call() {
    let r = rig(&["wiki-a"], RigOpts::default()).await;
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
    let (rep, held, _, _, _) = processed(&o);
    assert_eq!((rep.inserted, held), (0, 0));
    assert_eq!(r.llm.calls(), 0);
    assert_eq!(r.ledger.open_reservations(), 0);
}

#[tokio::test]
async fn unchanged_payload_skips_the_llm() {
    let r = rig(
        &["wiki-a"],
        RigOpts {
            llm: true,
            ..RigOpts::default()
        },
    )
    .await;
    r.llm.push_json(numbers());
    let page = fixture("emergency_a.html");
    let a = commit(
        &r.pool,
        &r.blobs,
        Cap::html("https://wiki-a.example.org/spain", "wiki-a", &page, at(0)),
    )
    .await;
    r.proc.process(&a).await.unwrap();
    assert_eq!(r.llm.calls(), 1);
    // A recrawl with an identical payload: no second paid call.
    let a2 = commit(
        &r.pool,
        &r.blobs,
        Cap::html("https://wiki-a.example.org/spain", "wiki-a", &page, at(60)),
    )
    .await;
    let o = r.proc.process(&a2).await.unwrap();
    assert!(!processed(&o).4, "payload unchanged");
    assert_eq!(r.llm.calls(), 1);
}

#[tokio::test]
async fn sources_of_one_publisher_never_agree() {
    let r = rig(
        &["wiki-a", "wiki-b"],
        RigOpts {
            llm: true,
            ..RigOpts::default()
        },
    )
    .await;
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
    r.proc.process(&a).await.unwrap();
    // Another source id, but the same registrable domain: one voice, nothing promoted.
    r.llm.push_json(numbers());
    let b = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://mirror.example.org/spain",
            "wiki-b",
            &fixture("emergency_b.html"),
            at(5),
        ),
    )
    .await;
    let o = r.proc.process(&b).await.unwrap();
    let (rep, held, promoted, _, _) = processed(&o);
    assert_eq!((rep.inserted, held, promoted), (0, 1, 0));
    assert_eq!(r.proc.pending().len().await.unwrap(), 2);
    assert_eq!(count(&r.pool, "SELECT count(*) FROM records").await, 0);
}

/// Inserts an active official `emergency_numbers` record (country-level) for `country`.
async fn official_numbers(pool: &sqlx::PgPool, i: i64, country: &str, age_s: i64) {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO records (kind, source_id, url_key, record_key, licence_class, current_version,
                              current_data_hash, source_class, host, updated_at)
         VALUES ('emergency_numbers', 'gov-' || $1::text, decode(md5($1::text), 'hex'), 'k', 'facts_only', 1,
                 '\\x00', 'official', 'gov' || $1::text || '.example', now() - make_interval(secs => $2))
         RETURNING record_id",
    )
    .bind(i)
    .bind(age_s as f64)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO record_versions (record_id, version, capture_id, captured_at, data, data_hash,
                                      kind_schema, extractor_ver, method, licence_class, use_policy)
         VALUES ($1, 1, 'c', now(), $2, '\\x00', 'emergency_numbers@1', 't', 'llm', 'facts_only', '{}')",
    )
    .bind(id)
    .bind(json!({"country": country, "numbers": [{"number": "112"}]}))
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn index_peers_filter_by_place_before_the_limit() {
    use vc_core::{KindId, SourceId};
    use vcrawl::data::peers::{MAX_INDEX_PEERS, PeerScope, index_peers};
    let pool = testkit::pg().await;
    official_numbers(&pool, 0, "ES", 3600).await;
    for i in 1..=MAX_INDEX_PEERS + 20 {
        official_numbers(&pool, i, "FR", 0).await;
    }
    let kinds = [KindId::new("emergency_numbers")];
    let me = SourceId("wiki-a".into());
    let es = PeerScope::of(&[], &json!({"country_code": "ES"}));
    let peers = index_peers(&pool, &kinds, &me, &es).await.unwrap();
    assert_eq!(peers.len(), 1, "the older ES peer is not crowded out");
    assert_eq!(peers[0].source_id.0, "gov-0");
    assert_eq!(peers[0].publisher.as_deref(), Some("gov0.example"));
    // No location at all → no peers.
    let none = PeerScope::of(&[], &json!({}));
    assert!(
        index_peers(&pool, &kinds, &me, &none)
            .await
            .unwrap()
            .is_empty()
    );
}
