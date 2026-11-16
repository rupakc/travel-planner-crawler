//! `vcrawl admin`: rebuild-captures, purge, source, reextract, status (Postgres + in-memory
//! raw bucket).

mod admin_common;

use admin_common::*;
use chrono::Duration;
use testkit::MemBlobStore;
use testkit::vc_index_fixtures::{licence, prov_at, record};
use vc_core::index::{PurgeSelector, RecordSink, TombstoneReason};
use vc_core::licence::LicenceClass;
use vc_core::{CaptureRef, SegmentId};
use vcrawl::admin::purge::{process_purge, request_purge};
use vcrawl::admin::rebuild::{RebuildArgs, parse_cdxj_path, rebuild_captures};
use vcrawl::admin::reextract::{ReextractArgs, reextract};
use vcrawl::admin::source;

// ---------------------------------------------------------------- rebuild-captures

#[tokio::test]
async fn rebuild_captures_from_cdxj_equivalent() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();

    // An archive segment, an ephemeral one (noarchive, with an expiry) and one that a purge
    // rewrote (generation r1) where the crash came before the old objects were deleted.
    let a = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(3, 1)),
        false,
        &[
            entry(meta("https://a.example/1", at(3, 1), Some("src-a")), "one"),
            entry(meta("https://a.example/2", at(3, 1), Some("src-a")), "two"),
        ],
    )
    .await;
    let mut eph = meta("https://b.example/x", at(4, 2), None);
    eph.raw_expires_at = Some(at(4, 2) + Duration::days(30));
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(4, 2)),
        true,
        &[entry(eph, "eph")],
    )
    .await;
    let c = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(5, 3)),
        false,
        &[
            entry(meta("https://c.example/1", at(5, 3), None), "c1"),
            entry(meta("https://c.example/2", at(5, 3), None), "c2"),
            entry(meta("https://c.example/3", at(5, 3), None), "c3"),
        ],
    )
    .await;
    let gone = c.captures[1].meta.capture_id;
    purge_from_segment(&pool, &store, &c, &[gone], false).await;
    assert!(
        store.contains(&c.cdxj_path),
        "the r0 index outlived the crash"
    );

    let before = raw_rows(&pool).await;
    assert_eq!(count(&pool, "SELECT count(*) FROM captures").await, 5);

    // Disaster: the rows are lost.
    sqlx::query("DELETE FROM captures")
        .execute(&pool)
        .await
        .expect("wipe");
    sqlx::query("DELETE FROM segments")
        .execute(&pool)
        .await
        .expect("wipe");

    let rep = rebuild_captures(&pool, &store, &RebuildArgs::default())
        .await
        .expect("rebuild");
    assert_eq!(rep.segments_seen, 3);
    assert_eq!(rep.segments_inserted, 3);
    assert_eq!(rep.captures_inserted, 5, "the purged capture stays purged");
    assert_eq!(rep.superseded_generations, 1);
    assert!(rep.missing_objects.is_empty());
    assert_eq!(
        raw_rows(&pool).await,
        before,
        "rebuilt rows equal the committed ones"
    );
    let purged_back: i64 =
        sqlx::query_scalar("SELECT count(*) FROM captures WHERE capture_id = $1")
            .bind(gone.to_string())
            .fetch_one(&pool)
            .await
            .expect("q");
    assert_eq!(purged_back, 0);

    // Idempotent, and the verify mode agrees.
    let again = rebuild_captures(&pool, &store, &RebuildArgs::default())
        .await
        .expect("again");
    assert_eq!((again.segments_inserted, again.captures_inserted), (0, 0));
    let verify = RebuildArgs {
        verify: true,
        ..Default::default()
    };
    let v = rebuild_captures(&pool, &store, &verify)
        .await
        .expect("verify");
    assert!(v.mismatches.is_empty(), "{:?}", v.mismatches);

    // A drifted row is reported, naming the segment.
    sqlx::query("UPDATE captures SET warc_offset = warc_offset + 1 WHERE segment_id = $1")
        .bind(a.segment_id.to_string())
        .execute(&pool)
        .await
        .expect("tamper");
    let v = rebuild_captures(&pool, &store, &verify)
        .await
        .expect("verify");
    assert_eq!(v.mismatches.len(), 1, "{:?}", v.mismatches);
    assert!(v.mismatches[0].contains(&a.segment_id.to_string()));
}

