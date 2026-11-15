//! Blob-store semantics (`ifGenerationMatch=0`): FsBlobStore, and the same contract on the
//! testkit fake.

use std::sync::Arc;

use bytes::Bytes;
use vc_core::Error;
use vc_core::store::{BlobStore, PutOutcome};
use vc_store::FsBlobStore;
use vc_store::segment::upload;

fn stores(dir: &std::path::Path) -> Vec<(&'static str, Arc<dyn BlobStore>)> {
    vec![
        ("fs", Arc::new(FsBlobStore::new(dir))),
        ("mem", Arc::new(testkit::MemBlobStore::new())),
    ]
}

#[tokio::test]
async fn upload_idempotent_same_bytes() {
    let dir = tempfile::tempdir().expect("tmp");
    for (name, s) in stores(dir.path()) {
        let b = Bytes::from_static(b"segment bytes");
        let crc = crc32c::crc32c(&b);
        let p = "segments/2026/11/15/A.warc.zst";
        assert_eq!(
            s.put_if_absent(p, b.clone(), crc).await.expect(name),
            PutOutcome::Created
        );
        assert_eq!(
            s.put_if_absent(p, b.clone(), crc).await.expect(name),
            PutOutcome::AlreadyIdentical,
            "{name}"
        );
        assert_eq!(
            upload(s.as_ref(), p, b.clone()).await.expect(name),
            PutOutcome::AlreadyIdentical
        );
        assert_eq!(s.get(p, None).await.expect(name), b);
        assert_eq!(&s.get(p, Some(2..7)).await.expect(name)[..], b"gment");
        assert_eq!(s.list("segments/").await.expect(name), vec![p.to_string()]);
    }
}

#[tokio::test]
async fn upload_mismatch_errors() {
    let dir = tempfile::tempdir().expect("tmp");
    for (name, s) in stores(dir.path()) {
        let p = "cdxj/2026/11/15/A.cdxj";
        let a = Bytes::from_static(b"one");
        let b = Bytes::from_static(b"two");
        s.put_if_absent(p, a.clone(), crc32c::crc32c(&a))
            .await
            .expect(name);
        let e = s
            .put_if_absent(p, b.clone(), crc32c::crc32c(&b))
            .await
            .expect_err(name);
        assert!(matches!(e, Error::Conflict(_)), "{name}: {e:?}");
        let e = upload(s.as_ref(), p, b).await.expect_err(name);
        assert!(matches!(e, Error::Conflict(_)), "{name}: {e:?}");
        // The original object is untouched.
        assert_eq!(s.get(p, None).await.expect(name), a);
        // A wrong crc32c is refused before anything is written.
        let e = s.put_if_absent("x/y", a.clone(), 1).await.expect_err(name);
        assert!(matches!(e, Error::Storage(_)), "{name}: {e:?}");
        assert!(!s.list("x/").await.expect(name).iter().any(|p| p == "x/y"));
    }
}

#[tokio::test]
async fn fs_store_semantics() {
    let dir = tempfile::tempdir().expect("tmp");
    let s = FsBlobStore::new(dir.path());
    assert!(matches!(
        s.get("missing", None).await,
        Err(Error::NotFound(_))
    ));
    s.delete("missing").await.expect("delete missing is ok");
    for bad in ["../escape", "/abs", "", "a/../../b", "dir/"] {
        assert!(
            s.put_if_absent(bad, Bytes::new(), 0).await.is_err(),
            "{bad}"
        );
    }
    let b = Bytes::from_static(b"abc");
    s.put_if_absent("a/1", b.clone(), crc32c::crc32c(&b))
        .await
        .expect("put");
    s.put_if_absent("a/2", b.clone(), crc32c::crc32c(&b))
        .await
        .expect("put");
    s.put_if_absent("b/1", b.clone(), crc32c::crc32c(&b))
        .await
        .expect("put");
    assert_eq!(s.list("a/").await.expect("list"), vec!["a/1", "a/2"]);
    assert!(s.get("a/1", Some(1..9)).await.is_err());
    s.delete("a/1").await.expect("delete");
    assert_eq!(s.list("").await.expect("list"), vec!["a/2", "b/1"]);
}
