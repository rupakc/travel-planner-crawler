//! # vc-frontier
//!
//! [`PgFrontier`]: the Postgres frontier and the politeness core (spec §5.2, §5.3, §17.2).
//!
//! * **Claims** take one ready key with `FOR UPDATE SKIP LOCKED` (not parked, under budget,
//!   `next_allowed_at ≤ now()`, no live claim), give it a new epoch from `epoch_seq` and lease up
//!   to [`vc_core::MAX_URLS_PER_CLAIM`] due URLs in the same transaction. The key row is the
//!   serialisation point: every claim moves `next_allowed_at` to `deadline + delay`, so a holder
//!   that dies mid-claim still leaves `max(deadline, last request end + delay)` before the next.
//! * **Fencing:** every write by a holder is checked against `claims.epoch`; DB `now()` is the
//!   only clock. Any failure to verify a lease is `LeaseLost` (fail closed).
//! * **Rate:** one token bucket row (`rate_bucket 'global'`) shared by every process, taken
//!   once per request together with the lease check in one statement.
//! * **Completion** releases the lease (`deadline = least(deadline, now())`) and schedules the
//!   key at `now() + max(delay, back-off)`; delay = `max(Crawl-delay, min delay, 10 × EWMA,
//!   penalty) ≤ 1 h` ([`schedule`]). Parks follow the ladder 24 h → ×2 → 30 d.
//! * **Enqueue** is batched (`ON CONFLICT DO NOTHING`, sorted to avoid deadlocks).
//! * **Recrawl:** the Cho & Garcia-Molina estimator ([`estimator`]).
//!
//! This is the only crate (besides testkit) that may call `ClaimPermit::mint`. Lock order, to
//! stay deadlock-free: `politeness_keys` row → `claims` row → `url_state` rows.

pub mod estimator;
pub mod schedule;

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use vc_config::FrontierConfig;
use vc_core::frontier::{
    Claim, ClaimPermit, Frontier, KeyOutcome, LeaseCheck, LeasedUrl, NewUrl, ParkReason,
    PermitScope, UrlResult, Validators,
};
use vc_core::media::{FetchMode, MediaType};
use vc_core::policy::{RobotsEntry, RobotsStatus};
use vc_core::{
    CanonicalUrl, CaptureId, CaptureRef, DbTime, Epoch, Error, LeaseLost, LeaseLostReason,
    MAX_URLS_PER_CLAIM, OriginKey, PayloadHash, PolitenessKey, Result, SourceId, UrlKey,
};

use crate::schedule::{KeyRow, next_key_state, park_for};

/// How many candidate keys one `claim` call tries before giving up.
const CLAIM_ATTEMPTS: usize = 16;
/// robots.txt bodies are stored up to this size (spec §5.2).
const ROBOTS_MAX_BYTES: usize = 500 * 1024;
/// 404/410 in a row before a URL is `gone`.
const GONE_AFTER: i16 = 2;

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn ms_i32(ms: i64) -> i32 {
    i32::try_from(ms.clamp(0, i64::from(i32::MAX))).unwrap_or(i32::MAX)
}

fn robots_state(s: RobotsStatus) -> &'static str {
    match s {
        RobotsStatus::Ok => "ok",
        RobotsStatus::NotFound => "not_found",
        RobotsStatus::Unauthorized => "unauthorized",
        RobotsStatus::Challenge => "challenge",
        RobotsStatus::Unavailable => "unavailable",
    }
}

fn parse_robots_state(s: &str) -> Option<RobotsStatus> {
    Some(match s {
        "ok" => RobotsStatus::Ok,
        "not_found" => RobotsStatus::NotFound,
        "unauthorized" => RobotsStatus::Unauthorized,
        "challenge" => RobotsStatus::Challenge,
        "unavailable" => RobotsStatus::Unavailable,
        _ => return None,
    })
}

/// The Postgres frontier.
#[derive(Debug, Clone)]
pub struct PgFrontier {
    /// Pool.
    pub pool: PgPool,
    /// Settings.
    pub config: FrontierConfig,
    holder: String,
}

/// Picks one ready key. `FOR UPDATE OF k SKIP LOCKED`: concurrent claimers skip each other's
/// candidates, and a row updated by a committed claim is re-checked (its `next_allowed_at` has
/// moved past `now()`), so two claimers can never take the same key.
const PICK_KEY: &str = "
SELECT k.key, k.delay_ms, k.min_delay_ms
FROM politeness_keys k
WHERE k.shard = ANY($1)
  AND k.enabled
  AND k.key <> ALL($2)
  AND k.next_allowed_at <= now()
  AND (k.parked_until IS NULL OR k.parked_until <= now())
  AND (k.budget_date < (now() AT TIME ZONE 'UTC')::date OR k.used_today < k.budget_day)
  AND EXISTS (SELECT 1 FROM url_state u
              WHERE u.key = k.key
                AND ((u.state = 'due' AND u.next_fetch_at <= now()) OR u.state = 'leased'))
ORDER BY k.next_allowed_at, k.key
LIMIT 1
FOR UPDATE OF k SKIP LOCKED";

const LEASE_URLS: &str = "
WITH pick AS (
    SELECT url_key FROM url_state
    WHERE key = $1 AND state = 'due' AND next_fetch_at <= now()
    ORDER BY priority DESC, next_fetch_at, url_key
    LIMIT $2
    FOR UPDATE
)
UPDATE url_state u SET state = 'leased', lease_epoch = $3
FROM pick
WHERE u.url_key = pick.url_key
RETURNING u.url_key, u.priority, u.next_fetch_at, u.depth, u.expect, u.fetch_mode, u.source_id,
          u.etag, u.last_modified, u.payload_hash, u.last_capture_id, u.body_captured_at";