#[tokio::test]
async fn rebuild_skips_segments_whose_object_is_gone_and_honours_prefix() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let m = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(6, 1)),
        false,
        &[entry(meta("https://d.example/", at(6, 1), None), "d")],
    )
    .await;
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(7, 1)),
        false,
        &[entry(meta("https://e.example/", at(7, 1), None), "e")],
    )
    .await;
    sqlx::query("DELETE FROM captures")
        .execute(&pool)
        .await
        .expect("wipe");
    sqlx::query("DELETE FROM segments")
        .execute(&pool)
        .await
        .expect("wipe");
    vc_core::store::BlobStore::delete(&store, &m.path)
        .await
        .expect("rm");

    let rep = rebuild_captures(
        &pool,
        &store,
        &RebuildArgs {
            prefix: Some("cdxj/2026/11/06/".into()),
            ..Default::default()
        },
    )
    .await
    .expect("rebuild");
    assert_eq!(rep.segments_seen, 0);
    assert_eq!(rep.missing_objects, vec![m.cdxj_path.clone()]);
    assert_eq!(count(&pool, "SELECT count(*) FROM segments").await, 0);

    let all = rebuild_captures(&pool, &store, &RebuildArgs::default())
        .await
        .expect("all");
    assert_eq!((all.segments_seen, all.captures_inserted), (1, 1));
}

#[test]
fn cdxj_paths_parse_generation() {
    let id = SegmentId::at(at(3, 1));
    let (_, c0) = vc_store::paths::segment_paths(id, false);
    let (_, c2) = vc_store::paths::segment_paths_gen(id, true, 2);
    assert_eq!(parse_cdxj_path(&c0), Some((id, 0)));
    assert_eq!(parse_cdxj_path(&c2), Some((id, 2)));
    assert_eq!(parse_cdxj_path("cdxj/2026/11/03/notaulid.cdxj"), None);
    assert_eq!(parse_cdxj_path("cdxj/2026/11/03/x.warc.zst"), None);
}

// ---------------------------------------------------------------- purge

const ALICE: &str = "Alice \"Wanderer\" Example";

/// A Stack Exchange Travel dump capture holding two qa_thread records: Alice asks in one and
/// Bob answers alone in the other (separate captures).
async fn stack_exchange(pool: &sqlx::PgPool, store: &MemBlobStore) -> (CaptureRef, CaptureRef) {
    let alice_cap = meta(
        "https://travel.stackexchange.com/questions/1",
        at(8, 1),
        Some("stackexchange-travel"),
    );
    let bob_cap = meta(
        "https://travel.stackexchange.com/questions/2",
        at(8, 1),
        Some("stackexchange-travel"),
    );
    let (ar, br) = (alice_cap.capture_ref(), bob_cap.capture_ref());
    commit_segment(
        pool,
        store,
        SegmentId::at(at(8, 1)),
        false,
        &[entry(alice_cap, "q1"), entry(bob_cap, "q2")],
    )
    .await;
    let ix = vc_index::PgIndex::new(pool.clone());
    for (cap, url, data, attribution) in [
        (
            ar,
            "https://travel.stackexchange.com/questions/1",
            serde_json::json!({
                "name": "Visa on arrival in Laos?",
                "question": {"author": ALICE, "score": 3, "body": format!("Asked by {ALICE}")},
                "answers": [{"author": "Bob", "score": 5}],
            }),
            format!("{ALICE}, Bob; Stack Exchange, CC BY-SA 4.0"),
        ),
        (
            br,
            "https://travel.stackexchange.com/questions/2",
            serde_json::json!({
                "name": "Night buses in Vietnam",
                "question": {"author": "Bob", "score": 1},
                "answers": [],
            }),
            "Bob; Stack Exchange, CC BY-SA 4.0".to_string(),
        ),
    ] {
        let lic = licence(LicenceClass::ShareAlike, Some("CC-BY-SA-4.0"), &attribution);
        let mut p = prov_at("stackexchange-travel", lic, url, at(8, 1));
        p.capture = cap;
        let key = url.rsplit('/').next().expect("id").to_string();
        ix.upsert(vec![record("qa_thread", &key, data)], &p)
            .await
            .expect("upsert");
    }
    (ar, br)
}

