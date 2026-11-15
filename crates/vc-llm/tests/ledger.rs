//! PgLedger on a real Postgres (testkit container): §17.4 arithmetic and the stale settle.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{clock_at, dyn_clock};
use sqlx::postgres::PgPoolOptions;
use vc_config::{LlmConfig, PurposeShares};
use vc_core::llm::{BudgetLedger, Purpose, Usage, Usd};
use vc_llm::PgLedger;

fn usd(d: f64) -> Usd {
    Usd::from_dollars(d)
}

fn cost(d: f64) -> Usage {
    Usage {
        input_tokens: 1,
        cost: usd(d),
        ..Usage::default()
    }
}

async fn status_of(pool: &sqlx::PgPool, id: &str) -> String {
    let (s,): (String,) = sqlx::query_as("SELECT status FROM llm_ledger WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    s
}

/// Review focus 5: 100 concurrent reservations against a nearly exhausted cap never exceed
/// it, on the day row (day-binding case) or on the month row (month-binding case).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn budget_race_never_exceeds_cap() {
    // (date, daily, monthly, shares, binding period)
    let only_extraction = PurposeShares {
        extraction: 1.0,
        embedding: 0.0,
        triage: 0.0,
        auditor: 0.0,
        scout: 0.0,
        place: 0.0,
    };
    let cases = [
        (
            (2026, 11, 15),
            1.0,
            40.0,
            PurposeShares::default(),
            "2026-11-15",
        ),
        ((2026, 11, 30), 40.0, 1.0, only_extraction, "2026-11"),
    ];
    for ((y, m, d), daily, monthly, shares, binding) in cases {
        let (_, opts) = testkit::pg_with_url().await;
        let pool = PgPoolOptions::new()
            .max_connections(48)
            .acquire_timeout(Duration::from_secs(60))
            .connect_with(opts)
            .await
            .unwrap();
        let clock = clock_at(y, m, d);
        let config = LlmConfig {
            daily_usd: daily,
            monthly_usd: monthly,
            shares,
            ..LlmConfig::default()
        };
        let ledger = Arc::new(PgLedger::new(pool.clone(), config, dyn_clock(&clock)));
        // Nearly exhaust the binding cap: 0.95 of 1.00 spent.
        let r = ledger
            .reserve(Purpose::Extraction, usd(0.95))
            .await
            .unwrap()
            .expect("first reservation fits");
        ledger.settle(r, cost(0.95)).await.unwrap();

        let mut tasks = Vec::new();
        for _ in 0..100 {
            let l = ledger.clone();
            tasks.push(tokio::spawn(async move {
                l.reserve(Purpose::Extraction, usd(0.01)).await
            }));
        }
        let mut granted = 0;
        for t in tasks {
            if t.await.unwrap().unwrap().is_some() {
                granted += 1;
            }
        }
        assert_eq!(
            granted, 5,
            "exactly the remaining 0.05 is granted ({binding})"
        );

        let day = format!("{y:04}-{m:02}-{d:02}");
        let month = format!("{y:04}-{m:02}");
        for period in [day.as_str(), month.as_str()] {
            let row = ledger.row(period).await.unwrap().unwrap();
            assert!(
                row.spent.saturating_add(row.reserved) <= row.cap,
                "{period}: spent {} + reserved {} > cap {}",
                row.spent,
                row.reserved,
                row.cap
            );
        }
        let b = ledger.row(binding).await.unwrap().unwrap();
        assert_eq!(b.spent.saturating_add(b.reserved), b.cap);
        let (open,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM llm_ledger WHERE status = 'reserved'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(open, 5);
    }
}

/// The day cap is min(daily_usd, 1.5 × remaining month / remaining days), fixed per day.
#[tokio::test]
async fn dynamic_daily_cap() {
    let pool = testkit::pg().await;
    let clock = clock_at(2026, 11, 29);
    let config = LlmConfig {
        daily_usd: 1.50,
        monthly_usd: 40.0,
        ..LlmConfig::default()
    };
    let ledger = PgLedger::new(pool.clone(), config, dyn_clock(&clock));
    // 39 of 40 already spent this month; 2 days left (29th, 30th).
    sqlx::query("INSERT INTO llm_budget (period, cap, spent) VALUES ('2026-11', 40, 39)")
        .execute(&pool)
        .await
        .unwrap();

    let r = ledger
        .reserve(Purpose::Extraction, usd(0.75))
        .await
        .unwrap();
    assert!(r.is_some(), "1.5 × 1.00 / 2 = 0.75 fits exactly");
    assert_eq!(
        ledger.row("2026-11-29").await.unwrap().unwrap().cap,
        usd(0.75)
    );
    assert!(
        ledger
            .reserve(Purpose::Extraction, usd(0.01))
            .await
            .unwrap()
            .is_none(),
        "the dynamic cap (0.75), not daily_usd (1.50), binds"
    );

    // Next day: remaining = 40 − 39 − 0.75 (still reserved) = 0.25 → 1.5 × 0.25 / 1 = 0.375.
    clock.advance(Duration::from_secs(24 * 3600));
    assert!(
        ledger
            .reserve(Purpose::Extraction, usd(0.01))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        ledger.row("2026-11-30").await.unwrap().unwrap().cap,
        usd(0.375)
    );

    // A fresh month: 1.5 × 40 / 31 = 1.94 > 1.50, so daily_usd binds.
    clock.advance(Duration::from_secs(24 * 3600));
    assert!(
        ledger
            .reserve(Purpose::Extraction, usd(0.01))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        ledger.row("2026-12-01").await.unwrap().unwrap().cap,
        usd(1.50)
    );
}

/// Each purpose is capped at its share of the monthly cap, independently of the others.
#[tokio::test]
async fn purpose_share_enforced() {
    let pool = testkit::pg().await;
    let clock = clock_at(2026, 11, 1);
    let config = LlmConfig {
        daily_usd: 40.0,
        monthly_usd: 40.0,
        ..LlmConfig::default()
    };
    let ledger = PgLedger::new(pool.clone(), config, dyn_clock(&clock));
    // Scout: 4 % of 40 = 1.60. (Day cap: min(40, 1.5 × 40 / 30) = 2.00.)
    let r = ledger.reserve(Purpose::Scout, usd(1.60)).await.unwrap();
    assert!(r.is_some());
    assert!(
        ledger
            .reserve(Purpose::Scout, usd(0.01))
            .await
            .unwrap()
            .is_none(),
        "scout share exhausted"
    );
    let p = ledger
        .purpose_row("2026-11", Purpose::Scout)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.cap, usd(1.60));
    assert_eq!(p.reserved, usd(1.60));
    // A refused reservation leaves no trace on the day or month rows.
    let day = ledger.row("2026-11-01").await.unwrap().unwrap();
    assert_eq!(day.reserved, usd(1.60));
    // Another purpose still has room.
    assert!(
        ledger
            .reserve(Purpose::Extraction, usd(0.30))
            .await
            .unwrap()
            .is_some()
    );
    // Settling moves reserved → spent on the purpose row too.
    ledger.settle(r.unwrap(), cost(1.0)).await.unwrap();
    let p = ledger
        .purpose_row("2026-11", Purpose::Scout)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((p.reserved, p.spent), (Usd::ZERO, usd(1.0)));
    assert!(
        ledger
            .reserve(Purpose::Scout, usd(0.60))
            .await
            .unwrap()
            .is_some()
    );
}

