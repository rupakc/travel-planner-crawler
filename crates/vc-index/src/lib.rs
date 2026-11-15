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
#[derive(Debug, Clone)]
pub struct PgIndex {
    /// Pool.
    pub pool: PgPool,
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
