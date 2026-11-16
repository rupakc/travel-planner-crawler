//! The raw store: blob store, this instance's spool and its segment roller.

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::PgPool;
use vc_config::Config;
use vc_core::store::{BlobStore, SegmentCommitter};
use vc_core::{Clock, Result};
use vc_store::{PgSegmentCommitter, SegmentRoller, Spool, SpoolOptions};

/// The raw-bucket blob store (`[store] backend`).
pub async fn blob_store(cfg: &Config) -> Result<Arc<dyn BlobStore>> {
    vc_store::blob_store_from_config(&cfg.store).await
}

/// `<store.spool_dir>/<role>-<instance>`: each role instance writes only its own spool.
pub fn spool_dir(cfg: &Config, role: &str, instance: &str) -> PathBuf {
    let safe: String = instance
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    PathBuf::from(&cfg.store.spool_dir).join(format!("{role}-{safe}"))
}

/// Opens this instance's spool (truncating torn tails) and builds its roller.
pub fn spool_and_roller(
    cfg: &Config,
    dir: PathBuf,
    store: Arc<dyn BlobStore>,
    committer: Arc<dyn SegmentCommitter>,
    clock: Arc<dyn Clock>,
) -> Result<(Spool, Arc<SegmentRoller>)> {
    let spool = Spool::open(dir, SpoolOptions::from(&cfg.store), clock.clone())?;
    let roller = SegmentRoller::new(spool.clone(), store, committer, clock, &cfg.store);
    Ok((spool, Arc::new(roller)))
}

/// The production committer (`vc_commit_segment`).
pub fn committer(pool: PgPool) -> Arc<dyn SegmentCommitter> {
    Arc::new(PgSegmentCommitter::new(pool))
}
