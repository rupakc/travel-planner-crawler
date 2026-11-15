//! # vc-llm (Task 8, stub)
//!
//! `VertexLlm`, `VertexEmbedder`, `PgLedger` (two-row reservation, §17.4), `VertexBatchRunner`
//! (§17.3), the billing breaker and `llm_cache`. With `llm.enabled = false` every call returns
//! `LlmError::Disabled` without touching the network. Block 14xx.

use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use vc_config::LlmConfig;
use vc_core::llm::{
    BatchIngest, BatchItem, BatchRunner, BudgetLedger, Embedder, LlmClient, LlmError, LlmRequest,
    LlmResponse, Purpose, ReconcileReport, Reservation, Usage, Usd,
};
use vc_core::{BatchJobId, ModelId, Result};

/// Gemini on Vertex AI (REST `generateContent`).
#[derive(Debug, Clone)]
pub struct VertexLlm {
    /// Settings.
    pub config: LlmConfig,
}

#[async_trait]
impl LlmClient for VertexLlm {
    async fn generate(&self, _r: &Reservation, _req: LlmRequest) -> Result<LlmResponse> {
        if !self.config.enabled {
            return Err(LlmError::Disabled.into());
        }
        todo!("Task 8")
    }
}

/// Vertex text embeddings.
#[derive(Debug, Clone)]
pub struct VertexEmbedder {
    /// Settings.
    pub config: LlmConfig,
    /// Model id.
    pub model: ModelId,
}

#[async_trait]
impl Embedder for VertexEmbedder {
    fn model(&self) -> &ModelId {
        &self.model
    }
    fn dim(&self) -> usize {
        self.config.embedding_dim
    }
    async fn embed(&self, _r: &Reservation, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if !self.config.enabled {
            return Err(LlmError::Disabled.into());
        }
        todo!("Task 8")
    }
}

/// The Postgres budget ledger.
#[derive(Debug, Clone)]
pub struct PgLedger {
    /// Pool.
    pub pool: PgPool,
    /// Settings.
    pub config: LlmConfig,
}

#[async_trait]
impl BudgetLedger for PgLedger {
    async fn reserve(&self, _purpose: Purpose, _est: Usd) -> Result<Option<Reservation>> {
        todo!("Task 8")
    }
    async fn reserve_batch(&self, _purpose: Purpose, _est: Usd) -> Result<Option<Reservation>> {
        todo!("Task 8")
    }
    async fn settle(&self, _r: Reservation, _actual: Usage) -> Result<()> {
        todo!("Task 8")
    }
}

/// The Vertex batch-prediction runner.
#[derive(Debug, Clone)]
pub struct VertexBatchRunner {
    /// Pool.
    pub pool: PgPool,
    /// Settings.
    pub config: LlmConfig,
}

#[async_trait]
impl BatchRunner for VertexBatchRunner {
    async fn prepare(&self, _r: Reservation, _items: Vec<BatchItem>) -> Result<BatchJobId> {
        todo!("Task 8")
    }
    async fn submit(&self, _id: &BatchJobId) -> Result<()> {
        todo!("Task 8")
    }
    async fn reconcile(&self, _timeout: Duration) -> Result<ReconcileReport> {
        todo!("Task 8")
    }
    async fn ingest(&self, _id: &BatchJobId) -> Result<BatchIngest> {
        todo!("Task 8")
    }
}
