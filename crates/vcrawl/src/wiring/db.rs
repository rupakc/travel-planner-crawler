//! The Postgres pool.
//!
//! The password comes from, in order: the URL itself (development, tests), the secret file
//! `VC_DATABASE_PASSWORD_FILE` (the deploy kit), or the secret `database.password_secret`
//! through the configured [`SecretSource`]. Every connection gets `statement_timeout` and
//! `lock_timeout` from `[database]`.

use std::str::FromStr;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use vc_config::DatabaseConfig;
use vc_core::secrets::SecretSource;
use vc_core::{Error, Result};

use super::config::secret_file;

/// Connection options for `[database]`, password resolved (see the module docs).
pub async fn connect_options(
    cfg: &DatabaseConfig,
    secrets: Option<&dyn SecretSource>,
) -> Result<PgConnectOptions> {
    let mut opts = PgConnectOptions::from_str(&cfg.url)
        .map_err(|e| Error::Invalid(format!("database.url: {e}")))?;
    let url_has_password = url::Url::parse(&cfg.url)
        .ok()
        .and_then(|u| u.password().map(|p| !p.is_empty()))
        .unwrap_or(false);
    if !url_has_password {
        if let Some(pw) = secret_file("DATABASE_PASSWORD")? {
            opts = opts.password(&pw);
        } else if let (Some(name), Some(src)) = (cfg.password_secret.as_deref(), secrets) {
            let s = src.get(name).await?;
            opts = opts.password(s.expose());
        }
    }
    Ok(opts.options([
        ("statement_timeout", cfg.statement_timeout_ms.to_string()),
        ("lock_timeout", cfg.lock_timeout_ms.to_string()),
    ]))
}

/// A pool for `[database]`.
pub async fn connect(cfg: &DatabaseConfig, secrets: Option<&dyn SecretSource>) -> Result<PgPool> {
    let opts = connect_options(cfg, secrets).await?;
    PgPoolOptions::new()
        .max_connections(cfg.max_connections.max(1))
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(opts)
        .await
        .map_err(|e| Error::Db(format!("connect: {e}")))
}
