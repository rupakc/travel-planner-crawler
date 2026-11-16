//! Postgres implementations of [`ClientStore`] and [`Catalog`] (`api_clients`, `sources`,
//! counts of `records` / `entities` / `changes`).

use async_trait::async_trait;
use chrono::Utc;
use sqlx::PgPool;
use vc_core::licence::{Licence, LicenceClass};
use vc_core::{Error, Result};

use crate::auth::{ClientStore, StoredClient};
use crate::catalog::Catalog;
use crate::dto::{LicenceDto, SourceDto, StatsDto};

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

fn parse_class(s: &str) -> Option<LicenceClass> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// The API's Postgres store.
#[derive(Clone)]
pub struct PgApiStore {
    pool: PgPool,
}

impl PgApiStore {
    /// Wraps a pool.
    pub fn new(pool: PgPool) -> Self {
        PgApiStore { pool }
    }

    /// Inserts or replaces a client (admin tooling; `token_hash` from
    /// [`crate::auth::hash_token`]).
    pub async fn put_client(&self, c: &StoredClient) -> Result<()> {
        let classes: Vec<String> = c
            .licence_classes
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        sqlx::query(
            "INSERT INTO api_clients (client_id, name, token_hash, licence_classes, rate_per_min, enabled)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (client_id) DO UPDATE SET name = EXCLUDED.name,
               token_hash = EXCLUDED.token_hash, licence_classes = EXCLUDED.licence_classes,
               rate_per_min = EXCLUDED.rate_per_min, enabled = EXCLUDED.enabled",
        )
        .bind(&c.client_id)
        .bind(&c.name)
        .bind(&c.token_hash)
        .bind(&classes)
        .bind(i32::try_from(c.rate_per_min).unwrap_or(i32::MAX))
        .bind(c.enabled)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }
}

type ClientRow = (String, String, String, Vec<String>, i32, bool);
type SourceRow = (
    String,
    String,
    String,
    String,
    serde_json::Value,
    String,
    bool,
);

#[async_trait]
impl ClientStore for PgApiStore {
    async fn get(&self, client_id: &str) -> Result<Option<StoredClient>> {
        let row: Option<ClientRow> = sqlx::query_as(
            "SELECT client_id, name, token_hash, licence_classes, rate_per_min, enabled
             FROM api_clients WHERE client_id = $1",
        )
        .bind(client_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row.map(
            |(client_id, name, token_hash, classes, rate, enabled)| StoredClient {
                client_id,
                name,
                token_hash,
                licence_classes: classes.iter().filter_map(|s| parse_class(s)).collect(),
                rate_per_min: u32::try_from(rate).unwrap_or(0),
                enabled,
            },
        ))
    }

    async fn touch(&self, client_id: &str) -> Result<()> {
        sqlx::query("UPDATE api_clients SET last_used_at = now() WHERE client_id = $1")
            .bind(client_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }
}

#[async_trait]
impl Catalog for PgApiStore {
    async fn sources(&self) -> Result<Vec<SourceDto>> {
        let rows: Vec<SourceRow> = sqlx::query_as(
            "SELECT source_id, name, category, source_class, licence, method, enabled
             FROM sources WHERE licence_class <> 'restricted' ORDER BY source_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let mut out = Vec::with_capacity(rows.len());
        for (source_id, name, category, source_class, licence, method, enabled) in rows {
            let lic: Licence = serde_json::from_value(licence)?;
            let Some(licence) = LicenceDto::from_core(&lic) else {
                continue;
            };
            out.push(SourceDto {
                source_id,
                name,
                category,
                source_class,
                licence,
                method,
                enabled,
            });
        }
        Ok(out)
    }

    async fn stats(&self) -> Result<StatsDto> {
        let (sources_enabled, records_active, entities_active, changes_seq_max): (
            i64,
            i64,
            i64,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM sources WHERE enabled),
                    (SELECT count(*) FROM records WHERE state = 'active'),
                    (SELECT count(*) FROM entities WHERE state = 'active'),
                    (SELECT max(seq) FROM changes)",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(StatsDto {
            sources_enabled,
            records_active,
            entities_active,
            changes_seq_max,
            generated_at: Utc::now(),
        })
    }

    async fn ready(&self) -> Result<()> {
        self.ping().await
    }
}
