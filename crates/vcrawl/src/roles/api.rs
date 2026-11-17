//! Role `api`: the internal `/v1` read API (spec §5.11, §17.5).
//!
//! TLS only, with the self-signed certificate and key from Secret Manager
//! (`crawler-api-tls-cert`, `crawler-api-tls-key`); missing or invalid material fails closed
//! (the role exits instead of serving plain HTTP). Like every role, it serves the crawler's
//! `/metrics` + `/healthz` on `obs.metrics_addr` (`api:9464`, scraped by otelcol) and writes
//! its heartbeat; vc-api's own request counters stay on `api.metrics_bind` (localhost).
//! Search gets the vector leg only when the LLM is enabled, and every query embedding reserves
//! through the embedding budget (a refusal makes that search text-only).

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use vc_core::extract::PlaceResolver;
use vc_index::QueryEmbedder;

use crate::Ctx;
use crate::data::{api, embed, shutdown};
use crate::wiring;

/// Runs the `api` role until shutdown.
pub async fn run(ctx: Ctx) -> anyhow::Result<()> {
    run_with_tls_dir(ctx, Path::new(vc_api::tls::SECRETS_DIR)).await
}

/// [`run`] with the installed TLS pair looked up in `dir` instead of `/run/secrets` (tests).
pub async fn run_with_tls_dir(ctx: Ctx, dir: &Path) -> anyhow::Result<()> {
    wiring::obs::init_tracing(&ctx, "api");
    let cfg = ctx.cfg.clone();
    let secrets = wiring::sources::secrets(&cfg);
    // TLS first: without a certificate there is nothing to serve.
    let tls = vc_api::tls::tls_from_dir_or_secrets(dir, secrets.as_ref(), &cfg.api)
        .await
        .context("api TLS material")?;
    let pool = ctx.pool().await?;
    let _obs = wiring::obs::start(&ctx, "api", pool.clone()).await?;
    let places: Arc<dyn PlaceResolver> =
        Arc::new(vc_extract::FilePlaceResolver::load(&cfg.extract).context("place data")?);
    let qe: Option<Arc<dyn QueryEmbedder>> = if cfg.llm.enabled {
        let clock: Arc<dyn vc_core::Clock> = Arc::new(vc_core::SystemClock);
        let vertex =
            vc_llm::VertexClient::new(&cfg.llm, Arc::new(vc_llm::AdcToken::new()), clock.clone())?
                .with_shared_breaker(pool.clone());
        let ledger = Arc::new(vc_llm::PgLedger::new(pool.clone(), cfg.llm.clone(), clock));
        let embedder = Arc::new(
            vc_llm::VertexEmbedder::new(cfg.llm.clone(), vertex).with_ledger(ledger.clone()),
        );
        Some(Arc::new(embed::BudgetQueryEmbedder::new(
            embedder,
            ledger,
            embed::estimator(&cfg.llm),
        )))
    } else {
        None
    };
    let state = api::state(pool, cfg.api.clone(), cfg.index.clone(), places, qe);
    vc_api::tls::serve(state, tls, shutdown::signal())
        .await
        .context("api server")
}
