//! `vcrawl migrate [--contract]` (spec §8, §17.6).
//!
//! * `vcrawl migrate` applies the **expand** migrations (`migrations/*.sql`, embedded at build
//!   time) under sqlx's advisory lock. Every one keeps the previous image working (the
//!   `migration_n_minus_1` test replays the previous image's recorded queries against the new
//!   schema). Migrations this image does not know (a newer image already ran) are tolerated, so
//!   a rollback target can run `migrate` too. On success the image is recorded as seen
//!   (`heartbeats` row `role = 'image'`, `progress.first_seen` kept from the first run).
//! * `vcrawl migrate --contract` applies `migrations/contract/*.sql` (destructive steps of an
//!   earlier expand, tracked in their own table `_vc_contract_migrations`). It refuses unless
//!   this image was first seen at least `vcrawl.contract_min_days` (7) ago and, within that
//!   window, no other image ran `migrate` and no rollback is listed in
//!   `<data_dir>/deploy/rollbacks`.

use std::path::Path;

use anyhow::Context as _;
use chrono::{DateTime, Duration, Utc};
use sqlx::migrate::Migrator;
use sqlx::{Executor, PgPool};

use crate::Ctx;

/// The expand migrations shipped in this image.
pub static EXPAND: Migrator = sqlx::migrate!("../../migrations");
/// The contract migrations shipped in this image.
pub static CONTRACT: Migrator = sqlx::migrate!("../../migrations/contract");

/// The migrations table of the contract set (separate from the expand set's).
pub const CONTRACT_TABLE: &str = "_vc_contract_migrations";

/// The identity of the running image: `VC_IMAGE_DIGEST` at run time, else the `VC_GIT_SHA`
/// baked in at build time, else the crate version.
pub fn image_id() -> String {
    std::env::var("VC_IMAGE_DIGEST")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| option_env!("VC_GIT_SHA").map(str::to_string))
        .unwrap_or_else(|| format!("vcrawl-{}", env!("CARGO_PKG_VERSION")))
}

/// The highest expand migration version this image ships (`io.voyager-crawler.schema`).
pub fn schema_version() -> i64 {
    EXPAND.iter().map(|m| m.version).max().unwrap_or(0)
}

/// Runs the command.
pub async fn run(ctx: Ctx, contract: bool) -> anyhow::Result<()> {
    let pool = crate::admin::db::connect_with(&ctx.cfg, 2).await?;
    let image = image_id();
    if contract {
        let rollbacks =
            read_rollbacks(&Path::new(&ctx.cfg.vcrawl.data_dir).join("deploy/rollbacks"))?;
        let n = migrate_contract(
            &pool,
            &image,
            Utc::now(),
            &rollbacks,
            ctx.cfg.vcrawl.contract_min_days,
        )
        .await?;
        println!(
            "{}",
            serde_json::json!({ "contract_applied": n, "image": image })
        );
    } else {
        let n = migrate_expand(&pool, &image).await?;
        println!(
            "{}",
            serde_json::json!({ "applied": n, "schema": schema_version(), "image": image })
        );
    }
    Ok(())
}

async fn applied(pool: &PgPool, table: &str) -> anyhow::Result<i64> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(table)
        .fetch_one(pool)
        .await?;
    if !exists {
        return Ok(0);
    }
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {table} WHERE success"
    )))
    .fetch_one(pool)
    .await?)
}

async fn apply(pool: &PgPool, m: &Migrator, table: &str) -> anyhow::Result<i64> {
    let before = applied(pool, table).await?;
    let mut conn = pool.acquire().await?;
    // Long data moves (partition attach) must not hit the session's statement timeout; each
    // migration sets its own lock_timeout.
    conn.execute("SET statement_timeout = 0").await?;
    m.run(&mut *conn).await.context("apply migrations")?;
    conn.execute("RESET statement_timeout").await?;
    drop(conn);
    Ok(applied(pool, table).await? - before)
}

/// Applies the expand set and records `image` as seen. Returns the migrations applied.
pub async fn migrate_expand(pool: &PgPool, image: &str) -> anyhow::Result<i64> {
    let mut m = sqlx::migrate!("../../migrations");
    m.set_ignore_missing(true);
    let n = apply(pool, &m, "_sqlx_migrations").await?;
    record_image(pool, image).await?;
    tracing::info!(
        applied = n,
        schema = schema_version(),
        "migrate: expand done"
    );
    Ok(n)
}

/// Records that `image` ran against this database (first-seen is kept).
pub async fn record_image(pool: &PgPool, image: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO heartbeats (role, instance, last_beat, progress)
         VALUES ('image', $1, now(), jsonb_build_object('first_seen', now()))
         ON CONFLICT (role, instance) DO UPDATE SET last_beat = now()",
    )
    .bind(image)
    .execute(pool)
    .await?;
    Ok(())
}

/// Applies the contract set if [`contract_gate`] allows it. Returns the migrations applied.
pub async fn migrate_contract(
    pool: &PgPool,
    image: &str,
    now: DateTime<Utc>,
    rollbacks: &[DateTime<Utc>],
    min_days: u32,
) -> anyhow::Result<i64> {
    let first_seen: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT (progress->>'first_seen')::timestamptz FROM heartbeats
         WHERE role = 'image' AND instance = $1",
    )
    .bind(image)
    .fetch_optional(pool)
    .await?
    .flatten();
    let others: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT instance, last_beat FROM heartbeats WHERE role = 'image' AND instance <> $1",
    )
    .bind(image)
    .fetch_all(pool)
    .await?;
    contract_gate(now, first_seen, &others, rollbacks, min_days)
        .map_err(|why| anyhow::anyhow!("migrate --contract refused: {why}"))?;
    let mut m = sqlx::migrate!("../../migrations/contract");
    m.dangerous_set_table_name(CONTRACT_TABLE);
    let n = apply(pool, &m, CONTRACT_TABLE).await?;
    tracing::info!(applied = n, "migrate: contract done");
    Ok(n)
}

/// The §17.6 rule for contract migrations: the current image (first seen at `first_seen`) has
/// run for `min_days`, and within that window no other image ran (`others`: image → last
/// migrate) and no rollback happened.
pub fn contract_gate(
    now: DateTime<Utc>,
    first_seen: Option<DateTime<Utc>>,
    others: &[(String, DateTime<Utc>)],
    rollbacks: &[DateTime<Utc>],
    min_days: u32,
) -> Result<(), String> {
    let Some(since) = first_seen else {
        return Err("this image never ran `vcrawl migrate` against this database".into());
    };
    let window = Duration::days(i64::from(min_days));
    if now - since < window {
        return Err(format!(
            "this image has run for {} h, {min_days} days are required",
            (now - since).num_hours()
        ));
    }
    let horizon = now - window;
    if let Some((img, at)) = others.iter().find(|(_, at)| *at > horizon) {
        return Err(format!("image {img} ran at {at}, within the window"));
    }
    if let Some(at) = rollbacks.iter().find(|at| **at >= horizon) {
        return Err(format!("a rollback happened at {at}"));
    }
    Ok(())
}

/// Reads `deploy.sh`'s rollback log (one RFC 3339 time per line; missing file = none).
pub fn read_rollbacks(path: &Path) -> anyhow::Result<Vec<DateTime<Utc>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).context("read the rollback log"),
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            DateTime::parse_from_rfc3339(l)
                .map(|t| t.with_timezone(&Utc))
                .with_context(|| format!("rollback log line {l:?}"))
        })
        .collect()
}