/// Lease check + one global token + the key's daily request count, in one statement.
const TAKE_TOKEN: &str = "
WITH lease AS (
    SELECT 1 FROM claims
    WHERE key = $1 AND epoch = $2 AND completed_at IS NULL AND deadline > now()
), tok AS (
    UPDATE rate_bucket
    SET tokens = least($3, tokens + greatest(0, extract(epoch FROM clock_timestamp() - updated_at)::float8) * $4) - 1,
        capacity = $3,
        refill_per_s = $4,
        updated_at = clock_timestamp()
    WHERE name = 'global'
      AND EXISTS (SELECT 1 FROM lease)
      AND least($3, tokens + greatest(0, extract(epoch FROM clock_timestamp() - updated_at)::float8) * $4) >= 1
    RETURNING 1
), used AS (
    UPDATE politeness_keys
    SET used_today = CASE WHEN budget_date < (now() AT TIME ZONE 'UTC')::date THEN 1
                          ELSE used_today + 1 END,
        budget_date = (now() AT TIME ZONE 'UTC')::date
    WHERE key = $1 AND EXISTS (SELECT 1 FROM tok)
    RETURNING 1
)
SELECT EXISTS (SELECT 1 FROM tok)";

const ENQUEUE: &str = "
WITH t AS (
    SELECT * FROM unnest($1::bytea[], $2::text[], $3::text[], $4::text[], $5::bytea[],
                         $6::int2[], $7::real[], $8::timestamptz[], $9::text[], $10::text[])
           AS t(url_key, url, key, source_id, discovered_from, depth, priority, not_before,
                fetch_mode, expect)
), ins AS (
    INSERT INTO urls (url_key, url, key, source_id, discovered_from, depth)
    SELECT url_key, url, key, source_id, discovered_from, depth FROM t
    ON CONFLICT (url_key) DO NOTHING
    RETURNING url_key
)
INSERT INTO url_state (url_key, key, priority, next_fetch_at, fetch_mode, expect, source_id, depth)
SELECT t.url_key, t.key, t.priority, coalesce(t.not_before, now()), t.fetch_mode, t.expect,
       t.source_id, t.depth
FROM t JOIN ins ON ins.url_key = t.url_key
ON CONFLICT (url_key) DO NOTHING";

/// The url keys of a batch that a standing exclusion names ($1 keys, $2 urls, $3 hosts,
/// $4 source ids; see `PgFrontier::tombstoned`).
const ENQUEUE_TOMBSTONED: &str = "
SELECT t.url_key
FROM unnest($1::bytea[], $2::text[], $3::text[], $4::text[]) AS t(url_key, url, host, source_id)
WHERE EXISTS (
    SELECT 1 FROM tombstones x
    WHERE (x.selector_type = 'url' AND x.selector = encode(t.url_key, 'hex'))
       OR (x.selector_type = 'url_prefix' AND starts_with(t.url, x.selector))
       OR (x.selector_type = 'host' AND x.selector = t.host)
       OR (x.selector_type = 'source' AND x.selector = t.source_id))";

type LeasedRow = (
    Vec<u8>,
    f32,
    DateTime<Utc>,
    i16,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<Vec<u8>>,
    Option<String>,
    Option<DateTime<Utc>>,
);

