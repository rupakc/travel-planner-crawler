//! Role `fetcher` (Task 15): the claim loop (spec §5.2, §5.5, §17.1, §17.2).
//!
//! One claim at a time per worker (`vcrawl.fetcher_concurrency` workers):
//!
//! 1. **Claim** a ready key ([`Frontier::claim`]); nothing ready → wait.
//! 2. **robots.txt first**: every origin in `robots_due` is fetched under the same permit,
//!    spooled raw (fsync) and stored ([`Frontier::store_robots`]). 401/403 or a challenge on
//!    robots.txt makes the whole key do-not-crawl: a WARC `metadata` record notes it and the key
//!    is parked. Paths that robots newly disallows are tombstoned
//!    ([`RecordSink::tombstone`], reason `robots`; Review Focus 1).
//! 3. **Each leased URL**, sequentially (the permit client paces by the key delay):
//!    `http` → [`PermitClient::fetch`] with the committed validators; `render` →
//!    [`Renderer::render`] (sub-requests and the DOM are spooled); `adapter:<id>` → the
//!    [`SourceAdapter`] with the permit. The record is appended to the spool **and fsynced**,
//!    then [`Frontier::mark_fetched`] moves the URL to `fetched_pending` (validators untouched,
//!    §17.2). A 304 is a `revisit` of the last committed capture, so it can never point at a
//!    body we do not have.
//! 4. **Complete** the claim with the per-URL results and the key signal.
//!
//! A cross-key redirect is **enqueued**, never followed. A challenge, 401 or 403 **parks** the
//! key (detect → park; nothing is retried with another identity). 429/503 honour
//! `Retry-After`. The overall daily raw-bytes cap is shared by every fetcher through Postgres
//! ([`RawBytesBudget`]). An adapter response's `Expires` (MET Norway's terms) is a lower bound
//! for the row's next fetch.
//!
//! The [`SegmentRoller`] runs next to the workers: on start it replays every sealed spool file
//! (steps 2–3 of §5.5 are idempotent by segment id), then it seals, uploads and commits every
//! `roller_tick_s`. Backpressure (spool over 2 GB or uploads failing > 15 min) pauses the
//! workers.
//!
//! ## Failpoints (feature `failpoints`)
//!
//! `fetcher::after_spool` (record fsynced, URL not yet `fetched_pending`) and
//! `fetcher::before_complete` (URLs marked, claim not completed). An error there abandons the
//! claim exactly like a crash: the permit is dropped without completion.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tokio::sync::watch;
use vc_config::Config;
use vc_core::fetch::{Exchange, FetchError, FetchRequest, PermitClient, RequestPurpose};
use vc_core::frontier::{
    Claim, ClaimPermit, Frontier, KeyOutcome, KeySignal, LeasedUrl, NewUrl, ParkReason, UrlOutcome,
    UrlResult,
};
use vc_core::index::{PurgeSelector, RecordSink, TombstoneReason};
use vc_core::media::{FetchMode, MediaType};
use vc_core::policy::{Policy, RobotsEntry, RobotsStatus, RobotsSummary};
use vc_core::render::Renderer;
use vc_core::secrets::SecretSource;
use vc_core::sources::{AdapterCtx, SourceAdapter, SourceSpec};
use vc_core::store::{CaptureMeta, CaptureRole, CaptureSink, SpoolPayload, SpoolRecord};
use vc_core::usepolicy::{RawUse, UsePolicy, UseSignals};
use vc_core::{
    CanonicalUrl, CaptureId, Clock, Error, ErrorClass, LeaseLostReason, OriginKey, Result, UrlKey,
};
use vc_frontier::PgFrontier;
use vc_store::warc::{LastPayload, dedupe_payload};
use vc_store::{SegmentRoller, Spool};

use crate::Ctx;
use crate::wiring;
use crate::wiring::rawcap::{PgRawBytes, RawBytesBudget};
use crate::wiring::signals::header_signals;
use crate::wiring::sources::Sources;

/// Runs the `fetcher` role until SIGTERM / Ctrl-C.
pub async fn run(ctx: Ctx) -> anyhow::Result<()> {
    wiring::obs::init_tracing(&ctx, "fetcher");
    let pool = ctx.pool().await?;
    let parts = FetcherParts::from_ctx(&ctx, pool.clone()).await?;
    let fetcher = Arc::new(Fetcher::new(parts));
    let _obs = wiring::obs::start(&ctx, "fetcher", pool).await?;
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("fetcher: shutting down");
        let _ = tx.send(true);
    });
    fetcher.run_until(rx).await?;
    Ok(())
}

/// Waits for SIGTERM (docker stop) or Ctrl-C.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The next recrawl time of a URL (Cho & Garcia-Molina, bounded by the source cadence).
#[async_trait]
pub trait Recrawl: Send + Sync {
    /// When to fetch `url` again, in `[min, max]` from now.
    async fn next_fetch_at(
        &self,
        url: &UrlKey,
        min: Duration,
        max: Duration,
    ) -> Result<DateTime<Utc>>;
}

#[async_trait]
impl Recrawl for PgFrontier {
    async fn next_fetch_at(
        &self,
        url: &UrlKey,
        min: Duration,
        max: Duration,
    ) -> Result<DateTime<Utc>> {
        self.estimate_next_fetch(url, min, max).await
    }
}

/// Tombstones what robots.txt newly disallows (Review Focus 1).
#[derive(Clone)]
pub struct RobotsTombstoner {
    /// Pool (to list an origin's URLs and retire their frontier rows).
    pub pool: PgPool,
    /// The record sink (`vc-index`).
    pub sink: Arc<dyn RecordSink>,
}

