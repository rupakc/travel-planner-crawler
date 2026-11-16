//! `fetched_pending` can never get stuck (data review B1/B7): a segment committed before the
//! worker's `mark_fetched` sends the URL straight to `due`, and a sweeper returns
//! `fetched_pending` rows nobody will ever commit (a lost spool) to `due`.

use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use vc_config::FrontierConfig;
use vc_core::frontier::{Frontier, KeyOutcome, NewUrl, UrlOutcome, UrlResult};
use vc_core::{CanonicalUrl, UrlKey};
use vc_frontier::PgFrontier;

fn cfg() -> FrontierConfig {
    FrontierConfig {
        min_delay_s: 0,
        holder: Some("test".into()),
        ..FrontierConfig::default()
    }
}

async fn seed(f: &PgFrontier, host: &str, n: usize) -> Vec<UrlKey> {
    let batch: Vec<NewUrl> = (0..n)
        .map(|i| NewUrl::new(CanonicalUrl::parse(&format!("https://{host}/p/{i}")).unwrap()))
        .collect();
    f.enqueue(&batch).await.expect("enqueue");
    batch.iter().map(|u| u.url.key()).collect()
}

async fn state_of(pool: &PgPool, u: &UrlKey) -> (String, Option<i64>) {
    sqlx::query_as("SELECT state, lease_epoch FROM url_state WHERE url_key = $1")
        .bind(u.as_bytes().as_slice())
        .fetch_one(pool)
        .await
        .expect("url_state row")
}

async fn commit_primary(pool: &PgPool, seg: &str, u: &UrlKey, epoch: i64) {
    let manifest = serde_json::json!({
        "segment_id": seg,
        "path": format!("segments/{seg}.warc.zst"), "cdxj_path": format!("cdxj/{seg}.cdxj"),
        "bytes": 1, "crc32c": 1, "record_count": 1, "created_at": Utc::now(),
        "captures": [{
            "capture_id": format!("C{seg}"), "captured_at": Utc::now(),
            "offset": 0, "length": 1, "warc_record_id": "<urn:uuid:x>",
            "url_key": u.to_hex(), "url": "https://race.example.com/p/0",
            "key": "example.com", "role": "primary", "fetch_mode": "http", "epoch": epoch,
            "status": 200, "media_type": "html", "use_policy_fetch": {},
            "validators": {"etag": "\"v1\""}
        }]
    });
    let (ok,): (bool,) = sqlx::query_as("SELECT vc_commit_segment($1)")
        .bind(manifest)
        .fetch_one(pool)
        .await
        .expect("commit");
    assert!(ok);
}

/// B7: the roller seals and commits the frame before the worker marks the URL: the commit
/// finds it still `leased` (no flip), so `mark_fetched` must go straight to `due`.
#[tokio::test]
async fn mark_fetched_after_commit_goes_straight_to_due() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "race.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    let epoch = c.permit.epoch().0;
    commit_primary(&pool, "01JSEGMENTRACE00000000000001", &urls[0], epoch).await;
    assert_eq!(
        state_of(&pool, &urls[0]).await,
        ("leased".into(), Some(epoch))
    );
    f.mark_fetched(&c.permit, &urls[0], Utc::now() + chrono::Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(state_of(&pool, &urls[0]).await, ("due".into(), None));
    // Completion naming it Fetched leaves it alone.
    let o = KeyOutcome {
        urls: vec![UrlOutcome {
            url_key: urls[0],
            result: UrlResult::Fetched,
        }],
        ..KeyOutcome::normal()
    };
    f.complete(c.permit, o).await.unwrap();
    assert_eq!(state_of(&pool, &urls[0]).await, ("due".into(), None));
}

/// Without a committed capture, `mark_fetched` still waits for the commit.
#[tokio::test]
async fn mark_fetched_without_commit_stays_pending() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "race.example.com", 1).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    let epoch = c.permit.epoch().0;
    // A capture of another epoch does not count.
    commit_primary(&pool, "01JSEGMENTRACE00000000000002", &urls[0], epoch - 1).await;
    f.mark_fetched(&c.permit, &urls[0], Utc::now())
        .await
        .unwrap();
    assert_eq!(
        state_of(&pool, &urls[0]).await,
        ("fetched_pending".into(), Some(epoch))
    );
}

/// B1/B7: a `fetched_pending` row older than the window (its spool frame lost) goes back to
/// `due`; a recent one is left for its commit.
#[tokio::test]
async fn sweeper_returns_stale_pending_rows_to_due() {
    let pool = testkit::pg().await;
    let f = PgFrontier::new(pool.clone(), cfg());
    let urls = seed(&f, "stale.example.com", 2).await;
    let c = f.claim(&[0], 5).await.unwrap().expect("claim");
    let epoch = c.permit.epoch().0;
    for u in &urls {
        f.mark_fetched(&c.permit, u, Utc::now() + chrono::Duration::days(1))
            .await
            .unwrap();
    }
    f.complete(c.permit, KeyOutcome::normal()).await.unwrap();
    sqlx::query(
        "UPDATE url_state SET pending_since = now() - interval '7 hours' WHERE url_key = $1",
    )
    .bind(urls[0].as_bytes().as_slice())
    .execute(&pool)
    .await
    .unwrap();
    let n = f
        .sweep_stale_pending(Duration::from_secs(6 * 3600))
        .await
        .unwrap();
    assert_eq!(n, 1);
    let (state, ep) = state_of(&pool, &urls[0]).await;
    assert_eq!((state.as_str(), ep), ("due", None));
    let due_now: bool =
        sqlx::query_scalar("SELECT next_fetch_at <= now() FROM url_state WHERE url_key = $1")
            .bind(urls[0].as_bytes().as_slice())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(due_now, "the lost capture is fetched again");
    assert_eq!(
        state_of(&pool, &urls[1]).await,
        ("fetched_pending".into(), Some(epoch))
    );
    // A late commit of the swept URL is still applied (validators) without a flip.
    commit_primary(&pool, "01JSEGMENTSTALE0000000000001", &urls[0], epoch).await;
    assert_eq!(state_of(&pool, &urls[0]).await.0, "due");
}
