//! The extracted-text tier (spec §5.5): `text/YYYY/MM/<segment>.jsonl.zst` per committed
//! segment, written idempotently; ephemeral captures go under `ephemeral/text/`.

#[path = "processor_support.rs"]
mod support;

use std::sync::Arc;

use support::*;
use vc_core::store::BlobStore;
use vcrawl::data::sources::SourceCache;
use vcrawl::data::text_tier::{TextRow, TextTier};

fn writer(r: &Rig) -> TextTier {
    TextTier::new(
        r.pool.clone(),
        r.blobs.clone() as Arc<dyn BlobStore>,
        vc_parse::registry(),
        vc_core::parse::Limits::default(),
        3,
    )
    .with_sources(SourceCache::with_ttl(
        r.pool.clone(),
        std::time::Duration::ZERO,
    ))
}

/// Runs one tick and returns the number written, failing with the per-segment errors (if any).
async fn written(w: &TextTier) -> usize {
    let t = w.tick(10).await.expect("claim");
    assert!(t.is_clean(), "text tier segments failed: {:?}", t.failed);
    t.written
}

async fn segment_of(r: &Rig, capture_id: &str) -> String {
    sqlx::query_scalar("SELECT segment_id FROM captures WHERE capture_id = $1")
        .bind(capture_id)
        .fetch_one(&r.pool)
        .await
        .unwrap()
}

async fn rows(r: &Rig, path: &str) -> Vec<TextRow> {
    let b = r.blobs.get(path, None).await.expect("text object");
    vc_store::text::decode_text_tier(&b).unwrap()
}

#[tokio::test]
async fn text_tier_written_per_segment_idempotently() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    let c = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://events.example.org/e/jazz-night",
            "events-test",
            &fixture("event_v1.html"),
            at(0),
        ),
    )
    .await;
    let seg = segment_of(&r, &c.capture_id.to_string()).await;
    let w = writer(&r);
    assert_eq!(written(&w).await, 1);

    // The path the spec gives: text/YYYY/MM/<segment-ULID>.jsonl.zst.
    let path = format!("text/2026/11/{seg}.jsonl.zst");
    assert_eq!(
        path,
        vc_store::paths::text_path(seg.parse().unwrap(), false)
    );
    let got = rows(&r, &path).await;
    assert_eq!(got.len(), 1);
    let row = &got[0];
    assert_eq!(row.capture_id, c.capture_id.to_string());
    assert_eq!(row.url, "https://events.example.org/e/jazz-night");
    assert_eq!(row.source_id, "events-test");
    assert!(
        row.main_text
            .as_deref()
            .unwrap_or("")
            .contains("Blue Room trio"),
        "main text kept"
    );
    assert!(
        row.structured
            .iter()
            .any(|o| o.to_string().contains("MusicEvent")),
        "structured data kept"
    );
    assert!(r.blobs.list("ephemeral/").await.unwrap().is_empty());

    // Nothing left to claim; writing the segment again is a no-op success with equal bytes.
    let before = r.blobs.get(&path, None).await.unwrap();
    assert_eq!(written(&w).await, 0);
    w.write_segment(&seg).await.unwrap();
    assert_eq!(r.blobs.get(&path, None).await.unwrap(), before);
    assert_eq!(
        count(
            &r.pool,
            "SELECT count(*) FROM text_tier_segments WHERE written_at IS NOT NULL AND archive_rows = 1"
        )
        .await,
        1
    );
    // A second instance finds nothing to do either.
    assert_eq!(written(&writer(&r)).await, 0);
}

#[tokio::test]
async fn noarchive_capture_goes_to_ephemeral_text_and_errors_are_skipped() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    let noarchive = fixture("event_v1.html").replace(
        "<meta charset=\"utf-8\">",
        "<meta charset=\"utf-8\"><meta name=\"robots\" content=\"noarchive\">",
    );
    let c = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://events.example.org/e/jazz-night",
            "events-test",
            &noarchive,
            at(0),
        ),
    )
    .await;
    let mut gone = Cap::html(
        "https://events.example.org/e/gone",
        "events-test",
        "<html><body>Not found</body></html>",
        at(1),
    );
    gone.status = Some(404);
    let g = commit(&r.pool, &r.blobs, gone).await;
    assert_eq!(written(&writer(&r)).await, 2);

    let seg = segment_of(&r, &c.capture_id.to_string()).await;
    let got = rows(&r, &format!("ephemeral/text/2026/11/{seg}.jsonl.zst")).await;
    assert_eq!(got.len(), 1, "body noarchive → ephemeral text");
    assert!(
        r.blobs
            .get(&format!("text/2026/11/{seg}.jsonl.zst"), None)
            .await
            .is_err(),
        "nothing under text/ for it"
    );
    let gseg = segment_of(&r, &g.capture_id.to_string()).await;
    assert!(
        r.blobs.list("text/").await.unwrap().is_empty(),
        "404 has no text"
    );
    let (a, e): (i32, i32) = sqlx::query_as(
        "SELECT archive_rows, ephemeral_rows FROM text_tier_segments WHERE segment_id = $1",
    )
    .bind(&gseg)
    .fetch_one(&r.pool)
    .await
    .unwrap();
    assert_eq!((a, e), (0, 0), "an empty segment is still marked written");
}

#[tokio::test]
async fn expired_claim_is_retried() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    let c = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://events.example.org/e/jazz-night",
            "events-test",
            &fixture("event_v1.html"),
            at(0),
        ),
    )
    .await;
    let seg = segment_of(&r, &c.capture_id.to_string()).await;
    // A writer claimed it and crashed.
    sqlx::query("INSERT INTO text_tier_segments (segment_id, claimed_at) VALUES ($1, now())")
        .bind(&seg)
        .execute(&r.pool)
        .await
        .unwrap();
    assert_eq!(written(&writer(&r)).await, 0, "claim still leased");
    sqlx::query("UPDATE text_tier_segments SET claimed_at = now() - interval '1 hour'")
        .execute(&r.pool)
        .await
        .unwrap();
    assert_eq!(written(&writer(&r)).await, 1, "expired claim retried");
    assert_eq!(
        rows(&r, &format!("text/2026/11/{seg}.jsonl.zst"))
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn failed_segment_is_surfaced_not_swallowed() {
    let r = rig(&["events-test"], RigOpts::default()).await;
    let c = commit(
        &r.pool,
        &r.blobs,
        Cap::html(
            "https://events.example.org/e/jazz-night",
            "events-test",
            &fixture("event_v1.html"),
            at(0),
        ),
    )
    .await;
    let seg = segment_of(&r, &c.capture_id.to_string()).await;
    let path: String = sqlx::query_scalar("SELECT path FROM segments WHERE segment_id = $1")
        .bind(&seg)
        .fetch_one(&r.pool)
        .await
        .unwrap();
    r.blobs.delete(&path).await.unwrap();
    let t = writer(&r).tick(10).await.unwrap();
    assert_eq!((t.claimed, t.written), (1, 0));
    assert_eq!(t.failed.len(), 1);
    assert_eq!(t.failed[0].0, seg);
    assert!(
        matches!(t.failed[0].1, vc_core::Error::NotFound(_)),
        "{:?}",
        t.failed
    );
}
