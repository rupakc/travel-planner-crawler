//! `PgLedger`: the race-free budget ledger (spec §5.8, §17.4).
//!
//! A reservation updates the **day** row, the **month** row and the month's **purpose** row
//! of the budget in one statement (three data-modifying CTEs, each gated on the previous one);
//! unless all three returned a row, the transaction rolls back and the caller makes no call.
//! Concurrent reservations serialise on the row locks and Postgres re-checks the `WHERE`
//! against the latest row version, so no cap can be exceeded.
//!
//! Lock order is always month → day → purpose (reserve, settle and stale settle), so the
//! ledger cannot deadlock with itself.
//!
//! - The day cap is dynamic, fixed when the day row is created:
//!   `min(daily_usd, 1.5 × remaining_month / remaining_days)`.
//! - Purpose rows cap each purpose at its share of the monthly cap.
//! - Non-batch reservations still open after `stale_settle_h` are settled as spent
//!   ([`PgLedger::settle_stale`]); batch reservations stay open until ingest.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Datelike, NaiveDate};
use sqlx::{PgPool, Postgres, Transaction};
use vc_config::LlmConfig;
use vc_core::llm::{BudgetLedger, Purpose, Reservation, Usage, Usd};
use vc_core::{Clock, Error, ReservationId, Result};

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

/// `(cap, reserved, spent)` of one budget row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BudgetRow {
    /// Cap.
    pub cap: Usd,
    /// Open reservations.
    pub reserved: Usd,
    /// Settled spend.
    pub spent: Usd,
}

/// Days left in the month of `d`, including `d`.
pub fn remaining_days(d: NaiveDate) -> u32 {
    let (y, m) = (d.year(), d.month());
    let first_next = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1)
    }
    .expect("valid date");
    (first_next - d).num_days() as u32
}

/// The Postgres budget ledger.
#[derive(Debug, Clone)]
pub struct PgLedger {
    /// Pool.
    pub pool: PgPool,
    /// Settings.
    pub config: LlmConfig,
    clock: Arc<dyn Clock>,
}

fn share(c: &LlmConfig, p: Purpose) -> f64 {
    let s = &c.shares;
    match p {
        Purpose::Extraction => s.extraction,
        Purpose::Embedding => s.embedding,
        Purpose::Triage => s.triage,
        Purpose::Auditor => s.auditor,
        Purpose::Scout => s.scout,
        Purpose::Place => s.place,
    }
}

impl PgLedger {
    /// A ledger on `pool`, with `clock` deciding the current day.
    pub fn new(pool: PgPool, config: LlmConfig, clock: Arc<dyn Clock>) -> Self {
        PgLedger {
            pool,
            config,
            clock,
        }
    }

    /// One budget row (`YYYY-MM-DD` or `YYYY-MM`), if it exists.
    pub async fn row(&self, period: &str) -> Result<Option<BudgetRow>> {
        let r: Option<(i64, i64, i64)> = sqlx::query_as(
            "SELECT (cap * 1000000)::bigint, (reserved * 1000000)::bigint,
                    (spent * 1000000)::bigint
             FROM llm_budget WHERE period = $1",
        )
        .bind(period)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(r.map(|(c, r, s)| BudgetRow {
            cap: Usd(c),
            reserved: Usd(r),
            spent: Usd(s),
        }))
    }

