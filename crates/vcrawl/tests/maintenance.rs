//! Role `maintenance`: partitions, ephemeral expiry, restore drill, digest, purge, export stub.

mod admin_common;

use std::sync::Arc;

use admin_common::*;
use chrono::{Duration, NaiveDate};
use testkit::{FakeClock, MemBlobStore};
use vc_core::SegmentId;
use vc_core::index::{PurgeSelector, TombstoneReason};
use vc_core::store::BlobStore;
use vc_obs::Metrics;
use vcrawl::roles::maintenance::{Job, Maintenance, Schedule, export_due};

fn maint(pool: &sqlx::PgPool, store: Arc<MemBlobStore>) -> (Maintenance, Arc<Metrics>) {
    let clock = Arc::new(FakeClock::new(at(15, 0)));
    let metrics = Arc::new(Metrics::new(clock, std::time::Duration::from_secs(180)));
    (
        Maintenance {
            pool: pool.clone(),
            store,
            metrics: Some(metrics.clone()),
            partitions_ahead_months: 2,
            agentic: None,
            ephemeral_days: 30,
        },
        metrics,
    )
}

#[tokio::test]
async fn ephemeral_expiry_cuts_expired_captures_then_drops_the_segment() {
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    let (m, _) = maint(&pool, store.clone());
    let mut early = meta("https://g.example/a", at(1, 1), None);
    early.raw_expires_at = Some(at(1, 1) + Duration::days(30));
    let mut late = meta("https://g.example/b", at(1, 2), None);
    late.raw_expires_at = Some(at(1, 2) + Duration::days(40));
    let keep = late.capture_id;
    let seg = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(1, 1)),
        true,
        &[entry(early, "a"), entry(late, "b")],
    )
    .await;
    assert!(seg.path.starts_with("ephemeral/"));

    assert_eq!(m.expire_ephemeral(at(20, 0)).await.expect("none"), (0, 0));
    let t1 = at(1, 1) + Duration::days(31);
    assert_eq!(m.expire_ephemeral(t1).await.expect("one"), (1, 1));
    let left: Vec<String> = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_all(&pool)
        .await
        .expect("q");
    assert_eq!(left, vec![keep.to_string()]);
    assert!(!store.contains(&seg.path), "old generation deleted");

    let t2 = at(1, 2) + Duration::days(41);
    assert_eq!(m.expire_ephemeral(t2).await.expect("all"), (1, 1));
    assert_eq!(count(&pool, "SELECT count(*) FROM captures").await, 0);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM segments WHERE purged_at IS NOT NULL"
        )
        .await,
        1
    );
    assert!(
        store.paths().iter().all(|p| !p.starts_with("ephemeral/")),
        "{:?}",
        store.paths()
    );
    assert_eq!(m.expire_ephemeral(t2).await.expect("again"), (0, 0));
}

#[tokio::test]
async fn ephemeral_expiry_survives_lifecycle_deleted_objects() {
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    let (m, _) = maint(&pool, store.clone());
    let mut c = meta("https://h.example/", at(1, 1), None);
    c.raw_expires_at = Some(at(2, 1));
    let seg = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(1, 1)),
        true,
        &[entry(c, "x")],
    )
    .await;
    // The bucket's 30-day lifecycle rule got there first.
    store.delete(&seg.path).await.expect("rm");
    store.delete(&seg.cdxj_path).await.expect("rm");
    assert_eq!(m.expire_ephemeral(at(3, 0)).await.expect("expire"), (1, 1));
    assert_eq!(count(&pool, "SELECT count(*) FROM captures").await, 0);
}

#[tokio::test]
async fn restore_drill_reports_equivalence() {
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    let (m, metrics) = maint(&pool, store.clone());
    let a = commit_segment(
        &pool,
        &store,
        SegmentId::at(at(2, 1)),
        false,
        &[
            entry(meta("https://i.example/1", at(2, 1), None), "1"),
            entry(meta("https://i.example/2", at(2, 1), None), "2"),
        ],
    )
    .await;
    let rep = m.restore_drill(at(15, 0)).await.expect("drill");
    assert!(rep.ok && rep.checked == 1, "{rep:?}");
    assert_eq!(metrics.restore_drill_ok.get(), Some(1));

    // A capture row that the bucket does not have (e.g. a restore that lost a commit).
    sqlx::query("DELETE FROM captures WHERE capture_id = $1")
        .bind(a.captures[0].meta.capture_id.to_string())
        .execute(&pool)
        .await
        .expect("drift");
    let rep = m.restore_drill(at(16, 0)).await.expect("drill");
    assert!(!rep.ok);
    assert_eq!(metrics.restore_drill_ok.get(), Some(0));
    let p: serde_json::Value = sqlx::query_scalar(
        "SELECT progress FROM heartbeats WHERE role = 'maintenance' AND instance = 'restore_drill'",
    )
    .fetch_one(&pool)
    .await
    .expect("beat");
    assert_eq!(p["ok"], false);
    assert!(p["last_ok_at"].is_string(), "the last success is kept: {p}");

    // The age gauge follows the last success.
    let mut sched = Schedule::default();
    for j in Job::ALL {
        sched.done(j, at(16, 0));
    }
    m.tick(&mut sched, at(16, 0) + Duration::minutes(1)).await;
    assert_eq!(
        metrics.restore_drill_age_seconds.get(),
        Some((Duration::days(1) + Duration::minutes(1)).num_seconds() as f64)
    );
}

