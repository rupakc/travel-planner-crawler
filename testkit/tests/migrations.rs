//! Migration tests against a real pgvector/pgvector:pg17 (Task 0A, Step 2).

use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, Row};
use vc_core::frontier::Validators;
use vc_core::media::{FetchMode, MediaType};
use vc_core::store::{CaptureMeta, CaptureRole, ManifestCapture, SegmentManifest};
use vc_core::usepolicy::{UsePolicy, UseSignals};
use vc_core::{CanonicalUrl, CaptureId, Epoch, PayloadHash, SegmentId};

#[tokio::test]
async fn migrations_apply_on_empty_db() {
    let pool = testkit::pg().await;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied, 11);
    for t in [
        "sources",
        "source_candidates",
        "origins",
        "politeness_keys",
        "claims",
        "rate_bucket",
        "urls",
        "url_state",
        "segments",
        "captures",
        "work_items",
        "llm_budget",
        "llm_ledger",
        "batch_jobs",
        "llm_cache",
        "records",
        "record_versions",
        "extraction_runs",
        "observations",
        "entities",
        "entity_claims",
        "entity_redirects",
        "embeddings",
        "api_clients",
        "changes",
        "purge_requests",
        "tombstones",
        "audits",
        "crawl_digest",
        "heartbeats",
    ] {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(t)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(exists, "table {t} missing");
    }
    let parts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_inherits WHERE inhparent = 'url_state'::regclass",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(parts, 16);
    // A second run is a no-op.
    testkit::MIGRATOR.run(&pool).await.unwrap();
}

async fn seed_url(pool: &PgPool, url: &CanonicalUrl, epoch: i64) {
    sqlx::query("INSERT INTO politeness_keys (key) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(url.politeness_key().as_str())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO urls (url_key, url, key) VALUES ($1, $2, $3)")
        .bind(url.key().as_bytes().as_slice())
        .bind(url.as_str())
        .bind(url.politeness_key().as_str())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO url_state (url_key, key, state, lease_epoch) VALUES ($1, $2, 'fetched_pending', $3)",
    )
    .bind(url.key().as_bytes().as_slice())
    .bind(url.politeness_key().as_str())
    .bind(epoch)
    .execute(pool)
    .await
    .unwrap();
}

fn manifest(
    url: &CanonicalUrl,
    at: DateTime<Utc>,
    epoch: i64,
    etag: &str,
    body: u8,
) -> SegmentManifest {
    let meta = CaptureMeta {
        capture_id: CaptureId::at(at),
        captured_at: at,
        epoch: Epoch(epoch),
        orphan_epoch: false,
        url_key: url.key(),
        url: url.clone(),
        key: url.politeness_key(),
        source_id: None,
        role: CaptureRole::Primary,
        fetch_mode: FetchMode::Http,
        status: Some(200),
        media_type: MediaType::Html,
        payload_hash: Some(PayloadHash([body; 32])),
        payload_len: 10,
        validators: Validators {
            etag: Some(etag.into()),
            last_modified: None,
        },
        revisit_of: None,
        use_signals: UseSignals::default(),
        use_policy_fetch: UsePolicy::ALLOW_ALL,
        raw_expires_at: None,
        next_fetch_at: Some(at + Duration::days(1)),
    };
    let id = SegmentId::at(at);
    SegmentManifest {
        segment_id: id,
        path: format!("segments/{}/{id}.warc.zst", at.format("%Y/%m/%d")),
        cdxj_path: format!("cdxj/{id}.cdxj"),
        bytes: 100,
        crc32c: 42,
        record_count: 1,
        ephemeral: false,
        created_at: at,
        captures: vec![ManifestCapture {
            meta,
            offset: 0,
            length: 100,
            warc_record_id: format!("<urn:uuid:{}>", ulid::Ulid::generate()),
        }],
    }
}

