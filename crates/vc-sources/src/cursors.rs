//! Persisted adapter resume cursors (table `adapter_cursors`, migration 1701).
//!
//! One row per leased `adapter:<id>` URL row. The fetcher loads the cursor before a
//! [`ResumableAdapter`](crate::adapters::ResumableAdapter) run, saves the report's cursor
//! when the run stopped early and deletes it when the run completed, so a restart continues
//! the stream. A cursor saved by a different adapter id is ignored (the row changed hands).

use sqlx::PgPool;
use vc_core::ids::UrlKey;
use vc_core::{Error, Result};

/// The longest cursor kept (the column's CHECK); longer ones are not saved (start over).
pub const MAX_CURSOR_LEN: usize = 2048;

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

/// The Postgres cursor store.
#[derive(Debug, Clone)]
pub struct PgAdapterCursors {
    pool: PgPool,
}

impl PgAdapterCursors {
    /// A store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        PgAdapterCursors { pool }
    }

    /// The cursor saved for `url` by `adapter`, if any.
    pub async fn load(&self, url: &UrlKey, adapter: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT cursor FROM adapter_cursors WHERE url_key = $1 AND adapter = $2")
            .bind(url.0.as_slice())
            .bind(adapter)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }

    /// Saves (replaces) the cursor of `url`. Returns false when the cursor is too long to keep
    /// (any older cursor is deleted then, so the next run starts over).
    pub async fn save(&self, url: &UrlKey, adapter: &str, cursor: &str) -> Result<bool> {
        if cursor.len() > MAX_CURSOR_LEN {
            self.clear(url).await?;
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO adapter_cursors (url_key, adapter, cursor, updated_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (url_key) DO UPDATE
             SET adapter = EXCLUDED.adapter, cursor = EXCLUDED.cursor, updated_at = now()",
        )
        .bind(url.0.as_slice())
        .bind(adapter)
        .bind(cursor)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(true)
    }

    /// Deletes the cursor of `url` (the stream completed).
    pub async fn clear(&self, url: &UrlKey) -> Result<()> {
        sqlx::query("DELETE FROM adapter_cursors WHERE url_key = $1")
            .bind(url.0.as_slice())
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    /// Deletes cursors whose URL row left the crawl (no `url_state` row, or `disabled`) and
    /// cursors not updated for `max_age_days`. Returns the number removed.
    pub async fn prune(&self, max_age_days: i32) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM adapter_cursors c
             WHERE c.updated_at < now() - make_interval(days => $1)
                OR NOT EXISTS (SELECT 1 FROM url_state s
                               WHERE s.url_key = c.url_key AND s.state <> 'disabled')",
        )
        .bind(max_age_days)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(r.rows_affected())
    }
}
