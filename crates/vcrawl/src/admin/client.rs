//! `vcrawl admin client add|list|revoke` (spec §5.11): read-API clients in `api_clients`.
//!
//! * `add --name <n> [--stdin]`: the bearer token is read from Secret Manager
//!   (`crawler-api-token-<n>`, through `[secrets]`) or, with `--stdin`, from the first line of
//!   stdin. It must have the form `<n>.<secret>` (the API selects the row by the part before the
//!   `.`). Only its argon2id hash is stored; adding an existing name rotates its token and
//!   re-enables it. The token is never printed or logged.
//! * `list`: every client (id, name, enabled, licence classes, rate, last use), never a hash.
//! * `revoke --name <n>`: disables the client; the API refuses it within
//!   [`vc_api::auth::CACHE_TTL`] (60 s).

use anyhow::Context as _;
use clap::Subcommand;
use serde::Serialize;
use sqlx::{PgPool, Row};
use vc_api::auth::{StoredClient, hash_token};
use vc_api::pg::PgApiStore;
use vc_core::licence::LicenceClass;
use vc_core::secrets::{Secret, SecretSource};

use crate::Ctx;

/// `admin client …`.
#[derive(Debug, Clone, Subcommand)]
pub enum ClientCmd {
    /// Registers (or rotates) a client from its token in Secret Manager or on stdin.
    Add {
        /// Client name = client id = the token prefix (`[a-z0-9-]`).
        #[arg(long)]
        name: String,
        /// Read the token from stdin instead of `crawler-api-token-<name>`.
        #[arg(long)]
        stdin: bool,
    },
    /// Lists clients (never their token hashes).
    List,
    /// Disables a client.
    Revoke {
        /// Client name.
        #[arg(long)]
        name: String,
    },
}

/// One row of `client list`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientRow {
    /// Id (the token prefix).
    pub client_id: String,
    /// Display name.
    pub name: String,
    /// Not revoked.
    pub enabled: bool,
    /// Licence classes it may receive.
    pub licence_classes: Vec<String>,
    /// Requests per minute.
    pub rate_per_min: i32,
    /// Last successful authentication (RFC 3339).
    pub last_used_at: Option<String>,
}

/// The Secret Manager name of a client's token.
pub fn secret_name(name: &str) -> String {
    format!("crawler-api-token-{name}")
}

fn check_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 63
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-'),
        "client name {name:?} must be 1-63 characters of [a-z0-9-]"
    );
    Ok(())
}

/// Runs the command.
pub async fn run(ctx: &Ctx, cmd: ClientCmd) -> anyhow::Result<serde_json::Value> {
    let pool = crate::wiring::db::ops_pool(&ctx.cfg, 2).await?;
    Ok(match cmd {
        ClientCmd::Add { name, stdin } => {
            let token = if stdin {
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .context("read the token from stdin")?;
                Secret::new(line.trim())
            } else {
                let secrets = crate::wiring::sources::secrets(&ctx.cfg);
                token_from(secrets.as_ref(), &name).await?
            };
            add(&pool, &name, &token).await?
        }
        ClientCmd::List => serde_json::to_value(list(&pool).await?)?,
        ClientCmd::Revoke { name } => revoke(&pool, &name).await?,
    })
}

/// The token of `name` from `crawler-api-token-<name>`.
pub async fn token_from(secrets: &dyn SecretSource, name: &str) -> anyhow::Result<Secret> {
    check_name(name)?;
    let s = secrets
        .get(&secret_name(name))
        .await
        .with_context(|| format!("secret {}", secret_name(name)))?;
    Ok(Secret::new(s.expose().trim()))
}

/// `client add`: stores the argon2id hash of `token` for `name` (insert or rotate).
pub async fn add(pool: &PgPool, name: &str, token: &Secret) -> anyhow::Result<serde_json::Value> {
    check_name(name)?;
    let t = token.expose();
    let secret = t
        .strip_prefix(name)
        .and_then(|r| r.strip_prefix('.'))
        .unwrap_or("");
    // The message never contains the token.
    anyhow::ensure!(
        secret.len() >= 16 && !secret.chars().any(char::is_whitespace),
        "the token must be `{name}.<secret>` with a secret of at least 16 characters"
    );
    let rotated: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM api_clients WHERE client_id = $1)")
            .bind(name)
            .fetch_one(pool)
            .await?;
    PgApiStore::new(pool.clone())
        .put_client(&StoredClient {
            client_id: name.to_string(),
            name: name.to_string(),
            token_hash: hash_token(t)?,
            licence_classes: vec![
                LicenceClass::Open,
                LicenceClass::ShareAlike,
                LicenceClass::FactsOnly,
                LicenceClass::ApiTerms,
            ],
            rate_per_min: 600,
            enabled: true,
        })
        .await?;
    tracing::info!(client = name, rotated, "api client registered");
    Ok(serde_json::json!({ "client_id": name, "enabled": true, "rotated": rotated }))
}

/// `client list`.
pub async fn list(pool: &PgPool) -> anyhow::Result<Vec<ClientRow>> {
    let rows = sqlx::query(
        "SELECT client_id, name, enabled, licence_classes, rate_per_min,
                to_char(last_used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS last_used
         FROM api_clients ORDER BY client_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ClientRow {
            client_id: r.get("client_id"),
            name: r.get("name"),
            enabled: r.get("enabled"),
            licence_classes: r.get("licence_classes"),
            rate_per_min: r.get("rate_per_min"),
            last_used_at: r.get("last_used"),
        })
        .collect())
}

/// `client revoke`: disables the client (its row and history stay).
pub async fn revoke(pool: &PgPool, name: &str) -> anyhow::Result<serde_json::Value> {
    let n = sqlx::query("UPDATE api_clients SET enabled = false WHERE client_id = $1")
        .bind(name)
        .execute(pool)
        .await?
        .rows_affected();
    anyhow::ensure!(n == 1, "no api client {name}");
    tracing::info!(client = name, "api client revoked");
    Ok(serde_json::json!({ "client_id": name, "enabled": false }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_token_prefixes() {
        assert!(check_name("voyager").is_ok());
        assert!(check_name("app-2").is_ok());
        for bad in ["", "Voyager", "a.b", "-x", "a b"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }
}
