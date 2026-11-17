//! Role `scheduler` (Task 15): sources → seeds, sitemaps and feeds → the frontier; source
//! limits; recrawl hygiene; robots refresh (spec §5.2, §5.3, §5.10).
//!
//! One pass every `vcrawl.scheduler_interval_s`:
//!
//! 1. **Sources**: `sources/*.yaml` are loaded and validated, upserted into `sources`
//!    (rows missing from the directory are disabled, never deleted).
//! 2. **Seeds**: every enabled, ingestible source's seeds, feeds and sitemaps (and
//!    `adapter:<id>` rows for API/dump sources) are enqueued (`ON CONFLICT DO NOTHING`, so a URL
//!    already in the frontier keeps its own schedule). Links found on pages, sitemaps and feeds
//!    are enqueued by the processor.
//! 3. **Source limits** on their politeness keys: `min_delay_s` can only **raise** the key's
//!    minimum delay, `daily_budget` can only **lower** its budget.
//! 4. **Recrawl**: due rows of disabled sources are retired (`disabled`). Recrawl times
//!    themselves are set at fetch time (estimator bounded by the source cadence, and an API's
//!    `Expires`, e.g. MET Norway's), and the scheduler never moves them earlier — except:
//! 5. **robots refresh**: for origins whose robots.txt expired and whose key has nothing due in
//!    the next 24 h, one ordinary page row (`http`/`render`, never an adapter row, so MET
//!    Norway's `Expires` stands) is made due now. Its claim fetches robots.txt first
//!    (`robots_due`), which tombstones newly disallowed paths.
//! 6. **Gauges**: `due_urls`, `parked_keys_ratio`; old raw-bytes counter rows are pruned, and
//!    so are adapter resume cursors of rows that left the crawl (retired above) or that
//!    nobody resumed for [`CURSOR_MAX_AGE_DAYS`].
//!
//! **Ramp** (`vcrawl.source_allowlist`, go-live.md §3): when set, steps 2 and 3 cover only the
//! allowlisted sources. Every spec is still upserted (the catalogue stays complete), but no
//! other source gets frontier rows or key limits. A configured but unreadable allowlist fails
//! the pass before anything is written.
//!
//! The LLM startup probe runs here (once per deploy) when `llm.enabled`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use vc_config::Config;
use vc_core::frontier::Frontier;
use vc_core::sources::SourceSpec;
use vc_core::{Error, Result};
use vc_sources::SeedReport;

use crate::Ctx;
use crate::wiring;
use crate::wiring::rawcap::PgRawBytes;

/// Runs the `scheduler` role until SIGTERM / Ctrl-C.
pub async fn run(ctx: Ctx) -> anyhow::Result<()> {
    wiring::obs::init_tracing(&ctx, "scheduler");
    let pool = ctx.pool().await?;
    wiring::llm::probe_if_enabled(&ctx.cfg, &pool, ctx.clock.clone()).await?;
    let frontier = wiring::frontier(&ctx.cfg, pool.clone(), "scheduler", &ctx.instance);
    let s = Scheduler::new(ctx.cfg.clone(), pool.clone(), frontier)
        .with_metrics(ctx.metrics.clone(), ctx.tracker.clone());
    let _obs = wiring::obs::start(&ctx, "scheduler", pool).await?;
    let mut rx = ctx.stop();
    let every = Duration::from_secs(ctx.cfg.vcrawl.scheduler_interval_s.max(1));
    while !*rx.borrow() {
        match s.pass_from_dir().await {
            Ok(r) => tracing::info!(
                sources_inserted = r.sources.inserted,
                sources_updated = r.sources.updated,
                sources_disabled = r.sources.disabled,
                enqueued = r.enqueued,
                keys_limited = r.keys_limited,
                retired = r.retired,
                held_by_allowlist = r.held_by_allowlist,
                robots_refresh = r.robots_refresh,
                "scheduler pass"
            ),
            Err(e) => tracing::warn!(error_class = ?e.class(), error = %e, "scheduler pass failed"),
        }
        tokio::select! {
            _ = tokio::time::sleep(every) => {}
            _ = rx.changed() => {}
        }
    }
    Ok(())
}

