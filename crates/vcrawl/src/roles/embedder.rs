//! Role `embedder`: embeds current, embeddable records under the embedding budget
//! (spec §5.8, §5.11). With `VC_LLM_ENABLED=false` (the default) it idles: no model call, no
//! reservation, but still serves `/metrics` + `/healthz` and writes its heartbeat (otelcol
//! scrapes `embedder:9464`; the crawler-down alert needs the heartbeat).

use std::sync::Arc;
use std::time::Duration;

use vc_core::Clock;

use crate::Ctx;
use crate::data::embed::{EmbedLoop, estimator};
use crate::data::shutdown;
use crate::wiring;

/// Pause between passes when there was nothing to do.
const IDLE: Duration = Duration::from_secs(30);
/// Pause after the budget refused a reservation.
const REFUSED: Duration = Duration::from_secs(15 * 60);

/// Runs the `embedder` role until shutdown.
pub async fn run(ctx: Ctx) -> anyhow::Result<()> {
    wiring::obs::init_tracing(&ctx, "embedder");
    let cfg = ctx.cfg.clone();
    let mut stop = ctx.stop();
    let pool = ctx.pool().await?;
    let _obs = wiring::obs::start(&ctx, "embedder", pool.clone()).await?;
    if !cfg.llm.enabled {
        tracing::info!("llm disabled: embedder idle");
        while !shutdown::sleep_or_stop(&mut stop, Duration::from_secs(3600)).await {}
        return Ok(());
    }
    let clock: Arc<dyn Clock> = Arc::new(vc_core::SystemClock);
    let vertex =
        vc_llm::VertexClient::new(&cfg.llm, Arc::new(vc_llm::AdcToken::new()), clock.clone())?
            .with_shared_breaker(pool.clone());
    let ledger = Arc::new(vc_llm::PgLedger::new(pool.clone(), cfg.llm.clone(), clock));
    let embedder =
        Arc::new(vc_llm::VertexEmbedder::new(cfg.llm.clone(), vertex).with_ledger(ledger.clone()));
    let index = vc_index::PgIndex::with_config(pool, cfg.index.clone());
    let lp = EmbedLoop::new(index, embedder, ledger, estimator(&cfg.llm))
        .with_sizes(200, vc_llm::MAX_TEXTS_PER_REQUEST);
    lp.ensure_index().await?;
    loop {
        let wait = match lp.pass().await {
            Ok(p) if p.budget_refused => REFUSED,
            Ok(p) if p.embedded == 0 => IDLE,
            Ok(_) => Duration::ZERO,
            Err(e) => {
                tracing::warn!(error_class = ?e.class(), error = %e, "embedder pass failed");
                IDLE
            }
        };
        if shutdown::sleep_or_stop(&mut stop, wait).await {
            return Ok(());
        }
    }
}
