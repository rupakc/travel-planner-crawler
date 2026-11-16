//! Process-wide resources of the data-path roles: the Postgres pool and the raw blob store.
//!
//! Temporary home: once the shared `Ctx` grows pool/blob constructors (t15-fetch), the roles
//! switch to those and these become thin wrappers.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use vc_config::Config;
use vc_core::secrets::SecretSource;
use vc_core::store::BlobStore;

/// Connection options from `[database]`: the URL, the password from Secret Manager when
/// `password_secret` is set, and the session `statement_timeout` / `lock_timeout`.
pub async fn connect_options(
    cfg: &Config,
    secrets: &dyn SecretSource,
) -> anyhow::Result<PgConnectOptions> {
    let db = &cfg.database;
    let mut opts = PgConnectOptions::from_str(&db.url).context("database.url")?;
    if let Some(name) = &db.password_secret {
        let pw = secrets
            .get(name)
            .await
            .with_context(|| format!("database password secret {name}"))?;
        opts = opts.password(pw.expose());
    }
    Ok(opts.options([
        ("statement_timeout", db.statement_timeout_ms.to_string()),
        ("lock_timeout", db.lock_timeout_ms.to_string()),
    ]))
}

/// The Postgres pool (`database.max_connections`).
pub async fn pool(cfg: &Config, secrets: &dyn SecretSource) -> anyhow::Result<PgPool> {
    let opts = connect_options(cfg, secrets).await?;
    PgPoolOptions::new()
        .max_connections(cfg.database.max_connections.max(1))
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(opts)
        .await
        .context("connect to Postgres")
}

/// The secret source from `[secrets]`.
pub fn secrets(cfg: &Config) -> Arc<dyn SecretSource> {
    vc_secrets::from_config(&cfg.secrets)
}

/// The raw-bucket blob store from `[store]`.
pub async fn blobs(cfg: &Config) -> anyhow::Result<Arc<dyn BlobStore>> {
    vc_store::blob_store_from_config(&cfg.store)
        .await
        .context("raw blob store")
}
