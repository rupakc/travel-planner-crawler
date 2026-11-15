//! LLMs, embeddings, the race-free budget ledger and the batch runner (spec §5.8, §17.3,
//! §17.4). Implemented by `vc-llm` (`VertexLlm`, `VertexEmbedder`, `PgLedger`,
//! `VertexBatchRunner`) and testkit fakes.
//!
//! Every model use goes through the ledger: reserve first, call, then settle. A reservation
//! counts against **both** the day row and the month row of `llm_budget` in one statement. With
//! `VC_LLM_ENABLED=false` (the default) every call fails with [`LlmError::Disabled`] and no
//! network request is made.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{ErrorClass, Result};
use crate::ids::{BatchJobId, ModelId, ReservationId, WorkId};

/// What a model call is for. Each purpose has a share of the monthly cap (§17.4): extraction
/// 70 %, embedding 8 %, triage 8 %, auditor 7 %, scout 4 %, place 3 %.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// Kind extraction from unstructured text.
    Extraction,
    /// Embeddings.
    Embedding,
    /// Link triage.
    Triage,
    /// Quality auditor.
    Auditor,
    /// Source scout.
    Scout,
    /// Place disambiguation.
    Place,
}

impl Purpose {
    /// All purposes.
    pub const ALL: [Purpose; 6] = [
        Purpose::Extraction,
        Purpose::Embedding,
        Purpose::Triage,
        Purpose::Auditor,
        Purpose::Scout,
        Purpose::Place,
    ];

    /// The stable snake_case name (also the Vertex `purpose` label).
    pub fn as_str(&self) -> &'static str {
        match self {
            Purpose::Extraction => "extraction",
            Purpose::Embedding => "embedding",
            Purpose::Triage => "triage",
            Purpose::Auditor => "auditor",
            Purpose::Scout => "scout",
            Purpose::Place => "place",
        }
    }
}

/// Money in **micro-USD** (1 USD = 1,000,000). Integer arithmetic only; stored as
/// `numeric(14,6)` dollars in Postgres.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Usd(pub i64);

impl Usd {
    /// Zero.
    pub const ZERO: Usd = Usd(0);

    /// From micro-USD.
    pub fn from_micros(m: i64) -> Usd {
        Usd(m)
    }

    /// From dollars (rounded to the nearest micro-USD).
    pub fn from_dollars(d: f64) -> Usd {
        Usd((d * 1_000_000.0).round() as i64)
    }

    /// Micro-USD.
    pub fn micros(&self) -> i64 {
        self.0
    }

    /// Dollars (for display and SQL numeric).
    pub fn dollars(&self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Saturating addition.
    pub fn saturating_add(self, o: Usd) -> Usd {
        Usd(self.0.saturating_add(o.0))
    }

    /// Saturating subtraction.
    pub fn saturating_sub(self, o: Usd) -> Usd {
        Usd(self.0.saturating_sub(o.0))
    }
}

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${:.6}", self.dollars())
    }
}

/// A held reservation against the day and month rows of `llm_budget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    /// Ledger row id.
    pub id: ReservationId,
    /// Purpose.
    pub purpose: Purpose,
    /// Reserved estimate.
    pub est: Usd,
    /// The day row (`llm_budget.period = 'YYYY-MM-DD'`).
    pub day: NaiveDate,
    /// The month row (`llm_budget.period = 'YYYY-MM'`).
    pub month: String,
    /// Batch reservations stay open until ingest (exempt from the 1 h auto-settle).
    pub batch: bool,
    /// When it was made.
    pub created_at: DateTime<Utc>,
}

/// Actual usage of a call (or of a whole batch).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Thinking tokens.
    pub thinking_tokens: u64,
    /// Cached input tokens.
    pub cached_input_tokens: u64,
    /// Cost at list price (batch price for batch calls).
    pub cost: Usd,
}

/// The race-free budget ledger.
#[async_trait]
pub trait BudgetLedger: Send + Sync {
    /// Reserves `est` against both the day and the month row (and the purpose share) in one
    /// statement; `Ok(None)` when any cap would be exceeded (then **no call**).
    async fn reserve(&self, purpose: Purpose, est: Usd) -> Result<Option<Reservation>>;

    /// Like [`BudgetLedger::reserve`] for a whole batch job; the reservation stays open until
    /// ingest (no 1 h auto-settle).
    async fn reserve_batch(&self, purpose: Purpose, est: Usd) -> Result<Option<Reservation>>;

    /// Settles with the actual usage (zero usage releases the reservation). Idempotent.
    async fn settle(&self, r: Reservation, actual: Usage) -> Result<()>;
}

/// Gemini thinking level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Off / budget 0 where supported.
    #[default]
    Off,
    /// Minimal.
    Minimal,
    /// Low.
    Low,
}

/// One generation request. The page text goes in `input` and is always treated as untrusted
/// data by the system prompt; there are no tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    /// Model (`None` = the configured `VC_LLM_MODEL`).
    pub model: Option<ModelId>,
    /// Purpose (also the `purpose` label).
    pub purpose: Purpose,
    /// System prompt.
    pub system: String,
    /// User content (untrusted data).
    pub input: String,
    /// JSON Schema for `responseSchema` (JSON output when set).
    pub response_schema: Option<serde_json::Value>,
    /// `maxOutputTokens` (400 for extraction).
    pub max_output_tokens: u32,
    /// Thinking level.
    pub thinking: ThinkingLevel,
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Normal stop.
    Stop,
    /// Hit `maxOutputTokens`.
    MaxTokens,
    /// Blocked by safety filters.
    Safety,
    /// Anything else.
    Other,
}

