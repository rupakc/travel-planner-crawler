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
    // P0 owns 0001..0011; P1 crates add their own blocks (11xx..18xx), so check the P0 set
    // is fully applied rather than an exact total.
    let p0: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM _sqlx_migrations WHERE success AND version BETWEEN 1 AND 99",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(p0, 11);
    let failed: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE NOT success")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(failed, 0);
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

/// `m` turned into a revisit of `of` (a 304, or an identical-payload `status`), carrying
/// `etag` and C0's payload hash.
fn revisit(mut m: SegmentManifest, of: &SegmentManifest, status: u16) -> SegmentManifest {
    let c0 = &of.captures[0].meta;
    let meta = &mut m.captures[0].meta;
    meta.status = Some(status);
    meta.payload_hash = c0.payload_hash;
    meta.revisit_of = Some(c0.capture_ref());
    m
}

/// `(last_capture_id, body_captured_at, last_captured_at, etag, fetch_count)` of a URL.
async fn pointers(
    pool: &PgPool,
    url: &CanonicalUrl,
) -> (
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<String>,
    i32,
) {
    let r = sqlx::query(
        "SELECT last_capture_id, body_captured_at, last_captured_at, etag, fetch_count
         FROM url_state WHERE url_key = $1",
    )
    .bind(url.key().as_bytes().as_slice())
    .fetch_one(pool)
    .await
    .unwrap();
    (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4))
}

/// B5 (1103): after C0 200, R1 304, R2 304 and an identical-payload 200 (a dedupe revisit),
/// url_state still points at C0's body; validators and the fetch clock keep moving. A 2xx
/// without a payload hash is not a body either; a new body moves the pointer.
#[tokio::test]
async fn commit_points_url_state_at_the_last_body_capture() {
    let pool = testkit::pg().await;
    let url = CanonicalUrl::parse("https://example.com/body").unwrap();
    seed_url(&pool, &url, 1).await;
    let t = Utc::now() - Duration::minutes(30);
    let to_us = |t: DateTime<Utc>| t.timestamp_micros();

    let c0 = manifest(&url, t, 1, "\"v1\"", 1);
    assert!(commit(&pool, &c0).await);
    let c0_id = c0.captures[0].meta.capture_id.to_string();
    let (id, body_at, last_at, _, _) = pointers(&pool, &url).await;
    assert_eq!(id.as_deref(), Some(c0_id.as_str()));
    assert_eq!(body_at.map(to_us), Some(to_us(t)));
    assert_eq!(last_at, body_at);

    let steps = [(1, 304, "\"v1\""), (2, 304, "\"v1\""), (3, 200, "\"v1b\"")];
    for (i, status, etag) in steps {
        let at = t + Duration::minutes(i);
        let r = revisit(manifest(&url, at, 1 + i, etag, 9), &c0, status);
        assert!(commit(&pool, &r).await);
        let (id, body_at, last_at, et, n) = pointers(&pool, &url).await;
        assert_eq!(id.as_deref(), Some(c0_id.as_str()), "step {i}: still C0");
        assert_eq!(body_at.map(to_us), Some(to_us(t)), "step {i}");
        assert_eq!(last_at.map(to_us), Some(to_us(at)), "step {i}: last fetch");
        assert_eq!(et.as_deref(), Some(etag), "step {i}: validators refresh");
        assert_eq!(n, 1 + i as i32);
    }
    let ph: Option<Vec<u8>> = sqlx::query_scalar("SELECT payload_hash FROM url_state")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ph, Some(vec![1; 32]), "C0's payload hash is kept");

    // A 2xx primary without a payload hash holds no body: the pointer stays.
    let mut nohash = manifest(&url, t + Duration::minutes(4), 5, "\"v1c\"", 4);
    nohash.captures[0].meta.payload_hash = None;
    assert!(commit(&pool, &nohash).await);
    assert_eq!(
        pointers(&pool, &url).await.0.as_deref(),
        Some(c0_id.as_str())
    );

    // A new body moves it (with its own timestamp).
    let at = t + Duration::minutes(5);
    let c5 = manifest(&url, at, 6, "\"v2\"", 2);
    assert!(commit(&pool, &c5).await);
    let (id, body_at, last_at, _, _) = pointers(&pool, &url).await;
    assert_eq!(id, Some(c5.captures[0].meta.capture_id.to_string()));
    assert_eq!(body_at.map(to_us), Some(to_us(at)));
    assert_eq!(last_at, body_at);

    // A late, older body never moves it back (monotonic guard).
    let late = manifest(&url, t + Duration::seconds(30), 7, "\"old\"", 7);
    assert!(commit(&pool, &late).await);
    assert_eq!(
        pointers(&pool, &url).await.0,
        Some(c5.captures[0].meta.capture_id.to_string())
    );

    // Releasing the pointer (vc_rewrite_segment) clears its timestamp too.
    sqlx::query("UPDATE url_state SET last_capture_id = NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(pointers(&pool, &url).await.1, None);
}

