//! Raw-first and crash consistency (spec §5.5, §17.2) with `fail` failpoints.
//!
//! Failpoints are process-global, so every test here takes `SERIAL` first.

mod common;

use std::io::Write;
use std::sync::Arc;

use common::*;
use sqlx::PgPool;
use testkit::{FakeClock, MemBlobStore};
use vc_core::store::{BlobStore, CaptureSink, SegmentCommitter};
use vc_core::{Clock, Error};
use vc_store::{PgSegmentCommitter, SegmentRoller, Spool, SpoolOptions};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn opts() -> SpoolOptions {
    SpoolOptions {
        max_bytes: 1 << 30,
        max_age: chrono::Duration::minutes(10),
        backpressure_bytes: 1 << 30,
    }
}

fn roller(
    spool: &Spool,
    store: Arc<dyn BlobStore>,
    committer: Arc<dyn SegmentCommitter>,
    clock: Arc<dyn Clock>,
) -> SegmentRoller {
    let cfg = vc_config::StoreConfig {
        zstd_level: 3,
        ..Default::default()
    };
    SegmentRoller::new(spool.clone(), store, committer, clock, &cfg)
}

/// A process killed at any step between the spool append and the spool deletion loses no
/// capture and duplicates none after restart (replay of sealed + open files).
#[tokio::test]
async fn spool_replay_after_crash() {
    let _g = SERIAL.lock().await;
    for fp in [
        "spool::before_seal",
        "segment::before_upload",
        "segment::after_upload",
        "segment::before_commit",
        "segment::after_commit",
    ] {
        let scenario = fail::FailScenario::setup();
        let dir = tempfile::tempdir().expect("tmp");
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(t0()));
        let store = Arc::new(MemBlobStore::new());
        let committer = Arc::new(RecordingCommitter::default());

        let mut appended = Vec::new();
        {
            let mut spool = Spool::open(dir.path(), opts(), clock.clone()).expect("open");
            for i in 0..3 {
                let r = response(
                    &format!("https://example.com/{i}"),
                    t0() + chrono::Duration::seconds(i),
                    format!("body {i}").as_bytes(),
                );
                appended.push(spool.append(r).await.expect("append"));
            }
            // One noarchive capture goes to the ephemeral class.
            let mut r = response("https://example.com/noarchive", t0(), b"ephemeral body");
            r.meta.raw_expires_at = Some(t0() + chrono::Duration::days(30));
            appended.push(spool.append(r).await.expect("append"));

            fail::cfg(fp, "return").expect("cfg");
            let ro = roller(&spool, store.clone(), committer.clone(), clock.clone());
            assert!(ro.flush().await.is_err(), "{fp}: crash expected");
            fail::remove(fp);
        } // "process dies": spool and roller dropped.

        // A torn, unacknowledged frame at the tail of the open file.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("archive.open"))
            .expect("open file");
        f.write_all(b"VCSP\x01\x40\x00\x00\x00\xff\xff")
            .expect("torn");
        drop(f);

        // Restart: replay.
        let spool = Spool::open(dir.path(), opts(), clock.clone()).expect("reopen");
        let ro = roller(&spool, store.clone(), committer.clone(), clock.clone());
        ro.flush().await.expect("replay");

        appended.sort();
        assert_eq!(
            committer.capture_ids(),
            appended,
            "{fp}: every capture exactly once"
        );
        assert!(
            spool.sealed().expect("sealed").is_empty(),
            "{fp}: spool drained"
        );
        assert!(
            spool.open_entries().expect("open").is_empty(),
            "{fp}: no torn frame published"
        );
        let manifests = committer.manifests();
        assert_eq!(manifests.iter().filter(|m| m.ephemeral).count(), 1, "{fp}");
        for m in &manifests {
            assert!(
                store.contains(&m.path) && store.contains(&m.cdxj_path),
                "{fp}"
            );
            let prefix = if m.ephemeral {
                "ephemeral/"
            } else {
                "segments/"
            };
            assert!(m.path.starts_with(prefix), "{fp}: {}", m.path);
        }
        scenario.teardown();
    }
}

