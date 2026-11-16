//! Role `maintenance` (Task 15; spec §5.5, §5.6, §9, §17.6, §17.7, §17.9).
//!
//! One loop, ticking every minute; each job runs on its own interval and is idempotent, so a
//! restart only re-runs work that is safe to repeat:
//!
//! | job | every | what |
//! |---|---|---|
//! | partitions | 6 h | `vc_ensure_partitions(vcrawl.partitions_ahead_months)` |
//! | purge | 15 min | advances every unfinished `purge_requests` row (index → raw → done) |
//! | ephemeral expiry | 1 h | cuts captures past `raw_expires_at` (noarchive / TDM) out of their segments |
//! | restore drill | 7 d | rebuilds the newest segments' rows from their CDXJ in memory and compares them with the database; exports `restore_drill_ok` |
//! | digest | 1 h | writes yesterday's `crawl_digest` row once (parked keys, statuses, yields, spend) |
//! | Parquet export | 24 h | stub: lists partitions older than 12 months that are due for export |
//! | source scout | 7 d | under-covered `(destination, kind)` cells → `agentic::scout` proposals (+ `<data_dir>/proposals/<date>.md`) |
//! | quality auditor | 7 d | `agentic::auditor` over sampled current versions, page text from the text tier |
//!
//! The two agentic jobs run only with an [`Agentic`] bundle and do nothing (no query, no
//! reservation, no model call) while `llm.enabled` is false.
//!
//! Not here, by design: Chromium recycling. The maintenance container has no route to the
//! `render` network; `vc-render` re-creates its CDP session every `render.restart_after_renders`
//! renders inside the fetcher. The monthly full restore drill (an ephemeral Spot VM restoring
//! the base backup + WAL) needs the bootstrap's drill template and is not wired yet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;
use vc_core::store::BlobStore;
use vc_core::{Clock, SystemClock};
use vc_obs::{HeartbeatWriter, Metrics, ProgressTracker};

use vc_core::policy::Policy;

use crate::Ctx;
use crate::admin::{purge, rebuild};
use crate::agentic::AgenticLlm;
use crate::agentic::auditor::{AuditReport, AuditSettings, AuditText, run_auditor};
use crate::agentic::scout::{ScoutCell, ScoutReport, run_scout, write_proposals_md};

/// Segments checked per restore drill.
pub const DRILL_SEGMENTS: i64 = 50;
/// Partitions older than this many months are due for Parquet export (spec §5.6).
pub const EXPORT_AFTER_MONTHS: u32 = 12;

/// The maintenance jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Job {
    /// Monthly partitions ahead.
    Partitions,
    /// Purge requests.
    Purge,
    /// Ephemeral raw expiry.
    Ephemeral,
    /// Restore drill (equivalence check).
    RestoreDrill,
    /// Daily digest.
    Digest,
    /// Parquet export (stub).
    ParquetExport,
    /// Source scout (agentic, propose only).
    Scout,
    /// Quality auditor (agentic).
    Auditor,
}

impl Job {
    /// All jobs, in run order.
    pub const ALL: [Job; 8] = [
        Job::Partitions,
        Job::Purge,
        Job::Ephemeral,
        Job::RestoreDrill,
        Job::Digest,
        Job::ParquetExport,
        Job::Scout,
        Job::Auditor,
    ];

    /// How often the job runs.
    pub fn interval(self) -> Duration {
        match self {
            Job::Partitions => Duration::hours(6),
            Job::Purge => Duration::minutes(15),
            Job::Ephemeral | Job::Digest => Duration::hours(1),
            Job::RestoreDrill | Job::Scout | Job::Auditor => Duration::days(7),
            Job::ParquetExport => Duration::days(1),
        }
    }
}

/// When each job last ran (in memory; every job is idempotent, so a restart runs all once).
#[derive(Debug, Clone, Default)]
pub struct Schedule {
    last: HashMap<Job, DateTime<Utc>>,
}

impl Schedule {
    /// The jobs due at `now`.
    pub fn due(&self, now: DateTime<Utc>) -> Vec<Job> {
        Job::ALL
            .into_iter()
            .filter(|j| self.last.get(j).is_none_or(|t| now - *t >= j.interval()))
            .collect()
    }

