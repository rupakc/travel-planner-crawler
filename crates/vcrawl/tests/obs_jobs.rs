//! The maintenance role's operational jobs (go-live review 10, 14): the hourly stale LLM
//! settle and the gauges it exports, on a real Postgres.

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use testkit::FakeClock;
use vc_core::Clock;
use vc_core::llm::{BudgetLedger, Purpose, Usd};
use vcrawl::obs_jobs::OpsJobs;

async fn status(pool: &sqlx::PgPool, id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM llm_ledger WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn stale_reservations_settle_hourly_and_gauges_export() {
    let pool = testkit::pg().await;
    let clock = Arc::new(FakeClock::new(
        Utc.with_ymd_and_hms(2026, 11, 16, 10, 0, 0).unwrap(),
    ));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let mut cfg = vc_config::Config::default();
    cfg.llm.stale_settle_h = 1;
    cfg.llm.forced_off = Some("llm.enabled requires VC_PROJECT_CAP_USD above 0".into());
    let ledger = vc_llm::PgLedger::new(pool.clone(), cfg.llm.clone(), dyn_clock.clone());
    let a = ledger
        .reserve(Purpose::Extraction, Usd::from_dollars(0.01))
        .await
        .unwrap()
        .expect("reserved");
    let metrics = Arc::new(vc_obs::Metrics::new(
        dyn_clock.clone(),
        Duration::from_secs(180),
    ));
    let mut jobs = OpsJobs::new(pool.clone(), metrics.clone(), &cfg, dyn_clock.clone());

    // First pass (30 min later): the settle runs (nothing is stale yet), gauges exported.
    clock.advance(Duration::from_secs(30 * 60));
    assert_eq!(jobs.tick().await, Some(0));
    assert_eq!(metrics.llm_config_invalid.get(), Some(1));
    assert_eq!(metrics.host_units_failed.get(), Some(0));
    assert!(metrics.llm_month_spend_ratio.get().is_some());

    // Another reservation (b) an hour after a.
    clock.advance(Duration::from_secs(30 * 60));
    let b = ledger
        .reserve(Purpose::Extraction, Usd::from_dollars(0.01))
        .await
        .unwrap()
        .expect("reserved");
    // 50 min after the last settle: a is stale (80 min) but the hourly pass is not due.
    clock.advance(Duration::from_secs(20 * 60));
    assert_eq!(jobs.tick().await, None);
    assert_eq!(status(&pool, &a.id.to_string()).await, "reserved");
    // 61 min after the last settle: a (91 min) is settled as spent, b (31 min) is not.
    clock.advance(Duration::from_secs(11 * 60));
    assert_eq!(jobs.tick().await, Some(1));
    assert_eq!(status(&pool, &a.id.to_string()).await, "expired");
    assert_eq!(status(&pool, &b.id.to_string()).await, "reserved");
    // The settled estimate now counts as month spend.
    assert!(metrics.llm_month_spend_ratio.get().unwrap() == 0.0);
    jobs.tick().await;
    assert!(metrics.llm_month_spend_ratio.get().unwrap() > 0.0);
}