/// One generation response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmResponse {
    /// Raw text.
    pub text: String,
    /// Parsed JSON when a schema was requested and the text parsed.
    pub json: Option<serde_json::Value>,
    /// Usage.
    pub usage: Usage,
    /// Model that answered.
    pub model: ModelId,
    /// Finish reason.
    pub finish: FinishReason,
    /// Served from `llm_cache` (no call, no cost).
    pub from_cache: bool,
}

/// LLM failures and refusals.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LlmError {
    /// `VC_LLM_ENABLED=false`: no network call was made.
    #[error("llm disabled")]
    Disabled,
    /// The ledger refused the reservation.
    #[error("llm budget exhausted")]
    BudgetExhausted,
    /// The 6 h breaker is open (after a billing/budget 403/429).
    #[error("llm breaker open until {until}")]
    BreakerOpen {
        /// When it closes.
        until: DateTime<Utc>,
    },
    /// Vertex rate-limited the call (not billing related).
    #[error("llm rate limited")]
    RateLimited,
    /// The model is not available (startup probe).
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),
    /// The response was not valid for the schema.
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    /// HTTP error from Vertex.
    #[error("vertex http {status}: {message}")]
    Http {
        /// Status.
        status: u16,
        /// Error message from Vertex (never contains prompt text).
        message: String,
    },
    /// Timeout.
    #[error("llm timeout")]
    Timeout,
    /// Transport failure.
    #[error("llm transport: {0}")]
    Transport(String),
}

impl LlmError {
    /// The coarse class.
    pub fn class(&self) -> ErrorClass {
        match self {
            LlmError::Disabled | LlmError::BudgetExhausted | LlmError::BreakerOpen { .. } => {
                ErrorClass::Budget
            }
            LlmError::RateLimited => ErrorClass::RateLimited,
            LlmError::ModelUnavailable(_) | LlmError::InvalidResponse(_) => ErrorClass::Permanent,
            LlmError::Http { status, .. } if *status >= 500 => ErrorClass::Transient,
            LlmError::Http { .. } => ErrorClass::Permanent,
            LlmError::Timeout | LlmError::Transport(_) => ErrorClass::Transient,
        }
    }
}

/// Online generation (Vertex `generateContent`; labels `{app: voyager-crawler, purpose}` on
/// every request).
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Generates under a reservation (the caller settles it with the returned usage).
    async fn generate(&self, r: &Reservation, req: LlmRequest) -> Result<LlmResponse>;
}

/// Text embeddings.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// The model (e.g. `text-embedding-005`).
    fn model(&self) -> &ModelId;

    /// Vector dimension (768 for `text-embedding-005`).
    fn dim(&self) -> usize;

    /// Embeds texts (one vector per text, same order) under a reservation.
    async fn embed(&self, r: &Reservation, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// One line of a batch input (`key = <work_id>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchItem {
    /// The work item this answers.
    pub key: WorkId,
    /// The request.
    pub request: LlmRequest,
}

/// `batch_jobs.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchState {
    /// Input written to GCS, row inserted, not yet submitted.
    Prepared,
    /// Submitted to Vertex (job name recorded).
    Submitted,
    /// Vertex finished successfully; results not yet ingested.
    Succeeded,
    /// Failed (or cancelled after 72 h); items nacked.
    Failed,
    /// Results ingested; reservation settled.
    Ingested,
}

/// What [`BatchRunner::reconcile`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileReport {
    /// Jobs found in Vertex (label `batch_id`) whose row lacked a job name, now adopted.
    pub adopted: u32,
    /// Jobs cancelled after 72 h.
    pub cancelled: u32,
    /// Jobs that finished since the last run (ready for ingest).
    pub finished: Vec<BatchJobId>,
}

/// The result of [`BatchRunner::ingest`].
#[derive(Debug, Clone, PartialEq)]
pub enum BatchIngest {
    /// Not finished yet.
    Pending,
    /// Results mapped by key; `missing` keys (absent or failed lines) must be nacked.
    Ingested {
        /// Successful results.
        results: Vec<(WorkId, LlmResponse)>,
        /// Keys without a usable result.
        missing: Vec<WorkId>,
    },
    /// The job failed or timed out; every key must be nacked.
    Failed {
        /// Error message (no prompt text).
        error: String,
        /// All keys.
        keys: Vec<WorkId>,
    },
}

/// The Vertex batch-prediction lifecycle (§17.3): prepare → submit → reconcile → ingest.
#[async_trait]
pub trait BatchRunner: Send + Sync {
    /// Writes the input JSONL to GCS and inserts a `prepared` row holding the (batch)
    /// reservation.
    async fn prepare(&self, r: Reservation, items: Vec<BatchItem>) -> Result<BatchJobId>;

    /// Submits a prepared job (display name = row id, label `batch_id`); the row becomes
    /// `submitted`. Never resubmits a row without listing Vertex jobs first.
    async fn submit(&self, id: &BatchJobId) -> Result<()>;

    /// On start and every 10 minutes: adopts orphaned Vertex jobs, cancels jobs older than
    /// `timeout` (72 h), reports finished jobs.
    async fn reconcile(&self, timeout: Duration) -> Result<ReconcileReport>;

    /// Ingests a finished job's results, settles its reservation and marks it `ingested`.
    async fn ingest(&self, id: &BatchJobId) -> Result<BatchIngest>;
}