/// A failed fsync is not acknowledged and leaves nothing behind to publish.
#[tokio::test]
async fn unacknowledged_append_is_never_published() {
    let _g = SERIAL.lock().await;
    let scenario = fail::FailScenario::setup();
    let dir = tempfile::tempdir().expect("tmp");
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(t0()));
    let mut spool = Spool::open(dir.path(), opts(), clock.clone()).expect("open");
    let ok = spool
        .append(response("https://example.com/ok", t0(), b"ok"))
        .await
        .expect("append");
    fail::cfg("spool::before_fsync", "return").expect("cfg");
    let e = spool
        .append(response("https://example.com/lost", t0(), b"lost"))
        .await;
    assert!(matches!(e, Err(Error::Storage(_))));
    fail::remove("spool::before_fsync");
    let committer = Arc::new(RecordingCommitter::default());
    let ro = roller(
        &spool,
        Arc::new(MemBlobStore::new()),
        committer.clone(),
        clock,
    );
    ro.flush().await.expect("flush");
    assert_eq!(committer.capture_ids(), vec![ok]);
    scenario.teardown();
}

/// Segments roll at the size and age limits; backpressure and upload-failure pause.
#[tokio::test]
async fn roll_limits_and_backpressure() {
    let _g = SERIAL.lock().await;
    let scenario = fail::FailScenario::setup();
    let dir = tempfile::tempdir().expect("tmp");
    let fake = Arc::new(FakeClock::new(t0()));
    let clock: Arc<dyn Clock> = fake.clone();
    let small = SpoolOptions {
        max_bytes: 8_000,
        max_age: chrono::Duration::minutes(10),
        backpressure_bytes: 9_000,
    };
    let mut spool = Spool::open(dir.path(), small, clock.clone()).expect("open");
    let committer = Arc::new(RecordingCommitter::default());
    let cfg = vc_config::StoreConfig {
        upload_failure_pause_s: 900,
        ..Default::default()
    };
    let ro = SegmentRoller::new(
        spool.clone(),
        Arc::new(MemBlobStore::new()),
        committer.clone(),
        clock.clone(),
        &cfg,
    );

    // Size: a 9 KB body overflows 8 KB → sealed on append.
    spool
        .append(response("https://example.com/big", t0(), &[b'x'; 9000]))
        .await
        .expect("append");
    assert_eq!(spool.sealed().expect("sealed").len(), 1);
    assert!(spool.over_backpressure());
    assert!(ro.should_pause_fetchers());
    ro.tick().await.expect("tick");
    assert!(!ro.should_pause_fetchers());

    // Age: a small record is sealed only after 10 minutes.
    spool
        .append(response("https://example.com/s", fake.now(), b"s"))
        .await
        .expect("append");
    ro.tick().await.expect("tick");
    assert_eq!(committer.manifests().len(), 1);
    fake.advance(std::time::Duration::from_secs(11 * 60));
    ro.tick().await.expect("tick");
    assert_eq!(committer.manifests().len(), 2);

    // Uploads failing for more than 15 minutes pause fetchers.
    spool
        .append(response("https://example.com/f", fake.now(), b"f"))
        .await
        .expect("append");
    fail::cfg("segment::before_upload", "return").expect("cfg");
    assert!(ro.flush().await.is_err());
    assert!(!ro.should_pause_fetchers());
    fake.advance(std::time::Duration::from_secs(16 * 60));
    assert!(ro.tick().await.is_err());
    assert!(ro.should_pause_fetchers());
    fail::remove("segment::before_upload");
    ro.tick().await.expect("recovered");
    assert!(ro.failing_since().is_none() && !ro.should_pause_fetchers());
    scenario.teardown();
}

async fn seed_url(pool: &PgPool, u: &str, epoch: i64) {
    let cu = url(u);
    sqlx::query("INSERT INTO urls (url_key, url, key) VALUES ($1, $2, $3)")
        .bind(cu.key().as_bytes().to_vec())
        .bind(cu.as_str())
        .bind(cu.politeness_key().as_str())
        .execute(pool)
        .await
        .expect("urls");
    sqlx::query(
        "INSERT INTO url_state (url_key, key, state, lease_epoch, etag)
         VALUES ($1, $2, 'fetched_pending', $3, '\"old\"')",
    )
    .bind(cu.key().as_bytes().to_vec())
    .bind(cu.politeness_key().as_str())
    .bind(epoch)
    .execute(pool)
    .await
    .expect("url_state");
}

