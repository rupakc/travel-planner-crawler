//! `vcrawl admin reprocess --source <id>…`: hands committed primary captures back to the
//! processor (one `capture_ready` item each, the same payload `vc_commit_segment` writes) after
//! a processor fix, e.g. new adapter record mappings. No fetch, no LLM decision changes: the
//! processor is idempotent per (capture, record key), so a capture that already produced its
//! records writes nothing new.
//!
//! Dedupe key: the capture id, like the commit's own item, so a capture already waiting in the
//! queue is not queued twice. Purged segments and tombstoned sources are skipped (the
//! processor checks the rest).

use clap::Args;
use serde::Serialize;
use sqlx::PgPool;

use crate::Ctx;

/// `admin reprocess` arguments.
#[derive(Debug, Clone, Args)]
pub struct ReprocessArgs {
    /// Sources whose captures to reprocess (repeatable; at least one).
    #[arg(long = "source", required = true)]
    pub sources: Vec<String>,
    /// Count only.
    #[arg(long)]
    pub dry_run: bool,
}

/// What was queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReprocessReport {
    /// Committed primary captures of the sources.
    pub eligible: i64,
    /// Work items queued (captures already waiting are not queued again).
    pub queued: i64,
}

/// Runs the command.
pub async fn run(ctx: &Ctx, a: ReprocessArgs) -> anyhow::Result<serde_json::Value> {
    let pool = crate::wiring::db::ops_pool(&ctx.cfg, 4).await?;
    Ok(serde_json::to_value(reprocess(&pool, &a).await?)?)
}

const ELIGIBLE: &str = "SELECT c.capture_id, c.captured_at
    FROM captures c JOIN segments s ON s.segment_id = c.segment_id
    WHERE c.role = 'primary' AND c.source_id = ANY($1) AND s.purged_at IS NULL";

/// Queues the captures.
pub async fn reprocess(pool: &PgPool, a: &ReprocessArgs) -> anyhow::Result<ReprocessReport> {
    anyhow::ensure!(!a.sources.is_empty(), "--source is required");
    let eligible: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM ({ELIGIBLE}) e"
    )))
    .bind(&a.sources)
    .fetch_one(pool)
    .await?;
    if a.dry_run {
        return Ok(ReprocessReport {
            eligible,
            queued: 0,
        });
    }
    let queued: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(w) FROM (
           SELECT vc_enqueue('capture_ready',
                    jsonb_build_object('capture_id', e.capture_id, 'captured_at', e.captured_at),
                    e.capture_id) AS w
           FROM ({ELIGIBLE} ORDER BY c.captured_at, c.capture_id) e) q"
    )))
    .bind(&a.sources)
    .fetch_one(pool)
    .await?;
    tracing::info!(
        sources = a.sources.len(),
        queued,
        eligible,
        "reprocess queued"
    );
    Ok(ReprocessReport { eligible, queued })
}