impl RobotsTombstoner {
    async fn urls_of(&self, p: &ClaimPermit, origin: &OriginKey) -> Result<Vec<CanonicalUrl>> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT url FROM urls WHERE key = $1")
            .bind(p.key().as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::Db(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter_map(|(u,)| CanonicalUrl::parse(&u).ok())
            .filter(|u| u.origin() == *origin)
            .collect())
    }

    async fn tombstone(&self, urls: &[CanonicalUrl]) -> Result<u64> {
        for u in urls {
            self.sink
                .tombstone(&PurgeSelector::Url(u.key()), TombstoneReason::Robots)
                .await?;
        }
        let keys: Vec<Vec<u8>> = urls.iter().map(|u| u.key().as_bytes().to_vec()).collect();
        sqlx::query(
            "UPDATE url_state SET state = 'tombstoned' WHERE url_key = ANY($1) AND state = 'due'",
        )
        .bind(&keys)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Db(e.to_string()))?;
        Ok(urls.len() as u64)
    }
}

/// Everything a fetcher needs (built by [`FetcherParts::from_ctx`] in production; tests swap in
/// a stub policy, a static resolver or another blob store).
pub struct FetcherParts {
    /// Configuration.
    pub cfg: Arc<Config>,
    /// The frontier.
    pub frontier: Arc<dyn Frontier>,
    /// Recrawl estimator.
    pub recrawl: Arc<dyn Recrawl>,
    /// The permit client.
    pub client: Arc<dyn PermitClient>,
    /// The renderer (`None` = render rows are fetched over HTTP).
    pub renderer: Option<Arc<dyn Renderer>>,
    /// The policy.
    pub policy: Arc<dyn Policy>,
    /// This instance's spool.
    pub spool: Spool,
    /// Its segment roller.
    pub roller: Arc<SegmentRoller>,
    /// Source specs.
    pub sources: Arc<Sources>,
    /// Adapters by id.
    pub adapters: Arc<HashMap<&'static str, Arc<dyn SourceAdapter>>>,
    /// Secrets (adapter API keys).
    pub secrets: Arc<dyn SecretSource>,
    /// The daily raw-bytes cap.
    pub raw_bytes: Arc<dyn RawBytesBudget>,
    /// Robots tombstoning (`None`: not wired).
    pub tombstoner: Option<RobotsTombstoner>,
    /// Pool for change-feed lookups (`None`: every changed entry is rescheduled).
    pub pool: Option<PgPool>,
    /// Clock.
    pub clock: Arc<dyn Clock>,
    /// Metrics (optional).
    pub metrics: Option<Arc<vc_obs::Metrics>>,
    /// Progress tracker (optional).
    pub tracker: Option<Arc<vc_obs::ProgressTracker>>,
}

impl FetcherParts {
    /// The production wiring.
    pub async fn from_ctx(ctx: &Ctx, pool: PgPool) -> Result<Self> {
        let cfg = ctx.cfg.clone();
        let clock = ctx.clock.clone();
        let pg = wiring::frontier(&cfg, pool.clone(), "fetcher", &ctx.instance);
        let frontier: Arc<dyn Frontier> = pg.clone();
        let policy = wiring::fetch::policy(&cfg)?;
        let client = wiring::fetch::permit_client(
            &cfg,
            frontier.clone(),
            policy.clone(),
            clock.clone(),
            None,
        );
        let renderer = wiring::fetch::renderer(
            &cfg,
            client.clone(),
            frontier.clone(),
            policy.clone(),
            clock.clone(),
        );
        let store = wiring::store::blob_store(&cfg).await?;
        let dir = wiring::store::spool_dir(&cfg, "fetcher", &ctx.instance);
        let (spool, roller) = wiring::store::spool_and_roller(
            &cfg,
            dir,
            store,
            wiring::store::committer(pool.clone()),
            clock.clone(),
        )?;
        Ok(FetcherParts {
            frontier,
            recrawl: pg,
            client,
            renderer,
            policy,
            spool,
            roller,
            sources: Arc::new(Sources::load(&cfg)?),
            adapters: Arc::new(wiring::sources::adapters(&cfg)),
            secrets: wiring::sources::secrets(&cfg),
            raw_bytes: Arc::new(PgRawBytes::new(pool.clone(), cfg.fetch.daily_raw_bytes_cap)),
            tombstoner: Some(RobotsTombstoner {
                sink: Arc::new(vc_index::PgIndex::new(pool.clone())),
                pool: pool.clone(),
            }),
            pool: Some(pool),
            clock,
            metrics: Some(ctx.metrics.clone()),
            tracker: Some(ctx.tracker.clone()),
            cfg,
        })
    }
}

/// What one pass of a worker did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pass {
    /// No key was ready.
    Idle,
    /// Paused (backpressure or the daily raw-bytes cap).
    Paused,
    /// A claim was processed.
    Claimed(ClaimReport),
}

/// What one claim did (ids and counts only).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClaimReport {
    /// Per-URL results (every leased URL appears).
    pub urls: Vec<UrlOutcome>,
    /// The key signal.
    pub signal: Option<KeySignal>,
    /// Captures appended to the spool (fsynced).
    pub spooled: Vec<CaptureId>,
    /// Cross-key redirect targets enqueued.
    pub enqueued: Vec<CanonicalUrl>,
    /// URLs tombstoned after a robots refresh.
    pub tombstoned: u64,
    /// Whether the completion was accepted (false: lease lost).
    pub completed: bool,
}

/// The fetcher.
pub struct Fetcher {
    p: FetcherParts,
}

