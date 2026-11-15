//! Sources, statistics and readiness (`/v1/sources`, `/v1/stats`, `/readyz`).

use async_trait::async_trait;
use chrono::Utc;
use vc_core::Result;

use crate::dto::{SourceDto, StatsDto};

/// Catalogue data that is not part of the vc-core read model.
#[async_trait]
pub trait Catalog: Send + Sync {
    /// Every source, by id.
    async fn sources(&self) -> Result<Vec<SourceDto>>;
    /// Counts.
    async fn stats(&self) -> Result<StatsDto>;
    /// Readiness of the backing store.
    async fn ready(&self) -> Result<()>;
}

/// An in-memory [`Catalog`].
#[derive(Debug, Default)]
pub struct MemCatalog {
    /// Sources.
    pub sources: Vec<SourceDto>,
}

#[async_trait]
impl Catalog for MemCatalog {
    async fn sources(&self) -> Result<Vec<SourceDto>> {
        Ok(self.sources.clone())
    }
    async fn stats(&self) -> Result<StatsDto> {
        Ok(StatsDto {
            sources_enabled: i64::try_from(self.sources.iter().filter(|s| s.enabled).count())
                .unwrap_or(i64::MAX),
            records_active: 0,
            entities_active: 0,
            changes_seq_max: None,
            generated_at: Utc::now(),
        })
    }
    async fn ready(&self) -> Result<()> {
        Ok(())
    }
}
