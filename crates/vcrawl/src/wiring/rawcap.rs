//! The overall daily raw-bytes cap (spec §5.2: default 3 GB/day), enforced **across
//! processes** with a counter row in Postgres.
//!
//! The counter is table `raw_bytes_daily` (migration 1801): one row per UTC day (DB time, like
//! every other politeness clock), `bytes` = decoded bytes spooled that day. Every add is one
//! atomic upsert, so concurrent fetchers never lose an increment. Old days are pruned by
//! [`PgRawBytes::prune`] (scheduler pass), together with the `raw_bytes:YYYY-MM-DD` rows the
//! first image kept in `rate_bucket` (1801 carried their totals over).

use async_trait::async_trait;
use sqlx::PgPool;
use vc_core::{Error, Result};

/// Name prefix of the legacy per-day rows in `rate_bucket` (pruned only).
pub const LEGACY_ROW_PREFIX: &str = "raw_bytes:";

/// Today, in SQL (DB time, UTC).
macro_rules! today {
    () => {
        "(now() AT TIME ZONE 'UTC')::date"
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

fn i64_of(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn u64_of(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

impl PgRawBytes {
    /// A counter with a daily `cap` in bytes (`fetch.daily_raw_bytes_cap`; 0 = no cap).
    pub fn new(pool: PgPool, cap: u64) -> Self {
        PgRawBytes { pool, cap }
    }

    /// Today's total (DB time).
    pub async fn today(&self) -> Result<u64> {
        let v: Option<i64> = sqlx::query_scalar(concat!(
            "SELECT bytes FROM raw_bytes_daily WHERE day = ",
            today!()
        ))
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(v.map_or(0, u64_of))
    }

    /// Deletes the counters of earlier days (and the legacy `rate_bucket` rows of earlier
    /// days). Returns the number of rows removed.
    pub async fn prune(&self) -> Result<u64> {
        let new = sqlx::query(concat!(
            "DELETE FROM raw_bytes_daily WHERE day < ",
            today!()
        ))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        let legacy = sqlx::query(
            "DELETE FROM rate_bucket WHERE name LIKE 'raw\\_bytes:%'
             AND name < ('raw_bytes:' || to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD'))",
        )
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(new.rows_affected() + legacy.rows_affected())
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
        let t: i64 = sqlx::query_scalar(concat!(
            "INSERT INTO raw_bytes_daily (day, bytes, updated_at) VALUES (",
            today!(),
            ", $1, clock_timestamp())
             ON CONFLICT (day) DO UPDATE
             SET bytes = least(raw_bytes_daily.bytes::numeric + EXCLUDED.bytes, 9223372036854775807)::bigint,
                 updated_at = clock_timestamp()
             RETURNING bytes"
        ))
        .bind(i64_of(bytes))
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(u64_of(t))
    }
}
