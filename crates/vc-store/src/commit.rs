//! `PgSegmentCommitter`: the segment commit is one call to the SQL function
//! `vc_commit_segment(jsonb)` (r5 Q10), which runs in one transaction.

use async_trait::async_trait;
use sqlx::PgPool;
use vc_core::store::{SegmentCommitter, SegmentManifest};
use vc_core::{Error, Result};

use crate::purge::SegmentRewrite;

/// Commits segments through `vc_commit_segment(jsonb)`.
#[derive(Debug, Clone)]
pub struct PgSegmentCommitter {
    /// Pool.
    pub pool: PgPool,
}

impl PgSegmentCommitter {
    /// A committer on `pool`.
    pub fn new(pool: PgPool) -> Self {
        PgSegmentCommitter { pool }
    }

    /// Applies a purge rewrite (`vc_rewrite_segment`). Returns `false` when it was already
    /// applied (the segment no longer has the old path).
    pub async fn commit_rewrite(&self, rw: &SegmentRewrite) -> Result<bool> {
        let v = serde_json::to_value(rw)?;
        sqlx::query_scalar::<_, bool>("SELECT vc_rewrite_segment($1)")
            .bind(v)
            .fetch_one(&self.pool)
            .await
            .map_err(db)
    }
}

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

#[async_trait]
impl SegmentCommitter for PgSegmentCommitter {
    async fn commit(&self, m: &SegmentManifest) -> Result<()> {
        let v = serde_json::to_value(m)?;
        let fresh = sqlx::query_scalar::<_, bool>("SELECT vc_commit_segment($1)")
            .bind(v)
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        if !fresh {
            tracing::debug!(segment = %m.segment_id, "store: segment already committed");
        }
        Ok(())
    }
}