async fn counts(pool: &PgPool) -> (i64, i64, i64) {
    let q = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .expect(sql)
    };
    (
        q("SELECT count(*) FROM segments").await,
        q("SELECT count(*) FROM captures").await,
        q("SELECT count(*) FROM work_items WHERE kind = 'capture_ready'").await,
    )
}

async fn url_state(pool: &PgPool, u: &str) -> (String, Option<String>) {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT state, etag FROM url_state WHERE url_key = $1",
    )
    .bind(url(u).key().as_bytes().to_vec())
    .fetch_one(pool)
    .await
    .expect("url_state row")
}

/// Nothing derived from a capture is visible (captures, segments, capture_ready, url_state
/// validators) until `vc_commit_segment` commits it, wherever the process stops before that.
#[tokio::test]
async fn no_capture_visible_before_commit() {
    let _g = SERIAL.lock().await;
    let pool = testkit::pg().await;
    let u = "https://example.com/page";
    seed_url(&pool, u, 1).await;
    let scenario = fail::FailScenario::setup();
    let dir = tempfile::tempdir().expect("tmp");
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(t0()));
    let store = Arc::new(MemBlobStore::new());
    let committer: Arc<dyn SegmentCommitter> = Arc::new(PgSegmentCommitter::new(pool.clone()));
    let mut spool = Spool::open(dir.path(), opts(), clock.clone()).expect("open");
    let id = spool
        .append(response(u, t0(), b"<html>new</html>"))
        .await
        .expect("append");

    // Appended + fsynced, but not committed.
    assert_eq!(counts(&pool).await, (0, 0, 0));
    assert_eq!(
        url_state(&pool, u).await,
        ("fetched_pending".into(), Some("\"old\"".into()))
    );

    for fp in [
        "segment::before_upload",
        "segment::after_upload",
        "segment::before_commit",
    ] {
        fail::cfg(fp, "return").expect("cfg");
        let ro = roller(&spool, store.clone(), committer.clone(), clock.clone());
        assert!(ro.flush().await.is_err(), "{fp}");
        fail::remove(fp);
        assert_eq!(counts(&pool).await, (0, 0, 0), "{fp}: nothing visible");
        assert_eq!(
            url_state(&pool, u).await,
            ("fetched_pending".into(), Some("\"old\"".into())),
            "{fp}: old validators kept (no 304 without a stored body)"
        );
    }
    // The segment object may already exist; the commit still has not happened.
    assert!(!store.paths().is_empty());

    // Restart and replay: committed exactly once.
    let spool = Spool::open(dir.path(), opts(), clock.clone()).expect("reopen");
    let ro = roller(&spool, store.clone(), committer.clone(), clock.clone());
    let ms = ro.tick().await.expect("replay");
    assert_eq!(ms.len(), 1);
    assert_eq!(counts(&pool).await, (1, 1, 1));
    assert_eq!(
        url_state(&pool, u).await,
        ("due".into(), Some("\"v1\"".into()))
    );
    let cid: String = sqlx::query_scalar("SELECT capture_id FROM captures")
        .fetch_one(&pool)
        .await
        .expect("cap");
    assert_eq!(cid, id.to_string());

    // A replayed commit (crash after commit, before the spool delete) is a no-op.
    committer.commit(&ms[0]).await.expect("recommit");
    assert_eq!(counts(&pool).await, (1, 1, 1));

    // The committed offsets read the capture back.
    let (off, len): (i64, i64) = sqlx::query_as("SELECT warc_offset, warc_length FROM captures")
        .fetch_one(&pool)
        .await
        .expect("offsets");
    let recs = vc_store::read_capture(store.as_ref(), &ms[0].path, off as u64, len as u64)
        .await
        .expect("read capture");
    assert_eq!(recs[1].capture_id(), Some(id));
    scenario.teardown();
}
