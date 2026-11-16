//! Embeddings under the LLM budget (spec §5.8, §5.11).
//!
//! * [`EmbedLoop`]: `PgIndex::pending_embeddings` → reserve (`Purpose::Embedding`) per batch →
//!   embed → `store_embedding`. A refused reservation stops the pass (no call is made); the
//!   records stay pending for the next pass.
//! * [`BudgetQueryEmbedder`]: the `vc-index` [`QueryEmbedder`] for search; a refused
//!   reservation or a failed call makes the search text-only.
//!
//! Every call reserves first and is settled: with the actual cost when the embedder settles it
//! itself (`VertexEmbedder::with_ledger`; settling is idempotent), else with the estimate, and
//! released (zero usage) when the call fails without settling.

use std::sync::Arc;

use async_trait::async_trait;
use vc_core::llm::{BudgetLedger, Embedder, Purpose, Reservation, Usage, Usd};
use vc_core::{ModelId, Result};
use vc_index::{EmbeddingTask, PgIndex, QueryEmbedder};

/// Estimates the cost of embedding texts with `chars` characters in total.
pub type Estimator = Arc<dyn Fn(usize) -> Usd + Send + Sync>;

/// The list-price estimator from `[llm]` (`embedding_usd_per_1k_chars`), at least 1 µUSD.
pub fn estimator(cfg: &vc_config::LlmConfig) -> Estimator {
    let p = vc_llm::Pricing::from_config(cfg);
    Arc::new(move |chars| {
        let e = p.embed_cost(chars);
        if e < Usd(1) { Usd(1) } else { e }
    })
}

/// One reserved embedding call. `Ok(None)` when the ledger refused (no call made).
pub async fn reserved_embed(
    embedder: &dyn Embedder,
    ledger: &dyn BudgetLedger,
    est: &Estimator,
    texts: &[String],
) -> Result<Option<Vec<Vec<f32>>>> {
    let chars = texts.iter().map(|t| t.chars().count()).sum();
    let est = est(chars);
    let Some(r): Option<Reservation> = ledger.reserve(Purpose::Embedding, est).await? else {
        return Ok(None);
    };
    match embedder.embed(&r, texts).await {
        Ok(v) => {
            // A no-op when the embedder already settled the actual cost.
            ledger
                .settle(
                    r,
                    Usage {
                        cost: est,
                        ..Usage::default()
                    },
                )
                .await?;
            Ok(Some(v))
        }
        Err(e) => {
            ledger.settle(r, Usage::default()).await?;
            Err(e)
        }
    }
}

/// What one embedder pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmbedPass {
    /// Records embedded and stored.
    pub embedded: usize,
    /// Model calls made.
    pub calls: usize,
    /// The ledger refused a reservation (the pass stopped there).
    pub budget_refused: bool,
}

/// The embedder role's loop body.
pub struct EmbedLoop {
    index: PgIndex,
    embedder: Arc<dyn Embedder>,
    ledger: Arc<dyn BudgetLedger>,
    est: Estimator,
    batch: usize,
    per_call: usize,
}

impl std::fmt::Debug for EmbedLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedLoop")
            .field("model", self.embedder.model())
            .field("batch", &self.batch)
            .finish_non_exhaustive()
    }
}

impl EmbedLoop {
    /// A loop reading `batch` pending records per pass and embedding `per_call` per request.
    pub fn new(
        index: PgIndex,
        embedder: Arc<dyn Embedder>,
        ledger: Arc<dyn BudgetLedger>,
        est: Estimator,
    ) -> Self {
        EmbedLoop {
            index,
            embedder,
            ledger,
            est,
            batch: 200,
            per_call: 5,
        }
    }

    /// Sets the batch sizes.
    pub fn with_sizes(mut self, batch: usize, per_call: usize) -> Self {
        self.batch = batch.max(1);
        self.per_call = per_call.max(1);
        self
    }

    /// The model.
    pub fn model(&self) -> &ModelId {
        self.embedder.model()
    }

    /// Creates the model's HNSW index if missing (outside a transaction).
    pub async fn ensure_index(&self) -> Result<bool> {
        self.index
            .ensure_embedding_index(self.embedder.model(), self.embedder.dim())
            .await
    }

    /// One pass over the pending records.
    pub async fn pass(&self) -> Result<EmbedPass> {
        let model = self.embedder.model().clone();
        let tasks: Vec<EmbeddingTask> = self
            .index
            .pending_embeddings(&model, u32::try_from(self.batch).unwrap_or(u32::MAX))
            .await?;
        let mut out = EmbedPass::default();
        for chunk in tasks.chunks(self.per_call) {
            let texts: Vec<String> = chunk.iter().map(|t| t.text.clone()).collect();
            let Some(vecs) = reserved_embed(
                self.embedder.as_ref(),
                self.ledger.as_ref(),
                &self.est,
                &texts,
            )
            .await?
            else {
                out.budget_refused = true;
                break;
            };
            out.calls += 1;
            for (t, v) in chunk.iter().zip(vecs) {
                self.index.store_embedding(t, &model, &v).await?;
                out.embedded += 1;
            }
        }
        if out.calls > 0 || out.budget_refused {
            tracing::info!(
                model = %model.0,
                embedded = out.embedded,
                calls = out.calls,
                budget_refused = out.budget_refused,
                "embedder pass"
            );
        }
        Ok(out)
    }
}

/// Query embeddings for hybrid search, under the embedding budget.
pub struct BudgetQueryEmbedder {
    embedder: Arc<dyn Embedder>,
    ledger: Arc<dyn BudgetLedger>,
    est: Estimator,
}

impl BudgetQueryEmbedder {
    /// Wraps an embedder.
    pub fn new(embedder: Arc<dyn Embedder>, ledger: Arc<dyn BudgetLedger>, est: Estimator) -> Self {
        BudgetQueryEmbedder {
            embedder,
            ledger,
            est,
        }
    }
}

#[async_trait]
impl QueryEmbedder for BudgetQueryEmbedder {
    fn model(&self) -> &ModelId {
        self.embedder.model()
    }

    fn dim(&self) -> usize {
        self.embedder.dim()
    }

    async fn embed_query(&self, q: &str) -> Result<Option<Vec<f32>>> {
        let q = q.trim();
        if q.is_empty() {
            return Ok(None);
        }
        let texts = [q.to_string()];
        Ok(reserved_embed(
            self.embedder.as_ref(),
            self.ledger.as_ref(),
            &self.est,
            &texts,
        )
        .await?
        .and_then(|mut v| v.pop()))
    }
}
