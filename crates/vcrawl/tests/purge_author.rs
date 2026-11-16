//! Author erasure (final review B2, B3): the scrub touches only the author's records and
//! attribution fields, every capture holding the author's posts is cut (later re-captures that
//! only touched a record included), the name is never stored, and a re-ingested dump leaves no
//! trace.

mod admin_common;

use admin_common::*;
use testkit::MemBlobStore;
use testkit::vc_index_fixtures::{licence, prov_at, record};
use vc_core::index::{PurgeSelector, RecordSink, TombstoneReason};
use vc_core::licence::LicenceClass;
use vc_core::store::CaptureMeta;
use vc_core::{CaptureRef, SegmentId};
use vcrawl::admin::purge::{process_purge, request_purge};

const JORDAN: &str = "Jordan";
const SRC: &str = "stackexchange-travel";

async fn ingest(
    pool: &sqlx::PgPool,
    cap: CaptureRef,
    url: &str,
    key: &str,
    data: serde_json::Value,
    attribution: &str,
) {
    let lic = licence(LicenceClass::ShareAlike, Some("CC-BY-SA-4.0"), attribution);
    let mut p = prov_at(SRC, lic, url, cap.captured_at);
    p.capture = cap;
    vc_index::PgIndex::new(pool.clone())
        .upsert(vec![record("qa_thread", key, data)], &p)
        .await
        .expect("upsert");
}

fn cap(url: &str, day: u32, hour: u32) -> CaptureMeta {
    meta(url, at(day, hour), Some(SRC))
}

/// A row snapshot of a record and its versions (everything the scrub could touch).
async fn snapshot(pool: &sqlx::PgPool, key: &str) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'record', (SELECT to_jsonb(r) - 'updated_at' FROM records r WHERE r.record_key = $1),
            'versions', (SELECT jsonb_agg(to_jsonb(v) ORDER BY v.version) FROM record_versions v
                         JOIN records r ON r.record_id = v.record_id WHERE r.record_key = $1))",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .expect("snapshot")
}

/// B2: the scrub is scoped to the author's records and attribution fields. "Jordan" inside
/// "Jordanson", a country called Jordan and another user's post that mentions Jordan are
/// untouched; the author's record is scrubbed with its hash recomputed and a change written.
#[tokio::test]
async fn author_scrub_touches_only_the_authors_records() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let q1 = "https://travel.stackexchange.com/questions/1";
    let q2 = "https://travel.stackexchange.com/questions/2";
    let q3 = "https://travel.stackexchange.com/questions/3";
    let (c1, c2, c3) = (cap(q1, 8, 1), cap(q2, 8, 2), cap(q3, 8, 3));
    let refs = (c1.capture_ref(), c2.capture_ref(), c3.capture_ref());
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(8, 1)),
        false,
        &[entry(c1, "q1"), entry(c2, "q2"), entry(c3, "q3")],
    )
    .await;
    ingest(
        &pool,
        refs.0,
        q1,
        "1",
        serde_json::json!({
            "name": "Petra from Amman?",
            "question": {"author": JORDAN, "score": 3, "body": "I am Jordan, any tips?"},
            "answers": [{"author": "Bob", "score": 5, "body": "Ask Jordan's hotel"}],
        }),
        "Jordan, Bob; Stack Exchange, CC BY-SA 4.0",
    )
    .await;
    ingest(
        &pool,
        refs.1,
        q2,
        "2",
        serde_json::json!({
            "name": "Visa for Jordan",
            "question": {"author": "Jordanson", "score": 1, "body": "Is Jordan visa-free? Jordan!"},
            "answers": [],
        }),
        "Jordanson; Stack Exchange, CC BY-SA 4.0",
    )
    .await;
    ingest(
        &pool,
        refs.2,
        q3,
        "3",
        serde_json::json!({"name": "Jordan", "country": "Jordan", "author": {"name": "Alice"}}),
        "Alice; Stack Exchange, CC BY-SA 4.0",
    )
    .await;
    let before2 = snapshot(&pool, "2").await;
    let before3 = snapshot(&pool, "3").await;

    let id = request_purge(
        &pool,
        &PurgeSelector::Author(JORDAN.into()),
        TombstoneReason::OptOut,
        "t",
    )
    .await
    .expect("request");
    let out = process_purge(&pool, &store, id, at(9, 0))
        .await
        .expect("purge");
    assert_eq!(out.records_tombstoned, 1, "{out:?}");

    assert_eq!(
        snapshot(&pool, "2").await,
        before2,
        "Jordanson's thread was touched"
    );
    assert_eq!(
        snapshot(&pool, "3").await,
        before3,
        "the country record was touched"
    );

    let (data, hash, attribution, excerpt): (
        serde_json::Value,
        Vec<u8>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT v.data, v.data_hash, v.attribution, v.excerpt FROM record_versions v
             JOIN records r ON r.record_id = v.record_id WHERE r.record_key = '1'",
    )
    .fetch_one(&pool)
    .await
    .expect("version");
    assert_eq!(data["question"]["author"], "[removed]");
    assert_eq!(data["question"]["body"], "[removed]");
    assert_eq!(data["question"]["score"], 3);
    assert_eq!(
        data["answers"][0]["author"], "Bob",
        "other authors' posts are kept"
    );
    assert_eq!(hash, vc_index::data_hash(&data), "data_hash recomputed");
    assert_eq!(
        attribution.as_deref(),
        Some("[removed], Bob; Stack Exchange, CC BY-SA 4.0")
    );
    assert!(excerpt.is_none());
    let (cur, licence): (Vec<u8>, serde_json::Value) =
        sqlx::query_as("SELECT current_data_hash, licence FROM records WHERE record_key = '1'")
            .fetch_one(&pool)
            .await
            .expect("record");
    assert_eq!(cur, hash);
    assert_eq!(
        licence["attribution"],
        "[removed], Bob; Stack Exchange, CC BY-SA 4.0"
    );
    let changes = count(
        &pool,
        "SELECT count(*) FROM changes c JOIN records r ON c.owner_id = r.record_id::text
         WHERE r.record_key = '1' AND c.op = 'tombstone'",
    )
    .await;
    assert!(changes >= 1);
}