    /// Marks `job` as run at `now`.
    pub fn done(&mut self, job: Job, now: DateTime<Utc>) {
        self.last.insert(job, now);
    }
}

/// The result of a restore drill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DrillReport {
    /// Every checked segment matched.
    pub ok: bool,
    /// Segments checked.
    pub checked: u64,
    /// Differences (segment ids and what differs; no content).
    pub mismatches: Vec<String>,
}

/// The maintenance jobs over one database and raw bucket.
#[derive(Clone)]
pub struct Maintenance {
    /// Pool.
    pub pool: PgPool,
    /// Raw bucket.
    pub store: Arc<dyn BlobStore>,
    /// Metrics (`restore_drill_ok`, `restore_drill_age_seconds`).
    pub metrics: Option<Arc<Metrics>>,
    /// Months of partitions kept ahead.
    pub partitions_ahead_months: u32,
    /// The agentic loops (scout, auditor); `None` skips them.
    pub agentic: Option<Agentic>,
}

/// What the agentic maintenance jobs need.
#[derive(Clone)]
pub struct Agentic {
    /// Model + budget seam (a no-op when the LLM is disabled).
    pub llm: AgenticLlm,
    /// Policy (denylist, brand domains) for scout suggestions.
    pub policy: Arc<dyn Policy>,
    /// Where `<date>.md` proposal files go.
    pub proposals_dir: PathBuf,
    /// Page text for the auditor.
    pub text: Arc<StoreAuditText>,
    /// Under-covered cell thresholds.
    pub cells: CellSettings,
}

/// Thresholds of the under-covered cell query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellSettings {
    /// A destination counts once it has this many live records (any kind).
    pub min_destination_records: i64,
    /// A `(destination, kind)` cell is under-covered below this many live records.
    pub min_cell_records: i64,
    /// Cells per scout run.
    pub max_cells: i64,
    /// A cell proposed for within this many days is not asked again.
    pub recent_days: i32,
}

impl Default for CellSettings {
    fn default() -> Self {
        CellSettings {
            min_destination_records: 20,
            min_cell_records: 5,
            max_cells: 10,
            recent_days: 90,
        }
    }
}

/// City-scoped kinds the scout looks for sources of (their schemas carry `city` + `country`).
pub const SCOUT_KINDS: &[&str] = &[
    "activity",
    "attraction",
    "event",
    "hotel",
    "tip",
    "transport_option",
    "walking_tour",
];

/// The under-covered `(destination, kind)` cells: destinations (`city, country` of live
/// current versions) with at least `min_destination_records` records, crossed with
/// [`SCOUT_KINDS`], where the kind has fewer than `min_cell_records` records and no scout
/// proposal was made for the cell in the last `recent_days` days. Busiest destinations first.
pub async fn under_covered_cells(
    pool: &PgPool,
    s: &CellSettings,
) -> anyhow::Result<Vec<ScoutCell>> {
    let kinds: Vec<String> = SCOUT_KINDS.iter().map(|k| k.to_string()).collect();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "WITH cur AS (
           SELECT r.kind, btrim(v.data->>'city') AS city, btrim(v.data->>'country') AS country
           FROM records r JOIN record_versions v
             ON v.record_id = r.record_id AND v.version = r.current_version
           WHERE r.state IN ('active', 'stale')
             AND coalesce(btrim(v.data->>'city'), '') <> ''
             AND coalesce(btrim(v.data->>'country'), '') <> ''),
         dest AS (SELECT city, country, city || ', ' || country AS name, count(*) AS n
                  FROM cur GROUP BY city, country HAVING count(*) >= $1),
         have AS (SELECT city, country, kind, count(*) AS n FROM cur GROUP BY city, country, kind)
         SELECT d.name, k.kind
         FROM dest d CROSS JOIN unnest($3::text[]) AS k(kind)
         LEFT JOIN have h ON h.city = d.city AND h.country = d.country AND h.kind = k.kind
         WHERE coalesce(h.n, 0) < $2
           AND NOT EXISTS (SELECT 1 FROM source_candidates c
                           WHERE c.destination = d.name AND k.kind = ANY(c.kinds)
                             AND c.created_at > now() - make_interval(days => $5))
         ORDER BY d.n DESC, coalesce(h.n, 0), d.name, k.kind
         LIMIT $4",
    )
    .bind(s.min_destination_records)
    .bind(s.min_cell_records)
    .bind(&kinds)
    .bind(s.max_cells)
    .bind(s.recent_days)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(destination, kind)| ScoutCell { destination, kind })
        .collect())
}

