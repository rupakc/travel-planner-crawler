//! `vcrawl admin reextract --kind <kind>`: queue re-extraction after a prompt or schema change
//! (spec §5.6 `extraction_runs`, §5.7).
//!
//! Creates an `extraction_runs` row and enqueues one `extract` work item per capture that
//! holds the **current** version of a live (`active`/`stale`) record of the kind. By default
//! only a canary of 200 captures is queued (run status `canary`); `--limit 0` queues all of
//! them (`rolling`). The processor reads the extracted-text tier, not Coldline WARCs, and
//! every LLM call still reserves through the budget ledger.
//!
//! Work payload (the processor's contract): `{"capture_id", "captured_at", "kind", "run_id",
//! "reason": "reextract"}`, dedupe key `reextract:<kind>:<capture_id>` (re-running the command
//! while items are pending queues nothing twice).

use clap::Args;
use serde::Serialize;
use sqlx::PgPool;
use vc_core::ExtractionRunId;

use crate::Ctx;

/// Canary size (spec §5.6).
pub const CANARY: i64 = 200;

/// `admin reextract` arguments.
#[derive(Debug, Clone, Args)]
pub struct ReextractArgs {
    /// The kind (`kinds.toml` id).
    #[arg(long)]
    pub kind: String,
    /// Captures to queue (0 = all; default: the 200-capture canary).
    #[arg(long, default_value_t = CANARY)]
    pub limit: i64,
    /// Count only.
    #[arg(long)]
    pub dry_run: bool,
}

/// What was queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReextractReport {
    /// The run (None on a dry run).
    pub run_id: Option<String>,
    /// `canary` or `rolling`.
    pub status: String,
    /// Captures eligible.
    pub eligible: i64,
    /// Work items queued (already-pending items are not queued again).
    pub queued: i64,
}

/// Runs the command.
pub async fn run(ctx: &Ctx, a: ReextractArgs) -> anyhow::Result<serde_json::Value> {
    let pool = crate::wiring::db::ops_pool(&ctx.cfg, 4).await?;
    Ok(serde_json::to_value(reextract(&pool, &a).await?)?)
}

const ELIGIBLE: &str = "SELECT DISTINCT v.capture_id, v.captured_at
    FROM records r JOIN record_versions v
      ON v.record_id = r.record_id AND v.version = r.current_version
    WHERE r.kind = $1 AND r.state IN ('active', 'stale')";

/// Queues the run.
pub async fn reextract(pool: &PgPool, a: &ReextractArgs) -> anyhow::Result<ReextractReport> {
    anyhow::ensure!(a.limit >= 0, "--limit must be ≥ 0");
    let status = if a.limit == 0 { "rolling" } else { "canary" };
    let eligible: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM ({ELIGIBLE}) e"
    )))
    .bind(&a.kind)
    .fetch_one(pool)
    .await?;
    if a.dry_run {
        return Ok(ReextractReport {
            run_id: None,
            status: status.into(),
            eligible,
            queued: 0,
        });
    }
    let run_id = ExtractionRunId::new().to_string();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO extraction_runs (run_id, kind, schema_ver, status, scope)
         VALUES ($1, $2,
                 coalesce((SELECT max(split_part(kind_schema, '@', 2)::integer)
                           FROM record_versions v JOIN records r ON r.record_id = v.record_id
                           WHERE r.kind = $2 AND split_part(kind_schema, '@', 2) ~ '^[0-9]+$'), 1),
                 $3, jsonb_build_object('reason', 'reextract', 'limit', $4::bigint))",
    )
    .bind(&run_id)
    .bind(&a.kind)
    .bind(status)
    .bind(a.limit)
    .execute(&mut *tx)
    .await?;
    let queued: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(w) FROM (
           SELECT vc_enqueue('extract',
                    jsonb_build_object('capture_id', e.capture_id, 'captured_at', e.captured_at,
                                       'kind', $1::text, 'run_id', $2::text,
                                       'reason', 'reextract'),
                    'reextract:' || $1 || ':' || e.capture_id) AS w
           FROM ({ELIGIBLE} ORDER BY v.captured_at DESC, v.capture_id
                 LIMIT CASE WHEN $3 = 0 THEN NULL ELSE $3 END) e) q"
    )))
    .bind(&a.kind)
    .bind(&run_id)
    .bind(a.limit)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(kind = %a.kind, queued, eligible, "reextract queued");
    Ok(ReextractReport {
        run_id: Some(run_id),
        status: status.into(),
        eligible,
        queued,
    })
}
