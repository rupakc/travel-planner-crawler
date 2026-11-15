//! `VertexEmbedder`: `text-embedding-005` (768 dimensions) via the Vertex `predict` method.
//!
//! ```text
//! POST https://{loc}-aiplatform.googleapis.com/v1/projects/{p}/locations/{loc}/publishers/google/models/text-embedding-005:predict
//! { "instances": [{"content": "…", "task_type": "RETRIEVAL_DOCUMENT"}],
//!   "parameters": {"autoTruncate": true, "outputDimensionality": 768},
//!   "labels": {"app": "voyager-crawler", "purpose": "embedding"} }
//! → { "predictions": [{"embeddings": {"values": […], "statistics": {"token_count": n, "truncated": false}}}] }
//! ```
//!
//! The model is regional (`llm.embedding_location`). Vertex documents `labels` on `predict` as
//! honoured for Imagen only, so embedding spend is attributed by the ledger, not by label; the
//! labels are still sent on every request.
//!
//! [`Embedder::embed`] returns no usage, so when a ledger is attached the embedder settles the
//! reservation itself with the actual cost (settle is idempotent; a later caller settle is a
//! no-op).

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Method;
use serde_json::{Value, json};
use vc_config::LlmConfig;
use vc_core::llm::{BudgetLedger, Embedder, LlmError, Reservation, Usage, Usd};
use vc_core::{ModelId, Result};

use crate::client::{VertexClient, labels};
use crate::pricing::Pricing;

/// Texts per `predict` request. The Vertex docs give both 250 and 5 as the instance limit for
/// text-embedding-005; 5 is valid under either reading and requests are not billed.
pub const MAX_TEXTS_PER_REQUEST: usize = 5;

/// Vertex text embeddings.
#[derive(Clone)]
pub struct VertexEmbedder {
    /// Settings.
    pub config: LlmConfig,
    /// Model id.
    pub model: ModelId,
    vertex: VertexClient,
    pricing: Pricing,
    ledger: Option<Arc<dyn BudgetLedger>>,
}

impl std::fmt::Debug for VertexEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VertexEmbedder")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl VertexEmbedder {
    /// An embedder for `config.embedding_model`.
    pub fn new(config: LlmConfig, vertex: VertexClient) -> Self {
        let pricing = Pricing::from_config(&config);
        VertexEmbedder {
            model: ModelId::new(&config.embedding_model),
            config,
            vertex,
            pricing,
            ledger: None,
        }
    }

    /// Settles each reservation with the actual cost after a successful call.
    pub fn with_ledger(mut self, ledger: Arc<dyn BudgetLedger>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// The reservation estimate for `texts` (list price per character).
    pub fn estimate(&self, texts: &[String]) -> Usd {
        self.pricing
            .embed_cost(texts.iter().map(|t| t.chars().count()).sum())
    }

    async fn embed_chunk(
        &self,
        chunk: &[String],
        tokens: &mut u64,
    ) -> std::result::Result<Vec<Vec<f32>>, LlmError> {
        let body = json!({
            "instances": chunk
                .iter()
                .map(|t| json!({"content": t, "task_type": "RETRIEVAL_DOCUMENT"}))
                .collect::<Vec<_>>(),
            "parameters": {"autoTruncate": true, "outputDimensionality": self.config.embedding_dim},
            "labels": labels(vc_core::llm::Purpose::Embedding),
        });
        let url = self.vertex.model_url(
            &self.config.embedding_location,
            self.model.as_str(),
            "predict",
        );
        let v = self
            .vertex
            .call(Method::POST, &url, &[], Some(&body))
            .await?;
        let preds = v
            .get("predictions")
            .and_then(Value::as_array)
            .ok_or_else(|| LlmError::InvalidResponse("predict: no predictions".into()))?;
        if preds.len() != chunk.len() {
            return Err(LlmError::InvalidResponse(format!(
                "predict: {} predictions for {} texts",
                preds.len(),
                chunk.len()
            )));
        }
        let mut out = Vec::with_capacity(preds.len());
        for p in preds {
            *tokens += p
                .pointer("/embeddings/statistics/token_count")
                .and_then(Value::as_f64)
                .unwrap_or(0.0) as u64;
            let vals: Vec<f32> = p
                .pointer("/embeddings/values")
                .and_then(Value::as_array)
                .ok_or_else(|| LlmError::InvalidResponse("predict: no values".into()))?
                .iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect::<Option<_>>()
                .ok_or_else(|| LlmError::InvalidResponse("predict: non-numeric value".into()))?;
            if vals.len() != self.config.embedding_dim {
                return Err(LlmError::InvalidResponse(format!(
                    "predict: dimension {} != {}",
                    vals.len(),
                    self.config.embedding_dim
                )));
            }
            out.push(vals);
        }
        Ok(out)
    }
}

#[async_trait]
impl Embedder for VertexEmbedder {
    fn model(&self) -> &ModelId {
        &self.model
    }

    fn dim(&self) -> usize {
        self.config.embedding_dim
    }

    async fn embed(&self, r: &Reservation, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if !self.config.enabled {
            return Err(LlmError::Disabled.into());
        }
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        let mut tokens = 0u64;
        let mut done_chars = 0usize;
        let mut result = Ok(());
        for chunk in texts.chunks(MAX_TEXTS_PER_REQUEST) {
            match self.embed_chunk(chunk, &mut tokens).await {
                Ok(v) => {
                    done_chars += chunk.iter().map(|t| t.chars().count()).sum::<usize>();
                    out.extend(v);
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        let usage = Usage {
            input_tokens: tokens,
            cost: self.pricing.embed_cost(done_chars),
            ..Usage::default()
        };
        tracing::info!(
            target: "llm_usage",
            purpose = "embedding",
            model = self.model.as_str(),
            texts = texts.len(),
            input_tokens = tokens,
            cost_micro_usd = usage.cost.micros(),
            "embed call"
        );
        // Settle what was actually billed, also when a later chunk failed.
        if let Some(l) = &self.ledger {
            l.settle(r.clone(), usage).await?;
        }
        result?;
        Ok(out)
    }
}
