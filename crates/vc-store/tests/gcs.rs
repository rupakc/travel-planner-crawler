//! GcsBlobStore against fsouza/fake-gcs-server (container).

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::*;
use testkit::FakeClock;
use testkit::vc_store_fixtures::{FAKE_GCS_BUCKET, fake_gcs};
use vc_core::store::{BlobStore, CaptureSink, PutOutcome};
use vc_core::{Clock, Error};
use vc_store::{
    GcsBlobStore, GcsOptions, SegmentRoller, Spool, SpoolOptions, read_capture, read_segment,
};

async fn store() -> GcsBlobStore {
    GcsBlobStore::new(
        FAKE_GCS_BUCKET,
        GcsOptions {
            endpoint: Some(fake_gcs().endpoint.clone()),
            anonymous: true,
        },
    )
    .await
    .expect("gcs client")
}

#[tokio::test]
async fn gcs_integration() {
    let s = store().await;
    let run = ulid::Ulid::generate();
    let p = format!("t/{run}/segments/a.warc.zst");
    let b = Bytes::from_static(b"0123456789 segment bytes");
    let crc = crc32c::crc32c(&b);

    // ifGenerationMatch=0: create, identical retry, conflicting bytes.
    assert_eq!(
        s.put_if_absent(&p, b.clone(), crc).await.expect("put"),
        PutOutcome::Created
    );
    assert_eq!(
        s.put_if_absent(&p, b.clone(), crc).await.expect("again"),
        PutOutcome::AlreadyIdentical
    );
    let other = Bytes::from_static(b"different");
    let e = s
        .put_if_absent(&p, other.clone(), crc32c::crc32c(&other))
        .await
        .expect_err("conflict");
    assert!(matches!(e, Error::Conflict(_)), "{e:?}");
    assert!(matches!(
        s.put_if_absent(&p, b.clone(), crc ^ 1).await,
        Err(Error::Storage(_))
    ));

    // Reads: whole, range, missing.
    assert_eq!(s.get(&p, None).await.expect("get"), b);
    assert_eq!(&s.get(&p, Some(2..6)).await.expect("range")[..], b"2345");
    assert!(matches!(
        s.get(&format!("t/{run}/missing"), None).await,
        Err(Error::NotFound(_))
    ));

    // A full segment round trip through the roller.
    let dir = tempfile::tempdir().expect("tmp");
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(t0()));
    let mut spool = Spool::open(dir.path(), SpoolOptions::default(), clock.clone()).expect("spool");
    let id = spool
        .append(response("https://example.com/g", t0(), b"<html>gcs</html>"))
        .await
        .expect("append");
    let committer = Arc::new(RecordingCommitter::default());
    let gs: Arc<dyn BlobStore> = Arc::new(s.clone());
    let ro = SegmentRoller::new(
        spool.clone(),
        gs.clone(),
        committer.clone(),
        clock,
        &Default::default(),
    );
    let m = ro.flush().await.expect("flush").remove(0);
    let recs = read_segment(gs.as_ref(), &m.path).await.expect("segment");
    assert_eq!(recs[1].capture_id(), Some(id));
    let c = &m.captures[0];
    let recs = read_capture(gs.as_ref(), &m.path, c.offset, c.length)
        .await
        .expect("capture");
    assert_eq!(recs[1].capture_id(), Some(id));
}

/// `list` and `delete` go through `StorageControl`, which speaks gRPC only; fake-gcs-server
/// serves the JSON API only (it answers the HTTP/2 preface with 404). Run this against an
/// emulator with gRPC (or real GCS) by setting `VC_TEST_GCS_ENDPOINT` and
/// `cargo test -p vc-store --test gcs -- --ignored`.
#[tokio::test]
#[ignore = "needs a gRPC-capable GCS endpoint (fake-gcs-server is JSON-only)"]
async fn gcs_list_delete_grpc() {
    let s = store().await;
    let run = ulid::Ulid::generate();
    let p = format!("t/{run}/x");
    let b = Bytes::from_static(b"x");
    s.put_if_absent(&p, b.clone(), crc32c::crc32c(&b))
        .await
        .expect("put");
    assert_eq!(
        s.list(&format!("t/{run}/")).await.expect("list"),
        vec![p.clone()]
    );
    s.delete(&p).await.expect("delete");
    s.delete(&p).await.expect("delete missing is ok");
    assert!(matches!(s.get(&p, None).await, Err(Error::NotFound(_))));
}