/// B3: a later capture that only re-saw the author's record (no new version) is cut out too,
/// and the name is never stored: requests and standing exclusions keep only a hash.
#[tokio::test]
async fn author_purge_cuts_every_capture_and_stores_only_a_hash() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let q = "https://travel.stackexchange.com/questions/9";
    let other = "https://travel.stackexchange.com/questions/10";
    let data = serde_json::json!({
        "name": "Wadi Rum camps",
        "question": {"author": JORDAN, "score": 2, "body": "Which camp?"},
        "answers": [],
    });
    let (first, keep) = (cap(q, 8, 1), cap(other, 8, 1));
    let (first_ref, keep_ref) = (first.capture_ref(), keep.capture_ref());
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(8, 1)),
        false,
        &[entry(first, "v1"), entry(keep, "x")],
    )
    .await;
    ingest(
        &pool,
        first_ref,
        q,
        "9",
        data.clone(),
        "Jordan; Stack Exchange",
    )
    .await;
    // A day later the same page is captured again; the record is only touched.
    let again = cap(q, 9, 1);
    let again_ref = again.capture_ref();
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(9, 1)),
        false,
        &[entry(again, "v1 again")],
    )
    .await;
    ingest(
        &pool,
        again_ref,
        q,
        "9",
        data.clone(),
        "Jordan; Stack Exchange",
    )
    .await;
    assert_eq!(
        count(&pool, "SELECT count(*) FROM record_versions").await,
        1,
        "a touch, not a version"
    );

    let id = request_purge(
        &pool,
        &PurgeSelector::Author(JORDAN.into()),
        TombstoneReason::OptOut,
        "t",
    )
    .await
    .expect("request");
    let out = process_purge(&pool, &store, id, at(10, 0))
        .await
        .expect("purge");
    assert_eq!(out.captures_removed, 2, "{out:?}");
    let caps: Vec<String> = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_all(&pool)
        .await
        .expect("caps");
    assert_eq!(caps, vec![keep_ref.capture_id.to_string()]);

    let stored = count(
        &pool,
        "SELECT (SELECT count(*) FROM purge_requests WHERE strpos(selector::text, 'Jordan') > 0)
              + (SELECT count(*) FROM tombstones WHERE strpos(selector, 'Jordan') > 0)",
    )
    .await;
    assert_eq!(stored, 0, "the plain name is stored");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM tombstones WHERE selector_type = 'author' AND selector LIKE 'sha256:%'").await,
        1
    );
}

/// B3: the next dump re-ingests the author's thread (and a new one): nothing of theirs comes
/// back, other authors' records do.
#[tokio::test]
async fn reingested_dump_leaves_no_trace_of_a_purged_author() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let q = "https://travel.stackexchange.com/questions/20";
    let first = cap(q, 8, 1);
    let first_ref = first.capture_ref();
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(8, 1)),
        false,
        &[entry(first, "d")],
    )
    .await;
    let thread = serde_json::json!({
        "name": "Aqaba diving",
        "question": {"author": JORDAN, "score": 2, "body": "Best reef?"},
        "answers": [],
    });
    ingest(
        &pool,
        first_ref,
        q,
        "20",
        thread.clone(),
        "Jordan; Stack Exchange",
    )
    .await;
    let id = request_purge(
        &pool,
        &PurgeSelector::Author(JORDAN.into()),
        TombstoneReason::OptOut,
        "t",
    )
    .await
    .expect("request");
    process_purge(&pool, &store, id, at(9, 0))
        .await
        .expect("purge");

    // The next dump: the same thread, a new thread by the author, one by someone else.
    let dump = cap("https://archive.example/travel.stackexchange.com.7z", 10, 1);
    let dump_ref = dump.capture_ref();
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(10, 1)),
        false,
        &[entry(dump, "dump")],
    )
    .await;
    ingest(&pool, dump_ref, q, "20", thread, "Jordan; Stack Exchange").await;
    ingest(
        &pool,
        dump_ref,
        "https://travel.stackexchange.com/questions/21",
        "21",
        serde_json::json!({"name": "Petra by night", "answers": [{"author": JORDAN, "body": "go"}]}),
        "Jordan; Stack Exchange",
    )
    .await;
    ingest(
        &pool,
        dump_ref,
        "https://travel.stackexchange.com/questions/22",
        "22",
        serde_json::json!({"name": "Dead Sea", "question": {"author": "Bob", "body": "salt?"}}),
        "Bob; Stack Exchange",
    )
    .await;
    let mentions = count(
        &pool,
        "SELECT (SELECT count(*) FROM record_versions WHERE strpos(data::text || coalesce(attribution, ''), 'Jordan') > 0)
              + (SELECT count(*) FROM records WHERE strpos(coalesce(licence::text, '') || coalesce(title, '') || coalesce(search_text, ''), 'Jordan') > 0)",
    )
    .await;
    assert_eq!(mentions, 0, "the author came back with the dump");
    let served: Vec<String> = sqlx::query_scalar(
        "SELECT record_key FROM records WHERE state = 'active' ORDER BY record_key",
    )
    .fetch_all(&pool)
    .await
    .expect("records");
    assert_eq!(served, vec!["22".to_string()]);
}