type RobotsRow = (
    String,
    Option<i16>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

type KeyColumns = (i32, Option<i32>, Option<i32>, i32, i16, i16, bool);

impl PgFrontier {
    /// Borrows a third-party key for one render sub-request made under another key's permit:
    /// granted only when that key is ready (`next_allowed_at` passed, not parked, no claim in
    /// flight), and then its `next_allowed_at` moves on by its own delay, exactly as if it had
    /// been claimed. A key never seen before is created with the default limits. Shared by
    /// every process through Postgres. `false`: skip the sub-resource.
    pub async fn borrow_subresource(&self, key: &PolitenessKey) -> Result<bool> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "INSERT INTO politeness_keys (key, min_delay_ms, delay_ms, budget_day)
             VALUES ($1, $2, $2, $3) ON CONFLICT (key) DO NOTHING",
        )
        .bind(key.as_str())
        .bind(self.min_delay_ms())
        .bind(i32::try_from(self.config.daily_budget).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let granted = sqlx::query(
            "UPDATE politeness_keys k
             SET next_allowed_at = now() + make_interval(
                     secs => greatest(k.delay_ms, k.min_delay_ms, $2)::float8 / 1000.0),
                 updated_at = now()
             WHERE k.key = $1
               AND k.enabled
               AND k.next_allowed_at <= now()
               AND (k.parked_until IS NULL OR k.parked_until <= now())
               AND NOT EXISTS (SELECT 1 FROM claims c
                               WHERE c.key = k.key AND c.completed_at IS NULL
                                 AND c.deadline > now())",
        )
        .bind(key.as_str())
        .bind(self.min_delay_ms())
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected()
            == 1;
        tx.commit().await.map_err(db)?;
        Ok(granted)
    }

    /// Ends a borrowed sub-request ([`PgFrontier::borrow_subresource`]): the key's
    /// `next_allowed_at` is at least its delay after the request **ended** (a slow response
    /// must not shorten the gap). Never moves it earlier.
    pub async fn subresource_done(&self, key: &PolitenessKey) -> Result<()> {
        sqlx::query(
            "UPDATE politeness_keys k
             SET next_allowed_at = greatest(k.next_allowed_at, now() + make_interval(
                     secs => greatest(k.delay_ms, k.min_delay_ms, $2)::float8 / 1000.0))
             WHERE k.key = $1",
        )
        .bind(key.as_str())
        .bind(self.min_delay_ms())
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    /// Returns `fetched_pending` rows pending for longer than `older_than` to `due` (now):
    /// their spool frame is lost, so no commit will ever release them. A late commit is still
    /// applied (validators, capture row) by `vc_commit_segment`; it simply finds nothing to
    /// flip. Idempotent; any number of processes may run it.
    pub async fn sweep_stale_pending(&self, older_than: Duration) -> Result<u64> {
        let secs = i64::try_from(older_than.as_secs()).unwrap_or(i64::MAX);
        let n = sqlx::query(
            "UPDATE url_state SET state = 'due', lease_epoch = NULL, pending_since = NULL,
                                  next_fetch_at = least(next_fetch_at, now())
             WHERE state = 'fetched_pending'
               AND coalesce(pending_since, '-infinity') < now() - make_interval(secs => $1)",
        )
        .bind(secs as f64)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Db(e.to_string()))?
        .rows_affected();
        if n > 0 {
            tracing::warn!(
                urls = n,
                "frontier: stale fetched_pending rows returned to due"
            );
        }
        Ok(n)
    }

    /// Creates a frontier.
    pub fn new(pool: PgPool, config: FrontierConfig) -> Self {
        let holder = config.holder.clone().unwrap_or_else(|| {
            let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "vcrawl".into());
            format!("{host}:{}", std::process::id())
        });
        PgFrontier {
            pool,
            config,
            holder,
        }
    }

    fn min_delay_ms(&self) -> i32 {
        ms_i32(i64::try_from(self.config.min_delay_s.saturating_mul(1000)).unwrap_or(i64::MAX))
    }

    /// Why a permit's lease is not usable (for error reporting after a fenced write matched no
    /// row). Never returns "ok": if the lease looks fine now, the write raced a newer claim.
    async fn lease_lost(&self, p: &ClaimPermit) -> LeaseLost {
        match self.check(p).await {
            Err(e) => e,
            Ok(()) => p.lease_lost(LeaseLostReason::Superseded),
        }
    }

    /// The next recrawl time for a URL from its change history (Cho & Garcia-Molina, bounded by
    /// the source cadence `[min, max]`). Records the interval in `url_state.est_interval_s`.
    /// The fetcher passes the result to [`Frontier::mark_fetched`].
    pub async fn estimate_next_fetch(
        &self,
        url: &UrlKey,
        min: Duration,
        max: Duration,
    ) -> Result<DateTime<Utc>> {
        let row: Option<(i32, i32, Option<i32>)> = sqlx::query_as(
            "SELECT fetch_count, change_count, est_interval_s FROM url_state WHERE url_key = $1",
        )
        .bind(url.as_bytes().as_slice())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        let Some((n, x, est)) = row else {
            return Err(Error::NotFound(format!("url {url}")));
        };
        let interval = est
            .and_then(|s| u64::try_from(s).ok())
            .map(Duration::from_secs)
            .unwrap_or(min);
        let next = estimator::next_interval(
            u32::try_from(n).unwrap_or(0),
            u32::try_from(x).unwrap_or(0),
            interval,
            min,
            max,
        );
        let est_s = i32::try_from(next.as_secs()).unwrap_or(i32::MAX);
        let (at,): (DateTime<Utc>,) = sqlx::query_as(
            "UPDATE url_state SET est_interval_s = $2 WHERE url_key = $1 \
             RETURNING now() + make_interval(secs => $3)",
        )
        .bind(url.as_bytes().as_slice())
        .bind(est_s)
        .bind(secs(next))
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(at)
    }

    /// One claim attempt on the next candidate key. `Ok(Some(None))`: no candidate at all;
    /// `Ok(None)`: this candidate did not work out, try another.
    async fn try_claim_key(
        &self,
        shards: &[i16],
        max_urls: usize,
        excluded: &mut Vec<String>,
    ) -> Result<Option<Option<Claim>>> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let picked: Option<(String, i32, i32)> = sqlx::query_as(PICK_KEY)
            .bind(shards)
            .bind(&*excluded)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        let Some((key, delay_ms, min_delay_ms)) = picked else {
            tx.rollback().await.map_err(db)?;
            return Ok(Some(None));
        };
        excluded.push(key.clone());

        // A live claim (e.g. after `extend`) blocks the key regardless of next_allowed_at.
        let live: Option<(bool,)> = sqlx::query_as(
            "SELECT completed_at IS NULL AND deadline > now() FROM claims WHERE key = $1 FOR UPDATE",
        )
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        if live == Some((true,)) {
            tx.rollback().await.map_err(db)?;
            return Ok(None);
        }

        let (epoch,): (i64,) = sqlx::query_as("SELECT nextval('epoch_seq')")
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        // No live claim: every `leased` URL of the key belongs to a dead epoch (§17.2).
        // `fetched_pending` URLs are never touched here.
        sqlx::query(
            "UPDATE url_state SET state = 'due', lease_epoch = NULL WHERE key = $1 AND state = 'leased'",
        )
        .bind(&key)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let mut rows: Vec<LeasedRow> = sqlx::query_as(LEASE_URLS)
            .bind(&key)
            .bind(i64::try_from(max_urls).unwrap_or(1))
            .bind(epoch)
            .fetch_all(&mut *tx)
            .await
            .map_err(db)?;
        if rows.is_empty() {
            // Keep the dead-lease reset; nothing to claim on this key right now.
            tx.commit().await.map_err(db)?;
            return Ok(None);
        }
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));
        let keys: Vec<Vec<u8>> = rows.iter().map(|r| r.0.clone()).collect();
        let texts: Vec<(Vec<u8>, String)> =
            sqlx::query_as("SELECT url_key, url FROM urls WHERE url_key = ANY($1)")
                .bind(&keys)
                .fetch_all(&mut *tx)
                .await
                .map_err(db)?;
        let n = u32::try_from(rows.len()).unwrap_or(1);
        let budget = Duration::from_secs(self.config.per_url_budget_s).saturating_mul(n);
        let (deadline,): (DateTime<Utc>,) = sqlx::query_as(
            "INSERT INTO claims (key, holder, epoch, claimed_at, deadline, completed_at)
             VALUES ($1, $2, $3, now(), now() + make_interval(secs => $4), NULL)
             ON CONFLICT (key) DO UPDATE
             SET holder = EXCLUDED.holder, epoch = EXCLUDED.epoch, claimed_at = EXCLUDED.claimed_at,
                 deadline = EXCLUDED.deadline, completed_at = NULL
             RETURNING deadline",
        )
        .bind(&key)
        .bind(&self.holder)
        .bind(epoch)
        .bind(secs(budget))
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let max_ms =
            i64::try_from(self.config.max_delay_s.saturating_mul(1000)).unwrap_or(i64::MAX);
        let delay_ms = i64::from(delay_ms)
            .max(i64::from(min_delay_ms))
            .max(i64::from(self.min_delay_ms()))
            .min(max_ms);
        let delay = Duration::from_millis(u64::try_from(delay_ms).unwrap_or(0));
        // Until completion the key is held for the whole lease plus one delay, so a holder that
        // dies right after a request still leaves `last request end + delay` before the next.
        sqlx::query(
            "UPDATE politeness_keys
             SET next_allowed_at = $2 + make_interval(secs => $3),
                 used_today = CASE WHEN budget_date < (now() AT TIME ZONE 'UTC')::date THEN 0
                                   ELSE used_today END,
                 budget_date = (now() AT TIME ZONE 'UTC')::date,
                 updated_at = now()
             WHERE key = $1",
        )
        .bind(&key)
        .bind(deadline)
        .bind(secs(delay))
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        let pk = PolitenessKey(key.clone());
        let mut urls = Vec::with_capacity(rows.len());
        let mut origins: Vec<OriginKey> = Vec::new();
        let mut extra_hosts: Vec<String> = Vec::new();
        for r in rows {
            let Some((_, text)) = texts.iter().find(|(k, _)| *k == r.0) else {
                return Err(Error::Invalid(format!(
                    "url_state row without urls row: {}",
                    hex(&r.0)
                )));
            };
            let url = CanonicalUrl::parse(text).map_err(|e| Error::Invalid(e.to_string()))?;
            let origin = url.origin();
            if !origins.contains(&origin) {
                origins.push(origin);
            }
            if url.politeness_key() != pk && !extra_hosts.iter().any(|h| h == url.host()) {
                extra_hosts.push(url.host().to_string());
            }
            urls.push(leased_url(r, url)?);
        }
        let origin_strs: Vec<String> = origins.iter().map(|o| o.0.clone()).collect();
        let fresh: Vec<(String,)> = sqlx::query_as(
            "SELECT origin FROM origins WHERE origin = ANY($1) AND robots_expires_at > now()",
        )
        .bind(&origin_strs)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        let fresh: HashSet<String> = fresh.into_iter().map(|(o,)| o).collect();
        let robots_due: Vec<OriginKey> = origins
            .into_iter()
            .filter(|o| !fresh.contains(&o.0))
            .collect();
        tx.commit().await.map_err(db)?;

        tracing::debug!(key = %pk, epoch, urls = urls.len(), "claimed");
        let scope = PermitScope {
            key: pk.clone(),
            extra_hosts,
        };
        #[allow(clippy::disallowed_methods)] // vc-frontier is the one place permits are minted
        let permit = ClaimPermit::mint(pk, Epoch(epoch), DbTime(deadline), Arc::new(scope), delay);
        Ok(Some(Some(Claim {
            permit,
            urls,
            robots_due,
        })))
    }

    /// The completion transaction. `Ok(false)`: the epoch is not current (stale holder).
    async fn complete_tx(
        &self,
        p: &ClaimPermit,
        outcome: &KeyOutcome,
    ) -> std::result::Result<bool, sqlx::Error> {
        let key = p.key().as_str();
        let epoch = p.epoch().0;
        let mut tx = self.pool.begin().await?;
        let row: Option<KeyColumns> = sqlx::query_as(
            "SELECT min_delay_ms, crawl_delay_ms, ewma_latency_ms, penalty_ms, consecutive_5xx,
                    park_level, coalesce(parked_until > now(), false)
             FROM politeness_keys WHERE key = $1 FOR UPDATE",
        )
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?;
        let claim: Option<(i64, bool)> = sqlx::query_as(
            "SELECT epoch, completed_at IS NULL FROM claims WHERE key = $1 FOR UPDATE",
        )
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?;
        let (Some(row), Some((current, true))) = (row, claim) else {
            tx.rollback().await?;
            return Ok(false);
        };
        if current != epoch {
            tx.rollback().await?;
            return Ok(false);
        }

        let mut fetched = Vec::new();
        let mut retry_keys = Vec::new();
        let mut retry_at = Vec::new();
        let mut gone = Vec::new();
        let mut disallowed = Vec::new();
        for o in &outcome.urls {
            let k = o.url_key.as_bytes().to_vec();
            match &o.result {
                UrlResult::Fetched => fetched.push(k),
                UrlResult::NotAttempted => {}
                UrlResult::Retry { at } | UrlResult::Failed { retry_at: at, .. } => {
                    retry_keys.push(k);
                    retry_at.push(*at);
                }
                UrlResult::Gone => gone.push(k),
                UrlResult::Disallowed => disallowed.push(k),
            }
        }
        sqlx::query(
            "UPDATE url_state SET gone_count = 0
             WHERE url_key = ANY($1) AND key = $2 AND lease_epoch = $3 AND state = 'fetched_pending'",
        )
        .bind(&fetched)
        .bind(key)
        .bind(epoch)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE url_state u SET state = 'due', lease_epoch = NULL, next_fetch_at = v.at
             FROM unnest($1::bytea[], $2::timestamptz[]) AS v(k, at)
             WHERE u.url_key = v.k AND u.key = $3 AND u.lease_epoch = $4 AND u.state = 'leased'",
        )
        .bind(&retry_keys)
        .bind(&retry_at)
        .bind(key)
        .bind(epoch)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE url_state SET
                 gone_count = gone_count + 1,
                 state = CASE WHEN state <> 'leased' THEN state
                              WHEN gone_count + 1 >= $4 THEN 'gone' ELSE 'due' END,
                 next_fetch_at = CASE WHEN state = 'leased' THEN now() + interval '1 day'
                                      ELSE next_fetch_at END,
                 lease_epoch = CASE WHEN state = 'leased' THEN NULL ELSE lease_epoch END
             WHERE url_key = ANY($1) AND key = $2 AND lease_epoch = $3
               AND state IN ('leased', 'fetched_pending')",
        )
        .bind(&gone)
        .bind(key)
        .bind(epoch)
        .bind(GONE_AFTER)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE url_state SET state = 'disabled', lease_epoch = NULL
             WHERE url_key = ANY($1) AND key = $2 AND lease_epoch = $3 AND state = 'leased'",
        )
        .bind(&disallowed)
        .bind(key)
        .bind(epoch)
        .execute(&mut *tx)
        .await?;
        // Everything else this epoch still leases goes back to due unchanged.
        sqlx::query(
            "UPDATE url_state SET state = 'due', lease_epoch = NULL
             WHERE key = $1 AND lease_epoch = $2 AND state = 'leased'",
        )
        .bind(key)
        .bind(epoch)
        .execute(&mut *tx)
        .await?;

        let kr = KeyRow {
            min_delay_ms: i64::from(row.0),
            crawl_delay_ms: row.1.map(i64::from),
            ewma_latency_ms: row.2.map(i64::from),
            penalty_ms: i64::from(row.3),
            consecutive_5xx: i64::from(row.4),
            park_level: i64::from(row.5),
            parked: row.6,
        };
        let next = next_key_state(&self.config, &kr, outcome);
        let (park_s, park_level, park_reason) = match &next.park {
            Some((pk, reason)) => (
                Some(secs(pk.duration)),
                Some(i16::try_from(pk.level).unwrap_or(i16::MAX)),
                Some(serde_json::to_string(reason).unwrap_or_default()),
            ),
            None => (None, None, None),
        };
        sqlx::query(
            "UPDATE politeness_keys SET
                 crawl_delay_ms = $2, ewma_latency_ms = $3, penalty_ms = $4, consecutive_5xx = $5,
                 delay_ms = $6,
                 next_allowed_at = now() + make_interval(secs => $7),
                 parked_until = CASE WHEN $8::float8 IS NULL THEN parked_until
                                     ELSE now() + make_interval(secs => $8) END,
                 park_level = coalesce($9, park_level),
                 park_reason = coalesce($10, park_reason),
                 updated_at = now()
             WHERE key = $1",
        )
        .bind(key)
        .bind(next.crawl_delay_ms.map(ms_i32))
        .bind(next.ewma_latency_ms.map(ms_i32))
        .bind(ms_i32(next.penalty_ms))
        .bind(i16::try_from(next.consecutive_5xx).unwrap_or(i16::MAX))
        .bind(ms_i32(next.delay_ms))
        .bind(next.wait_ms as f64 / 1000.0)
        .bind(park_s)
        .bind(park_level)
        .bind(park_reason)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE claims SET completed_at = now(), deadline = least(deadline, now())
             WHERE key = $1 AND epoch = $2",
        )
        .bind(key)
        .bind(epoch)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The URLs of `chunk` that a standing exclusion (`tombstones`: `url`, `url_prefix`,
    /// `host`, `source`) names, so a URL first discovered under a purged host or prefix after
    /// the purge never enters the crawl.
    async fn tombstoned(&self, chunk: &[&NewUrl]) -> Result<HashSet<UrlKey>> {
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(ENQUEUE_TOMBSTONED)
            .bind(
                chunk
                    .iter()
                    .map(|n| n.url.key().0.to_vec())
                    .collect::<Vec<_>>(),
            )
            .bind(
                chunk
                    .iter()
                    .map(|n| n.url.as_str().to_string())
                    .collect::<Vec<_>>(),
            )
            .bind(
                chunk
                    .iter()
                    .map(|n| n.url.host().trim_end_matches('.').to_ascii_lowercase())
                    .collect::<Vec<_>>(),
            )
            .bind(
                chunk
                    .iter()
                    .map(|n| n.source_id.as_ref().map(|s| s.0.clone()))
                    .collect::<Vec<_>>(),
            )
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        Ok(rows
            .into_iter()
            .filter_map(|k| UrlKey::from_slice(&k).ok())
            .collect())
    }

    async fn enqueue_chunk(&self, chunk: &[&NewUrl]) -> Result<u64> {
        let excluded = self.tombstoned(chunk).await?;
        let kept: Vec<&NewUrl>;
        let chunk = if excluded.is_empty() {
            chunk
        } else {
            kept = chunk
                .iter()
                .copied()
                .filter(|n| !excluded.contains(&n.url.key()))
                .collect();
            if kept.is_empty() {
                return Ok(0);
            }
            &kept[..]
        };
        let mut keys: Vec<String> = chunk.iter().map(|n| n.key.0.clone()).collect();
        keys.sort();
        keys.dedup();
        let mut origins: Vec<(String, String)> = chunk
            .iter()
            .map(|n| (n.url.origin().0, n.key.0.clone()))
            .collect();
        origins.sort();
        origins.dedup_by(|a, b| a.0 == b.0);

        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "INSERT INTO politeness_keys (key, min_delay_ms, delay_ms, budget_day)
             SELECT k, $2, $2, $3 FROM unnest($1::text[]) AS k
             ON CONFLICT (key) DO NOTHING",
        )
        .bind(&keys)
        .bind(self.min_delay_ms())
        .bind(i32::try_from(self.config.daily_budget).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let (o, ok): (Vec<String>, Vec<String>) = origins.into_iter().unzip();
        sqlx::query(
            "INSERT INTO origins (origin, key) SELECT * FROM unnest($1::text[], $2::text[])
             ON CONFLICT (origin) DO NOTHING",
        )
        .bind(&o)
        .bind(&ok)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let r = sqlx::query(ENQUEUE)
            .bind(
                chunk
                    .iter()
                    .map(|n| n.url.key().0.to_vec())
                    .collect::<Vec<_>>(),
            )
            .bind(
                chunk
                    .iter()
                    .map(|n| n.url.as_str().to_string())
                    .collect::<Vec<_>>(),
            )
            .bind(chunk.iter().map(|n| n.key.0.clone()).collect::<Vec<_>>())
            .bind(
                chunk
                    .iter()
                    .map(|n| n.source_id.as_ref().map(|s| s.0.clone()))
                    .collect::<Vec<_>>(),
            )
            .bind(
                chunk
                    .iter()
                    .map(|n| n.discovered_from.map(|k| k.0.to_vec()))
                    .collect::<Vec<_>>(),
            )
            .bind(chunk.iter().map(|n| i16::from(n.depth)).collect::<Vec<_>>())
            .bind(chunk.iter().map(|n| n.priority).collect::<Vec<_>>())
            .bind(chunk.iter().map(|n| n.not_before).collect::<Vec<_>>())
            .bind(
                chunk
                    .iter()
                    .map(|n| n.fetch_mode.to_string())
                    .collect::<Vec<_>>(),
            )
            .bind(
                chunk
                    .iter()
                    .map(|n| n.expect.map(|m| m.as_str().to_string()))
                    .collect::<Vec<_>>(),
            )
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(r.rows_affected())
    }
}