/// A reservation left open by a crashed caller is settled as spent after 1 h.
#[tokio::test]
async fn stale_reservation_settled_as_spent() {
    let pool = testkit::pg().await;
    let clock = clock_at(2026, 11, 15);
    let ledger = PgLedger::new(pool.clone(), LlmConfig::default(), dyn_clock(&clock));
    let r = ledger
        .reserve(Purpose::Extraction, usd(0.10))
        .await
        .unwrap()
        .unwrap();

    clock.advance(Duration::from_secs(59 * 60));
    assert_eq!(ledger.settle_stale().await.unwrap(), 0, "not stale yet");
    clock.advance(Duration::from_secs(2 * 60));
    assert_eq!(ledger.settle_stale().await.unwrap(), 1);
    assert_eq!(status_of(&pool, &r.id.to_string()).await, "expired");
    for period in ["2026-11-15", "2026-11"] {
        let row = ledger.row(period).await.unwrap().unwrap();
        assert_eq!(
            (row.reserved, row.spent),
            (Usd::ZERO, usd(0.10)),
            "{period}"
        );
    }
    // The late caller's settle is a no-op (idempotent), and a second sweep finds nothing.
    ledger.settle(r, cost(0.02)).await.unwrap();
    assert_eq!(
        ledger.row("2026-11").await.unwrap().unwrap().spent,
        usd(0.10)
    );
    assert_eq!(ledger.settle_stale().await.unwrap(), 0);
}

/// Batch reservations stay open until ingest: the 1 h sweep never touches them.
#[tokio::test]
async fn batch_reservation_not_auto_settled() {
    let pool = testkit::pg().await;
    let clock = clock_at(2026, 11, 15);
    let ledger = PgLedger::new(pool.clone(), LlmConfig::default(), dyn_clock(&clock));
    let r = ledger
        .reserve_batch(Purpose::Extraction, usd(0.20))
        .await
        .unwrap()
        .unwrap();
    assert!(r.batch);
    clock.advance(Duration::from_secs(48 * 3600));
    assert_eq!(ledger.settle_stale().await.unwrap(), 0);
    assert_eq!(status_of(&pool, &r.id.to_string()).await, "reserved");
    let m = ledger.row("2026-11").await.unwrap().unwrap();
    assert_eq!(m.reserved, usd(0.20));
    ledger.settle(r, cost(0.05)).await.unwrap();
    let m = ledger.row("2026-11").await.unwrap().unwrap();
    assert_eq!((m.reserved, m.spent), (Usd::ZERO, usd(0.05)));
}

/// Zero usage releases the reservation; settling twice changes nothing.
#[tokio::test]
async fn settle_zero_releases_and_is_idempotent() {
    let pool = testkit::pg().await;
    let clock = clock_at(2026, 11, 15);
    let ledger = PgLedger::new(pool.clone(), LlmConfig::default(), dyn_clock(&clock));
    let r = ledger
        .reserve(Purpose::Triage, usd(0.05))
        .await
        .unwrap()
        .unwrap();
    ledger.settle(r.clone(), Usage::default()).await.unwrap();
    ledger.settle(r.clone(), cost(0.04)).await.unwrap();
    assert_eq!(status_of(&pool, &r.id.to_string()).await, "released");
    let d = ledger.row("2026-11-15").await.unwrap().unwrap();
    assert_eq!((d.reserved, d.spent), (Usd::ZERO, Usd::ZERO));
    assert!(ledger.reserve(Purpose::Triage, usd(-1.0)).await.is_err());
    let back = ledger.reservation(r.id).await.unwrap().unwrap();
    assert_eq!(
        (back.purpose, back.est, back.batch),
        (Purpose::Triage, usd(0.05), false)
    );
}
