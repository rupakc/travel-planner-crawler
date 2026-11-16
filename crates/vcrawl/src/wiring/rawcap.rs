//! The overall daily raw-bytes cap (spec §5.2: default 3 GB/day), enforced **across
//! processes** with a counter row in Postgres.
//!
//! The counter lives in the `rate_bucket` table (migration 0003, "global token bucket(s)") as
//! one row per UTC day named `raw_bytes:YYYY-MM-DD` (`tokens` = decoded bytes spooled that day,
//! `capacity` = the cap). No migration is needed and the frontier's `global` row is never
//! touched (its statements all filter `name = 'global'`). The day is DB time, like every other
//! politeness clock. Old day rows are pruned by [`PgRawBytes::prune`] (scheduler pass).

use async_trait::async_trait;
use sqlx::PgPool;
use vc_core::{Error, Result};

/// Name prefix of the per-day counter rows.
pub const ROW_PREFIX: &str = "raw_bytes:";

/// Today's row name, in SQL (DB time).
macro_rules! day_name {
    () => {
        "('raw_bytes:' || to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD'))"
    };
}

/// A daily raw-bytes budget shared by every fetcher.
#[async_trait]
pub trait RawBytesBudget: Send + Sync {
    /// Whether today's cap is reached (no further fetches today).
    async fn exhausted(&self) -> Result<bool>;
    /// Adds `bytes` spooled; returns today's total.
    async fn add(&self, bytes: u64) -> Result<u64>;
}

/// The Postgres counter (module docs).
#[derive(Debug, Clone)]
pub struct PgRawBytes {
    pool: PgPool,
    cap: u64,
}

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

fn f64_of(n: u64) -> f64 {
    // Exact below 2^53 bytes (8 PB): far above any daily cap.
    n as f64
}

impl PgRawBytes {
    /// A counter with a daily `cap` in bytes (`fetch.daily_raw_bytes_cap`; 0 = no cap).
    pub fn new(pool: PgPool, cap: u64) -> Self {
        PgRawBytes { pool, cap }
    }

    /// Today's total (DB time).
    pub async fn today(&self) -> Result<u64> {
        let v: Option<(f64,)> = sqlx::query_as(concat!(
            "SELECT tokens FROM rate_bucket WHERE name = ",
            day_name!()
        ))
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(v.map(|(t,)| t.max(0.0) as u64).unwrap_or(0))
    }

    /// Deletes the counter rows of earlier days. Returns the number removed.
    pub async fn prune(&self) -> Result<u64> {
        let r = sqlx::query(concat!(
            "DELETE FROM rate_bucket WHERE name LIKE 'raw\\_bytes:%' AND name < ",
            day_name!()
        ))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(r.rows_affected())
    }
}

#[async_trait]
impl RawBytesBudget for PgRawBytes {
    async fn exhausted(&self) -> Result<bool> {
        if self.cap == 0 {
            return Ok(false);
        }
        Ok(self.today().await? >= self.cap)
    }

    async fn add(&self, bytes: u64) -> Result<u64> {
        let (t,): (f64,) = sqlx::query_as(concat!(
            "INSERT INTO rate_bucket (name, tokens, capacity, refill_per_s, updated_at) VALUES (",
            day_name!(),
            ", $1, $2, 1, clock_timestamp())
             ON CONFLICT (name) DO UPDATE
             SET tokens = rate_bucket.tokens + EXCLUDED.tokens,
                 capacity = EXCLUDED.capacity,
                 updated_at = clock_timestamp()
             RETURNING tokens"
        ))
        .bind(f64_of(bytes))
        .bind(f64_of(self.cap))
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(t.max(0.0) as u64)
    }
}