async fn commit(pool: &PgPool, m: &SegmentManifest) -> bool {
    sqlx::query_scalar("SELECT vc_commit_segment($1)")
        .bind(serde_json::to_value(m).unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn state(pool: &PgPool, url: &CanonicalUrl) -> (String, Option<String>, Option<i64>, i32) {
    let r = sqlx::query(
        "SELECT state, etag, lease_epoch, change_count FROM url_state WHERE url_key = $1",
    )
    .bind(url.key().as_bytes().as_slice())
    .fetch_one(pool)
    .await
    .unwrap();
    (r.get(0), r.get(1), r.get(2), r.get(3))
}

#[tokio::test]
async fn commit_segment_guard_is_monotonic() {
    let pool = testkit::pg().await;
    let url = CanonicalUrl::parse("https://example.com/page").unwrap();
    seed_url(&pool, &url, 7).await;
    let t = Utc::now() - Duration::minutes(5);

    // The newer capture (epoch 7, the pending epoch) commits first.
    let newer = manifest(&url, t, 7, "\"new\"", 1);
    assert!(commit(&pool, &newer).await);
    assert_eq!(
        state(&pool, &url).await,
        ("due".into(), Some("\"new\"".into()), None, 0)
    );
    assert!(!commit(&pool, &newer).await, "idempotent by segment id");

    // A late/orphan segment with an OLDER capture must not move validators back.
    let older = manifest(&url, t - Duration::minutes(1), 3, "\"old\"", 2);
    assert!(commit(&pool, &older).await);
    assert_eq!(state(&pool, &url).await.1.as_deref(), Some("\"new\""));

    let captures: i64 = sqlx::query_scalar("SELECT count(*) FROM captures")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(captures, 2, "both captures are recorded");
    let work: i64 =
        sqlx::query_scalar("SELECT count(*) FROM work_items WHERE kind = 'capture_ready'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(work, 2);

    // A newer capture with a changed payload updates validators and counts a change.
    let newest = manifest(&url, t + Duration::minutes(1), 9, "\"newest\"", 3);
    assert!(commit(&pool, &newest).await);
    let (st, etag, _, changes) = state(&pool, &url).await;
    assert_eq!(
        (st.as_str(), etag.as_deref(), changes),
        ("due", Some("\"newest\""), 1)
    );
}

#[tokio::test]
async fn commit_flips_pending_only_for_its_own_epoch() {
    let pool = testkit::pg().await;
    let url = CanonicalUrl::parse("https://example.org/a").unwrap();
    seed_url(&pool, &url, 12).await;
    // An orphan capture of a dead epoch updates validators but leaves the pending lease alone.
    assert!(commit(&pool, &manifest(&url, Utc::now(), 11, "\"x\"", 1)).await);
    let (st, etag, ep, _) = state(&pool, &url).await;
    assert_eq!(
        (st.as_str(), etag.as_deref(), ep),
        ("fetched_pending", Some("\"x\""), Some(12))
    );
}

#[tokio::test]
async fn partitions_exist_for_current_and_next_two_months() {
    let pool = testkit::pg().await;
    let months: Vec<String> = sqlx::query_scalar(
        "SELECT to_char(vc_month_start(now()) + make_interval(months => m), 'YYYYMM') \
         FROM generate_series(0, 2) AS m",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for parent in ["captures", "record_versions", "observations"] {
        let children: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = $1::regclass",
        )
        .bind(parent)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            children.contains(&format!("{parent}_default")),
            "{parent}: {children:?}"
        );
        for m in &months {
            assert!(
                children.contains(&format!("{parent}_{m}")),
                "{parent}_{m} in {children:?}"
            );
        }
    }
    // Idempotent; extending moves rows out of DEFAULT.
    let created: i32 = sqlx::query_scalar("SELECT vc_ensure_partitions(2)")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(created, 0);
    sqlx::query(
        "INSERT INTO observations (series_key, observed_at, source_id, value, capture_id, captured_at) \
         VALUES ('fx:EUR:USD', vc_month_start(now()) + interval '4 months', 's', '1', 'c', now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    let created: i32 = sqlx::query_scalar("SELECT vc_ensure_partitions(4)")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(created, 6);
    let in_default: i64 = sqlx::query_scalar("SELECT count(*) FROM observations_default")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(in_default, 0);
}

#[tokio::test]
async fn enqueue_dedupes_pending_items() {
    let pool = testkit::pg().await;
    let q = "SELECT vc_enqueue('extract', '{}'::jsonb, 'k1')";
    let a: Option<i64> = sqlx::query_scalar(q).fetch_one(&pool).await.unwrap();
    let b: Option<i64> = sqlx::query_scalar(q).fetch_one(&pool).await.unwrap();
    assert!(a.is_some() && b.is_none());
    sqlx::query("UPDATE work_items SET state = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    let c: Option<i64> = sqlx::query_scalar(q).fetch_one(&pool).await.unwrap();
    assert!(c.is_some(), "a done item does not block a new one");
}
