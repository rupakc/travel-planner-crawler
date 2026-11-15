//! Purge rewrite against Postgres: `vc_rewrite_segment` moves offsets and deletes rows once.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::*;
use testkit::{FakeClock, MemBlobStore};
use vc_core::Clock;
use vc_core::store::{BlobStore, CaptureSink, SegmentCommitter};
use vc_store::{
    PgSegmentCommitter, SegmentRoller, Spool, SpoolOptions, finish_rewrite, read_capture,
    rewrite_segment_without,
};

#[tokio::test]
async fn purge_rewrite_updates_rows_once() {
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().expect("tmp");
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(t0()));
    let store: Arc<MemBlobStore> = Arc::new(MemBlobStore::new());
    let pc = PgSegmentCommitter::new(pool.clone());
    let committer: Arc<dyn SegmentCommitter> = Arc::new(pc.clone());
    let mut spool = Spool::open(dir.path(), SpoolOptions::default(), clock.clone()).expect("open");
    let a = spool
        .append(response("https://a.example/1", t0(), b"A"))
        .await
        .expect("a");
    let b = spool
        .append(response("https://b.example/1", t0(), b"B"))
        .await
        .expect("b");
    let c = spool
        .append(response("https://a.example/2", t0(), b"C"))
        .await
        .expect("c");
    let ro = SegmentRoller::new(
        spool.clone(),
        store.clone(),
        committer,
        clock,
        &Default::default(),
    );
    let m = ro.flush().await.expect("flush").remove(0);

    let rw = rewrite_segment_without(store.as_ref(), &m.path, &m.cdxj_path, &HashSet::from([b]))
        .await
        .expect("rewrite");
    assert!(pc.commit_rewrite(&rw).await.expect("commit"));
    assert!(
        !pc.commit_rewrite(&rw).await.expect("replay"),
        "replay is a no-op"
    );
    finish_rewrite(store.as_ref() as &dyn BlobStore, &rw)
        .await
        .expect("finish");

    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT capture_id, warc_offset, warc_length FROM captures ORDER BY warc_offset",
    )
    .fetch_all(&pool)
    .await
    .expect("rows");
    let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
    assert_eq!(ids, vec![a.to_string(), c.to_string()]);
    let (path, rewritten): (String, bool) =
        sqlx::query_as("SELECT path, rewritten_at IS NOT NULL FROM segments")
            .fetch_one(&pool)
            .await
            .expect("segment");
    assert_eq!(Some(path.clone()), rw.new_path);
    assert!(rewritten);
    for (id, off, len) in rows {
        let recs = read_capture(store.as_ref(), &path, off as u64, len as u64)
            .await
            .expect("read");
        assert_eq!(recs[1].capture_id().map(|c| c.to_string()), Some(id));
    }
    assert!(!store.contains(&m.path));
}