/// Per-claim state.
struct Run<'a> {
    permit: &'a ClaimPermit,
    report: ClaimReport,
    signal: KeySignal,
    latencies: Vec<Duration>,
    consecutive_5xx: u32,
    crawl_delay: Option<Duration>,
    summaries: HashMap<OriginKey, RobotsSummary>,
    stop: bool,
}

impl Run<'_> {
    fn result(&mut self, url: &UrlKey, result: UrlResult) {
        self.report.urls.push(UrlOutcome {
            url_key: *url,
            result,
        });
    }

    fn stop_with(&mut self, s: KeySignal) {
        self.signal = s;
        self.stop = true;
    }
}

fn chrono_of(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::days(365))
}

impl Fetcher {
    /// A fetcher over `parts`.
    pub fn new(parts: FetcherParts) -> Self {
        Fetcher { p: parts }
    }

    /// The parts.
    pub fn parts(&self) -> &FetcherParts {
        &self.p
    }

    /// Replays the spool on start, then runs the workers and the roller until `shutdown` turns
    /// true; flushes the spool on the way out.
    pub async fn run_until(self: &Arc<Self>, shutdown: watch::Receiver<bool>) -> Result<()> {
        if let Err(e) = self.tick_roller().await {
            tracing::warn!(error_class = ?e.class(), error = %e, "fetcher: spool replay failed");
        }
        let roller = {
            let me = self.clone();
            let mut stop = shutdown.clone();
            let every = Duration::from_secs(me.p.cfg.vcrawl.roller_tick_s.max(1));
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(every) => {}
                        _ = stop.changed() => break,
                    }
                    if let Err(e) = me.tick_roller().await {
                        tracing::warn!(error_class = ?e.class(), error = %e, "fetcher: roller tick failed");
                    }
                }
            })
        };
        let mut workers = Vec::new();
        for _ in 0..self.p.cfg.vcrawl.fetcher_concurrency.max(1) {
            let me = self.clone();
            let mut stop = shutdown.clone();
            workers.push(tokio::spawn(async move {
                let idle = Duration::from_millis(me.p.cfg.vcrawl.fetcher_idle_ms.max(10));
                while !*stop.borrow() {
                    let wait = match me.pass().await {
                        Ok(Pass::Claimed(_)) => Duration::ZERO,
                        Ok(Pass::Idle) => idle,
                        Ok(Pass::Paused) => idle.max(Duration::from_secs(5)),
                        Err(e) => {
                            tracing::warn!(error_class = ?e.class(), error = %e, "fetcher: claim abandoned");
                            idle.max(Duration::from_secs(1))
                        }
                    };
                    if !wait.is_zero() {
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = stop.changed() => {}
                        }
                    }
                }
            }));
        }
        for w in workers {
            let _ = w.await;
        }
        let _ = roller.await;
        self.p.roller.flush().await.map(|_| ())
    }

    /// One roller tick: seal due files, publish everything sealed. Returns captures committed.
    pub async fn tick_roller(&self) -> Result<u64> {
        let r = self.p.roller.tick().await;
        self.after_publish(r)
    }

    /// Seals everything and publishes it (shutdown, tests).
    pub async fn flush(&self) -> Result<u64> {
        let r = self.p.roller.flush().await;
        self.after_publish(r)
    }

    fn after_publish(&self, r: Result<Vec<vc_core::store::SegmentManifest>>) -> Result<u64> {
        if let Some(m) = &self.p.metrics {
            m.backpressure
                .set(i64::from(self.p.roller.should_pause_fetchers()));
        }
        let ms = r?;
        let n: u64 = ms.iter().map(|m| m.captures.len() as u64).sum();
        if n > 0 {
            if let Some(m) = &self.p.metrics {
                m.captures_committed.inc_by(n);
            }
            if let Some(t) = &self.p.tracker {
                t.record_progress(n);
            }
        }
        Ok(n)
    }

    /// One worker pass: claim one key and crawl it.
    pub async fn pass(&self) -> Result<Pass> {
        if self.p.roller.should_pause_fetchers() {
            tracing::warn!("fetcher: backpressure, pausing");
            return Ok(Pass::Paused);
        }
        if self.p.raw_bytes.exhausted().await? {
            tracing::info!("fetcher: daily raw-bytes cap reached, pausing");
            return Ok(Pass::Paused);
        }
        let max = self.p.cfg.frontier.max_urls_per_claim;
        let Some(claim) = self
            .p
            .frontier
            .claim(&self.p.cfg.frontier.shards, max)
            .await?
        else {
            if let Some(t) = &self.p.tracker {
                t.set_work_due(false);
            }
            return Ok(Pass::Idle);
        };
        if let Some(t) = &self.p.tracker {
            t.set_work_due(true);
        }
        Ok(Pass::Claimed(self.crawl(claim).await?))
    }

    /// Crawls one claim: robots first, then each leased URL, then completion. An `Err` means
    /// the claim was abandoned without completion (like a crash; the lease simply expires).
    pub async fn crawl(&self, claim: Claim) -> Result<ClaimReport> {
        let Claim {
            permit,
            urls,
            robots_due,
        } = claim;
        let mut run = Run {
            permit: &permit,
            report: ClaimReport::default(),
            signal: KeySignal::Normal,
            latencies: Vec::new(),
            consecutive_5xx: 0,
            crawl_delay: None,
            summaries: HashMap::new(),
            stop: false,
        };
        for origin in &robots_due {
            match self.refresh_robots(&mut run, origin).await {
                Ok(()) => {}
                Err(Error::LeaseLost(_)) => run.stop = true,
                Err(e) => return Err(e),
            }
            if run.stop {
                break;
            }
        }
        for u in &urls {
            if run.stop || self.p.raw_bytes.exhausted().await? {
                break;
            }
            match self.fetch_one(&mut run, u).await {
                Ok(()) => {}
                Err(Error::LeaseLost(_)) => run.stop = true,
                Err(e) => return Err(e),
            }
        }
        fail::fail_point!("fetcher::before_complete", |_| Err(Error::Storage(
            "failpoint fetcher::before_complete".into()
        )));
        let mean_latency = if run.latencies.is_empty() {
            None
        } else {
            let n = u32::try_from(run.latencies.len()).unwrap_or(u32::MAX);
            Some(run.latencies.iter().sum::<Duration>() / n)
        };
        let mut report = std::mem::take(&mut run.report);
        report.signal = Some(run.signal.clone());
        for u in &urls {
            if !report.urls.iter().any(|o| o.url_key == u.url_key) {
                report.urls.push(UrlOutcome {
                    url_key: u.url_key,
                    result: UrlResult::NotAttempted,
                });
            }
        }
        let outcome = KeyOutcome {
            urls: report.urls.clone(),
            signal: run.signal.clone(),
            requests: permit.requests_made(),
            mean_latency,
            crawl_delay: run.crawl_delay,
        };
        let key = permit.key().clone();
        match self.p.frontier.complete(permit, outcome).await {
            Ok(()) => report.completed = true,
            Err(e) => tracing::info!(key = %key, error = %e, "fetcher: completion rejected"),
        }
        Ok(report)
    }

    // -----------------------------------------------------------------------------------------
    // robots.txt
    // -----------------------------------------------------------------------------------------

    async fn refresh_robots(&self, run: &mut Run<'_>, origin: &OriginKey) -> Result<()> {
        let p = run.permit;
        let url = CanonicalUrl::parse(&format!("{}/robots.txt", origin.0))
            .map_err(|e| Error::Invalid(format!("robots url: {e}")))?;
        let old = self.p.frontier.robots(origin).await?;
        let now = self.p.clock.now();
        let res = self
            .p
            .client
            .fetch(p, FetchRequest::get(url.clone(), RequestPurpose::Robots))
            .await;
        let (status, http_status, body) = match res {
            Ok(ex) => {
                let code = ex.status.as_u16();
                run.latencies.push(ex.latency());
                let st = if self.p.policy.is_challenge(&ex) {
                    RobotsStatus::Challenge
                } else if ex.status.is_success() {
                    RobotsStatus::Ok
                } else if code == 401 || code == 403 {
                    RobotsStatus::Unauthorized
                } else if code == 429 || ex.status.is_server_error() {
                    RobotsStatus::Unavailable
                } else if ex.status.is_client_error() {
                    RobotsStatus::NotFound
                } else {
                    RobotsStatus::Unavailable
                };
                let body = (st == RobotsStatus::Ok).then(|| ex.body.clone());
                self.spool_robots(run, &url, SpoolPayload::Response(ex), Some(code), now)
                    .await?;
                (st, Some(code), body)
            }
            Err(FetchError::Challenge { status }) => {
                let st = if status == 401 || status == 403 {
                    RobotsStatus::Unauthorized
                } else {
                    RobotsStatus::Challenge
                };
                // The exchange of a challenge is not handed back: record the decision itself.
                let fields = serde_json::json!({
                    "type": "robots_blocked",
                    "url": url.as_str(),
                    "http_status": status,
                    "robots_status": if st == RobotsStatus::Unauthorized { "unauthorized" } else { "challenge" },
                    "action": "key_do_not_crawl",
                });
                self.spool_robots(
                    run,
                    &url,
                    SpoolPayload::Metadata { fields },
                    Some(status),
                    now,
                )
                .await?;
                (st, Some(status), None)
            }
            Err(FetchError::Lease(l)) => return Err(Error::LeaseLost(l)),
            Err(FetchError::DeadlineExceeded) => {
                return Err(Error::LeaseLost(p.lease_lost(LeaseLostReason::Expired)));
            }
            Err(e) => {
                tracing::info!(origin = %origin.0, error_class = ?e.class(), "robots.txt unavailable");
                (RobotsStatus::Unavailable, None, None)
            }
        };
        let ttl = if status == RobotsStatus::Unavailable {
            chrono::Duration::hours(1)
        } else {
            chrono::Duration::seconds(
                i64::try_from(self.p.cfg.policy.robots_ttl_s).unwrap_or(86_400),
            )
        };
        let entry = RobotsEntry {
            origin: origin.clone(),
            status,
            http_status,
            body,
            last_good: None,
            fetched_at: now,
            expires_at: now + ttl,
        };
        self.p.frontier.store_robots(p, entry.clone()).await?;
        // Review Focus 1: paths that robots newly disallows are tombstoned.
        if let (Some(old), Some(t)) = (old, &self.p.tombstoner)
            && matches!(old.status, RobotsStatus::Ok | RobotsStatus::NotFound)
            && entry.status == RobotsStatus::Ok
        {
            let urls = t.urls_of(p, origin).await?;
            let newly = self.p.policy.newly_disallowed(&old, &entry, &urls);
            if !newly.is_empty() {
                run.report.tombstoned += t.tombstone(&newly).await?;
                tracing::info!(origin = %origin.0, urls = newly.len(), "robots: newly disallowed paths tombstoned");
            }
        }
        let summary = self.p.policy.robots_summary(&entry);
        if let Some(d) = summary.crawl_delay {
            run.crawl_delay = Some(run.crawl_delay.map_or(d, |c| c.max(d)));
        }
        let blocked = summary.key_blocked
            || matches!(
                entry.status,
                RobotsStatus::Unauthorized | RobotsStatus::Challenge
            );
        run.summaries.insert(origin.clone(), summary);
        if blocked {
            tracing::info!(key = %p.key(), origin = %origin.0, "robots.txt blocks the key: parking");
            run.stop_with(KeySignal::Park(ParkReason::RobotsBlocked));
        }
        Ok(())
    }

    async fn spool_robots(
        &self,
        run: &mut Run<'_>,
        url: &CanonicalUrl,
        payload: SpoolPayload,
        status: Option<u16>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut meta = self.meta(run.permit, url, url.key(), None, now);
        meta.role = CaptureRole::Robots;
        meta.status = status;
        meta.media_type = match payload {
            SpoolPayload::Metadata { .. } => MediaType::Json,
            _ => MediaType::Text,
        };
        self.append(run, SpoolRecord { meta, payload }).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // One URL
    // -----------------------------------------------------------------------------------------

    fn meta(
        &self,
        p: &ClaimPermit,
        url: &CanonicalUrl,
        url_key: UrlKey,
        u: Option<&LeasedUrl>,
        now: DateTime<Utc>,
    ) -> CaptureMeta {
        CaptureMeta {
            capture_id: CaptureId::at(now),
            captured_at: now,
            epoch: p.epoch(),
            orphan_epoch: false,
            url_key,
            url: url.clone(),
            key: p.key().clone(),
            source_id: u.and_then(|u| u.source_id.clone()),
            role: CaptureRole::Primary,
            fetch_mode: u.map(|u| u.fetch_mode.clone()).unwrap_or_default(),
            status: None,
            media_type: MediaType::Other,
            payload_hash: None,
            payload_len: 0,
            validators: Default::default(),
            revisit_of: None,
            use_signals: UseSignals::default(),
            use_policy_fetch: UsePolicy::ALLOW_ALL,
            raw_expires_at: None,
            next_fetch_at: None,
        }
    }

    /// Appends (fsync) and counts the bytes against the daily cap.
    async fn append(&self, run: &mut Run<'_>, r: SpoolRecord) -> Result<CaptureId> {
        let len = match &r.payload {
            SpoolPayload::Response(ex) => ex.body.len() as u64,
            SpoolPayload::Resource { body, .. } => body.len() as u64,
            SpoolPayload::Revisit(_) | SpoolPayload::Metadata { .. } => 0,
        };
        let mut spool = self.p.spool.clone();
        let id = spool.append(r).await?;
        run.report.spooled.push(id);
        if len > 0 {
            self.p.raw_bytes.add(len).await?;
        }
        Ok(id)
    }

    fn source(&self, u: &LeasedUrl) -> Option<Arc<SourceSpec>> {
        u.source_id.as_ref().and_then(|s| self.p.sources.get(s))
    }

    /// `[min, max]` for the recrawl estimator from the source cadence.
    fn recrawl_bounds(&self, u: &LeasedUrl) -> (Duration, Duration) {
        let max = self
            .source(u)
            .and_then(|s| s.cadence.interval())
            .unwrap_or(Duration::from_secs(
                self.p.cfg.vcrawl.recrawl_default_s.max(60),
            ));
        let min = Duration::from_secs(self.p.cfg.vcrawl.recrawl_min_s).min(max);
        (min, max)
    }

    async fn next_fetch(&self, u: &LeasedUrl, not_before: Option<DateTime<Utc>>) -> DateTime<Utc> {
        let (min, max) = self.recrawl_bounds(u);
        let est = match self.p.recrawl.next_fetch_at(&u.url_key, min, max).await {
            Ok(t) => t,
            Err(_) => self.p.clock.now() + chrono_of(max),
        };
        match not_before {
            Some(nb) if nb > est => nb,
            _ => est,
        }
    }

    async fn fetch_one(&self, run: &mut Run<'_>, u: &LeasedUrl) -> Result<()> {
        match &u.fetch_mode {
            FetchMode::Adapter(id) => self.fetch_adapter(run, u, id).await,
            FetchMode::Render if self.p.renderer.is_some() => self.fetch_render(run, u).await,
            FetchMode::Http | FetchMode::Render => self.fetch_http(run, u).await,
        }
    }

    async fn fetch_http(&self, run: &mut Run<'_>, u: &LeasedUrl) -> Result<()> {
        let purpose = match u.expect {
            Some(MediaType::Sitemap | MediaType::SitemapText) => RequestPurpose::Sitemap,
            Some(MediaType::Rss | MediaType::Atom) => RequestPurpose::Feed,
            _ => RequestPurpose::Page,
        };
        let mut req = FetchRequest::get(u.url.clone(), purpose);
        req.validators = u.validators.clone();
        match self.p.client.fetch(run.permit, req).await {
            Ok(ex) => self.handle_exchange(run, u, ex).await,
            Err(e) => self.handle_fetch_error(run, u, e).await,
        }
    }

    fn signals_for(&self, run: &Run<'_>, u: &LeasedUrl, ex: &Exchange) -> UseSignals {
        let mut s = header_signals(&ex.response_headers);
        if run
            .summaries
            .get(&u.url.origin())
            .is_some_and(|r| r.ai_reserved)
        {
            s.ai_bot_blocked = true;
        }
        s
    }

    fn retry_in(&self, d: Duration) -> UrlResult {
        UrlResult::Retry {
            at: self.p.clock.now() + chrono_of(d),
        }
    }

    fn failed(&self, class: ErrorClass, at: DateTime<Utc>) -> UrlResult {
        UrlResult::Failed {
            class,
            retry_at: at,
        }
    }

    async fn handle_exchange(&self, run: &mut Run<'_>, u: &LeasedUrl, ex: Exchange) -> Result<()> {
        run.latencies.push(ex.latency());
        let code = ex.status.as_u16();
        let now = self.p.clock.now();
        if code == 304 {
            let Some(last) = u.last_capture else {
                // Validators are only ever sent from a committed capture; a stray 304 is not a
                // body we have, so it is never recorded as one.
                run.result(
                    &u.url_key,
                    self.failed(ErrorClass::Transient, now + chrono::Duration::hours(1)),
                );
                return Ok(());
            };
            run.consecutive_5xx = 0;
            let next = self.next_fetch(u, None).await;
            let signals = self.signals_for(run, u, &ex);
            let mut meta = self.meta(run.permit, &u.url, u.url_key, Some(u), now);
            meta.status = Some(304);
            meta.media_type = u.expect.unwrap_or(MediaType::Html);
            meta.payload_hash = u.last_payload_hash;
            meta.revisit_of = Some(last);
            meta.validators = ex.validators();
            self.finish_signals(&mut meta, signals, now);
            meta.next_fetch_at = Some(next);
            let rec = SpoolRecord {
                meta,
                payload: SpoolPayload::Revisit(ex),
            };
            return self.spool_primary(run, u, rec, next).await;
        }
        if ex.status.is_success() {
            run.consecutive_5xx = 0;
            let next = self.next_fetch(u, None).await;
            let signals = self.signals_for(run, u, &ex);
            let prefix = &ex.body[..ex.body.len().min(1024)];
            let (media, _) = MediaType::detect(u.expect, ex.content_type(), prefix);
            let mut meta = self.meta(run.permit, &u.url, u.url_key, Some(u), now);
            meta.status = Some(code);
            meta.media_type = media;
            meta.validators = ex.validators();
            self.finish_signals(&mut meta, signals, now);
            meta.next_fetch_at = Some(next);
            let last = match (u.last_payload_hash, u.last_capture) {
                (Some(hash), Some(capture)) => Some(LastPayload { hash, capture }),
                _ => None,
            };
            let rec = dedupe_payload(
                SpoolRecord {
                    meta,
                    payload: SpoolPayload::Response(ex),
                },
                last.as_ref(),
            );
            return self.spool_primary(run, u, rec, next).await;
        }
        match code {
            404 | 410 => run.result(&u.url_key, UrlResult::Gone),
            429 | 503 => {
                let retry_after = vc_fetch::retry_after(&ex.response_headers, now);
                run.result(
                    &u.url_key,
                    self.retry_in(retry_after.unwrap_or(Duration::from_secs(3600))),
                );
                run.stop_with(KeySignal::RateLimited { retry_after });
            }
            500..=599 => {
                run.consecutive_5xx += 1;
                run.result(
                    &u.url_key,
                    self.failed(ErrorClass::Transient, now + chrono::Duration::hours(1)),
                );
                let consecutive = run.consecutive_5xx;
                run.signal = KeySignal::ServerErrors { consecutive };
                if consecutive >= self.p.cfg.frontier.server_error_threshold.max(1) {
                    run.stop = true;
                }
            }
            _ => {
                let next = self.next_fetch(u, None).await;
                run.result(&u.url_key, self.failed(ErrorClass::Permanent, next));
            }
        }
        Ok(())
    }

    /// Fills the use signals/policy and the ephemeral expiry (noarchive / TDM → `ephemeral/`).
    fn finish_signals(&self, meta: &mut CaptureMeta, s: UseSignals, now: DateTime<Utc>) {
        let up = self.p.policy.use_policy(&s);
        meta.use_signals = s;
        meta.use_policy_fetch = up;
        if up.raw == RawUse::Ephemeral || s.noarchive || s.tdm_reserved {
            meta.raw_expires_at =
                Some(now + chrono::Duration::days(i64::from(self.p.cfg.store.ephemeral_days)));
        }
    }

    /// Raw first: append + fsync, then `fetched_pending` (§17.2).
    async fn spool_primary(
        &self,
        run: &mut Run<'_>,
        u: &LeasedUrl,
        rec: SpoolRecord,
        next: DateTime<Utc>,
    ) -> Result<()> {
        self.append(run, rec).await?;
        self.mark_fetched(run, u, next).await
    }

    async fn mark_fetched(
        &self,
        run: &mut Run<'_>,
        u: &LeasedUrl,
        next: DateTime<Utc>,
    ) -> Result<()> {
        fail::fail_point!("fetcher::after_spool", |_| Err(Error::Storage(
            "failpoint fetcher::after_spool".into()
        )));
        self.p
            .frontier
            .mark_fetched(run.permit, &u.url_key, next)
            .await?;
        run.result(&u.url_key, UrlResult::Fetched);
        Ok(())
    }

    async fn handle_fetch_error(
        &self,
        run: &mut Run<'_>,
        u: &LeasedUrl,
        e: FetchError,
    ) -> Result<()> {
        let now = self.p.clock.now();
        match e {
            FetchError::Lease(l) => return Err(Error::LeaseLost(l)),
            FetchError::DeadlineExceeded => run.stop = true,
            FetchError::CrossKeyRedirect(target) => {
                // Never followed inline: the target goes to the frontier under its own key.
                if self.p.policy.denied(&target).is_none() {
                    let nu = NewUrl {
                        key: target.politeness_key(),
                        source_id: u.source_id.clone(),
                        discovered_from: Some(u.url_key),
                        priority: u.priority,
                        depth: u.depth,
                        expect: u.expect,
                        fetch_mode: match u.fetch_mode {
                            FetchMode::Render => FetchMode::Render,
                            _ => FetchMode::Http,
                        },
                        not_before: None,
                        url: target.clone(),
                    };
                    self.p.frontier.enqueue(&[nu]).await?;
                    run.report.enqueued.push(target);
                }
                let next = self.next_fetch(u, None).await;
                run.result(&u.url_key, self.failed(ErrorClass::Permanent, next));
            }
            FetchError::Challenge { status } => {
                let reason = if status == 401 || status == 403 {
                    ParkReason::Unauthorized { status }
                } else {
                    ParkReason::Challenge
                };
                tracing::info!(key = %run.permit.key(), status, "challenge detected: parking the key");
                run.stop_with(KeySignal::Park(reason));
            }
            FetchError::RobotsDisallowed(_)
            | FetchError::Denied(_)
            | FetchError::Ssrf(_)
            | FetchError::OutOfScope(_) => run.result(&u.url_key, UrlResult::Disallowed),
            FetchError::RobotsUnavailable(_) => {
                // Fail closed; the next claim lists the origin in `robots_due`.
                run.result(&u.url_key, self.retry_in(Duration::from_secs(600)));
            }
            FetchError::TooManyRedirects
            | FetchError::BodyTooLarge { .. }
            | FetchError::ContentTypeNotAllowed(_) => {
                let next = self.next_fetch(u, None).await;
                run.result(&u.url_key, self.failed(ErrorClass::Permanent, next));
            }
            other => run.result(
                &u.url_key,
                self.failed(other.class(), now + chrono::Duration::hours(1)),
            ),
        }
        Ok(())
    }

    async fn fetch_render(&self, run: &mut Run<'_>, u: &LeasedUrl) -> Result<()> {
        let Some(renderer) = &self.p.renderer else {
            return self.fetch_http(run, u).await;
        };
        let budget = vc_render::budget_from(&self.p.cfg.render);
        match renderer.render(run.permit, &u.url, budget).await {
            Ok(out) => {
                let now = self.p.clock.now();
                let mut doc_status = None;
                let mut signals = UseSignals::default();
                for ex in out.exchanges {
                    run.latencies.push(ex.latency());
                    let is_doc =
                        ex.url == u.url || ex.redirects.first().is_some_and(|h| h.url == u.url);
                    if is_doc {
                        doc_status = Some(ex.status.as_u16());
                        signals = self.signals_for(run, u, &ex);
                    }
                    let mut meta = self.meta(run.permit, &ex.url, ex.url.key(), Some(u), now);
                    meta.role = CaptureRole::Subresource;
                    meta.status = Some(ex.status.as_u16());
                    let prefix = &ex.body[..ex.body.len().min(512)];
                    meta.media_type = MediaType::detect(None, ex.content_type(), prefix).0;
                    meta.validators = ex.validators();
                    let rec = SpoolRecord {
                        meta,
                        payload: SpoolPayload::Response(ex),
                    };
                    self.append(run, rec).await?;
                }
                if !out.blocked.is_empty() {
                    tracing::debug!(blocked = out.blocked.len(), "render: sub-requests refused");
                }
                let next = self.next_fetch(u, None).await;
                let mut meta = self.meta(run.permit, &u.url, u.url_key, Some(u), now);
                meta.status = doc_status.or(Some(200));
                meta.media_type = MediaType::Html;
                self.finish_signals(&mut meta, signals, now);
                meta.next_fetch_at = Some(next);
                let rec = SpoolRecord {
                    meta,
                    payload: SpoolPayload::Resource {
                        content_type: "text/html; charset=utf-8".into(),
                        body: out.dom_html,
                    },
                };
                self.spool_primary(run, u, rec, next).await
            }
            Err(Error::Fetch(e)) => self.handle_fetch_error(run, u, e).await,
            Err(Error::LeaseLost(l)) => Err(Error::LeaseLost(l)),
            Err(e) => {
                tracing::info!(error_class = ?e.class(), "render failed");
                let at = self.p.clock.now() + chrono::Duration::hours(1);
                run.result(&u.url_key, self.failed(e.class(), at));
                Ok(())
            }
        }
    }

    async fn fetch_adapter(&self, run: &mut Run<'_>, u: &LeasedUrl, id: &str) -> Result<()> {
        let now = self.p.clock.now();
        let (Some(adapter), Some(source)) = (self.p.adapters.get(id).cloned(), self.source(u))
        else {
            tracing::warn!(
                adapter = id,
                "adapter row without a registered adapter or source"
            );
            run.result(
                &u.url_key,
                self.failed(ErrorClass::Permanent, now + chrono::Duration::days(1)),
            );
            return Ok(());
        };
        let mut sink = RecordingSink {
            inner: self.p.spool.clone(),
            seen: Vec::new(),
            leased: u.url_key,
            leased_body: None,
        };
        let res = {
            let mut actx = AdapterCtx {
                permit: run.permit,
                client: self.p.client.as_ref(),
                sink: &mut sink,
                frontier: self.p.frontier.as_ref(),
                source: &source,
                url: u,
                secrets: self.p.secrets.as_ref(),
                clock: self.p.clock.as_ref(),
            };
            adapter.run(&mut actx).await
        };
        for s in &sink.seen {
            run.report.spooled.push(s.id);
            if s.len > 0 {
                self.p.raw_bytes.add(s.len).await?;
            }
        }
        match res {
            Ok(rep) => {
                // An API's `Expires` (MET Norway's terms): never refetch before it.
                let expires = sink.seen.iter().filter_map(|s| s.expires).max();
                if id == FCDO_ADAPTER
                    && let Some(body) = &sink.leased_body
                {
                    self.fcdo_change_feed(u, body).await?;
                }
                let next = self.next_fetch(u, expires).await;
                if !sink
                    .seen
                    .iter()
                    .any(|s| s.primary && s.url_key == u.url_key)
                {
                    // The leased row itself needs a primary capture so that the commit returns
                    // it to `due` (§17.2).
                    let mut meta = self.meta(run.permit, &u.url, u.url_key, Some(u), now);
                    meta.media_type = MediaType::Json;
                    meta.next_fetch_at = Some(next);
                    let fields = serde_json::json!({
                        "type": "adapter_run",
                        "adapter": id,
                        "requests": rep.requests,
                        "records": rep.records,
                        "bytes": rep.bytes,
                        "enqueued": rep.enqueued,
                        "complete": rep.complete,
                    });
                    let rec = SpoolRecord {
                        meta,
                        payload: SpoolPayload::Metadata { fields },
                    };
                    self.append(run, rec).await?;
                }
                self.mark_fetched(run, u, next).await
            }
            Err(Error::Fetch(e)) => self.handle_fetch_error(run, u, e).await,
            Err(Error::LeaseLost(l)) => Err(Error::LeaseLost(l)),
            Err(e) => {
                tracing::info!(adapter = id, error_class = ?e.class(), "adapter run failed");
                run.result(
                    &u.url_key,
                    self.failed(e.class(), now + chrono::Duration::hours(1)),
                );
                Ok(())
            }
        }
    }
}