/// [`AuditText`] over the raw store: the capture's row in its segment's text-tier object
/// (`text/…` or `ephemeral/text/…jsonl.zst`, `{capture_id, main_text}` rows written by the
/// processor), else the stored body parsed with `vc-parse` (main text). `None` when the capture or its bytes are gone.
pub struct StoreAuditText {
    /// Pool.
    pub pool: PgPool,
    /// Raw bucket.
    pub store: Arc<dyn BlobStore>,
    /// Parsers for the fallback.
    pub parse: vc_core::parse::HandlerRegistry,
    /// Parse limits.
    pub limits: vc_core::parse::Limits,
}

impl StoreAuditText {
    /// Over `pool` and `store` with the `[parse]` settings.
    pub fn new(pool: PgPool, store: Arc<dyn BlobStore>, cfg: &vc_config::ParseConfig) -> Self {
        StoreAuditText {
            pool,
            store,
            parse: vc_parse::registry_with(cfg),
            limits: vc_parse::limits_from(cfg),
        }
    }

    async fn lookup(&self, capture_id: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(DateTime<Utc>, Option<String>, String)> = sqlx::query_as(
            "SELECT captured_at, text_ref, segment_id FROM captures WHERE capture_id = $1 LIMIT 1",
        )
        .bind(capture_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((captured_at, text_ref, segment)) = row else {
            return Ok(None);
        };
        // The tier object of the capture's segment: archive text, or `ephemeral/text/` for
        // noarchive captures (and an explicit `text_ref` when one is recorded).
        let mut paths: Vec<String> = text_ref.into_iter().collect();
        if let Ok(sid) = segment.parse::<vc_core::SegmentId>() {
            paths.push(vc_store::paths::text_path(sid, false));
            paths.push(vc_store::paths::text_path(sid, true));
        }
        for path in paths {
            match self.store.get(&path, None).await {
                Ok(b) => {
                    let rows: Vec<serde_json::Value> = vc_store::text::decode_text_tier(&b)?;
                    let hit = rows
                        .iter()
                        .find(|r| r["capture_id"] == capture_id)
                        .and_then(|r| {
                            r.get("main_text")
                                .or_else(|| r.get("text"))
                                .and_then(|t| t.as_str())
                                .map(str::to_string)
                        });
                    if hit.is_some() {
                        return Ok(hit);
                    }
                }
                Err(vc_core::Error::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let cref = vc_core::CaptureRef {
            capture_id: capture_id.parse()?,
            captured_at,
        };
        let Some(c) = crate::data::capture::load(&self.pool, &cref).await? else {
            return Ok(None);
        };
        let body = match crate::data::capture::body(&self.pool, &self.store, &c).await {
            Ok(Some(b)) => b,
            Ok(None) | Err(vc_core::Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(vc_parse::parse(&self.parse, &c.meta, &body, &self.limits)?.and_then(|p| p.main_text))
    }
}

impl AuditText for StoreAuditText {
    async fn text(&self, capture_id: &str) -> vc_core::Result<Option<String>> {
        self.lookup(capture_id)
            .await
            .map_err(|e| vc_core::Error::Storage(format!("audit text: {e}")))
    }
}

impl Maintenance {
    /// Runs the due jobs at `now`; a failing job is logged and retried on its next tick.
    /// Returns the jobs that ran successfully.
    pub async fn tick(&self, sched: &mut Schedule, now: DateTime<Utc>) -> Vec<Job> {
        let mut ran = Vec::new();
        for job in sched.due(now) {
            let r = match job {
                Job::Partitions => self.ensure_partitions().await.map(drop),
                Job::Purge => self.purge(now).await.map(drop),
                Job::Ephemeral => self.expire_ephemeral(now).await.map(drop),
                Job::RestoreDrill => self.restore_drill(now).await.map(drop),
                Job::Digest => self.digest_yesterday(now).await.map(drop),
                Job::ParquetExport => self.parquet_export(now).await.map(drop),
                Job::Scout => self.scout(now).await.map(drop),
                Job::Auditor => self.audit().await.map(drop),
            };
            match r {
                Ok(()) => {
                    sched.done(job, now);
                    ran.push(job);
                }
                Err(e) => tracing::warn!(job = ?job, error = %e, "maintenance job failed"),
            }
        }
        self.refresh_drill_age(now).await;
        ran
    }

    /// Creates the monthly partitions ahead. Returns the partitions created.
    pub async fn ensure_partitions(&self) -> anyhow::Result<i32> {
        let months = i32::try_from(self.partitions_ahead_months.min(24))?;
        let n: i32 = sqlx::query_scalar("SELECT vc_ensure_partitions($1)")
            .bind(months)
            .fetch_one(&self.pool)
            .await?;
        if n > 0 {
            tracing::info!(created = n, "maintenance: partitions created");
        }
        Ok(n)
    }

    /// Advances every unfinished purge request.
    pub async fn purge(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<purge::PurgeOutcome>> {
        purge::process_pending(&self.pool, self.store.as_ref(), now).await
    }

    /// Cuts every capture whose `raw_expires_at` has passed out of its segment (a fully expired
    /// segment is dropped, its objects deleted). Returns `(captures, segments)`.
    pub async fn expire_ephemeral(&self, now: DateTime<Utc>) -> anyhow::Result<(u64, u64)> {
        let rows: Vec<(String, DateTime<Utc>, String, String, String)> = sqlx::query_as(
            "SELECT c.capture_id, c.captured_at, s.segment_id, s.path, s.cdxj_path
             FROM captures c JOIN segments s ON s.segment_id = c.segment_id
             WHERE c.raw_expires_at IS NOT NULL AND c.raw_expires_at <= $1
               AND s.purged_at IS NULL
             ORDER BY s.segment_id, c.capture_id",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        let doomed = rows
            .into_iter()
            .map(|(id, at, segment_id, path, cdxj_path)| {
                Ok(purge::DoomedCapture {
                    capture: vc_core::CaptureRef {
                        capture_id: id.parse()?,
                        captured_at: at,
                    },
                    segment_id,
                    path,
                    cdxj_path,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if doomed.is_empty() {
            return Ok((0, 0));
        }
        let r = purge::remove_captures(&self.pool, self.store.as_ref(), &doomed).await?;
        tracing::info!(
            captures = r.0,
            segments = r.1,
            "maintenance: ephemeral raw expired"
        );
        Ok(r)
    }

    /// Rebuilds the newest segments' rows from their CDXJ (in memory) and compares them with
    /// the database. Exports `restore_drill_ok` and records the result in `heartbeats`
    /// (`maintenance` / `restore_drill`).
    pub async fn restore_drill(&self, now: DateTime<Utc>) -> anyhow::Result<DrillReport> {
        let segs: Vec<(String, String)> = sqlx::query_as(
            "SELECT segment_id, cdxj_path FROM segments WHERE purged_at IS NULL
             ORDER BY committed_at DESC, segment_id DESC LIMIT $1",
        )
        .bind(DRILL_SEGMENTS)
        .fetch_all(&self.pool)
        .await?;
        let mut rep = DrillReport {
            ok: true,
            checked: 0,
            mismatches: vec![],
        };
        for (id, cdxj) in segs {
            let Some((sid, generation)) = rebuild::parse_cdxj_path(&cdxj) else {
                rep.mismatches.push(format!("segment {id}: bad cdxj path"));
                continue;
            };
            match rebuild::segment_from_cdxj(self.store.as_ref(), sid, generation, &cdxj).await {
                Ok(Some(seg)) => {
                    rep.mismatches
                        .extend(rebuild::diff_segment(&self.pool, &seg).await?);
                }
                Ok(None) => rep
                    .mismatches
                    .push(format!("segment {id}: object or index missing")),
                Err(e) => rep.mismatches.push(format!("segment {id}: {e}")),
            }
            rep.checked += 1;
        }
        rep.ok = rep.mismatches.is_empty();
        sqlx::query(
            "INSERT INTO heartbeats (role, instance, last_beat, progress)
             VALUES ('maintenance', 'restore_drill', $1, $2)
             ON CONFLICT (role, instance) DO UPDATE SET last_beat = $1,
               progress = EXCLUDED.progress
                 || jsonb_build_object('last_ok_at',
                      CASE WHEN (EXCLUDED.progress->>'ok')::boolean THEN to_jsonb($1)
                           ELSE heartbeats.progress->'last_ok_at' END)",
        )
        .bind(now)
        .bind(serde_json::json!({
            "ok": rep.ok,
            "checked": rep.checked,
            "mismatches": rep.mismatches.len(),
            "last_ok_at": if rep.ok { Some(now) } else { None },
        }))
        .execute(&self.pool)
        .await?;
        if let Some(m) = &self.metrics {
            m.restore_drill_ok.set(i64::from(rep.ok));
        }
        if rep.ok {
            tracing::info!(checked = rep.checked, "maintenance: restore drill ok");
        } else {
            tracing::error!(
                checked = rep.checked,
                mismatches = rep.mismatches.len(),
                "maintenance: restore drill FAILED"
            );
        }
        Ok(rep)
    }

    async fn refresh_drill_age(&self, now: DateTime<Utc>) {
        let Some(m) = &self.metrics else { return };
        let last: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT (progress->>'last_ok_at')::timestamptz FROM heartbeats
             WHERE role = 'maintenance' AND instance = 'restore_drill'",
        )
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
        .flatten();
        if let Some(t) = last {
            m.restore_drill_age_seconds
                .set((now - t).num_seconds().max(0) as f64);
        }
    }

    /// Writes yesterday's digest once. Returns it when it was written now.
    pub async fn digest_yesterday(
        &self,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let day = now.date_naive() - Duration::days(1);
        self.digest(day).await
    }

    /// Computes and stores the digest of `day` (UTC) unless it exists.
    pub async fn digest(&self, day: NaiveDate) -> anyhow::Result<Option<serde_json::Value>> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM crawl_digest WHERE day = $1)")
                .bind(day)
                .fetch_one(&self.pool)
                .await?;
        if exists {
            return Ok(None);
        }
        let d: serde_json::Value = sqlx::query_scalar(DIGEST_SQL)
            .bind(day)
            .fetch_one(&self.pool)
            .await?;
        let n = sqlx::query(
            "INSERT INTO crawl_digest (day, digest) VALUES ($1, $2) ON CONFLICT (day) DO NOTHING",
        )
        .bind(day)
        .bind(&d)
        .execute(&self.pool)
        .await?
        .rows_affected();
        tracing::info!(day = %day, "maintenance: digest written");
        Ok((n == 1).then_some(d))
    }

    /// Source scout: under-covered cells → proposals (and a `<date>.md` file when any).
    /// Nothing happens without an [`Agentic`] bundle or with the LLM disabled.
    pub async fn scout(&self, now: DateTime<Utc>) -> anyhow::Result<Option<ScoutReport>> {
        let Some(a) = &self.agentic else {
            return Ok(None);
        };
        if !a.llm.enabled() {
            return Ok(None);
        }
        let cells = under_covered_cells(&self.pool, &a.cells).await?;
        if cells.is_empty() {
            return Ok(None);
        }
        let rep = run_scout(&a.llm, &self.pool, a.policy.as_ref(), &cells).await?;
        if !rep.proposals.is_empty() {
            std::fs::create_dir_all(&a.proposals_dir)?;
            let path = write_proposals_md(&a.proposals_dir, now.date_naive(), &rep)?;
            tracing::info!(path = %path.display(), proposals = rep.proposals.len(), "maintenance: scout proposals written");
        }
        Ok(Some(rep))
    }

    /// Quality auditor. Nothing happens without an [`Agentic`] bundle or with the LLM
    /// disabled.
    pub async fn audit(&self) -> anyhow::Result<Option<AuditReport>> {
        let Some(a) = &self.agentic else {
            return Ok(None);
        };
        if !a.llm.enabled() {
            return Ok(None);
        }
        let rep = run_auditor(
            &a.llm,
            &self.pool,
            a.text.as_ref(),
            &AuditSettings::default(),
        )
        .await?;
        if !rep.below_threshold.is_empty() {
            tracing::warn!(
                sources = rep.below_threshold.len(),
                "maintenance: sources below the audit threshold"
            );
        }
        Ok(Some(rep))
    }

    /// Parquet export (stub): the month partitions of `record_versions`, `captures` and
    /// `observations` older than 12 months. Nothing is exported or detached yet.
    pub async fn parquet_export(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<String>> {
        let due = export_due(&self.partition_names().await?, now);
        if !due.is_empty() {
            tracing::warn!(
                partitions = due.len(),
                "maintenance: partitions due for Parquet export (export not implemented yet)"
            );
        }
        Ok(due)
    }

    async fn partition_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i
             JOIN pg_class c ON c.oid = i.inhrelid JOIN pg_class p ON p.oid = i.inhparent
             WHERE p.relname IN ('record_versions', 'captures', 'observations')
             ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

/// The partitions (`<table>_YYYYMM`) whose month ended more than 12 months before `now`.
pub fn export_due(names: &[String], now: DateTime<Utc>) -> Vec<String> {
    let cutoff = now.year() * 12 + now.month0() as i32 - EXPORT_AFTER_MONTHS as i32;
    names
        .iter()
        .filter(|n| {
            let Some((_, ym)) = n.rsplit_once('_') else {
                return false;
            };
            if ym.len() != 6 || !ym.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            let (y, m): (i32, i32) = (ym[..4].parse().unwrap_or(0), ym[4..].parse().unwrap_or(0));
            (1..=12).contains(&m) && y * 12 + (m - 1) < cutoff
        })
        .cloned()
        .collect()
}

/// The digest of one UTC day: parked keys, capture statuses, yields, LLM spend, purges.
const DIGEST_SQL: &str = "
WITH b AS (SELECT $1::date::timestamp AT TIME ZONE 'UTC' AS lo,
                  ($1::date + 1)::timestamp AT TIME ZONE 'UTC' AS hi)
SELECT jsonb_build_object(
  'day', $1::date,
  'parked_keys', (SELECT count(*) FROM politeness_keys WHERE parked_until > b.hi),
  'parked_top', (SELECT coalesce(jsonb_agg(jsonb_build_object('key', key, 'reason', park_reason,
                          'level', park_level) ORDER BY park_level DESC, key), '[]')
                 FROM (SELECT key, park_reason, park_level FROM politeness_keys
                       WHERE parked_until > b.hi ORDER BY park_level DESC, key LIMIT 20) p),
  'captures', (SELECT count(*) FROM captures WHERE captured_at >= b.lo AND captured_at < b.hi),
  'statuses', (SELECT coalesce(jsonb_object_agg(coalesce(status::text, 'none'), n), '{}')
               FROM (SELECT status, count(*) AS n FROM captures
                     WHERE captured_at >= b.lo AND captured_at < b.hi GROUP BY status) s),
  'top_errors', (SELECT coalesce(jsonb_agg(jsonb_build_object('kind', kind, 'error', e, 'n', n)
                          ORDER BY n DESC), '[]')
                 FROM (SELECT kind, left(last_error, 80) AS e, count(*) AS n FROM work_items
                       WHERE last_error IS NOT NULL AND updated_at >= b.lo AND updated_at < b.hi
                       GROUP BY kind, left(last_error, 80) ORDER BY n DESC LIMIT 10) w),
  'records_new', (SELECT count(*) FROM records WHERE first_seen >= b.lo AND first_seen < b.hi),
  'versions_new', (SELECT count(*) FROM record_versions
                   WHERE created_at >= b.lo AND created_at < b.hi),
  'llm_usd', (SELECT coalesce(jsonb_object_agg(purpose, usd), '{}') FROM
               (SELECT purpose, round(sum(coalesce(actual_usd, est_usd)), 4) AS usd
                FROM llm_ledger WHERE day = $1::date GROUP BY purpose) l),
  'purges_open', (SELECT count(*) FROM purge_requests WHERE state <> 'done'),
  'dead_work', (SELECT count(*) FROM work_items WHERE state = 'dead'))
FROM b";

/// Runs the `maintenance` role until SIGTERM / Ctrl-C.
pub async fn run(ctx: Ctx) -> anyhow::Result<()> {
    let cfg = ctx.cfg.clone();
    if let Err(e) = vc_obs::init_tracing(&cfg.obs, "maintenance") {
        eprintln!("maintenance: tracing not installed: {e}");
    }
    let pool = crate::wiring::db::ops_pool(&cfg, 4).await?;
    let store = vc_store::blob_store_from_config(&cfg.store).await?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let metrics = Arc::new(Metrics::new(
        clock.clone(),
        vc_obs::heartbeat_freshness(&cfg.obs),
    ));
    let tracker = Arc::new(ProgressTracker::new(
        clock.clone(),
        StdDuration::from_secs(cfg.obs.progress_window_s.max(60)),
    ));
    let mut tasks = Vec::new();
    if let Some(l) = vc_obs::bind_metrics(&cfg.obs).await? {
        tasks.push(tokio::spawn({
            let (m, t) = (metrics.clone(), tracker.clone());
            async move {
                let _ = vc_obs::server::serve(l, m, t).await;
            }
        }));
    }
    let instance = cfg
        .vcrawl
        .instance
        .clone()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "maintenance".into());
    let hb = HeartbeatWriter::new(
        pool.clone(),
        "maintenance",
        &instance,
        tracker.clone(),
        metrics.clone(),
    );
    tasks.push(tokio::spawn(
        hb.run(StdDuration::from_secs(cfg.obs.heartbeat_s.max(1))),
    ));
    let agentic = agentic_from_config(&cfg, &pool, &store).await?;
    let m = Maintenance {
        pool,
        store,
        metrics: Some(metrics),
        partitions_ahead_months: cfg.vcrawl.partitions_ahead_months,
        agentic: Some(agentic),
    };
    let mut sched = Schedule::default();
    let mut tick = tokio::time::interval(StdDuration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!("maintenance: started");
    let res = loop {
        tokio::select! {
            _ = tick.tick() => {
                tracker.set_work_due(true);
                let ran = m.tick(&mut sched, clock.now()).await;
                tracker.record_progress(ran.len().max(1) as u64);
            }
            r = shutdown() => break r,
        }
    };
    for t in tasks {
        t.abort();
    }
    tracing::info!("maintenance: stopped");
    res
}

/// The production [`Agentic`] bundle: Vertex + `PgLedger` when `llm.enabled`, else a
/// disabled seam (no client is built, nothing can be called).
async fn agentic_from_config(
    cfg: &vc_config::Config,
    pool: &PgPool,
    store: &Arc<dyn BlobStore>,
) -> anyhow::Result<Agentic> {
    use anyhow::Context as _;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ledger = Arc::new(vc_llm::PgLedger::new(
        pool.clone(),
        cfg.llm.clone(),
        clock.clone(),
    ));
    let client: Arc<dyn vc_core::llm::LlmClient> = if cfg.llm.enabled {
        let vertex = vc_llm::VertexClient::new(&cfg.llm, Arc::new(vc_llm::AdcToken::new()), clock)?
            .with_shared_breaker(pool.clone());
        Arc::new(vc_llm::VertexLlm::new(cfg.llm.clone(), vertex).with_cache(pool.clone()))
    } else {
        Arc::new(DisabledLlm)
    };
    let policy = vc_policy::VoyagerPolicy::new(cfg.policy.clone()).context("policy")?;
    Ok(Agentic {
        llm: AgenticLlm::new(client, ledger, cfg.llm.clone()),
        policy: Arc::new(policy),
        proposals_dir: PathBuf::from(&cfg.vcrawl.data_dir).join("proposals"),
        text: Arc::new(StoreAuditText::new(pool.clone(), store.clone(), &cfg.parse)),
        cells: CellSettings::default(),
    })
}

/// The client used while `llm.enabled = false`: every call is refused as disabled.
struct DisabledLlm;

#[async_trait::async_trait]
impl vc_core::llm::LlmClient for DisabledLlm {
    async fn generate(
        &self,
        _r: &vc_core::llm::Reservation,
        _req: vc_core::llm::LlmRequest,
    ) -> vc_core::Result<vc_core::llm::LlmResponse> {
        Err(vc_core::llm::LlmError::Disabled.into())
    }
}

async fn shutdown() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = term.recv() => Ok(()),
            r = tokio::signal::ctrl_c() => Ok(r?),
        }
    }
    #[cfg(not(unix))]
    {
        Ok(tokio::signal::ctrl_c().await?)
    }
}
