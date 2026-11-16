//! A Postgres pool for the operations commands (admin, maintenance, migrate).
//!
//! Local helper until `ctx.rs` grows shared pool constructors (the fetch slice owns them).
//! The password comes from `VC_DATABASE_PASSWORD_FILE` (compose secrets), else from the
//! configured secret (`database.password_secret`), else the URL itself.

use std::time::Duration;

use anyhow::Context as _;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

/// Connects with the `[database]` settings (session `statement_timeout` / `lock_timeout`).
pub async fn connect(cfg: &vc_config::Config) -> anyhow::Result<PgPool> {
    connect_with(cfg, cfg.database.max_connections.clamp(1, 4)).await
}

/// Like [`connect`] with an explicit pool size.
pub async fn connect_with(cfg: &vc_config::Config, max: u32) -> anyhow::Result<PgPool> {
    let db = &cfg.database;
    let mut opts: PgConnectOptions = db.url.parse().context("database.url")?;
    if let Some(pw) = password(cfg).await? {
        opts = opts.password(&pw);
    }
    let opts = opts.options([
        ("statement_timeout", db.statement_timeout_ms.to_string()),
        ("lock_timeout", db.lock_timeout_ms.to_string()),
    ]);
    PgPoolOptions::new()
        .max_connections(max.max(1))
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(opts)
        .await
        .context("connect to postgres")
}

async fn password(cfg: &vc_config::Config) -> anyhow::Result<Option<String>> {
    if let Ok(path) = std::env::var("VC_DATABASE_PASSWORD_FILE") {
        let pw = std::fs::read_to_string(&path).context("read VC_DATABASE_PASSWORD_FILE")?;
        return Ok(Some(pw.trim_end_matches(['\r', '\n']).to_string()));
    }
    let Some(name) = &cfg.database.password_secret else {
        return Ok(None);
    };
    match vc_secrets::from_config(&cfg.secrets).get(name).await {
        Ok(s) => Ok(Some(s.expose().to_string())),
        Err(vc_core::Error::NotFound(_)) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("database password secret: {:?}", e.class())),
    }
}
