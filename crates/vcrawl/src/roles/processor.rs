//! Role `processor`: committed captures → records (spec §5.7). See
//! [`crate::data::processor`] for the pipeline.
//!
//! The LLM is wired only when `VC_LLM_ENABLED=true`; then the model is probed once at start
//! (fail loudly only when the model does not exist; any other failure starts with the LLM
//! paused) and the spend breaker is shared through Postgres. Every extraction call reserves
//! through `PgLedger` under
//! the extraction share, and uncertain in-scope links are triaged (purpose `triage`). With the
//! LLM off, extraction is structured-only and every in-scope link keeps the heuristic default.
//!
//! Each loop also writes the extracted-text tier of a few committed segments
//! ([`crate::data::text_tier`]).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use vc_core::Clock;
use vc_core::extract::PlaceResolver;
use vc_core::frontier::Frontier;
use vc_core::kinds::KindRegistry;
use vc_extract::Extractor;

use crate::Ctx;
use crate::data::processor::{Processor, ProcessorDeps};
use crate::data::shutdown;
use crate::data::text_tier::TextTier;
use crate::wiring;

/// Segments written to the extracted-text tier per loop.
const TEXT_SEGMENTS_PER_TICK: usize = 4;

/// Pause when the queue was empty.
const IDLE: Duration = Duration::from_secs(2);

/// Runs the `processor` role until shutdown.
pub async fn run(ctx: Ctx) -> anyhow::Result<()> {
    wiring::obs::init_tracing(&ctx, "processor");
    let cfg = ctx.cfg.clone();
    let mut stop = shutdown::channel();
    let pool = ctx.pool().await?;
    let _obs = wiring::obs::start(&ctx, "processor", pool.clone()).await?;
    let blobs = wiring::store::blob_store(&cfg)
        .await
        .context("raw blob store")?;
    let kinds: Arc<dyn KindRegistry> =
        Arc::new(vc_kinds::FileKindRegistry::load(&cfg.kinds).context("kind registry")?);
    let places: Arc<dyn PlaceResolver> =
        Arc::new(vc_extract::FilePlaceResolver::load(&cfg.extract).context("place data")?);
    let mut extractor = Extractor::new(kinds.clone(), cfg.extract.clone()).with_places(places);
    let mut triage = None;
    if cfg.llm.enabled {
        let clock: Arc<dyn Clock> = Arc::new(vc_core::SystemClock);
        let vertex =
            vc_llm::VertexClient::new(&cfg.llm, Arc::new(vc_llm::AdcToken::new()), clock.clone())?
                .with_shared_breaker(pool.clone());
        let ledger = Arc::new(vc_llm::PgLedger::new(pool.clone(), cfg.llm.clone(), clock));
        let llm = vc_llm::VertexLlm::new(cfg.llm.clone(), vertex).with_cache(pool.clone());
        // Only a missing model is fatal; anything else starts with the LLM paused (a crash
        // loop over a transient Vertex error ends in watchdog reboots).
        let probe = llm.probe(ledger.as_ref()).await.context("llm probe")?;
        tracing::info!(?probe, model = %cfg.llm.model, "llm probe");
        let llm = Arc::new(llm);
        triage = Some(crate::agentic::AgenticLlm::new(
            llm.clone(),
            ledger.clone(),
            cfg.llm.clone(),
        ));
        extractor = extractor.with_llm(llm, ledger, cfg.llm.clone());
    } else {
        tracing::info!("llm disabled: structured extraction only");
    }
    let frontier: Arc<dyn Frontier> = Arc::new(vc_frontier::PgFrontier::new(
        pool.clone(),
        cfg.frontier.clone(),
    ));
    let queue = Arc::new(vc_queue::PgWorkQueue {
        pool: pool.clone(),
        config: cfg.queue.clone(),
    });
    let sink = Arc::new(vc_index::PgIndex::with_config(
        pool.clone(),
        cfg.index.clone(),
    ));
    let text = TextTier::new(
        pool.clone(),
        blobs.clone(),
        vc_parse::registry_with(&cfg.parse),
        vc_parse::limits_from(&cfg.parse),
        cfg.store.zstd_level,
    );
    let mut p = Processor::new(ProcessorDeps {
        pool,
        blobs,
        queue,
        sink,
        kinds,
        extractor: Arc::new(extractor),
        frontier: Some(frontier),
        handlers: vc_parse::registry_with(&cfg.parse),
        limits: vc_parse::limits_from(&cfg.parse),
    });
    p = p.with_metrics(ctx.metrics.clone());
    if let Some(t) = triage {
        p = p.with_triage(t);
    }
    let p = Arc::new(p);
    let n = cfg.vcrawl.processor_concurrency.max(1);
    loop {
        let mut wait = match p.tick(n).await {
            Ok(0) => IDLE,
            Ok(_) => Duration::ZERO,
            Err(e) => {
                tracing::warn!(error_class = ?e.class(), error = %e, "processor tick failed");
                IDLE
            }
        };
        match text.tick(TEXT_SEGMENTS_PER_TICK).await {
            Ok(t) => {
                if t.written > 0 {
                    wait = Duration::ZERO;
                }
                if !t.is_clean() {
                    tracing::warn!(
                        failed = t.failed.len(),
                        written = t.written,
                        "text tier tick had failed segments"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error_class = ?e.class(), error = %e, "text tier tick failed");
            }
        }
        if shutdown::sleep_or_stop(&mut stop, wait).await {
            return Ok(());
        }
    }
}
