//! `admin purge` leaves no trace in `held_records` or the extracted-text tier (`text/` and
//! `ephemeral/text/`), for URL and author (Stack Exchange) purges.

mod admin_common;

use admin_common::*;
use serde_json::json;
use testkit::MemBlobStore;
use testkit::vc_index_fixtures::{licence, prov_at, record};
use vc_core::index::{PurgeSelector, TombstoneReason};
use vc_core::licence::LicenceClass;
use vc_core::store::{BlobStore, CaptureMeta};
use vc_core::{CanonicalUrl, SegmentId};
use vcrawl::admin::purge::{process_purge, request_purge};

const ALICE: &str = "Alice \"Wanderer\" Example";
const SE: &str = "stackexchange-travel";

/// A held reference record (`held_records`, never served) for `url` with `data`.
async fn hold(pool: &sqlx::PgPool, key: &str, url: &str, data: serde_json::Value) {
    let rec = record("emergency_numbers", key, data);
    let p = prov_at(
        SE,
        licence(
            LicenceClass::ShareAlike,
            Some("CC-BY-SA-4.0"),
            "Stack Exchange",
        ),
        url,
        at(8, 1),
    );
    sqlx::query(
        "INSERT INTO held_records (kind, source_id, record_key, record, provenance, captured_at)
         VALUES ('emergency_numbers', $1, $2, $3, $4, $5)",
    )
    .bind(SE)
    .bind(key)
    .bind(serde_json::to_value(&rec).expect("record json"))
    .bind(serde_json::to_value(&p).expect("prov json"))
    .bind(at(8, 1))
    .execute(pool)
    .await
    .expect("held");
}

/// Text-tier rows (the processor's `TextRow` shape) for captures, in `text/` or
/// `ephemeral/text/`.
async fn tier(
    store: &MemBlobStore,
    sid: SegmentId,
    ephemeral: bool,
    caps: &[(&CaptureMeta, &str)],
) {
    let rows: Vec<_> = caps
        .iter()
        .map(|(m, text)| {
            json!({
                "capture_id": m.capture_id.to_string(), "captured_at": m.captured_at,
                "url": m.url.to_string(), "url_key": m.url_key.to_hex(),
                "source_id": SE, "media_type": "html", "status": 200,
                "title": null, "lang": "en", "main_text": text,
                "structured": [{"author": ALICE}],
            })
        })
        .collect();
    vc_store::text::put_text_tier(store, sid, ephemeral, &rows, 3)
        .await
        .expect("tier");
}

/// Every text-tier line in the store, as JSON text.
async fn tier_text(store: &MemBlobStore) -> String {
    let mut out = String::new();
    for p in store.paths() {
        if p.starts_with("text/") || p.starts_with("ephemeral/text/") {
            let b = store.get(&p, None).await.expect("get");
            let rows: Vec<serde_json::Value> =
                vc_store::text::decode_text_tier(&b).expect("decode");
            for r in rows {
                out.push_str(&r.to_string());
                out.push('\n');
            }
        }
    }
    out
}

async fn held_keys(pool: &sqlx::PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT record_key FROM held_records ORDER BY record_key")
        .fetch_all(pool)
        .await
        .expect("held")
}

