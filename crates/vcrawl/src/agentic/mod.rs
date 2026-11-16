//! Agentic loops (Task 17, spec §5.7, §17.4): link triage, source scout (propose only) and the
//! quality auditor.
//!
//! Every model call goes through the budget ledger with its own purpose (triage 8 %, auditor
//! 7 %, scout 4 % of the monthly cap): reserve first, call, then settle. With
//! `llm.enabled = false` every loop is a clean no-op (no reservation, no call, no writes), and
//! a refused reservation stops the loop without a call.
//!
//! Entry points for the roles (wired by Task 15):
//!
//! - [`triage::triage_links`]: the processor (or scheduler) calls it with the in-scope links
//!   its heuristics leave uncertain; verdicts are cached per URL pattern
//!   ([`triage::PgTriageCache`] keeps them in `llm_cache`).
//! - [`scout::run_scout`]: the maintenance role, weekly, for under-covered
//!   `(destination, kind)` cells. It **only proposes** (`source_candidates` +
//!   `docs/proposals/<date>.md` via [`scout::write_proposals_md`]); it never enqueues, claims or
//!   fetches anything.
//! - [`auditor::run_auditor`]: the maintenance role, weekly; audits sampled record versions
//!   and lowers record quality and the per-source `quality_score` on failures.
//!
//! The system prompts live in `prompts/agentic/*.md` (embedded at build time). Inputs from
//! crawled pages are always passed as untrusted JSON data in the user message; there are no
//! tools.

pub mod auditor;
pub mod scout;
pub mod triage;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use vc_config::LlmConfig;
use vc_core::llm::{BudgetLedger, LlmClient, LlmError, LlmRequest, LlmResponse, Purpose, Usage};
use vc_core::{Error, Result};
use vc_llm::Pricing;

/// The model and budget seam shared by the loops.
#[derive(Clone)]
pub struct AgenticLlm {
    /// The Gemini client (`vc_llm::VertexLlm` in production).
    pub client: Arc<dyn LlmClient>,
    /// The budget ledger (`vc_llm::PgLedger` in production).
    pub ledger: Arc<dyn BudgetLedger>,
    /// LLM settings (`enabled`, pricing for the reservation estimate).
    pub config: LlmConfig,
}

impl std::fmt::Debug for AgenticLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgenticLlm")
            .field("enabled", &self.config.enabled)
            .finish_non_exhaustive()
    }
}

/// The outcome of one budgeted call.
#[derive(Debug, Clone, PartialEq)]
pub enum Called {
    /// The model answered.
    Answer(LlmResponse),
    /// The LLM is disabled: nothing was reserved or called.
    Disabled,
    /// The ledger (or the spend breaker) refused: no call was made.
    Budget,
    /// The call failed (transient, invalid output, …); the loop skips this item.
    Failed,
}

impl Called {
    /// Whether the loop must stop (no further calls this run).
    pub fn stops(&self) -> bool {
        matches!(self, Called::Disabled | Called::Budget)
    }
}

impl AgenticLlm {
    /// Bundles a client, a ledger and the settings.
    pub fn new(
        client: Arc<dyn LlmClient>,
        ledger: Arc<dyn BudgetLedger>,
        config: LlmConfig,
    ) -> Self {
        AgenticLlm {
            client,
            ledger,
            config,
        }
    }

    /// Whether the loops may call the model at all.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// One reserved call under `req.purpose`: reserve the estimate, call, settle. A refused
    /// reservation means no call. Errors that never reached Vertex release the reservation;
    /// other failures settle the estimate (it may have been billed).
    pub async fn call(&self, req: LlmRequest) -> Result<Called> {
        if !self.enabled() {
            return Ok(Called::Disabled);
        }
        let purpose: Purpose = req.purpose;
        let est = Pricing::from_config(&self.config).estimate(&req, false);
        let Some(r) = self.ledger.reserve(purpose, est).await? else {
            tracing::info!(
                purpose = purpose.as_str(),
                "agentic call refused by the budget"
            );
            return Ok(Called::Budget);
        };
        match self.client.generate(&r, req).await {
            Ok(resp) => {
                self.ledger.settle(r, resp.usage.clone()).await?;
                Ok(Called::Answer(resp))
            }
            Err(e) => {
                let le = match &e {
                    Error::Llm(le) => Some(le.clone()),
                    _ => None,
                };
                let free = matches!(
                    le,
                    Some(
                        LlmError::Disabled
                            | LlmError::BudgetExhausted
                            | LlmError::BreakerOpen { .. }
                            | LlmError::RateLimited
                    )
                );
                let usage = if free {
                    Usage::default()
                } else {
                    Usage {
                        cost: est,
                        ..Usage::default()
                    }
                };
                self.ledger.settle(r, usage).await?;
                tracing::info!(
                    purpose = purpose.as_str(),
                    error_class = ?e.class(),
                    "agentic call failed"
                );
                Ok(match le {
                    Some(LlmError::Disabled) => Called::Disabled,
                    Some(LlmError::BudgetExhausted | LlmError::BreakerOpen { .. }) => {
                        Called::Budget
                    }
                    _ => Called::Failed,
                })
            }
        }
    }
}

/// The parsed JSON of an answer (`json`, else the text parsed), when it is an object.
pub(crate) fn answer_json(resp: &LlmResponse) -> Option<serde_json::Value> {
    resp.json
        .clone()
        .or_else(|| serde_json::from_str(&resp.text).ok())
        .filter(serde_json::Value::is_object)
}

/// At most `max` characters of `s`.
pub(crate) fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

pub(crate) fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}
