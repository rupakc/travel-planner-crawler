//! The raw-first oracle against a migrated database and a blob store.

use bytes::Bytes;
use sqlx::PgPool;
use testkit::oracle::check_raw_first;
use testkit::{MemBlobStore, assert_raw_first, pg};
use vc_core::store::BlobStore;

const SEG: &str = "segments/2026/01/01/01JSEG.warc.zst";
const CDXJ: &str = "cdxj/2026/01/01/01JSEG.cdxj";

async fn put(b: &MemBlobStore, path: &str) {
    let bytes = Bytes::from(format!("bytes of {path}"));
    b.put_if_absent(path, bytes.clone(), crc32c::crc32c(&bytes))
        .await
        .unwrap();
}

async fn segment(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO segments (segment_id, path, cdxj_path, bytes, crc32c, record_count, created_at)
         VALUES ('01JSEG', $1, $2, 10, 1, 1, '2026-01-01T00:00:00Z')",
    )
    .bind(SEG)
    .bind(CDXJ)
    .execute(pool)
    .await
    .unwrap();
}

async fn capture(pool: &PgPool, id: &str, segment: &str) {
    sqlx::query(
        "INSERT INTO captures (capture_id, captured_at, segment_id, warc_offset, warc_length,
            warc_record_id, url_key, url, key, role, fetch_mode, epoch, media_type, use_policy_fetch)
         VALUES ($1, '2026-01-01T00:00:00Z', $2, 0, 10, '<urn:uuid:x>', $3, 'http://blog.test/',
            'blog.test', 'primary', 'http', 1, 'html', '{}')",
    )
    .bind(id)
    .bind(segment)
    .bind(vec![7u8; 16])
    .execute(pool)
    .await
    .unwrap();
}

async fn version(pool: &PgPool, record_id: i64, capture_id: &str) {
    sqlx::query(
        "INSERT INTO record_versions (record_id, version, capture_id, captured_at, data, data_hash,
            kind_schema, extractor_ver, method, licence_class, use_policy)
         VALUES ($1, 1, $2, '2026-01-01T00:00:00Z', '{}', '\\x00', 'event@1', 't', 'structured',
            'open', '{}')",
    )
    .bind(record_id)
    .bind(capture_id)
    .execute(pool)
    .await
    .unwrap();
}

fn checks(v: &[testkit::oracle::RawFirstViolation]) -> Vec<&str> {
    v.iter().map(|x| x.check.as_str()).collect()
}

#[tokio::test]
async fn raw_first_passes_a_committed_capture() {
    let pool = pg().await;
    let blobs = MemBlobStore::new();
    put(&blobs, SEG).await;
    put(&blobs, CDXJ).await;
    segment(&pool).await;
    capture(&pool, "01JCAP", "01JSEG").await;
    version(&pool, 1, "01JCAP").await;
    sqlx::query(
        "INSERT INTO url_state (url_key, key, last_capture_id, last_captured_at, etag)
         VALUES ($1, 'blog.test', '01JCAP', '2026-01-01T00:00:00Z', '\"v1\"')",
    )
    .bind(vec![7u8; 16])
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(check_raw_first(&pool, &blobs).await.unwrap(), vec![]);
    assert_raw_first(&pool, &blobs).await;
}

#[tokio::test]
async fn raw_first_detects_every_kind_of_leak() {
    let pool = pg().await;
    let blobs = MemBlobStore::new();
    put(&blobs, SEG).await; // the CDXJ is missing
    segment(&pool).await;
    capture(&pool, "01JCAP", "01JSEG").await;
    capture(&pool, "01JORPHAN", "01JNOSEG").await;
    version(&pool, 1, "01JNOCAP").await;
    sqlx::query(
        "INSERT INTO observations (series_key, observed_at, source_id, value, capture_id, captured_at)
         VALUES ('fx:USDJPY', '2026-01-01T00:00:00Z', 's', '1', '01JNOCAP2', '2026-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO url_state (url_key, key, last_capture_id, etag)
         VALUES ($1, 'blog.test', '01JNOCAP3', NULL), ($2, 'blog.test', NULL, '\"v9\"')",
    )
    .bind(vec![1u8; 16])
    .bind(vec![2u8; 16])
    .execute(&pool)
    .await
    .unwrap();

    let v = check_raw_first(&pool, &blobs).await.unwrap();
    assert_eq!(
        checks(&v),
        vec![
            "version_without_capture",
            "observation_without_capture",
            "capture_without_segment",
            "cdxj_blob_missing",
            "url_state_capture_missing",
            "validators_without_capture",
        ],
        "{v:?}"
    );

    // Deleting the segment object is caught too; a purged segment is not checked.
    blobs.delete(SEG).await.unwrap();
    let v = check_raw_first(&pool, &blobs).await.unwrap();
    assert!(checks(&v).contains(&"segment_blob_missing"));
    sqlx::query("UPDATE segments SET purged_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    let v = check_raw_first(&pool, &blobs).await.unwrap();
    assert!(!checks(&v).contains(&"segment_blob_missing"));
}

#[tokio::test]
#[should_panic(expected = "raw-first oracle")]
async fn assert_raw_first_panics_with_a_report() {
    let pool = pg().await;
    version(&pool, 1, "01JNOCAP").await;
    assert_raw_first(&pool, &MemBlobStore::new()).await;
}
