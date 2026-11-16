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
//!
//! Not here, by design: Chromium recycling. The maintenance container has no route to the
//! `render` network; `vc-render` re-creates its CDP session every `render.restart_after_renders`
//! renders inside the fetcher. The monthly full restore drill (an ephemeral Spot VM restoring
//! the base backup + WAL) needs the bootstrap's drill template and is not wired yet.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;
use vc_core::store::BlobStore;
use vc_core::{Clock, SystemClock};
use vc_obs::{HeartbeatWriter, Metrics, ProgressTracker};

use crate::Ctx;
use crate::admin::{purge, rebuild};

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
}

impl Job {
    /// All jobs, in run order.
    pub const ALL: [Job; 6] = [
        Job::Partitions,
        Job::Purge,
        Job::Ephemeral,
        Job::RestoreDrill,
        Job::Digest,
        Job::ParquetExport,
    ];

    /// How often the job runs.
    pub fn interval(self) -> Duration {
        match self {
            Job::Partitions => Duration::hours(6),
            Job::Purge => Duration::minutes(15),
            Job::Ephemeral | Job::Digest => Duration::hours(1),
            Job::RestoreDrill => Duration::days(7),
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
    let pool = crate::admin::db::connect(&cfg).await?;
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
    let m = Maintenance {
        pool,
        store,
        metrics: Some(metrics),
        partitions_ahead_months: cfg.vcrawl.partitions_ahead_months,
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
