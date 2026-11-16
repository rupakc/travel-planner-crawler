//! Final-review fixes: a seal failure never fails a durable append (B21), one bad sealed file
//! never blocks the others (B13), a replay after the commit never re-uploads purged bytes
//! (B14), and a stale uncommitted rewrite generation never stalls a purge (B12).
//!
//! Failpoints are process-global, so every test here takes `SERIAL` first.

mod common;

use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;

use common::*;
use testkit::{FakeClock, MemBlobStore};
use vc_core::store::{BlobStore, CaptureSink, SegmentCommitter};
use vc_core::{Clock, SegmentId};
use vc_store::spool::SpoolEntry;
use vc_store::warc::encode;
use vc_store::{
    PgSegmentCommitter, SegmentRoller, Spool, SpoolOptions, build_segment, finish_rewrite,
    rewrite_segment_without, segment::upload,
};

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

/// B21: the frame is fsynced before the seal; a failing seal must not turn the acknowledged
/// append into an error (the caller would treat a durable capture as lost). The file is sealed
/// on the next `seal_due` instead.
#[tokio::test]
async fn append_is_ok_when_the_seal_after_a_durable_frame_fails() {
    let _g = SERIAL.lock().await;
    let scenario = fail::FailScenario::setup();
    let dir = tempfile::tempdir().expect("tmp");
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(t0()));
    let small = SpoolOptions {
        max_bytes: 100,
        ..opts()
    };
    let mut spool = Spool::open(dir.path(), small, clock.clone()).expect("open");
    fail::cfg("spool::before_seal", "return").expect("cfg");
    let id = spool
        .append(response("https://example.com/big", t0(), &[b'x'; 500]))
        .await
        .expect("a durable frame is acknowledged even if sealing fails");
    fail::remove("spool::before_seal");
    assert!(spool.sealed().expect("sealed").is_empty(), "not sealed yet");
    let open: Vec<_> = spool
        .open_entries()
        .expect("open")
        .into_iter()
        .map(|e| e.meta.capture_id)
        .collect();
    assert_eq!(open, vec![id]);

    // The next append still works and lands after the first frame; the roller seals both.
    let id2 = spool
        .append(response("https://example.com/next", t0(), b"n"))
        .await
        .expect("append after failed seal");
    let committer = Arc::new(RecordingCommitter::default());
    let ro = roller(
        &spool,
        Arc::new(MemBlobStore::new()),
        committer.clone(),
        clock,
    );
    ro.tick().await.expect("tick seals the full file");
    let mut want = vec![id, id2];
    want.sort();
    assert_eq!(committer.capture_ids(), want);
    scenario.teardown();
}

/// B21 (rename half-done): a seal that fails after the rename leaves no handle on the sealed
/// file, so later appends never change a sealed segment's content.
#[tokio::test]
async fn sealed_content_is_fixed_even_if_the_seal_fails_late() {
    let _g = SERIAL.lock().await;
    let scenario = fail::FailScenario::setup();
    let dir = tempfile::tempdir().expect("tmp");
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(t0()));
    let mut spool = Spool::open(dir.path(), opts(), clock.clone()).expect("open");
    let a = spool
        .append(response("https://example.com/a", t0(), b"a"))
        .await
        .expect("a");
    fail::cfg("spool::after_rename", "return").expect("cfg");
    assert!(spool.seal_all().is_err());
    fail::remove("spool::after_rename");
    let sealed = spool.sealed().expect("sealed");
    assert_eq!(sealed.len(), 1);
    let before = std::fs::read(&sealed[0].path).expect("read");
    let b = spool
        .append(response("https://example.com/b", t0(), b"b"))
        .await
        .expect("b");
    assert_eq!(
        std::fs::read(&sealed[0].path).expect("read"),
        before,
        "an append after a late seal failure went into the sealed file"
    );
    let ids: Vec<_> = sealed[0]
        .read_entries()
        .expect("entries")
        .into_iter()
        .map(|e| e.meta.capture_id)
        .collect();
    assert_eq!(ids, vec![a]);
    let open: Vec<_> = spool
        .open_entries()
        .expect("open")
        .into_iter()
        .map(|e| e.meta.capture_id)
        .collect();
    assert_eq!(open, vec![b]);
    scenario.teardown();
}

/// B13: a damaged sealed file is skipped (newer files still publish) and quarantined after
/// [`vc_store::segment::QUARANTINE_AFTER`] failures, with an alert counter.
#[tokio::test]
async fn bad_sealed_file_is_quarantined_and_newer_ones_publish() {
    let _g = SERIAL.lock().await;
    let dir = tempfile::tempdir().expect("tmp");
    let fake = Arc::new(FakeClock::new(t0()));
    let clock: Arc<dyn Clock> = fake.clone();
    let mut spool = Spool::open(dir.path(), opts(), clock.clone()).expect("open");
    spool
        .append(response("https://example.com/old", t0(), b"old"))
        .await
        .expect("old");
    let bad = spool.seal_all().expect("seal").remove(0);
    fake.advance(std::time::Duration::from_secs(5));
    let good = spool
        .append(response("https://example.com/new", fake.now(), b"new"))
        .await
        .expect("new");
    spool.seal_all().expect("seal");
    // Damage the older sealed file (bit rot / a bad disk block).
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&bad.path)
        .expect("open sealed");
    f.write_all(b"garbage").expect("damage");
    drop(f);

    let committer = Arc::new(RecordingCommitter::default());
    let ro = roller(
        &spool,
        Arc::new(MemBlobStore::new()),
        committer.clone(),
        clock,
    );
    let ms = ro
        .tick()
        .await
        .expect("the bad file does not fail the tick");
    assert_eq!(committer.capture_ids(), vec![good], "newer file published");
    assert_eq!(ms.len(), 1);
    assert!(ro.failing_since().is_none(), "a bad file is no outage");
    assert_eq!(ro.quarantined_total(), 0);
    for _ in 1..vc_store::segment::QUARANTINE_AFTER {
        ro.tick().await.expect("tick");
    }
    assert_eq!(ro.quarantined_total(), 1);
    assert!(spool.sealed().expect("sealed").is_empty());
    assert!(!bad.path.exists());
    let q = bad.path.with_extension("sealed.quarantined");
    assert!(q.exists(), "kept on disk for the operator: {}", q.display());
}

