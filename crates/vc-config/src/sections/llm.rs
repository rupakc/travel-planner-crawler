//! `[llm]`: Gemini on Vertex, embeddings, budget (owner: vc-llm).

use serde::{Deserialize, Serialize};

/// Shares of the monthly cap per purpose (§17.4). Must sum to 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PurposeShares {
    /// Extraction (0.70).
    pub extraction: f64,
    /// Embeddings (0.08).
    pub embedding: f64,
    /// Link triage (0.08).
    pub triage: f64,
    /// Quality auditor (0.07).
    pub auditor: f64,
    /// Source scout (0.04).
    pub scout: f64,
    /// Place disambiguation (0.03).
    pub place: f64,
}

impl Default for PurposeShares {
    fn default() -> Self {
        PurposeShares {
            extraction: 0.70,
            embedding: 0.08,
            triage: 0.08,
            auditor: 0.07,
            scout: 0.04,
            place: 0.03,
        }
    }
}

impl PurposeShares {
    /// Sum of all shares.
    pub fn sum(&self) -> f64 {
        self.extraction + self.embedding + self.triage + self.auditor + self.scout + self.place
    }
}

/// LLM settings. **Off by default**; the owner enables it after raising the project cap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    /// `VC_LLM_ENABLED` (default false: every call returns `LlmError::Disabled`).
    pub enabled: bool,
    /// `VC_LLM_MODEL`.
    pub model: String,
    /// Embedding model.
    pub embedding_model: String,
    /// Embedding dimension.
    pub embedding_dim: usize,
    /// GCP project.
    pub gcp_project: String,
    /// Vertex location (`global`).
    pub location: String,
    /// `VC_LLM_DAILY_USD` (upper bound of the dynamic daily cap).
    pub daily_usd: f64,
    /// `VC_LLM_MONTHLY_USD`.
    pub monthly_usd: f64,
    /// `VC_PROJECT_CAP_USD`: the project spend cap the owner set in the console. Required when
    /// enabled; the monthly cap must be ≤ 50 % of it.
    pub project_cap_usd: Option<f64>,
    /// Purpose shares of the monthly cap.
    pub shares: PurposeShares,
    /// `maxOutputTokens` for extraction.
    pub max_output_tokens: u32,
    /// Max input tokens per chunk.
    pub max_input_tokens: u32,
    /// Max chunks per capture.
    pub max_chunks: u32,
    /// Conservative chars-per-token ratio for estimates.
    pub chars_per_token: f64,
    /// Breaker duration after a billing/budget 403/429, hours.
    pub breaker_h: u64,
    /// Settle open (non-batch) reservations as spent after this many hours.
    pub stale_settle_h: u64,
    /// Cancel batch jobs after this many hours (72).
    pub batch_timeout_h: u64,
    /// Request timeout, seconds.
    pub request_timeout_s: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            enabled: false,
            model: "gemini-3.1-flash-lite".into(),
            embedding_model: "text-embedding-005".into(),
            embedding_dim: 768,
            gcp_project: "voyager-prod-ohzkl".into(),
            location: "global".into(),
            daily_usd: 1.50,
            monthly_usd: 40.0,
            project_cap_usd: None,
            shares: PurposeShares::default(),
            max_output_tokens: 400,
            max_input_tokens: 1200,
            max_chunks: 3,
            chars_per_token: 3.5,
            breaker_h: 6,
            stale_settle_h: 1,
            batch_timeout_h: 72,
            request_timeout_s: 60,
        }
    }
}