#[tokio::test]
async fn digest_is_written_once_per_day() {
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    let (m, _) = maint(&pool, store.clone());
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(14, 1)),
        false,
        &[
            entry(meta("https://j.example/1", at(14, 1), None), "1"),
            entry(meta("https://j.example/2", at(14, 23), None), "2"),
            entry(meta("https://j.example/3", at(15, 0), None), "3"),
        ],
    )
    .await;
    sqlx::query(
        "INSERT INTO politeness_keys (key, parked_until, park_level, park_reason)
         VALUES ('parked.example', now() + interval '400 days', 2, 'challenge'),
                ('free.example', NULL, 0, NULL)",
    )
    .execute(&pool)
    .await
    .expect("keys");
    let d = m
        .digest_yesterday(at(15, 3))
        .await
        .expect("digest")
        .expect("written");
    assert_eq!(d["day"], "2026-11-14");
    assert_eq!(d["captures"], 2, "{d}");
    assert_eq!(d["statuses"]["200"], 2);
    assert_eq!(d["parked_keys"], 1);
    assert_eq!(d["parked_top"][0]["reason"], "challenge");
    assert!(
        m.digest_yesterday(at(15, 9))
            .await
            .expect("again")
            .is_none()
    );
    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT digest FROM crawl_digest WHERE day = $1")
            .bind(NaiveDate::from_ymd_opt(2026, 11, 14).expect("d"))
            .fetch_one(&pool)
            .await
            .expect("row");
    assert_eq!(stored, d);
}

#[tokio::test]
async fn partitions_and_tick_run_due_jobs() {
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    let (mut m, _) = maint(&pool, store.clone());
    m.partitions_ahead_months = 5;
    let more = m.ensure_partitions().await.expect("partitions");
    assert!(more >= 3, "three tables × the extra months: {more}");
    assert_eq!(m.ensure_partitions().await.expect("again"), 0);

    // A purge requested with --index-only is finished by the maintenance tick.
    let id = vcrawl::admin::purge::request_purge(
        &pool,
        &PurgeSelector::Host("nothing.example".into()),
        TombstoneReason::OptOut,
        "t",
    )
    .await
    .expect("req");
    let mut sched = Schedule::default();
    let now = at(15, 0);
    let ran = m.tick(&mut sched, now).await;
    assert_eq!(ran, Job::ALL.to_vec(), "everything runs on the first tick");
    let state: String = sqlx::query_scalar("SELECT state FROM purge_requests WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("state");
    assert_eq!(state, "raw_done");
    assert!(
        m.tick(&mut sched, now + Duration::minutes(1))
            .await
            .is_empty()
    );
    assert_eq!(
        m.tick(&mut sched, now + Duration::minutes(16)).await,
        vec![Job::Purge]
    );
}

#[test]
fn schedule_due_follows_intervals() {
    let mut s = Schedule::default();
    let t = at(1, 0);
    assert_eq!(s.due(t).len(), Job::ALL.len());
    for j in Job::ALL {
        s.done(j, t);
    }
    assert!(s.due(t + Duration::minutes(14)).is_empty());
    assert_eq!(
        s.due(t + Duration::hours(1)),
        vec![Job::Purge, Job::Ephemeral, Job::Digest]
    );
    assert_eq!(s.due(t + Duration::days(7)).len(), Job::ALL.len());
}

#[test]
fn export_due_after_twelve_months() {
    let names: Vec<String> = [
        "captures_202510",
        "captures_202511",
        "record_versions_202509",
        "observations_default",
        "captures_2025xx",
        "captures_202513",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        export_due(&names, at(16, 0)),
        vec![
            "captures_202510".to_string(),
            "record_versions_202509".to_string()
        ]
    );
}

/// B10: a page whose final use policy (meta robots noarchive, found by the processor) says
/// `raw: ephemeral` gets an expiry `store.ephemeral_days` after its capture and is cut out of
/// its (archive) segment once that passes; an archived page is left alone.
#[tokio::test]
async fn noarchive_final_policy_routes_the_capture_to_expiry() {
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    let (m, _) = maint(&pool, store.clone());
    let noarchive = meta("https://n.example/page", at(1, 1), None);
    let archived = meta("https://n.example/other", at(1, 1), None);
    let (n, a) = (noarchive.capture_ref(), archived.capture_ref());
    commit_segment(
        &pool,
        &store,
        SegmentId::at(at(1, 1)),
        false,
        &[entry(noarchive, "n"), entry(archived, "a")],
    )
    .await;
    let mut p = vc_core::usepolicy::UsePolicy::ALLOW_ALL;
    p.raw = vc_core::usepolicy::RawUse::Ephemeral;
    vcrawl::data::capture::set_final_policy(&pool, &n, &p)
        .await
        .expect("final policy");
    vcrawl::data::capture::set_final_policy(&pool, &a, &vc_core::usepolicy::UsePolicy::ALLOW_ALL)
        .await
        .expect("final policy");

    // Before the window: routed, nothing cut.
    assert_eq!(m.expire_ephemeral(at(2, 0)).await.expect("route"), (0, 0));
    let exp: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT raw_expires_at FROM captures WHERE capture_id = $1")
            .bind(n.capture_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("expiry");
    assert_eq!(exp, Some(at(1, 1) + Duration::days(30)));
    // After it: the noarchive capture is gone, the archived one stays.
    assert_eq!(
        m.expire_ephemeral(at(1, 1) + Duration::days(31))
            .await
            .expect("expire"),
        (1, 1)
    );
    let left: Vec<String> = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_all(&pool)
        .await
        .expect("q");
    assert_eq!(left, vec![a.capture_id.to_string()]);
}