/// B14: a crash after `vc_commit_segment` leaves the sealed file; if a purge rewrote (or
/// dropped) the segment before the replay, the replay must not upload the original bytes again.
#[tokio::test]
async fn replay_after_commit_does_not_reupload_purged_bytes() {
    let _g = SERIAL.lock().await;
    let scenario = fail::FailScenario::setup();
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().expect("tmp");
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(t0()));
    let store = Arc::new(MemBlobStore::new());
    let pg = Arc::new(PgSegmentCommitter::new(pool.clone()));
    let mut spool = Spool::open(dir.path(), opts(), clock.clone()).expect("open");
    let id = spool
        .append(response("https://optout.example/p", t0(), b"PURGE-ME"))
        .await
        .expect("append");
    fail::cfg("segment::after_commit", "return").expect("cfg");
    let ro = roller(&spool, store.clone(), pg.clone(), clock.clone()).with_commit_probe(pg.clone());
    assert!(ro.flush().await.is_err(), "crash after commit");
    fail::remove("segment::after_commit");
    assert_eq!(spool.sealed().expect("sealed").len(), 1);

    // The purge runs before the fetcher restarts: the whole segment goes.
    let (path, cdxj): (String, String) = sqlx::query_as("SELECT path, cdxj_path FROM segments")
        .fetch_one(&pool)
        .await
        .expect("segment");
    let rw = rewrite_segment_without(store.as_ref(), &path, &cdxj, &HashSet::from([id]))
        .await
        .expect("rewrite");
    assert!(pg.commit_rewrite(&rw).await.expect("commit rewrite"));
    finish_rewrite(store.as_ref(), &rw).await.expect("finish");
    assert!(store.paths().is_empty());

    // Restart: the replay sees the segment is committed and only drops the spool file.
    let spool = Spool::open(dir.path(), opts(), clock.clone()).expect("reopen");
    let ro = roller(&spool, store.clone(), pg.clone(), clock).with_commit_probe(pg);
    let ms = ro.tick().await.expect("replay");
    assert!(ms.iter().all(|m| m.captures.is_empty()), "nothing new");
    assert!(
        store.paths().is_empty(),
        "purged bytes re-uploaded: {:?}",
        store.paths()
    );
    assert!(spool.sealed().expect("sealed").is_empty());
    scenario.teardown();
}

fn entry(u: &str, secs: i64, body: &[u8]) -> SpoolEntry {
    let r = response(u, t0() + chrono::Duration::seconds(secs), body);
    let enc = encode(&r).expect("enc");
    SpoolEntry {
        meta: r.meta,
        warc_record_id: enc.warc_record_id,
        record_count: enc.record_count,
        warc: enc.bytes,
    }
}

/// B12: a rewrite that crashed before its commit leaves an uncommitted `.r1`; a later purge
/// with a different removal set must not stall on the conflict but use the next free
/// generation, and report the stale one for deletion.
#[tokio::test]
async fn rewrite_uses_the_next_free_generation_after_a_stale_one() {
    let _g = SERIAL.lock().await;
    let store: Arc<dyn BlobStore> = Arc::new(MemBlobStore::new());
    let entries = vec![
        entry("https://a.example/1", 0, b"ONE"),
        entry("https://a.example/2", 1, b"TWO"),
        entry("https://a.example/3", 2, b"THREE"),
    ];
    let seg = build_segment(SegmentId::at(t0()), false, &entries, 3).expect("build");
    let m = &seg.manifest;
    upload(store.as_ref(), &m.path, seg.bytes.clone())
        .await
        .expect("up");
    upload(store.as_ref(), &m.cdxj_path, seg.cdxj.clone())
        .await
        .expect("up");
    let (one, two) = (m.captures[0].meta.capture_id, m.captures[1].meta.capture_id);

    // First purge: uploaded `.r1`, then crashed (never committed).
    let stale = rewrite_segment_without(store.as_ref(), &m.path, &m.cdxj_path, &[one].into())
        .await
        .expect("first");
    let stale_path = stale.new_path.clone().expect("r1");
    assert!(stale_path.ends_with(".r1.warc.zst"));
    assert!(stale.abandoned.is_empty());

    // Second purge removes more: `.r1` exists with other bytes.
    let rw = rewrite_segment_without(store.as_ref(), &m.path, &m.cdxj_path, &[one, two].into())
        .await
        .expect("a stale generation must not stall the purge");
    let p = rw.new_path.clone().expect("r2");
    assert!(p.ends_with(".r2.warc.zst"), "{p}");
    assert_eq!(rw.kept.len(), 1);
    let mut ab = rw.abandoned.clone();
    ab.sort();
    let mut want = vec![stale_path, stale.new_cdxj_path.clone().expect("cdxj")];
    want.sort();
    assert_eq!(ab, want, "the stale generation is reported for deletion");

    // Replaying the same rewrite is still idempotent (same generation).
    let again = rewrite_segment_without(store.as_ref(), &m.path, &m.cdxj_path, &[one, two].into())
        .await
        .expect("replay");
    assert_eq!(again.new_path, rw.new_path);
}