    /// One purpose row of a month, if it exists.
    pub async fn purpose_row(&self, month: &str, purpose: Purpose) -> Result<Option<BudgetRow>> {
        let r: Option<(i64, i64, i64)> = sqlx::query_as(
            "SELECT (cap * 1000000)::bigint, (reserved * 1000000)::bigint,
                    (spent * 1000000)::bigint
             FROM llm_purpose_budget WHERE period = $1 AND purpose = $2",
        )
        .bind(month)
        .bind(purpose.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(r.map(|(c, r, s)| BudgetRow {
            cap: Usd(c),
            reserved: Usd(r),
            spent: Usd(s),
        }))
    }

    /// A reservation still recorded in `llm_ledger` (any status).
    pub async fn reservation(&self, id: ReservationId) -> Result<Option<Reservation>> {
        let r: Option<(
            String,
            NaiveDate,
            String,
            i64,
            bool,
            chrono::DateTime<chrono::Utc>,
        )> = sqlx::query_as(
            "SELECT purpose, day, month, (est_usd * 1000000)::bigint, batch, created_at
                 FROM llm_ledger WHERE id = $1",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        let Some((p, day, month, est, batch, created_at)) = r else {
            return Ok(None);
        };
        let purpose = Purpose::ALL
            .into_iter()
            .find(|x| x.as_str() == p)
            .ok_or_else(|| Error::Invalid(format!("unknown purpose {p}")))?;
        Ok(Some(Reservation {
            id,
            purpose,
            est: Usd(est),
            day,
            month,
            batch,
            created_at,
        }))
    }

    async fn reserve_inner(
        &self,
        purpose: Purpose,
        est: Usd,
        batch: bool,
    ) -> Result<Option<Reservation>> {
        if est < Usd::ZERO {
            return Err(Error::Invalid("negative reservation".into()));
        }
        let now = self.clock.now();
        let day = now.date_naive();
        let dkey = day.format("%Y-%m-%d").to_string();
        let mkey = format!("{:04}-{:02}", day.year(), day.month());
        let month_cap = Usd::from_dollars(self.config.monthly_usd);
        let day_max = Usd::from_dollars(self.config.daily_usd);
        let purpose_cap = Usd::from_dollars(self.config.monthly_usd * share(&self.config, purpose));

        let mut tx = self.pool.begin().await.map_err(db)?;
        // Month row first (lock order month → day → purpose); a changed cap applies at once.
        sqlx::query(
            "INSERT INTO llm_budget (period, cap) VALUES ($1, $2::bigint / 1000000.0)
             ON CONFLICT (period) DO UPDATE SET cap = EXCLUDED.cap
             WHERE llm_budget.cap <> EXCLUDED.cap",
        )
        .bind(&mkey)
        .bind(month_cap.micros())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        // The day row, with the dynamic cap fixed at creation.
        sqlx::query(
            "INSERT INTO llm_budget (period, cap)
             SELECT $1, round(LEAST($2::bigint / 1000000.0,
                         1.5 * GREATEST(m.cap - m.spent - m.reserved, 0) / $3::int), 6)
             FROM llm_budget m WHERE m.period = $4
             ON CONFLICT (period) DO NOTHING",
        )
        .bind(&dkey)
        .bind(day_max.micros())
        .bind(remaining_days(day) as i32)
        .bind(&mkey)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "INSERT INTO llm_purpose_budget (period, purpose, cap)
             VALUES ($1, $2, $3::bigint / 1000000.0)
             ON CONFLICT (period, purpose) DO UPDATE SET cap = EXCLUDED.cap
             WHERE llm_purpose_budget.cap <> EXCLUDED.cap",
        )
        .bind(&mkey)
        .bind(purpose.as_str())
        .bind(purpose_cap.micros())
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        let (d, m, p): (bool, bool, bool) = sqlx::query_as(
            "WITH d AS (UPDATE llm_budget SET reserved = reserved + $3::bigint / 1000000.0
                        WHERE period = $1
                          AND spent + reserved + $3::bigint / 1000000.0 <= cap
                        RETURNING 1),
                  m AS (UPDATE llm_budget SET reserved = reserved + $3::bigint / 1000000.0
                        WHERE period = $2
                          AND spent + reserved + $3::bigint / 1000000.0 <= cap
                          AND EXISTS (SELECT 1 FROM d)
                        RETURNING 1),
                  p AS (UPDATE llm_purpose_budget
                        SET reserved = reserved + $3::bigint / 1000000.0
                        WHERE period = $2 AND purpose = $4
                          AND spent + reserved + $3::bigint / 1000000.0 <= cap
                          AND EXISTS (SELECT 1 FROM m)
                        RETURNING 1)
             SELECT EXISTS (SELECT 1 FROM d), EXISTS (SELECT 1 FROM m),
                    EXISTS (SELECT 1 FROM p)",
        )
        .bind(&dkey)
        .bind(&mkey)
        .bind(est.micros())
        .bind(purpose.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        if !(d && m && p) {
            tx.rollback().await.map_err(db)?;
            tracing::debug!(
                purpose = purpose.as_str(),
                day = d,
                month = m,
                purpose_ok = p,
                "llm reservation refused"
            );
            return Ok(None);
        }
        let r = Reservation {
            id: ReservationId::at(now),
            purpose,
            est,
            day,
            month: mkey,
            batch,
            created_at: now,
        };
        sqlx::query(
            "INSERT INTO llm_ledger (id, day, month, purpose, model, est_usd, batch, status,
                                     created_at)
             VALUES ($1, $2, $3, $4, $5, $6::bigint / 1000000.0, $7, 'reserved', $8)",
        )
        .bind(r.id.to_string())
        .bind(r.day)
        .bind(&r.month)
        .bind(purpose.as_str())
        .bind(&self.config.model)
        .bind(est.micros())
        .bind(batch)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(Some(r))
    }

    /// Closes reservation `id` with `actual` (status `settled`, `released` for zero usage, or
    /// the given override). Returns false if it was not open.
    async fn settle_id(
        tx: &mut Transaction<'_, Postgres>,
        id: &str,
        actual: &Usage,
        status: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        let zero = actual.cost == Usd::ZERO
            && actual.input_tokens == 0
            && actual.output_tokens == 0
            && actual.thinking_tokens == 0;
        let status = status.unwrap_or(if zero { "released" } else { "settled" });
        let row: Option<(i64, String, String, String)> = sqlx::query_as(
            "UPDATE llm_ledger
             SET status = $2,
                 actual_usd = COALESCE($3::bigint, (est_usd * 1000000)::bigint) / 1000000.0,
                 input_tokens = $4, output_tokens = $5, thinking_tokens = $6,
                 cached_input_tokens = $7, settled_at = $8
             WHERE id = $1 AND status = 'reserved'
             RETURNING (est_usd * 1000000)::bigint, to_char(day, 'YYYY-MM-DD'), month, purpose",
        )
        .bind(id)
        .bind(status)
        .bind(if status == "expired" {
            None
        } else {
            Some(actual.cost.micros())
        })
        .bind(actual.input_tokens as i64)
        .bind(actual.output_tokens as i64)
        .bind(actual.thinking_tokens as i64)
        .bind(actual.cached_input_tokens as i64)
        .bind(now)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
        let Some((est, day, month, purpose)) = row else {
            return Ok(false);
        };
        let spent = if status == "expired" {
            est
        } else {
            actual.cost.micros()
        };
        for period in [&month, &day] {
            sqlx::query(
                "UPDATE llm_budget
                 SET reserved = GREATEST(reserved - $2::bigint / 1000000.0, 0),
                     spent = spent + $3::bigint / 1000000.0
                 WHERE period = $1",
            )
            .bind(period)
            .bind(est)
            .bind(spent)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        }
        sqlx::query(
            "UPDATE llm_purpose_budget
             SET reserved = GREATEST(reserved - $3::bigint / 1000000.0, 0),
                 spent = spent + $4::bigint / 1000000.0
             WHERE period = $1 AND purpose = $2",
        )
        .bind(&month)
        .bind(&purpose)
        .bind(est)
        .bind(spent)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        Ok(true)
    }

    /// Settles every non-batch reservation older than `stale_settle_h` as spent (status
    /// `expired`, actual = estimate): a crashed caller never leaks budget, and never gets it
    /// back either. Returns how many were settled.
    pub async fn settle_stale(&self) -> Result<u64> {
        let now = self.clock.now();
        let cutoff = now - chrono::Duration::hours(self.config.stale_settle_h as i64);
        let ids: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM llm_ledger
             WHERE status = 'reserved' AND NOT batch AND created_at < $1
             ORDER BY created_at LIMIT 10000",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let mut n = 0;
        for (id,) in ids {
            let mut tx = self.pool.begin().await.map_err(db)?;
            if Self::settle_id(&mut tx, &id, &Usage::default(), Some("expired"), now).await? {
                n += 1;
            }
            tx.commit().await.map_err(db)?;
        }
        if n > 0 {
            tracing::warn!(count = n, "stale llm reservations settled as spent");
        }
        Ok(n)
    }
}

#[async_trait]
impl BudgetLedger for PgLedger {
    async fn reserve(&self, purpose: Purpose, est: Usd) -> Result<Option<Reservation>> {
        self.reserve_inner(purpose, est, false).await
    }

    async fn reserve_batch(&self, purpose: Purpose, est: Usd) -> Result<Option<Reservation>> {
        self.reserve_inner(purpose, est, true).await
    }

    async fn settle(&self, r: Reservation, actual: Usage) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        Self::settle_id(&mut tx, &r.id.to_string(), &actual, None, self.clock.now()).await?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_days_counts_today() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(remaining_days(d(2026, 11, 30)), 1);
        assert_eq!(remaining_days(d(2026, 11, 1)), 30);
        assert_eq!(remaining_days(d(2026, 12, 31)), 1);
        assert_eq!(remaining_days(d(2028, 2, 1)), 29);
    }
}
