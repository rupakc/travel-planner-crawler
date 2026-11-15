//! Purge rewrite: a segment rewritten without some captures keeps the others readable.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::*;
use vc_core::store::BlobStore;
use vc_store::segment::{read_capture, read_cdxj, read_segment};
use vc_store::spool::SpoolEntry;
use vc_store::warc::{encode, parse_http};
use vc_store::{build_segment, finish_rewrite, rewrite_segment_without, segment::upload};

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

#[tokio::test]
async fn purge_rewrite_removes_record_keeps_others() {
    let store: Arc<dyn BlobStore> = Arc::new(testkit::MemBlobStore::new());
    let entries = vec![
        entry("https://keep.example/1", 0, b"KEEP-ONE"),
        entry("https://optout.example/secret", 1, b"PURGE-ME-PLEASE"),
        entry("https://keep.example/2", 2, b"KEEP-TWO"),
    ];
    let seg = build_segment(vc_core::SegmentId::at(t0()), false, &entries, 3).expect("build");
    let m = &seg.manifest;
    upload(store.as_ref(), &m.path, seg.bytes.clone())
        .await
        .expect("up");
    upload(store.as_ref(), &m.cdxj_path, seg.cdxj.clone())
        .await
        .expect("up");
    let purged = m.captures[1].meta.capture_id;

    let rw = rewrite_segment_without(
        store.as_ref(),
        &m.path,
        &m.cdxj_path,
        &HashSet::from([purged]),
    )
    .await
    .expect("rewrite");
    let new_path = rw.new_path.clone().expect("kept captures → new segment");
    assert!(new_path.ends_with(".r1.warc.zst"), "{new_path}");
    assert_eq!(rw.kept.len(), 2);
    assert_eq!(rw.removed.len(), 1);
    assert_eq!(rw.removed[0].capture_id, purged);
    assert_eq!(rw.record_count, 4);

    // The purged bytes are gone from the new segment.
    let recs = read_segment(store.as_ref(), &new_path).await.expect("read");
    assert_eq!(recs.len(), 4);
    assert!(
        recs.iter()
            .all(|r| r.target_uri() != Some("https://optout.example/secret"))
    );
    // The kept captures read back at their new offsets.
    for (k, body) in rw.kept.iter().zip([&b"KEEP-ONE"[..], b"KEEP-TWO"]) {
        let recs = read_capture(store.as_ref(), &new_path, k.offset, k.length)
            .await
            .expect("cap");
        assert_eq!(recs[1].capture_id(), Some(k.capture_id));
        assert_eq!(&parse_http(&recs[1].block).expect("http").body[..], body);
    }
    // The new CDXJ matches.
    let cdx = read_cdxj(store.as_ref(), rw.new_cdxj_path.as_deref().expect("cdxj"))
        .await
        .expect("cdxj");
    assert_eq!(cdx.len(), 2);
    assert!(
        cdx.iter()
            .all(|e| e.filename == new_path && e.capture.meta.capture_id != purged)
    );

    // Idempotent replay of the upload step.
    let again = rewrite_segment_without(
        store.as_ref(),
        &m.path,
        &m.cdxj_path,
        &HashSet::from([purged]),
    )
    .await
    .expect("replay");
    assert_eq!(again, rw);

    // Old objects deleted last.
    finish_rewrite(store.as_ref(), &rw).await.expect("finish");
    assert!(store.get(&m.path, None).await.is_err());
    assert!(store.get(&m.cdxj_path, None).await.is_err());

    // A second purge of the rewritten segment goes to generation 2.
    let rw2 = rewrite_segment_without(
        store.as_ref(),
        &new_path,
        rw.new_cdxj_path.as_deref().expect("cdxj"),
        &HashSet::from([rw.kept[0].capture_id]),
    )
    .await
    .expect("rewrite 2");
    assert!(
        rw2.new_path
            .as_deref()
            .expect("path")
            .ends_with(".r2.warc.zst")
    );

    // Purging everything leaves no new segment.
    let all: HashSet<_> = m.captures.iter().map(|c| c.meta.capture_id).collect();
    let store2: Arc<dyn BlobStore> = Arc::new(testkit::MemBlobStore::new());
    upload(store2.as_ref(), &m.path, seg.bytes.clone())
        .await
        .expect("up");
    upload(store2.as_ref(), &m.cdxj_path, seg.cdxj.clone())
        .await
        .expect("up");
    let rw3 = rewrite_segment_without(store2.as_ref(), &m.path, &m.cdxj_path, &all)
        .await
        .expect("all");
    assert!(rw3.new_path.is_none() && rw3.kept.is_empty() && rw3.removed.len() == 3);
}
