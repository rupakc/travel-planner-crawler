//! Embeddings (spec §5.11, §17.9): one table for every model, a partial HNSW index per model
//! (halfvec, `m = 16`). Only current record versions whose use policy allows embedding are
//! embedded; an embedding is used only while its `content_hash` equals the record's current
//! `data_hash`, so a new version makes it stale until re-embedded.

use sqlx::{Executor, Row};
use vc_core::{Error, ModelId, RecordId, Result};

use crate::PgIndex;
use crate::util::{db, embed_text, hnsw_index_name, vector_literal};

/// One record to embed (from [`PgIndex::pending_embeddings`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingTask {
    /// The record.
    pub record_id: RecordId,
    /// The record's current `data_hash` (the embedding's `content_hash`).
    pub content_hash: Vec<u8>,
    /// The text to embed (title, excerpt and data strings; ≤ 8000 characters).
    pub text: String,
}

impl PgIndex {
    /// Creates the partial HNSW index for `model` (`vec::halfvec(dim)`, cosine, `m` and
    /// `ef_construction` from config) unless it exists. Returns whether it was created.
    /// Runs `CREATE INDEX CONCURRENTLY`, so call it outside a transaction (e.g. at start-up).
    pub async fn ensure_embedding_index(&self, model: &ModelId, dim: usize) -> Result<bool> {
        let name = hnsw_index_name(&model.0)?;
        if dim == 0 || dim > 4000 {
            return Err(Error::Invalid(
                "embedding dim must be 1..=4000 (halfvec)".into(),
            ));
        }
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(&name)
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        if exists {
            return Ok(false);
        }
        // `name` and the model literal are validated identifiers (alphanumerics and -_.@:/).
        let sql = format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS {name} ON embeddings
               USING hnsw ((vec::halfvec({dim})) halfvec_cosine_ops)
               WITH (m = {m}, ef_construction = {ef})
               WHERE model_id = '{model}'",
            m = self.config.hnsw_m.clamp(2, 100),
            ef = self.config.hnsw_ef_construction.clamp(4, 1000),
            model = model.0,
        );
        self.pool
            .execute(sqlx::AssertSqlSafe(sql))
            .await
            .map_err(db)?;
        tracing::info!(model = %model.0, dim, "embedding index created");
        Ok(true)
    }

    /// Current, servable records that allow embedding and have no up-to-date embedding for
    /// `model`, oldest first.
    pub async fn pending_embeddings(
        &self,
        model: &ModelId,
        limit: u32,
    ) -> Result<Vec<EmbeddingTask>> {
        let rows = sqlx::query(
            "SELECT r.record_id, r.current_data_hash, r.title, v.excerpt, v.data
             FROM records r
             JOIN record_versions v ON v.record_id = r.record_id AND v.version = r.current_version
             WHERE r.state IN ('active', 'stale') AND r.current_version > 0
               AND coalesce((v.use_policy ->> 'embed')::boolean, false)
               AND v.use_policy ->> 'index' = 'full'
               AND NOT EXISTS (
                   SELECT 1 FROM embeddings e
                   WHERE e.owner_type = 'record' AND e.owner_id = r.record_id::text
                     AND e.model_id = $1 AND e.content_hash = r.current_data_hash)
             ORDER BY r.record_id
             LIMIT $2",
        )
        .bind(&*model.0)
        .bind(i64::from(limit.clamp(1, 10_000)))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows
            .iter()
            .map(|r| EmbeddingTask {
                record_id: RecordId(r.get("record_id")),
                content_hash: r.get("current_data_hash"),
                text: embed_text(
                    r.get::<Option<String>, _>("title").as_deref(),
                    r.get::<Option<String>, _>("excerpt").as_deref(),
                    &r.get::<serde_json::Value, _>("data"),
                ),
            })
            .collect())
    }

    /// Stores the embedding of `task` for `model`. A no-op when the record changed or was
    /// tombstoned in the meantime (the next [`PgIndex::pending_embeddings`] picks it up again).
    pub async fn store_embedding(
        &self,
        task: &EmbeddingTask,
        model: &ModelId,
        vec: &[f32],
    ) -> Result<()> {
        let lit = vector_literal(vec)?;
        sqlx::query(
            "INSERT INTO embeddings (owner_type, owner_id, model_id, dim, vec, content_hash)
             SELECT 'record', $1::text, $2, $3, $4::text::halfvec, $5
             FROM records r
             WHERE r.record_id = $6 AND r.current_data_hash = $5
               AND r.state IN ('active', 'stale')
             ON CONFLICT (owner_type, owner_id, model_id) DO UPDATE SET
                dim = EXCLUDED.dim, vec = EXCLUDED.vec, content_hash = EXCLUDED.content_hash,
                created_at = now()",
        )
        .bind(task.record_id.0.to_string())
        .bind(&*model.0)
        .bind(i32::try_from(vec.len()).map_err(|_| Error::Invalid("embedding dim".into()))?)
        .bind(lit)
        .bind(&task.content_hash)
        .bind(task.record_id.0)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }
}