/// What one pass did (counts only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Source rows.
    pub sources: SeedReport,
    /// New frontier URLs.
    pub enqueued: u64,
    /// Politeness keys whose delay/budget the source limits changed.
    pub keys_limited: u64,
    /// Due rows of disabled sources retired.
    pub retired: u64,
    /// Origins whose robots refresh was scheduled.
    pub robots_refresh: u64,
    /// URLs due now.
    pub due_urls: i64,
    /// Adapter resume cursors pruned (row left the crawl, or not resumed for 30 days).
    pub cursors_pruned: u64,
    /// Enabled sources not seeded because `vcrawl.source_allowlist` leaves them out.
    pub held_by_allowlist: u64,
}

/// The scheduler.
pub struct Scheduler {
    cfg: Arc<Config>,
    pool: PgPool,
    frontier: Arc<dyn Frontier>,
    metrics: Option<Arc<vc_obs::Metrics>>,
    tracker: Option<Arc<vc_obs::ProgressTracker>>,
}

/// Adapter resume cursors nobody resumed for this long are dropped (the stream starts over).
pub const CURSOR_MAX_AGE_DAYS: i32 = 30;

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

impl Scheduler {
    /// A scheduler on `pool` and `frontier`.
    pub fn new(cfg: Arc<Config>, pool: PgPool, frontier: Arc<dyn Frontier>) -> Self {
        Scheduler {
            cfg,
            pool,
            frontier,
            metrics: None,
            tracker: None,
        }
    }

    /// Reports gauges and progress.
    pub fn with_metrics(
        mut self,
        m: Arc<vc_obs::Metrics>,
        t: Arc<vc_obs::ProgressTracker>,
    ) -> Self {
        self.metrics = Some(m);
        self.tracker = Some(t);
        self
    }

    /// One pass over `<sources.dir>`.
    pub async fn pass_from_dir(&self) -> Result<PassReport> {
        let specs = vc_sources::load_dir(std::path::Path::new(&self.cfg.sources.dir))?;
        self.pass(&specs).await
    }

    /// One pass over `specs` (module docs).
    pub async fn pass(&self, specs: &[SourceSpec]) -> Result<PassReport> {
        let allowlist = crate::wiring::allowlist::SourceAllowlist::load(&self.cfg)?;
        if let Some(t) = &self.tracker {
            t.set_work_due(true);
        }
        let (crawled, held): (Vec<SourceSpec>, Vec<SourceSpec>) = specs
            .iter()
            .cloned()
            .partition(|s| allowlist.as_ref().is_none_or(|a| a.contains(&s.id.0)));
        let sources = vc_sources::seed_sources(&self.pool, specs, true).await?;
        // A source an admin disabled keeps its spec's `enabled: true`: never seed it (else
        // new seed rows would be claimable until `retire_disabled` below).
        let off = self.disabled_ids().await?;
        let crawled: Vec<SourceSpec> = crawled
            .into_iter()
            .filter(|s| !off.contains(&s.id.0))
            .collect();
        let mut r = PassReport {
            sources,
            enqueued: vc_sources::seed_frontier(self.frontier.as_ref(), &crawled).await?,
            held_by_allowlist: held.iter().filter(|s| s.enabled).count() as u64,
            ..PassReport::default()
        };
        r.keys_limited = self.apply_source_limits(&crawled).await?;
        r.retired = self.retire_disabled().await?;
        r.cursors_pruned = vc_sources::cursors::PgAdapterCursors::new(self.pool.clone())
            .prune(CURSOR_MAX_AGE_DAYS)
            .await?;
        r.robots_refresh = self.schedule_robots_refresh().await?;
        r.due_urls = self.gauges().await?;
        PgRawBytes::new(self.pool.clone(), self.cfg.fetch.daily_raw_bytes_cap)
            .prune()
            .await?;
        if let Some(t) = &self.tracker {
            t.record_progress(1);
        }
        Ok(r)
    }

