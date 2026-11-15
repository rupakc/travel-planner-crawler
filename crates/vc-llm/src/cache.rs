//! `llm_cache(model, prompt_hash, input_hash) → output`: identical inputs are never paid twice.

use serde_json::Value;
use sqlx::PgPool;
use vc_core::llm::{LlmRequest, LlmResponse, Usage};
use vc_core::{Error, Result};

/// The cache key of a request: `(model, prompt_hash, input_hash)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    /// Model id.
    pub model: String,
    /// blake3 of the system prompt, schema, output cap and thinking level.
    pub prompt_hash: String,
    /// blake3 of the input text.
    pub input_hash: String,
}

impl CacheKey {
    /// The key of `req` for `model`.
    pub fn of(model: &str, req: &LlmRequest) -> Self {
        let prompt = serde_json::json!({
            "system": req.system,
            "schema": req.response_schema,
            "max_output_tokens": req.max_output_tokens,
            "thinking": req.thinking,
        });
        CacheKey {
            model: model.to_string(),
            prompt_hash: blake3::hash(prompt.to_string().as_bytes())
                .to_hex()
                .to_string(),
            input_hash: blake3::hash(req.input.as_bytes()).to_hex().to_string(),
        }
    }
}

/// The Postgres-backed response cache.
#[derive(Debug, Clone)]
pub struct LlmCache {
    pool: PgPool,
}

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

impl LlmCache {
    /// A cache on `pool`.
    pub fn new(pool: PgPool) -> Self {
        LlmCache { pool }
    }

    /// A cached response (`from_cache = true`, zero usage), counting the hit.
    pub async fn get(&self, key: &CacheKey) -> Result<Option<LlmResponse>> {
        let row: Option<(Value,)> = sqlx::query_as(
            "UPDATE llm_cache SET hits = hits + 1
             WHERE model = $1 AND prompt_hash = $2 AND input_hash = $3
             RETURNING output",
        )
        .bind(&key.model)
        .bind(&key.prompt_hash)
        .bind(&key.input_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        let Some((v,)) = row else { return Ok(None) };
        match serde_json::from_value::<LlmResponse>(v) {
            Ok(mut r) => {
                r.from_cache = true;
                r.usage = Usage::default();
                Ok(Some(r))
            }
            Err(_) => Ok(None),
        }
    }

    /// Stores a response (usage stripped). First writer wins.
    pub async fn put(&self, key: &CacheKey, resp: &LlmResponse) -> Result<()> {
        let mut stored = resp.clone();
        stored.usage = Usage::default();
        stored.from_cache = false;
        let v = serde_json::to_value(&stored)?;
        sqlx::query(
            "INSERT INTO llm_cache (model, prompt_hash, input_hash, output)
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(&key.model)
        .bind(&key.prompt_hash)
        .bind(&key.input_hash)
        .bind(v)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }
}
