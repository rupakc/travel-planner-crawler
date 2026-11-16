//! `e2e-runner prepare [--migrate]` | `e2e-runner run`
//!
//! * `prepare`: waits for Postgres and the fakes, applies the migrations when `--migrate` is
//!   given (only while `vcrawl migrate` is not wired yet), creates the fake GCS buckets and
//!   registers the E2E API client.
//! * `run`: drives `E2E_CYCLES` crawl cycles of `E2E_CYCLE_SECS` each (fake web cycle `n`,
//!   then "time passes": due URLs become due now), lets the processor drain for up to
//!   `E2E_DRAIN_SECS`, then runs every assertion and prints a verdict per assertion (Markdown
//!   table to `E2E_SUMMARY` when set). Exit 1 on any failure.

use std::time::Duration;

use anyhow::{Result, anyhow};
use e2e_runner::checks;
use e2e_runner::stack::{self, Env, FakeWebAdmin};
use e2e_runner::{ASSERTIONS, Outcome, RoleStatus, summary};
use testkit::fake_web::sites;
use vc_core::store::BlobStore;

fn num(k: &str, default: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("prepare") => prepare(args.iter().any(|a| a == "--migrate")).await,
        Some("run") => run().await,
        _ => Err(anyhow!("usage: e2e-runner prepare [--migrate] | run")),
    }
}

async fn prepare(migrate: bool) -> Result<()> {
    let env = Env::from_env()?;
    let pool = stack::pg(&env.database_url).await?;
    if migrate {
        testkit::MIGRATOR.run(&pool).await?;
        eprintln!("prepare: migrations applied by the runner (vcrawl migrate not wired yet)");
    }
    stack::create_buckets(&env).await?;
    for admin in [
        FakeWebAdmin::new(&env.fake_web),
        FakeWebAdmin::new(&env.trap),
    ] {
        stack::wait_for("fake web", Duration::from_secs(60), || admin.health()).await?;
    }
    stack::put_api_client(&pool, &env.api_token).await?;
    eprintln!("prepare: buckets {:?}, API client registered", env.buckets);
    Ok(())
}

async fn run() -> Result<()> {
    let env = Env::from_env()?;
    let roles = RoleStatus::from_env().map_err(|e| anyhow!(e))?;
    let pool = stack::pg(&env.database_url).await?;
    let web = FakeWebAdmin::new(&env.fake_web);
    let trap = FakeWebAdmin::new(&env.trap);

    let crawling = !["scheduler", "fetcher"]
        .iter()
        .any(|r| roles.missing.contains(*r) || roles.crashed.contains(*r));
    let cycles = num("E2E_CYCLES", 3) as usize;
    let cycle_secs = num("E2E_CYCLE_SECS", 75);
    if crawling {
        for c in 0..cycles {
            web.set_cycle(c).await?;
            let moved = if c > 0 && stack::table_exists(&pool, "url_state").await? {
                stack::make_due_now(&pool).await?
            } else {
                0
            };
            eprintln!(
                "cycle {c}/{cycles}: fake web at cycle {c}, {moved} URL(s) made due; crawling {cycle_secs} s"
            );
            tokio::time::sleep(Duration::from_secs(cycle_secs)).await;
            eprintln!(
                "cycle {c}: {} request(s) logged so far",
                web.log().await?.len()
            );
        }
        let drain = Duration::from_secs(num("E2E_DRAIN_SECS", 60));
        let until = tokio::time::Instant::now() + drain;
        while tokio::time::Instant::now() < until {
            if stack::backlog(&pool).await.unwrap_or(0) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        // Segments seal by age (store.segment_max_age_s): give the last one time to commit.
        tokio::time::sleep(Duration::from_secs(num("E2E_SEAL_SECS", 15))).await;
    } else {
        eprintln!(
            "scheduler or fetcher not running: no crawl cycles (every crawl assertion is skipped)"
        );
    }

    let mut results: Vec<(&'static str, Outcome)> = Vec::new();
    for spec in ASSERTIONS {
        let outcome = match roles.gate(&spec) {
            Some(o) => o,
            None => evaluate(spec.name, &env, &pool, &web, &trap)
                .await
                .unwrap_or_else(|e| Outcome::Fail(format!("could not gather evidence: {e:#}"))),
        };
        println!("{:<20} {outcome}", spec.name);
        results.push((spec.name, outcome));
    }
    let (md, failed) = summary(&results);
    if let Ok(p) = std::env::var("E2E_SUMMARY") {
        std::fs::write(&p, &md)?;
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

async fn evaluate(
    name: &str,
    env: &Env,
    pool: &sqlx::PgPool,
    web: &FakeWebAdmin,
    trap: &FakeWebAdmin,
) -> Result<Outcome> {
    Ok(match name {
        "polite" => checks::polite(&web.log().await?, &web.snapshot().await?),
        "raw_first" => {
            let store = stack::raw_store(env);
            let v: Vec<String> = e2e_runner::check_raw_first(pool, &store as &dyn BlobStore)
                .await?
                .iter()
                .map(ToString::to_string)
                .collect();
            let (c, s, rv) = stack::raw_counts(pool).await?;
            checks::raw_first(&v, c, s, rv)
        }
        "version_bump" => checks::version_bump(&stack::changing_versions(pool).await?),
        "api_provenance" => checks::api_provenance(&stack::api_evidence(env).await?),
        "challenge_parked" => checks::challenge_parked(
            &web.log().await?,
            stack::park_row(pool, sites::HOSTILE).await?.as_ref(),
            stack::db_now(pool).await?,
        ),
        "spa_rendered" => {
            let store = stack::raw_store(env);
            let bodies = stack::render_captures(pool, &store, sites::SPA).await?;
            checks::spa_rendered(&web.log().await?, &bodies)
        }
        "private_ip_blocked" => checks::private_ip_blocked(&web.log().await?, &trap.log().await?),
        "two_fetchers" => checks::two_fetchers(&stack::fetcher_instances(pool).await?),
        other => Outcome::Fail(format!("unknown assertion {other}")),
    })
}
