//! The LLM startup probe (spec §17.4): wired **only** when `VC_LLM_ENABLED` / `llm.enabled`
//! is true. A one-token call under a ledger reservation; an unavailable model fails startup
//! loudly, any other failure only pauses the LLM ([`ProbeOutcome::Paused`]). With the LLM off
//! nothing is built and nothing touches the network.

use std::sync::Arc;

use sqlx::PgPool;
use vc_config::Config;
use vc_core::{Clock, Result};
use vc_llm::{AdcToken, PgLedger, ProbeOutcome, VertexClient, VertexLlm};

/// Runs the probe when the LLM is enabled; `Ok(None)` when it is off (nothing was built).
pub async fn probe_if_enabled(
    cfg: &Config,
    pool: &PgPool,
    clock: Arc<dyn Clock>,
) -> Result<Option<ProbeOutcome>> {
    if !cfg.llm.enabled {
        return Ok(None);
    }
    let vertex = VertexClient::new(&cfg.llm, Arc::new(AdcToken::new()), clock.clone())?
        .with_shared_breaker(pool.clone());
    let llm = VertexLlm::new(cfg.llm.clone(), vertex);
    let ledger = PgLedger::new(pool.clone(), cfg.llm.clone(), clock);
    let out = llm.probe(&ledger).await?;
    tracing::info!(outcome = ?out, model = %cfg.llm.model, "llm startup probe");
    Ok(Some(out))
}
