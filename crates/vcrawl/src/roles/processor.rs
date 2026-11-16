//! Role `processor`: committed captures → records (spec §5.7). See
//! [`crate::data::processor`] for the pipeline.
//!
//! The LLM is wired only when `VC_LLM_ENABLED=true`; then the model is probed once at start
//! (fail loudly when unavailable) and every extraction call reserves through `PgLedger` under
//! the extraction share. With the LLM off, extraction is structured-only.

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
use crate::data::{db, shutdown};

/// Pause when the queue was empty.
const IDLE: Duration = Duration::from_secs(2);

/// Runs the `processor` role until shutdown.
pub async fn run(ctx: Ctx) -> anyhow::Result<()> {
    let cfg = ctx.cfg.clone();
    let mut stop = shutdown::channel();
    let secrets = db::secrets(&cfg);
    let pool = db::pool(&cfg, secrets.as_ref()).await?;
    let blobs = db::blobs(&cfg).await?;
    let kinds: Arc<dyn KindRegistry> =
        Arc::new(vc_kinds::FileKindRegistry::load(&cfg.kinds).context("kind registry")?);
    let places: Arc<dyn PlaceResolver> =
        Arc::new(vc_extract::FilePlaceResolver::load(&cfg.extract).context("place data")?);
    let mut extractor = Extractor::new(kinds.clone(), cfg.extract.clone()).with_places(places);
    if cfg.llm.enabled {
        let clock: Arc<dyn Clock> = Arc::new(vc_core::SystemClock);
        let vertex =
            vc_llm::VertexClient::new(&cfg.llm, Arc::new(vc_llm::AdcToken::new()), clock.clone())?;
        let ledger = Arc::new(vc_llm::PgLedger::new(pool.clone(), cfg.llm.clone(), clock));
        let llm = vc_llm::VertexLlm::new(cfg.llm.clone(), vertex).with_cache(pool.clone());
        let probe = llm.probe(ledger.as_ref()).await.context("llm probe")?;
        tracing::info!(?probe, model = %cfg.llm.model, "llm probe");
        extractor = extractor.with_llm(Arc::new(llm), ledger, cfg.llm.clone());
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
    let p = Arc::new(Processor::new(ProcessorDeps {
        pool,
        blobs,
        queue,
        sink,
        kinds,
        extractor: Arc::new(extractor),
        frontier: Some(frontier),
        handlers: vc_parse::registry_with(&cfg.parse),
        limits: vc_parse::limits_from(&cfg.parse),
    }));
    let n = cfg.vcrawl.processor_concurrency.max(1);
    loop {
        let wait = match p.tick(n).await {
            Ok(0) => IDLE,
            Ok(_) => Duration::ZERO,
            Err(e) => {
                tracing::warn!(error_class = ?e.class(), error = %e, "processor tick failed");
                IDLE
            }
        };
        if shutdown::sleep_or_stop(&mut stop, wait).await {
            return Ok(());
        }
    }
}
