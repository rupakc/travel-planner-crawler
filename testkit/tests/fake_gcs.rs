//! `fake_gcs()` + `FakeGcsStore` behave like `MemBlobStore` (GCS `ifGenerationMatch=0`).

use bytes::Bytes;
use testkit::fake_gcs;
use vc_core::Error;
use vc_core::store::{BlobStore, PutOutcome};

#[tokio::test]
async fn fake_gcs_store_has_blob_store_semantics() {
    let s = fake_gcs().await;
    let b = Bytes::from_static(b"hello warc");
    let crc = crc32c::crc32c(&b);
    let path = "segments/2026/01/01/01J A+B.warc.zst";

    assert_eq!(
        s.put_if_absent(path, b.clone(), crc).await.unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        s.put_if_absent(path, b.clone(), crc).await.unwrap(),
        PutOutcome::AlreadyIdentical
    );
    let other = Bytes::from_static(b"different!");
    assert!(matches!(
        s.put_if_absent(path, other.clone(), crc32c::crc32c(&other))
            .await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        s.put_if_absent("x", b.clone(), crc ^ 1).await,
        Err(Error::Storage(_))
    ));

    assert_eq!(s.get(path, None).await.unwrap(), b);
    assert_eq!(&s.get(path, Some(6..10)).await.unwrap()[..], b"warc");
    assert!(s.get(path, Some(0..0)).await.unwrap().is_empty());
    assert!(matches!(
        s.get("missing", None).await,
        Err(Error::NotFound(_))
    ));

    let c = Bytes::from_static(b"cdx");
    s.put_if_absent("cdxj/2026/01/01/01J.cdxj", c.clone(), crc32c::crc32c(&c))
        .await
        .unwrap();
    assert_eq!(s.list("segments/").await.unwrap(), vec![path.to_string()]);
    assert_eq!(s.list("").await.unwrap().len(), 2);

    s.delete(path).await.unwrap();
    s.delete(path).await.unwrap(); // missing is fine
    assert!(matches!(s.get(path, None).await, Err(Error::NotFound(_))));

    // Each call gets its own bucket.
    let t = fake_gcs().await;
    assert_ne!(s.bucket(), t.bucket());
    assert!(t.list("").await.unwrap().is_empty());
}