/// B5 (1103) backfill: a pointer left at the end of an old revisit chain (R2 -> R1 -> C0) is
/// resolved to C0; a chain that cannot be resolved keeps its pointer and timestamp.
#[tokio::test]
async fn body_capture_backfill_resolves_revisit_chains() {
    let pool = testkit::pg().await;
    let a = CanonicalUrl::parse("https://example.com/chain").unwrap();
    let b = CanonicalUrl::parse("https://example.com/broken").unwrap();
    seed_url(&pool, &a, 1).await;
    seed_url(&pool, &b, 1).await;
    let t = Utc::now() - Duration::minutes(30);
    let c0 = manifest(&a, t, 1, "\"v1\"", 1);
    let r1 = revisit(
        manifest(&a, t + Duration::minutes(1), 2, "\"v1\"", 1),
        &c0,
        304,
    );
    let r2 = revisit(
        manifest(&a, t + Duration::minutes(2), 3, "\"v1\"", 1),
        &r1,
        304,
    );
    for m in [&c0, &r1, &r2] {
        assert!(commit(&pool, m).await);
    }
    let orphan = manifest(&b, t, 1, "\"x\"", 5);
    let rb = revisit(
        manifest(&b, t + Duration::minutes(1), 2, "\"x\"", 5),
        &orphan,
        304,
    );
    assert!(
        commit(&pool, &rb).await,
        "a revisit of a capture never committed"
    );

    // Back to the pre-1103 shape: the pointer moved with every primary capture.
    for q in [
        "DROP TRIGGER url_state_clear_body_ref ON url_state",
        "DROP FUNCTION vc_url_state_clear_body_ref()",
        "ALTER TABLE url_state DROP COLUMN body_captured_at",
        "DELETE FROM _sqlx_migrations WHERE version = 1103",
    ] {
        sqlx::query(q).execute(&pool).await.unwrap();
    }
    for (u, m) in [(&a, &r2), (&b, &rb)] {
        sqlx::query(
            "UPDATE url_state SET last_capture_id = $2, last_captured_at = $3
             WHERE url_key = $1",
        )
        .bind(u.key().as_bytes().as_slice())
        .bind(m.captures[0].meta.capture_id.to_string())
        .bind(m.captures[0].meta.captured_at)
        .execute(&pool)
        .await
        .unwrap();
    }
    testkit::MIGRATOR.run(&pool).await.unwrap();

    let to_us = |t: Option<DateTime<Utc>>| t.map(|t| t.timestamp_micros());
    let (id, body_at, last_at, _, _) = pointers(&pool, &a).await;
    assert_eq!(id, Some(c0.captures[0].meta.capture_id.to_string()));
    assert_eq!(to_us(body_at), Some(t.timestamp_micros()));
    assert_eq!(
        to_us(last_at),
        Some((t + Duration::minutes(2)).timestamp_micros()),
        "the fetch clock is untouched"
    );
    let (id, body_at, last_at, _, _) = pointers(&pool, &b).await;
    assert_eq!(id, Some(rb.captures[0].meta.capture_id.to_string()));
    assert_eq!(body_at, last_at);
}
