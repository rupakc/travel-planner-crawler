//! Fetcher metrics: a sealed spool file the segment roller quarantines (unreadable) is exported
//! as `crawler_spool_quarantined_total`, which the spool-quarantine alert policy reads.

mod admin_common;
mod crawl_path_support;

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use crawl_path_support::*;
use testkit::MemBlobStore;
use vc_core::media::MediaType;
use vc_core::store::{SpoolPayload, SpoolRecord};
use vcrawl::roles::fetcher::Fetcher;

#[tokio::test]
async fn quarantined_spool_files_are_exported_as_a_counter() {
    let pool = testkit::pg().await;
    let dir = tempfile::tempdir().expect("tmp");
    let metrics = Arc::new(vc_obs::Metrics::new(
        Arc::new(vc_core::SystemClock),
        Duration::from_secs(180),
    ));
    let mut p = parts(
        &cfg(),
        &pool,
        &[],
        vec![],
        dir.path(),
        Arc::new(MemBlobStore::new()),
    );
    p.metrics = Some(metrics.clone());
    let spool = p.spool.clone();
    let f = Fetcher::new(p);

    let mut meta = admin_common::meta("https://example.com/a", chrono::Utc::now(), Some("test"));
    meta.media_type = MediaType::Json;
    spool
        .append_blocking(SpoolRecord {
            meta,
            payload: SpoolPayload::Metadata {
                fields: serde_json::json!({"type": "test"}),
            },
        })
        .expect("append");
    let sealed = spool.seal_all().expect("seal").remove(0);
    // Damage the sealed file (bit rot): it can never be published.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&sealed.path)
        .expect("open sealed")
        .write_all(b"garbage")
        .expect("damage");

    let exported = |m: &vc_obs::Metrics| {
        m.encode()
            .lines()
            .find(|l| l.starts_with("crawler_spool_quarantined_total "))
            .map(str::to_string)
    };
    for _ in 0..vc_store::segment::QUARANTINE_AFTER {
        f.tick_roller()
            .await
            .expect("a bad file does not fail the tick");
    }
    assert_eq!(f.parts().roller.quarantined_total(), 1);
    assert_eq!(metrics.spool_quarantined.get(), 1);
    assert_eq!(
        exported(&metrics).as_deref(),
        Some("crawler_spool_quarantined_total 1")
    );
    // Later ticks add only new quarantines (the roller's total is cumulative).
    f.tick_roller().await.expect("tick");
    f.flush().await.expect("flush");
    assert_eq!(metrics.spool_quarantined.get(), 1);
}