/// The FCDO adapter id (its Atom change feed drives [`Frontier::reschedule`]).
pub const FCDO_ADAPTER: &str = "fcdo_content_api";

impl Fetcher {
    /// FCDO change feed → [`Frontier::reschedule`]: every country whose feed entry is newer
    /// than its last committed capture (or never captured) is made due now. The adapter itself
    /// only enqueues (`ON CONFLICT DO NOTHING`), which cannot move an existing row.
    /// Politeness is untouched: the key's `next_allowed_at` (incl. a 429 `Retry-After`
    /// penalty), parking and budget still gate the claim.
    async fn fcdo_change_feed(&self, u: &LeasedUrl, body: &[u8]) -> Result<u64> {
        use vc_sources::adapters::fcdo_content_api::{FEED_PATH, INDEX_PATH, parse_changes};
        if u.url.as_url().path().trim_end_matches('/') != FEED_PATH {
            return Ok(0);
        }
        let changes = match parse_changes(body) {
            Ok(c) => c,
            Err(e) => {
                tracing::info!(error_class = ?e.class(), "fcdo change feed did not parse");
                return Ok(0);
            }
        };
        let now = self.p.clock.now();
        let mut n = 0;
        for c in changes {
            let Ok(target) = u.url.join(&format!("{INDEX_PATH}/{}", c.slug)) else {
                continue;
            };
            if target.host() != u.url.host() || target.port() != u.url.port() {
                continue;
            }
            let updated = DateTime::parse_from_rfc3339(c.updated.trim())
                .ok()
                .map(|t| t.with_timezone(&Utc));
            if let (Some(pool), Some(updated)) = (&self.p.pool, updated) {
                let last: Option<(Option<DateTime<Utc>>,)> =
                    sqlx::query_as("SELECT last_captured_at FROM url_state WHERE url_key = $1")
                        .bind(target.key().as_bytes().as_slice())
                        .fetch_optional(pool)
                        .await
                        .map_err(|e| Error::Db(e.to_string()))?;
                if let Some((Some(captured),)) = last
                    && captured >= updated
                {
                    continue; // already have this version
                }
            }
            if self.p.frontier.reschedule(&target.key(), now).await? {
                n += 1;
            }
        }
        if n > 0 {
            tracing::info!(rescheduled = n, "fcdo change feed: countries rescheduled");
        }
        Ok(n)
    }
}

