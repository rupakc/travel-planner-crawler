//! `[database]`: Postgres connection (shared by all crates; owned by the controller).

use serde::{Deserialize, Serialize};

/// Postgres settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Connection URL without the password (`VC_DATABASE_URL`).
    pub url: String,
    /// Secret Manager name holding the password (`None` = in the URL or trust auth).
    pub password_secret: Option<String>,
    /// Pool size per process.
    pub max_connections: u32,
    /// `statement_timeout` for every session, ms.
    pub statement_timeout_ms: u64,
    /// `lock_timeout` for every session, ms.
    pub lock_timeout_ms: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            url: "postgres://vcrawl@localhost:5432/vcrawl".into(),
            password_secret: Some("crawler-pg-password".into()),
            max_connections: 10,
            statement_timeout_ms: 30_000,
            lock_timeout_ms: 5_000,
        }
    }
}