#[tokio::test]
async fn purge_author_removes_attribution() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let (alice, bob) = stack_exchange(&pool, &store).await;
    let needle = "Wanderer";
    let mentions = "SELECT
        (SELECT count(*) FROM record_versions WHERE strpos(data::text || coalesce(attribution, '')
                || coalesce(title, '') || coalesce(excerpt, ''), 'Wanderer') > 0)
      + (SELECT count(*) FROM records WHERE strpos(coalesce(licence::text, '')
                || coalesce(title, '') || coalesce(search_text, ''), 'Wanderer') > 0)";
    assert!(count(&pool, mentions).await > 0, "fixture names {needle}");

    let id = request_purge(
        &pool,
        &PurgeSelector::Author(ALICE.into()),
        TombstoneReason::OptOut,
        "issue-42",
    )
    .await
    .expect("request");
    let now = at(9, 0);
    let out = process_purge(&pool, &store, id, now).await.expect("purge");
    assert_eq!(out.state, "raw_done");
    assert_eq!(out.records_tombstoned, 1);
    assert_eq!(out.captures_removed, 1);
    assert_eq!(out.segments_rewritten, 1);
    assert!(out.rows_scrubbed >= 2, "{out:?}");

    // The author's name is gone from every stored row …
    assert_eq!(count(&pool, mentions).await, 0);
    // … her thread is tombstoned, Bob's is not, and re-ingest stays excluded.
    let states: Vec<(String, String)> =
        sqlx::query_as("SELECT record_key, state FROM records ORDER BY record_key")
            .fetch_all(&pool)
            .await
            .expect("states");
    assert_eq!(
        states,
        vec![
            ("1".to_string(), "tombstoned".to_string()),
            ("2".to_string(), "active".to_string())
        ]
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM tombstones WHERE selector_type = 'author'"
        )
        .await,
        1
    );
    // The raw capture is cut out of the segment (rows and objects); Bob's stays readable.
    let caps: Vec<String> = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_all(&pool)
        .await
        .expect("caps");
    assert_eq!(caps, vec![bob.capture_id.to_string()]);
    let (path, cdxj): (String, String) = sqlx::query_as("SELECT path, cdxj_path FROM segments")
        .fetch_one(&pool)
        .await
        .expect("segment");
    assert!(path.contains(".r1."), "{path}");
    let entries = vc_store::read_cdxj(&store, &cdxj).await.expect("cdxj");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].capture.meta.capture_id, bob.capture_id);
    assert!(store.paths().iter().all(|p| {
        !p.ends_with(&format!("{}.warc.zst", SegmentId::at(at(8, 1)))) || p.contains(".r1.")
    }));
    let _ = alice;

    // Re-running is a no-op; after the backup window the request is done.
    let again = process_purge(&pool, &store, id, now).await.expect("again");
    assert_eq!(
        (again.state.as_str(), again.captures_removed),
        ("raw_done", 0)
    );
    let done = process_purge(&pool, &store, id, now + Duration::days(21))
        .await
        .expect("done");
    assert_eq!(done.state, "done");
    let st: (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as("SELECT index_done_at, done_at FROM purge_requests WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("row");
    assert!(st.0.is_some() && st.1.is_some());
}

#[tokio::test]
async fn purge_url_cuts_capture_and_text_tier_rows() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    let id = SegmentId::at(at(10, 1));
    let keep = meta("https://f.example/keep", at(10, 1), None);
    let drop = meta("https://f.example/drop", at(10, 1), None);
    let (kc, dc) = (keep.capture_id, drop.capture_id);
    commit_segment(
        &pool,
        &store,
        id,
        false,
        &[entry(keep, "k"), entry(drop, "d")],
    )
    .await;
    let rows = vec![
        serde_json::json!({"capture_id": kc.to_string(), "text": "kept"}),
        serde_json::json!({"capture_id": dc.to_string(), "text": "dropped"}),
    ];
    vc_store::text::put_text_tier(&store, id, false, &rows, 3)
        .await
        .expect("text");

    let sel = PurgeSelector::Url(
        vc_core::CanonicalUrl::parse("https://f.example/drop")
            .expect("u")
            .key(),
    );
    let rid = request_purge(&pool, &sel, TombstoneReason::OptOut, "t")
        .await
        .expect("req");
    let out = process_purge(&pool, &store, rid, at(11, 0))
        .await
        .expect("purge");
    assert_eq!((out.captures_removed, out.segments_rewritten), (1, 1));
    let left: Vec<String> = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_all(&pool)
        .await
        .expect("q");
    assert_eq!(left, vec![kc.to_string()]);
    let text = vc_core::store::BlobStore::get(&store, &vc_store::paths::text_path(id, false), None)
        .await
        .expect("text tier");
    let rows: Vec<serde_json::Value> = vc_store::text::decode_text_tier(&text).expect("decode");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["capture_id"], kc.to_string());
}