    /// Source `min_delay_s` raises, `daily_budget` lowers the limits of the source's keys.
    async fn apply_source_limits(&self, specs: &[SourceSpec]) -> Result<u64> {
        let ids = vc_sources::adapters::ids();
        // key → (min delay ms, daily budget): the strictest over the sources sharing it.
        let mut limits: BTreeMap<String, (i32, i32)> = BTreeMap::new();
        for s in specs {
            let delay_ms = i32::try_from(u64::from(s.min_delay_s).saturating_mul(1000))
                .unwrap_or(3_600_000)
                .min(3_600_000);
            let budget = i32::try_from(s.daily_budget).unwrap_or(i32::MAX);
            for nu in vc_sources::seed_urls(s, &ids)? {
                let e = limits.entry(nu.key.0).or_insert((delay_ms, budget));
                e.0 = e.0.max(delay_ms);
                e.1 = e.1.min(budget);
            }
        }
        let mut n = 0;
        for (key, (delay_ms, budget)) in limits {
            let res = sqlx::query(
                "UPDATE politeness_keys
                 SET min_delay_ms = greatest(min_delay_ms, $2),
                     budget_day = least(budget_day, $3),
                     updated_at = now()
                 WHERE key = $1 AND (min_delay_ms < $2 OR budget_day > $3)",
            )
            .bind(&key)
            .bind(delay_ms)
            .bind(budget)
            .execute(&self.pool)
            .await
            .map_err(db)?;
            n += res.rows_affected();
        }
        Ok(n)
    }

    /// Ids of sources disabled in the database (admin decision, licence or missing spec).
    async fn disabled_ids(&self) -> Result<std::collections::HashSet<String>> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT source_id FROM sources WHERE NOT enabled")
                .fetch_all(&self.pool)
                .await
                .map_err(db)?;
        Ok(ids.into_iter().collect())
    }

    /// Due rows of disabled sources leave the crawl.
    async fn retire_disabled(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE url_state u SET state = 'disabled'
             FROM sources s
             WHERE u.source_id = s.source_id AND NOT s.enabled AND u.state = 'due'",
        )
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(res.rows_affected())
    }

    /// Module docs, step 5.
    async fn schedule_robots_refresh(&self) -> Result<u64> {
        let batch = i64::from(self.cfg.vcrawl.robots_refresh_batch);
        let res = sqlx::query(
            "WITH stale AS (
                 SELECT o.origin, o.key FROM origins o
                 WHERE o.robots_expires_at < now()
                   AND NOT EXISTS (SELECT 1 FROM url_state d
                                   WHERE d.key = o.key AND d.state = 'due'
                                     AND d.next_fetch_at <= now() + interval '1 day')
                 ORDER BY o.robots_expires_at
                 LIMIT $1
             ), pick AS (
                 SELECT DISTINCT ON (s.origin) u.url_key
                 FROM stale s
                 JOIN urls x ON x.key = s.key
                            AND (x.url = s.origin OR x.url LIKE s.origin || '/%')
                 JOIN url_state u ON u.url_key = x.url_key
                 WHERE u.state = 'due' AND u.fetch_mode IN ('http', 'render')
                 ORDER BY s.origin, u.next_fetch_at
             )
             UPDATE url_state u SET next_fetch_at = now()
             FROM pick WHERE u.url_key = pick.url_key AND u.state = 'due'",
        )
        .bind(batch)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(res.rows_affected())
    }

    async fn gauges(&self) -> Result<i64> {
        let (due, parked, keys): (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM url_state WHERE state = 'due' AND next_fetch_at <= now()),
                    (SELECT count(*) FROM politeness_keys WHERE parked_until > now()),
                    (SELECT count(*) FROM politeness_keys)",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        if let Some(m) = &self.metrics {
            m.due_urls.set(due);
            m.parked_keys_ratio.set(if keys > 0 {
                parked as f64 / keys as f64
            } else {
                0.0
            });
        }
        Ok(due)
    }
}
