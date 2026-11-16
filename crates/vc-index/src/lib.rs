//! # vc-index (Task 11)
//!
//! The Postgres read model (spec §5.6, §5.9, §5.11, §17.7, §17.9). [`PgIndex`] implements both
//! halves of the contract in `vc_core::index`:
//!
//! * [`RecordSink`] — one transaction per capture: `records`, `record_versions` (a new version
//!   only when `data_hash` changes; older captures never overwrite newer ones), `observations`,
//!   entities + claims + redirects (the v1 resolution rule, licence-compatible merges only) and
//!   `changes` rows. Tombstones remove records from the API and index, are recorded as standing
//!   exclusions (`tombstones`) and are honoured on re-ingest.
//! * [`IndexReader`] — hybrid search (reciprocal rank fusion of `ts_rank` and vector
//!   similarity, × quality × freshness, with place / kind / licence filters), entities with
//!   redirects, record versions, the `(xid, seq)` change feed and observation series.
//!
//! Embeddings: [`PgIndex::ensure_embedding_index`] creates a per-model partial HNSW index
//! (halfvec, `m` from config), [`PgIndex::pending_embeddings`] / [`PgIndex::store_embedding`]
//! drive the embedder role, and a [`QueryEmbedder`] (wired by `vcrawl`, under the LLM budget)
//! adds the vector leg to search. Without one, search is text-only.
//!
//! Migration block 15xx (`1501_index_read_model.sql`).

mod embed;
mod entity;
mod reader;
mod sink;
mod util;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use vc_config::IndexConfig;
use vc_core::index::{
    ChangeCursor, ChangesPage, EntityLookup, IndexReader, Item, ObservationView, Page,
    PurgeSelector, RecordSink, RecordVersionView, SearchQuery, TombstoneReason, UpsertReport,
};
use vc_core::records::{ExtractedRecord, Provenance};
use vc_core::{EntityId, KindId, ModelId, RecordId, Result};

pub use embed::EmbeddingTask;

/// Embeds search text for the vector leg of hybrid search. `vcrawl` implements it over an
/// `Embedder` and the LLM budget; returning `Ok(None)` (e.g. budget refused) or an error makes
/// the search text-only.
#[async_trait]
pub trait QueryEmbedder: Send + Sync {
    /// The embedding model (must match the stored embeddings' `model_id`).
    fn model(&self) -> &ModelId;

    /// The vector dimension.
    fn dim(&self) -> usize;

    /// Embeds one query.
    async fn embed_query(&self, q: &str) -> Result<Option<Vec<f32>>>;
}

/// The Postgres index (writer and reader).
#[derive(Clone)]
pub struct PgIndex {
    /// Pool.
    pub pool: PgPool,
    config: IndexConfig,
    query_embedder: Option<Arc<dyn QueryEmbedder>>,
}

impl std::fmt::Debug for PgIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgIndex")
            .field("config", &self.config)
            .field(
                "query_embedder",
                &self.query_embedder.as_ref().map(|e| e.model().clone()),
            )
            .finish_non_exhaustive()
    }
}

impl PgIndex {
    /// An index over `pool` with default settings and text-only search.
    pub fn new(pool: PgPool) -> Self {
        Self::with_config(pool, IndexConfig::default())
    }

    /// An index over `pool` with `config`.
    pub fn with_config(pool: PgPool, config: IndexConfig) -> Self {
        PgIndex {
            pool,
            config,
            query_embedder: None,
        }
    }

    /// Adds the vector leg to search.
    pub fn with_query_embedder(mut self, e: Arc<dyn QueryEmbedder>) -> Self {
        self.query_embedder = Some(e);
        self
    }

    /// The settings.
    pub fn config(&self) -> &IndexConfig {
        &self.config
    }

    /// Merges entity `from` into `into` (operator merge): claims move, `from` becomes a
    /// redirect to `into` (earlier redirects to `from` are re-pointed, so chains never form)
    /// and a `redirect` change is written. Refused ([`vc_core::Error::Policy`]) unless the two
    /// are licence-compatible and of the same kind.
    pub async fn merge_entities(&self, from: &EntityId, into: &EntityId) -> Result<()> {
        entity::merge_public(self, from, into).await
    }
}

#[async_trait]
impl RecordSink for PgIndex {
    async fn upsert(&self, recs: Vec<ExtractedRecord>, prov: &Provenance) -> Result<UpsertReport> {
        sink::upsert(self, recs, prov).await
    }

    async fn tombstone(&self, sel: &PurgeSelector, reason: TombstoneReason) -> Result<u64> {
        sink::tombstone(self, sel, reason).await
    }
}

#[async_trait]
impl IndexReader for PgIndex {
    async fn search(&self, q: &SearchQuery) -> Result<Page<Item>> {
        reader::search(self, q).await
    }

    async fn entity(&self, id: &EntityId) -> Result<EntityLookup> {
        reader::entity(self, id).await
    }

    async fn record_versions(
        &self,
        id: RecordId,
        limit: u32,
        cursor: Option<String>,
    ) -> Result<Page<RecordVersionView>> {
        reader::record_versions(self, id, limit, cursor).await
    }

    async fn changes(
        &self,
        since: Option<ChangeCursor>,
        kinds: &[KindId],
        limit: u32,
    ) -> Result<ChangesPage> {
        reader::changes(self, since, kinds, limit).await
    }

    async fn observations(
        &self,
        series: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ObservationView>> {
        reader::observations(self, series, from, to, limit).await
    }
}