#[tokio::test]
async fn purge_record_is_index_only_and_index_only_flag_waits() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    stack_exchange(&pool, &store).await;
    let rec: i64 = sqlx::query_scalar("SELECT record_id FROM records WHERE record_key = '2'")
        .fetch_one(&pool)
        .await
        .expect("rec");
    let rid = request_purge(
        &pool,
        &PurgeSelector::Record(vc_core::RecordId(rec)),
        TombstoneReason::Purge,
        "t",
    )
    .await
    .expect("req");
    let first = vcrawl::admin::purge::run_index_stage(&pool, rid)
        .await
        .expect("index");
    assert_eq!(
        (first.state.as_str(), first.records_tombstoned),
        ("index_done", 1)
    );
    let out = process_purge(&pool, &store, rid, at(9, 0))
        .await
        .expect("purge");
    assert_eq!((out.state.as_str(), out.captures_removed), ("raw_done", 0));
    assert_eq!(count(&pool, "SELECT count(*) FROM captures").await, 2);
}

#[test]
fn purge_cli_needs_exactly_one_selector() {
    use clap::Parser;
    #[derive(Debug, Parser)]
    struct Cli {
        #[command(flatten)]
        a: vcrawl::admin::purge::PurgeArgs,
    }
    assert!(Cli::try_parse_from(["x"]).is_err());
    assert!(Cli::try_parse_from(["x", "--host", "a.example", "--author", "z"]).is_err());
    let c = Cli::try_parse_from(["x", "--host", "A.Example."]).expect("parse");
    assert_eq!(
        c.a.selector.selector().expect("sel"),
        PurgeSelector::Host("a.example".into())
    );
    assert!(vcrawl::admin::purge::parse_reason("gone").is_err());
}

// ---------------------------------------------------------------- source

fn spec(id: &str) -> vc_core::sources::SourceSpec {
    vc_sources::load_file(&fixtures().join(format!("sources/{id}.yaml"))).expect("spec")
}

#[tokio::test]
async fn admin_source_approve_disable_round_trip() {
    let pool = testkit::pg().await;
    vc_sources::seed_sources(
        &pool,
        &[spec("ops-demo-blog"), spec("ops-restricted")],
        false,
    )
    .await
    .expect("seed");
    for (n, state) in [("a", "due"), ("b", "due"), ("c", "leased")] {
        let u = vc_core::CanonicalUrl::parse(&format!("https://ops-demo.example/{n}")).expect("u");
        sqlx::query(
            "INSERT INTO url_state (url_key, key, source_id, state, lease_epoch)
             VALUES ($1, 'ops-demo.example', 'ops-demo-blog', $2,
                     CASE WHEN $2 = 'leased' THEN 1 END)",
        )
        .bind(u.key().0.to_vec())
        .bind(state)
        .execute(&pool)
        .await
        .expect("url");
    }
    let enabled = |rows: &[source::SourceRow], id: &str| {
        rows.iter().find(|r| r.source_id == id).map(|r| r.enabled)
    };
    let rows = source::list(&pool).await.expect("list");
    assert_eq!(enabled(&rows, "ops-demo-blog"), Some(true));
    assert_eq!(enabled(&rows, "ops-restricted"), Some(false));

    assert_eq!(
        source::disable(&pool, "ops-demo-blog")
            .await
            .expect("disable"),
        2
    );
    assert_eq!(
        enabled(&source::list(&pool).await.expect("list"), "ops-demo-blog"),
        Some(false)
    );
    let states = "SELECT string_agg(state, ',' ORDER BY state) FROM url_state";
    let s: String = sqlx::query_scalar(states)
        .fetch_one(&pool)
        .await
        .expect("s");
    assert_eq!(
        s, "disabled,disabled,leased",
        "leased work finishes normally"
    );

    // A re-seed of the unchanged spec keeps it disabled.
    vc_sources::seed_sources(&pool, &[spec("ops-demo-blog")], false)
        .await
        .expect("reseed");
    assert_eq!(
        enabled(&source::list(&pool).await.expect("list"), "ops-demo-blog"),
        Some(false)
    );

    let v = source::approve(&pool, "ops-demo-blog", "me")
        .await
        .expect("approve");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["urls_resumed"], 2);
    let s: String = sqlx::query_scalar(states)
        .fetch_one(&pool)
        .await
        .expect("s");
    assert_eq!(s, "due,due,leased");
    assert_eq!(
        enabled(&source::list(&pool).await.expect("list"), "ops-demo-blog"),
        Some(true)
    );

    let e = source::approve(&pool, "ops-restricted", "me")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("restricted"), "{e}");
    assert!(
        source::approve(&pool, "no-such-source", "me")
            .await
            .is_err()
    );
    assert!(source::disable(&pool, "no-such-source").await.is_err());
}