/// One record an adapter appended.
#[derive(Debug, Clone)]
struct Seen {
    id: CaptureId,
    url_key: UrlKey,
    primary: bool,
    len: u64,
    expires: Option<DateTime<Utc>>,
}

/// The spool, observed: which captures an adapter wrote, the `Expires` they carried, and the
/// body of the leased URL's own response.
struct RecordingSink {
    inner: Spool,
    seen: Vec<Seen>,
    leased: UrlKey,
    leased_body: Option<bytes::Bytes>,
}

/// The `Expires` header of a response, if it parses.
pub fn expires_of(ex: &Exchange) -> Option<DateTime<Utc>> {
    let v = ex
        .response_headers
        .get(http::header::EXPIRES)?
        .to_str()
        .ok()?;
    let t = httpdate::parse_http_date(v.trim()).ok()?;
    Some(DateTime::<Utc>::from(t))
}

#[async_trait]
impl CaptureSink for RecordingSink {
    async fn append(&mut self, r: SpoolRecord) -> Result<CaptureId> {
        let (len, expires) = match &r.payload {
            SpoolPayload::Response(ex) => (ex.body.len() as u64, expires_of(ex)),
            SpoolPayload::Revisit(ex) => (0, expires_of(ex)),
            SpoolPayload::Resource { body, .. } => (body.len() as u64, None),
            SpoolPayload::Metadata { .. } => (0, None),
        };
        let url_key = r.meta.url_key;
        let primary = r.meta.role == CaptureRole::Primary;
        if primary
            && url_key == self.leased
            && let SpoolPayload::Response(ex) = &r.payload
            && ex.status.is_success()
        {
            self.leased_body = Some(ex.body.clone());
        }
        let id = self.inner.append(r).await?;
        self.seen.push(Seen {
            id,
            url_key,
            primary,
            len,
            expires,
        });
        Ok(id)
    }
}