#[tokio::test]
async fn purge_url_leaves_no_trace_in_held_records_or_text_tier() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let sid = SegmentId::at(at(8, 1));
    let gone = meta("https://g.example/drop", at(8, 1), Some(SE));
    let kept = meta("https://g.example/keep", at(8, 1), Some(SE));
    let mut noarchive = meta("https://g.example/drop", at(8, 2), Some(SE));
    noarchive.raw_expires_at = Some(at(30, 0));
    commit_segment(
        &pool,
        &store,
        sid,
        false,
        &[
            entry(gone.clone(), "d"),
            entry(kept.clone(), "k"),
            entry(noarchive.clone(), "n"),
        ],
    )
    .await;
    tier(
        &store,
        sid,
        false,
        &[(&gone, "drop me"), (&kept, "keep me")],
    )
    .await;
    tier(&store, sid, true, &[(&noarchive, "drop me too")]).await;
    hold(&pool, "a", "https://g.example/drop", json!({"n": "112"})).await;
    hold(&pool, "b", "https://g.example/keep", json!({"n": "911"})).await;

    let key = CanonicalUrl::parse("https://g.example/drop")
        .expect("u")
        .key();
    let id = request_purge(
        &pool,
        &PurgeSelector::Url(key),
        TombstoneReason::OptOut,
        "t",
    )
    .await
    .expect("req");
    let out = process_purge(&pool, &store, id, at(9, 0))
        .await
        .expect("purge");
    assert_eq!(out.captures_removed, 2);

    assert_eq!(held_keys(&pool).await, vec!["b".to_string()]);
    let text = tier_text(&store).await;
    assert!(!text.contains("drop me"), "{text}");
    assert!(text.contains("keep me"));
    assert!(
        !store.contains(&vc_store::paths::text_path(sid, true)),
        "an emptied ephemeral tier object is deleted"
    );
}

#[tokio::test]
async fn purge_author_leaves_no_trace_in_held_records_or_text_tier() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let sid = SegmentId::at(at(8, 1));
    let alice = meta(
        "https://travel.stackexchange.com/questions/1",
        at(8, 1),
        Some(SE),
    );
    let bob = meta(
        "https://travel.stackexchange.com/questions/2",
        at(8, 1),
        Some(SE),
    );
    commit_segment(
        &pool,
        &store,
        sid,
        false,
        &[entry(alice.clone(), "q1"), entry(bob.clone(), "q2")],
    )
    .await;
    // Alice's thread is indexed from her capture.
    let mut p = prov_at(
        SE,
        licence(
            LicenceClass::ShareAlike,
            Some("CC-BY-SA-4.0"),
            &format!("{ALICE}; CC BY-SA 4.0"),
        ),
        "https://travel.stackexchange.com/questions/1",
        at(8, 1),
    );
    p.capture = alice.capture_ref();
    vc_core::index::RecordSink::upsert(
        &vc_index::PgIndex::new(pool.clone()),
        vec![record(
            "qa_thread",
            "1",
            json!({"name": "Laos visa", "question": {"author": ALICE}}),
        )],
        &p,
    )
    .await
    .expect("upsert");
    let rows: Vec<_> = [
        (&alice, format!("Asked by {ALICE}")),
        (&bob, "Night buses".into()),
    ]
    .into_iter()
    .map(|(m, t)| json!({"capture_id": m.capture_id.to_string(), "main_text": t, "structured": []}))
    .collect();
    vc_store::text::put_text_tier(&store, sid, false, &rows, 3)
        .await
        .expect("tier");
    hold(
        &pool,
        "alice",
        "https://travel.stackexchange.com/questions/1",
        json!({"author": ALICE}),
    )
    .await;
    hold(
        &pool,
        "quote",
        "https://travel.stackexchange.com/questions/3",
        json!({"note": format!("as {ALICE} said")}),
    )
    .await;
    hold(
        &pool,
        "bob",
        "https://travel.stackexchange.com/questions/2",
        json!({"author": "Bob"}),
    )
    .await;

    let id = request_purge(
        &pool,
        &PurgeSelector::Author(ALICE.into()),
        TombstoneReason::OptOut,
        "t",
    )
    .await
    .expect("req");
    process_purge(&pool, &store, id, at(9, 0))
        .await
        .expect("purge");

    // Only records attributed to the author go; a held record that merely quotes the string
    // stays (it could be a country or a city of that name; held rows expire within 7 days).
    assert_eq!(
        held_keys(&pool).await,
        vec!["bob".to_string(), "quote".to_string()]
    );
    let text = tier_text(&store).await;
    assert!(!text.contains("Wanderer"), "{text}");
    assert!(text.contains("Night buses"));
}