#[tokio::test]
async fn admin_source_approve_scout_proposal() {
    let pool = testkit::pg().await;
    let cid: i64 = sqlx::query_scalar(
        "INSERT INTO source_candidates (host, proposed_by, destination, kinds, checks)
         VALUES ('www.Laos-Guide.example', 'scout', 'Luang Prabang', '{event}',
                 '{\"robots\": \"allowed\"}') RETURNING candidate_id",
    )
    .fetch_one(&pool)
    .await
    .expect("candidate");
    let open = source::proposals(&pool).await.expect("proposals");
    assert_eq!(open.len(), 1);
    let v = source::approve(&pool, &cid.to_string(), "rupak")
        .await
        .expect("approve");
    assert_eq!(v["status"], "approved");
    assert_eq!(v["draft_spec"]["id"], "laos-guide-example");
    assert_eq!(v["draft_spec"]["enabled"], false, "a draft never crawls");
    let (status, by): (String, String) =
        sqlx::query_as("SELECT status, decided_by FROM source_candidates WHERE candidate_id = $1")
            .bind(cid)
            .fetch_one(&pool)
            .await
            .expect("row");
    assert_eq!((status.as_str(), by.as_str()), ("approved", "rupak"));
    assert!(source::proposals(&pool).await.expect("p").is_empty());
    assert!(
        source::approve(&pool, &cid.to_string(), "rupak")
            .await
            .is_err()
    );
    // No source row appears until the spec PR merges and is seeded.
    assert_eq!(count(&pool, "SELECT count(*) FROM sources").await, 0);
}

#[test]
fn admin_source_validate_static_checks() {
    let dir = fixtures().join("sources");
    let deny = repo().join("policy/denylist.yaml");
    let ok = source::validate_file(&dir, &deny, "ops-demo-blog").expect("validate");
    assert!(ok.ok, "{ok:?}");
    let bad = source::validate_file(&dir, &deny, "ops-denied-shop").expect("validate");
    assert!(!bad.ok);
    assert!(
        bad.errors.iter().any(|e| e.contains("booking.com")),
        "{:?}",
        bad.errors
    );
    assert!(source::validate_file(&dir, &deny, "../sources/ops-demo-blog").is_err());
    assert!(source::validate_file(&dir, &deny, "missing").is_err());
    assert_eq!(source::draft_id("WWW.Example.co.uk"), "example-co-uk");
}

// ---------------------------------------------------------------- reextract, status

#[tokio::test]
async fn reextract_queues_canary_once() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    stack_exchange(&pool, &store).await;
    let args = |limit| ReextractArgs {
        kind: "qa_thread".into(),
        limit,
        dry_run: false,
    };
    let dry = reextract(
        &pool,
        &ReextractArgs {
            dry_run: true,
            ..args(200)
        },
    )
    .await
    .expect("dry");
    assert_eq!((dry.eligible, dry.queued, dry.run_id), (2, 0, None));
    let one = reextract(&pool, &args(1)).await.expect("canary");
    assert_eq!((one.status.as_str(), one.queued), ("canary", 1));
    let all = reextract(&pool, &args(0)).await.expect("all");
    assert_eq!(
        (all.status.as_str(), all.queued),
        ("rolling", 1),
        "the pending canary item is not queued twice"
    );
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM work_items WHERE kind = 'extract' ORDER BY work_id LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("payload");
    assert_eq!(payload["kind"], "qa_thread");
    assert_eq!(payload["reason"], "reextract");
    assert_eq!(payload["run_id"], one.run_id.expect("run").as_str());
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM extraction_runs WHERE kind = 'qa_thread'"
        )
        .await,
        2
    );
    assert!(reextract(&pool, &args(-1)).await.is_err());
}

#[tokio::test]
async fn admin_status_counts() {
    let pool = testkit::pg().await;
    let store = MemBlobStore::new();
    stack_exchange(&pool, &store).await;
    let st = vcrawl::admin::status::status(&pool).await.expect("status");
    assert_eq!(st.records["active"], 2);
    assert_eq!(st.segments["count"], 1);
    assert_eq!(st.queue["capture_ready"]["ready"], 2);
    assert!(st.urls.as_object().expect("obj").is_empty());
}
