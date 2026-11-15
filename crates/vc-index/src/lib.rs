//! # vc-index (Task 11, stub)
//!
//! `PgRecordSink` (records, versions, observations, entities, changes in one transaction) and
//! `PgIndexReader` (hybrid search, entities, versions, change feed). Block 15xx.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use vc_core::index::{
    ChangeCursor, ChangesPage, EntityLookup, IndexReader, Item, ObservationView, Page,
    PurgeSelector, RecordSink, RecordVersionView, SearchQuery, TombstoneReason, UpsertReport,
};
use vc_core::records::{ExtractedRecord, Provenance};
use vc_core::{EntityId, KindId, RecordId, Result};

/// The Postgres index (writer and reader).
#[derive(Clone)]
pub struct PgIndex {
    /// Pool.
    pub pool: PgPool,
}

/// Embeds search text.
#[async_trait]
pub trait QueryEmbedder: Send + Sync {
    /// Model.
    fn model(&self) -> &vc_core::ModelId;
    /// Dim.
    fn dim(&self) -> usize;
    /// Embeds.
    async fn embed_query(&self, q: &str) -> Result<Option<Vec<f32>>>;
}

/// Task.
pub struct EmbeddingTask {
    /// Text.
    pub text: String,
}

impl PgIndex {
    /// New.
    pub fn new(pool: PgPool) -> Self {
        PgIndex { pool }
    }
    /// With.
    pub fn with_query_embedder(self, _e: std::sync::Arc<dyn QueryEmbedder>) -> Self {
        todo!()
    }
    /// Merge.
    pub async fn merge_entities(&self, _a: &EntityId, _b: &EntityId) -> Result<()> {
        todo!()
    }
    /// Ensure.
    pub async fn ensure_embedding_index(&self, _m: &vc_core::ModelId, _d: usize) -> Result<bool> {
        todo!()
    }
    /// Pending.
    pub async fn pending_embeddings(&self, _m: &vc_core::ModelId, _l: u32) -> Result<Vec<EmbeddingTask>> {
        todo!()
    }
    /// Store.
    pub async fn store_embedding(&self, _t: &EmbeddingTask, _m: &vc_core::ModelId, _v: &[f32]) -> Result<()> {
        todo!()
    }
}

#[async_trait]
impl RecordSink for PgIndex {
    async fn upsert(
        &self,
        _recs: Vec<ExtractedRecord>,
        _prov: &Provenance,
    ) -> Result<UpsertReport> {
        todo!("Task 11")
    }
    async fn tombstone(&self, _sel: &PurgeSelector, _reason: TombstoneReason) -> Result<u64> {
        todo!("Task 11")
    }
}

#[async_trait]
impl IndexReader for PgIndex {
    async fn search(&self, _q: &SearchQuery) -> Result<Page<Item>> {
        todo!("Task 11")
    }
    async fn entity(&self, _id: &EntityId) -> Result<EntityLookup> {
        todo!("Task 11")
    }
    async fn record_versions(
        &self,
        _id: RecordId,
        _limit: u32,
        _cursor: Option<String>,
    ) -> Result<Page<RecordVersionView>> {
        todo!("Task 11")
    }
    async fn changes(
        &self,
        _since: Option<ChangeCursor>,
        _kinds: &[KindId],
        _limit: u32,
    ) -> Result<ChangesPage> {
        todo!("Task 11")
    }
    async fn observations(
        &self,
        _series: &str,
        _from: DateTime<Utc>,
        _to: DateTime<Utc>,
        _limit: u32,
    ) -> Result<Vec<ObservationView>> {
        todo!("Task 11")
    }
}
