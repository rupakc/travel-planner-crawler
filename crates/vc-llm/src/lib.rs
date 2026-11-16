//! # vc-llm
//!
//! Gemini on Vertex AI for the crawler, always under the race-free budget (spec §5.8, §17.3,
//! §17.4):
//!
//! - [`VertexLlm`]: REST `generateContent` on the global endpoint with `responseSchema`,
//!   `maxOutputTokens`, `thinkingConfig` and `labels {app: voyager-crawler, purpose}`, an
//!   optional [`LlmCache`] (`llm_cache`), and the startup [`VertexLlm::probe`];
//! - [`VertexEmbedder`]: `text-embedding-005` (768) via `predict`;
//! - [`PgLedger`]: the two-row (+ purpose) reservation, the dynamic daily cap, purpose shares,
//!   and the 1 h stale settle (batch reservations exempt);
//! - [`VertexBatchRunner`]: batch prediction with JSONL in GCS (prepare → submit → reconcile →
//!   ingest, 72 h cancel);
//! - [`VertexClient`]: ADC tokens, error mapping and the 6 h [`Breaker`] on a billing/budget
//!   403/429, shared by every role through `llm_breaker` (migration 1401) and remembered
//!   across restarts ([`VertexClient::with_shared_breaker`]).
//!
//! With `llm.enabled = false` (the default) every call returns [`LlmError::Disabled`] without
//! touching the network. Logs carry token counts and cost only, never prompt or page text.
//!
//! [`LlmError::Disabled`]: vc_core::llm::LlmError::Disabled

mod batch;
mod cache;
mod client;
mod crc;
mod embed;
mod generate;
mod ledger;
mod pricing;
mod schema;

pub use batch::{KEY_LABEL, VertexBatchRunner, key_of};
pub use cache::{CacheKey, LlmCache};
pub use client::{
    APP_LABEL, AdcToken, BREAKER_SYNC, Breaker, CLOUD_PLATFORM_SCOPE, StaticToken, TokenSource,
    VertexClient, is_billing_error, labels,
};
pub use crc::crc32c;
pub use embed::{MAX_TEXTS_PER_REQUEST, VertexEmbedder};
pub use generate::{
    PROBE_PAUSE, ProbeOutcome, TEMPERATURE, VertexLlm, finish_reason, generate_body,
    parse_response, thinking_config, usage_of,
};
pub use ledger::{BudgetRow, PgLedger, remaining_days};
pub use pricing::{Pricing, thinking_allowance};
pub use schema::to_vertex_schema;