/// A lease that could not be verified because of a database error (fail closed). The error is
/// logged (its message only; never row content).
fn unverifiable(p: &ClaimPermit, e: &sqlx::Error) -> LeaseLost {
    tracing::warn!(key = %p.key(), epoch = p.epoch().0, error = %e, "lease unverifiable");
    p.lease_lost(LeaseLostReason::Unverifiable)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn leased_url(r: LeasedRow, url: CanonicalUrl) -> Result<LeasedUrl> {
    let (k, priority, _, depth, expect, mode, source, etag, lm, ph, cap_id, cap_at) = r;
    let last_capture = match (cap_id, cap_at) {
        (Some(id), Some(at)) => Some(CaptureRef {
            capture_id: CaptureId::from_str(&id)?,
            captured_at: at,
        }),
        _ => None,
    };
    let last_payload_hash = match ph {
        Some(b) => {
            Some(PayloadHash(b.as_slice().try_into().map_err(|_| {
                Error::Invalid("payload hash must be 32 bytes".into())
            })?))
        }
        None => None,
    };
    Ok(LeasedUrl {
        url_key: UrlKey::from_slice(&k)?,
        url,
        source_id: source.map(SourceId),
        priority,
        depth: u8::try_from(depth.clamp(0, 255)).unwrap_or(u8::MAX),
        expect: expect.as_deref().map(MediaType::from_str).transpose()?,
        fetch_mode: FetchMode::from_str(&mode)?,
        validators: Validators {
            etag,
            last_modified: lm,
        },
        last_payload_hash,
        last_capture,
    })
}

#[async_trait]
impl LeaseCheck for PgFrontier {
    async fn check(&self, p: &ClaimPermit) -> std::result::Result<(), LeaseLost> {
        let row: Option<(i64, bool, bool)> = sqlx::query_as(
            "SELECT epoch, completed_at IS NULL, deadline > now() FROM claims WHERE key = $1",
        )
        .bind(p.key().as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| unverifiable(p, &e))?;
        match row {
            Some((e, true, true)) if e == p.epoch().0 => Ok(()),
            Some((e, true, false)) if e == p.epoch().0 => {
                Err(p.lease_lost(LeaseLostReason::Expired))
            }
            _ => Err(p.lease_lost(LeaseLostReason::Superseded)),
        }
    }
}

#[async_trait]
impl Frontier for PgFrontier {
    async fn claim(&self, shard: &[i16], max_urls: usize) -> Result<Option<Claim>> {
        if max_urls == 0 || max_urls > MAX_URLS_PER_CLAIM {
            return Err(Error::Invalid(format!(
                "max_urls must be 1..={MAX_URLS_PER_CLAIM}"
            )));
        }
        let shards = if shard.is_empty() {
            self.config.shards.as_slice()
        } else {
            shard
        };
        let mut excluded = Vec::new();
        for _ in 0..CLAIM_ATTEMPTS {
            if let Some(r) = self.try_claim_key(shards, max_urls, &mut excluded).await? {
                return Ok(r);
            }
        }
        Ok(None)
    }

    async fn extend(&self, p: &ClaimPermit, by: Duration) -> std::result::Result<(), LeaseLost> {
        let unverifiable = |e: sqlx::Error| unverifiable(p, &e);
        let mut tx = self.pool.begin().await.map_err(unverifiable)?;
        let delay: Option<(i32,)> =
            sqlx::query_as("SELECT delay_ms FROM politeness_keys WHERE key = $1 FOR UPDATE")
                .bind(p.key().as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(unverifiable)?;
        let deadline: Option<(DateTime<Utc>,)> = sqlx::query_as(
            "UPDATE claims SET deadline = deadline + make_interval(secs => $3)
             WHERE key = $1 AND epoch = $2 AND completed_at IS NULL AND deadline > now()
             RETURNING deadline",
        )
        .bind(p.key().as_str())
        .bind(p.epoch().0)
        .bind(secs(by))
        .fetch_optional(&mut *tx)
        .await
        .map_err(unverifiable)?;
        let (Some((delay_ms,)), Some((deadline,))) = (delay, deadline) else {
            tx.rollback().await.map_err(unverifiable)?;
            return Err(self.lease_lost(p).await);
        };
        sqlx::query(
            "UPDATE politeness_keys
             SET next_allowed_at = greatest(next_allowed_at, $2 + make_interval(secs => $3))
             WHERE key = $1",
        )
        .bind(p.key().as_str())
        .bind(deadline)
        .bind(f64::from(delay_ms.max(0)) / 1000.0)
        .execute(&mut *tx)
        .await
        .map_err(unverifiable)?;
        tx.commit().await.map_err(unverifiable)?;
        #[allow(clippy::disallowed_methods)] // vc-frontier moves the deadline after extend
        p.extend_to(DbTime(deadline));
        Ok(())
    }

    async fn take_rate_token(&self, p: &ClaimPermit) -> std::result::Result<(), LeaseLost> {
        let unverifiable = |e: sqlx::Error| unverifiable(p, &e);
        let cap = self.config.global_rate_burst.max(1.0);
        let rate = self.config.global_rate_per_s.max(f64::MIN_POSITIVE);
        loop {
            let (got,): (bool,) = sqlx::query_as(TAKE_TOKEN)
                .bind(p.key().as_str())
                .bind(p.epoch().0)
                .bind(cap)
                .bind(rate)
                .fetch_one(&self.pool)
                .await
                .map_err(unverifiable)?;
            if got {
                return Ok(());
            }
            // Either the lease is gone or the bucket is empty.
            self.check(p).await?;
            let avail: Option<(f64,)> = sqlx::query_as(
                "SELECT least($1, tokens + greatest(0, extract(epoch FROM clock_timestamp() - updated_at)::float8) * $2)
                 FROM rate_bucket WHERE name = 'global'",
            )
            .bind(cap)
            .bind(rate)
            .fetch_optional(&self.pool)
            .await
            .map_err(unverifiable)?;
            let Some((avail,)) = avail else {
                sqlx::query(
                    "INSERT INTO rate_bucket (name, tokens, capacity, refill_per_s, updated_at)
                     VALUES ('global', 0, $1, $2, clock_timestamp()) ON CONFLICT (name) DO NOTHING",
                )
                .bind(cap)
                .bind(rate)
                .execute(&self.pool)
                .await
                .map_err(unverifiable)?;
                continue;
            };
            let wait = ((1.0 - avail) / rate).max(0.002);
            // Spread waiters a little so they do not all wake on the same instant.
            let jitter = f64::from(Utc::now().timestamp_subsec_nanos() % 1000) / 1000.0 * 0.1;
            let wait = Duration::from_secs_f64(wait * (1.0 + jitter));
            tokio::time::sleep(
                p.clamp_timeout(wait, Utc::now())
                    .max(Duration::from_millis(1)),
            )
            .await;
        }
    }

    async fn mark_fetched(
        &self,
        p: &ClaimPermit,
        url: &UrlKey,
        next_fetch_at: DateTime<Utc>,
    ) -> std::result::Result<(), LeaseLost> {
        let unverifiable = |e: sqlx::Error| unverifiable(p, &e);
        let mut tx = self.pool.begin().await.map_err(unverifiable)?;
        // Lock the claim row (shared) so a concurrent claim cannot supersede us mid-write. The
        // request already happened inside this epoch's lease, so an expired but not superseded
        // epoch may still record it (no duplicate fetch).
        let current: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM claims WHERE key = $1 AND epoch = $2 AND completed_at IS NULL FOR SHARE",
        )
        .bind(p.key().as_str())
        .bind(p.epoch().0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unverifiable)?;
        let updated = if current.is_some() {
            // Wait for a concurrent segment commit holding this row, then look again: if a
            // primary capture of this epoch is already committed, its commit found the URL
            // still `leased` and flipped nothing, so go straight to `due` (B7).
            sqlx::query("SELECT 1 FROM url_state WHERE url_key = $1 FOR UPDATE")
                .bind(url.as_bytes().as_slice())
                .execute(&mut *tx)
                .await
                .map_err(unverifiable)?;
            sqlx::query(
                "WITH done AS (
                     SELECT EXISTS (SELECT 1 FROM captures
                                    WHERE url_key = $1 AND epoch = $3 AND role = 'primary') AS c)
                 UPDATE url_state SET
                     state = CASE WHEN done.c THEN 'due' ELSE 'fetched_pending' END,
                     lease_epoch = CASE WHEN done.c THEN NULL ELSE lease_epoch END,
                     pending_since = CASE WHEN done.c THEN NULL ELSE now() END,
                     next_fetch_at = $4
                 FROM done
                 WHERE url_key = $1 AND key = $2 AND lease_epoch = $3
                   AND state IN ('leased', 'fetched_pending')",
            )
            .bind(url.as_bytes().as_slice())
            .bind(p.key().as_str())
            .bind(p.epoch().0)
            .bind(next_fetch_at)
            .execute(&mut *tx)
            .await
            .map_err(unverifiable)?
            .rows_affected()
        } else {
            0
        };
        if updated == 0 {
            tx.rollback().await.map_err(unverifiable)?;
            return Err(p.lease_lost(LeaseLostReason::Superseded));
        }
        tx.commit().await.map_err(unverifiable)?;
        Ok(())
    }

    async fn complete(
        &self,
        p: ClaimPermit,
        outcome: KeyOutcome,
    ) -> std::result::Result<(), LeaseLost> {
        match self.complete_tx(&p, &outcome).await {
            Ok(true) => {
                tracing::debug!(key = %p.key(), epoch = p.epoch().0, "completed");
                Ok(())
            }
            Ok(false) => {
                tracing::info!(key = %p.key(), epoch = p.epoch().0, "stale completion rejected");
                Err(p.lease_lost(LeaseLostReason::Superseded))
            }
            Err(e) => Err(unverifiable(&p, &e)),
        }
    }

    async fn enqueue(&self, batch: &[NewUrl]) -> Result<u64> {
        // Dedupe by url key (first wins) and sort, so concurrent enqueues lock in one order.
        let mut items: Vec<(UrlKey, &NewUrl)> = batch.iter().map(|n| (n.url.key(), n)).collect();
        items.sort_by_key(|a| a.0);
        items.dedup_by(|a, b| a.0 == b.0);
        let items: Vec<&NewUrl> = items.into_iter().map(|(_, n)| n).collect();
        let mut added = 0;
        for chunk in items.chunks(self.config.enqueue_batch.max(1)) {
            added += self.enqueue_chunk(chunk).await?;
        }
        Ok(added)
    }

    async fn park(&self, key: &PolitenessKey, reason: ParkReason) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "INSERT INTO politeness_keys (key, min_delay_ms, delay_ms, budget_day)
             VALUES ($1, $2, $2, $3) ON CONFLICT (key) DO NOTHING",
        )
        .bind(key.as_str())
        .bind(self.min_delay_ms())
        .bind(i32::try_from(self.config.daily_budget).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let (level, parked): (i16, bool) = sqlx::query_as(
            "SELECT park_level, coalesce(parked_until > now(), false)
             FROM politeness_keys WHERE key = $1 FOR UPDATE",
        )
        .bind(key.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        if !parked {
            let pk = park_for(&self.config, i64::from(level), &reason);
            sqlx::query(
                "UPDATE politeness_keys
                 SET parked_until = now() + make_interval(secs => $2), park_level = $3,
                     park_reason = $4, updated_at = now()
                 WHERE key = $1",
            )
            .bind(key.as_str())
            .bind(secs(pk.duration))
            .bind(i16::try_from(pk.level).unwrap_or(i16::MAX))
            .bind(serde_json::to_string(&reason)?)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        // Detect → park → stop: a live lease ends now (its holder may still complete).
        sqlx::query(
            "UPDATE claims SET deadline = least(deadline, now())
             WHERE key = $1 AND completed_at IS NULL",
        )
        .bind(key.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        tracing::info!(key = %key, reason = ?reason, already_parked = parked, "key parked");
        Ok(())
    }

    async fn robots(&self, origin: &OriginKey) -> Result<Option<RobotsEntry>> {
        let row: Option<RobotsRow> = sqlx::query_as(
            "SELECT robots_state, robots_http_status, robots_body, robots_last_good,
                    robots_last_good_at, robots_fetched_at, robots_expires_at
             FROM origins WHERE origin = $1",
        )
        .bind(origin.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        let Some((state, http, body, good, good_at, fetched, expires)) = row else {
            return Ok(None);
        };
        let (Some(status), Some(fetched_at), Some(expires_at)) =
            (parse_robots_state(&state), fetched, expires)
        else {
            return Ok(None);
        };
        Ok(Some(RobotsEntry {
            origin: origin.clone(),
            status,
            http_status: http.and_then(|s| u16::try_from(s).ok()),
            body: body.map(Into::into),
            last_good: good.zip(good_at).map(|(b, t)| (b.into(), t)),
            fetched_at,
            expires_at,
        }))
    }

    async fn store_robots(&self, p: &ClaimPermit, entry: RobotsEntry) -> Result<()> {
        let probe = CanonicalUrl::parse(&format!("{}/", entry.origin.as_str()))
            .map_err(|e| Error::Invalid(format!("robots origin: {e}")))?;
        if !p.scope().covers(&probe) {
            return Err(Error::Policy(format!(
                "origin {} is outside the permit for {}",
                entry.origin,
                p.key()
            )));
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        let live: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM claims
             WHERE key = $1 AND epoch = $2 AND completed_at IS NULL AND deadline > now()
             FOR SHARE",
        )
        .bind(p.key().as_str())
        .bind(p.epoch().0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        if live.is_none() {
            tx.rollback().await.map_err(db)?;
            return Err(self.lease_lost(p).await.into());
        }
        let body = entry
            .body
            .as_ref()
            .map(|b| b[..b.len().min(ROBOTS_MAX_BYTES)].to_vec());
        let (good, good_at) = match &entry.last_good {
            Some((b, t)) => (Some(b[..b.len().min(ROBOTS_MAX_BYTES)].to_vec()), Some(*t)),
            None => (None, None),
        };
        sqlx::query(
            "INSERT INTO origins (origin, key, robots_state, robots_http_status, robots_body,
                                  robots_last_good, robots_last_good_at, robots_fetched_at,
                                  robots_expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (origin) DO UPDATE SET
                 robots_state = EXCLUDED.robots_state,
                 robots_http_status = EXCLUDED.robots_http_status,
                 robots_body = EXCLUDED.robots_body,
                 robots_last_good = coalesce(EXCLUDED.robots_last_good, origins.robots_last_good),
                 robots_last_good_at = coalesce(EXCLUDED.robots_last_good_at, origins.robots_last_good_at),
                 robots_fetched_at = EXCLUDED.robots_fetched_at,
                 robots_expires_at = EXCLUDED.robots_expires_at",
        )
        .bind(entry.origin.as_str())
        .bind(p.key().as_str())
        .bind(robots_state(entry.status))
        .bind(entry.http_status.and_then(|s| i16::try_from(s).ok()))
        .bind(body)
        .bind(good)
        .bind(good_at)
        .bind(entry.fetched_at)
        .bind(entry.expires_at)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    /// `due` → `next_fetch_at = at`; `leased`/`fetched_pending` → `refetch_at` (applied by the
    /// `url_state_apply_refetch` trigger, migration 1101, when the URL returns to `due`);
    /// terminal states and unknown keys → `Ok(false)`. Key politeness is never touched.
    async fn reschedule(&self, url: &UrlKey, at: DateTime<Utc>) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE url_state SET
                 next_fetch_at = CASE WHEN state = 'due' THEN $2 ELSE next_fetch_at END,
                 refetch_at = CASE WHEN state = 'due' THEN refetch_at
                                   ELSE least(coalesce(refetch_at, $2), $2) END
             WHERE url_key = $1 AND state IN ('due', 'leased', 'fetched_pending')",
        )
        .bind(url.as_bytes().as_slice())
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(r.rows_affected() > 0)
    }
}
