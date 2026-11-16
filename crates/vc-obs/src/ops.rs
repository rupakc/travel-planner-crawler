//! The operational alert gauges that no role measures itself (go-live review 1, 4, 8, 14).
//! The maintenance role calls [`export_ops`] every minute; each gauge is set from Postgres,
//! or **unset** when its source is missing or stale, so the policy's absence condition fires
//! (a gauge that is never exported can never alert on its threshold).
//!
//! | gauge | source |
//! |---|---|
//! | `wal_archive_age_seconds` | `pg_stat_archiver.last_archived_time` |
//! | `base_backup_age_seconds` | `heartbeats('host', 'unit:voyager-crawler-basebackup.service')` `last_ok_at` (`deploy/host/unit-status.sh`) |
//! | `backup_restore_age_seconds` | the same for `voyager-crawler-restore-drill.service` (the light restore drill) |
//! | `host_units_failed` | `heartbeats('host', 'unit:*')` rows with `failed = true` |
//! | `deploy_drift_seconds` | `heartbeats('host', 'deploy')` `drift_since` (`deploy.sh`), unset when that row is older than [`DEPLOY_BEAT_STALE`] |
//! | `tls_cert_days_left` | the same row's `tls_not_after` (the installed certificate) |
//! | `llm_month_spend_ratio` | `llm_budget` month row `spent / cap` (0 when there is none yet) |
//! | `llm_config_invalid` | [`OpsInputs::llm_forced_off`] |

use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::PgPool;
use vc_core::{Error, Result};

use crate::metrics::Metrics;

/// The `deploy` heartbeat is written every 2 minutes; older than this, `deploy.sh` is not
/// running and the drift gauge is withdrawn (its absence alerts).
pub const DEPLOY_BEAT_STALE: chrono::Duration = chrono::Duration::minutes(30);

/// The base-backup unit's heartbeat instance.
pub const BASEBACKUP_UNIT: &str = "unit:voyager-crawler-basebackup.service";
/// The light restore drill unit's heartbeat instance.
pub const RESTORE_DRILL_UNIT: &str = "unit:voyager-crawler-restore-drill.service";

/// What [`export_ops`] needs from the configuration.
#[derive(Debug, Clone, Default)]
pub struct OpsInputs {
    /// `llm.forced_off`: why an invalid `[llm]` section forced the LLM off.
    pub llm_forced_off: Option<String>,
}

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

fn age(now: DateTime<Utc>, t: DateTime<Utc>) -> f64 {
    (now - t).num_milliseconds().max(0) as f64 / 1000.0
}

/// `notAfter` as `openssl x509 -enddate` prints it (`Nov 16 12:00:00 2028 GMT`).
pub fn parse_openssl_date(s: &str) -> Option<DateTime<Utc>> {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.strip_suffix(" GMT")?;
    NaiveDateTime::parse_from_str(t, "%b %d %H:%M:%S %Y")
        .ok()
        .map(|n| n.and_utc())
}

async fn last_ok_at(pool: &PgPool, instance: &str) -> Result<Option<DateTime<Utc>>> {
    let v: Option<Option<String>> = sqlx::query_scalar(
        "SELECT progress->>'last_ok_at' FROM heartbeats WHERE role = 'host' AND instance = $1",
    )
    .bind(instance)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    Ok(v.flatten()
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc)))
}

fn set_or_unset(g: &crate::metrics::FloatGauge, v: Option<f64>) {
    match v {
        Some(v) => g.set(v),
        None => g.unset(),
    }
}

/// Sets (or withdraws) the operational gauges at `now`. A query error leaves the other gauges
/// updated and is returned at the end.
pub async fn export_ops(
    pool: &PgPool,
    m: &Metrics,
    inputs: &OpsInputs,
    now: DateTime<Utc>,
) -> Result<()> {
    let mut first_err = None;
    let mut keep = |r: Result<()>| {
        if let Err(e) = r
            && first_err.is_none()
        {
            first_err = Some(e);
        }
    };
    m.llm_config_invalid
        .set(i64::from(inputs.llm_forced_off.is_some()));

    keep(
        async {
            let t: Option<Option<DateTime<Utc>>> =
                sqlx::query_scalar("SELECT last_archived_time FROM pg_stat_archiver")
                    .fetch_optional(pool)
                    .await
                    .map_err(db)?;
            set_or_unset(&m.wal_archive_age_seconds, t.flatten().map(|t| age(now, t)));
            Ok(())
        }
        .await,
    );
    keep(
        async {
            let t = last_ok_at(pool, BASEBACKUP_UNIT).await?;
            set_or_unset(&m.base_backup_age_seconds, t.map(|t| age(now, t)));
            let t = last_ok_at(pool, RESTORE_DRILL_UNIT).await?;
            set_or_unset(&m.backup_restore_age_seconds, t.map(|t| age(now, t)));
            Ok(())
        }
        .await,
    );
    keep(
        async {
            let n: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM heartbeats WHERE role = 'host' AND instance LIKE 'unit:%'
                   AND (progress->>'failed')::boolean",
            )
            .fetch_one(pool)
            .await
            .map_err(db)?;
            m.host_units_failed.set(n);
            Ok(())
        }
        .await,
    );
    keep(
        async {
            let row: Option<(DateTime<Utc>, Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT last_beat, progress->>'drift_since', progress->>'tls_not_after'
                 FROM heartbeats WHERE role = 'host' AND instance = 'deploy'",
            )
            .fetch_optional(pool)
            .await
            .map_err(db)?;
            let fresh = row
                .as_ref()
                .filter(|(beat, _, _)| now - *beat <= DEPLOY_BEAT_STALE);
            let drift = fresh.as_ref().map(|(_, since, _)| {
                since
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map_or(0.0, |s| age(now, s.with_timezone(&Utc)))
            });
            set_or_unset(&m.deploy_drift_seconds, drift);
            // The certificate is a fact about the disk, not about deploy.sh being alive.
            let left = row
                .as_ref()
                .and_then(|(_, _, t)| t.as_deref())
                .and_then(parse_openssl_date)
                .map(|t| (t - now).num_seconds() as f64 / 86_400.0);
            set_or_unset(&m.tls_cert_days_left, left);
            Ok(())
        }
        .await,
    );
    keep(
        async {
            let month = now.format("%Y-%m").to_string();
            let row: Option<(f64, f64)> = sqlx::query_as(
                "SELECT spent::float8, cap::float8 FROM llm_budget WHERE period = $1",
            )
            .bind(&month)
            .fetch_optional(pool)
            .await
            .map_err(db)?;
            let ratio = match row {
                Some((spent, cap)) if cap > 0.0 => spent / cap,
                Some((spent, _)) if spent > 0.0 => 1.0,
                _ => 0.0,
            };
            m.llm_month_spend_ratio.set(ratio);
            Ok(())
        }
        .await,
    );
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn openssl_dates_parse() {
        let want = Utc.with_ymd_and_hms(2028, 11, 6, 12, 0, 1).unwrap();
        assert_eq!(parse_openssl_date("Nov  6 12:00:01 2028 GMT"), Some(want));
        assert_eq!(parse_openssl_date("Nov 06 12:00:01 2028 GMT"), Some(want));
        assert_eq!(parse_openssl_date("Nov 6 12:00:01 2028"), None);
        assert_eq!(parse_openssl_date("garbage"), None);
    }
}
