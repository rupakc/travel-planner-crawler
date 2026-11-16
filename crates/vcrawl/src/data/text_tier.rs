//! The extracted-text tier (spec §5.5): for every committed segment, each capture's main text
//! and structured data go to `text/YYYY/MM/<segment-ULID>.jsonl.zst` (one JSON line per
//! capture, ordered by capture id), read later by re-extraction instead of Coldline WARCs.
//! Captures whose raw use is ephemeral (`noarchive`, from fetch-time or body signals) and every
//! capture of an ephemeral segment go to `ephemeral/text/…` instead, so the 30-day lifecycle
//! covers them.
//!
//! The writer is independent of per-capture processing: it claims segments without a written
//! text object (`text_tier_segments`, a lease so a crashed writer's segment is retried), reads
//! the segment once, parses each primary 2xx capture with a stored body (revisits repeat an
//! earlier body and are skipped; restricted or unknown sources are skipped as in the
//! processor) and writes create-if-absent, so it is idempotent across instances and retries.
//! Logs carry ids and counts only.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use vc_core::parse::{HandlerRegistry, Limits};
use vc_core::store::{BlobStore, CaptureRole};
use vc_core::usepolicy::{RawUse, UsePolicy};
use vc_core::{Error, Result, SegmentId};

use crate::data::capture::{self, StoredCapture};
use crate::data::sources::SourceCache;

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

/// One line of a text-tier object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextRow {
    /// Capture id (ULID).
    pub capture_id: String,
    /// Capture time (partition key).
    pub captured_at: DateTime<Utc>,
    /// Canonical URL.
    pub url: String,
    /// URL key (hex).
    pub url_key: String,
    /// Source.
    pub source_id: String,
    /// Media type fixed at fetch time.
    pub media_type: String,
    /// HTTP status.
    pub status: Option<u16>,
    /// Document title.
    pub title: Option<String>,
    /// Detected language.
    pub lang: Option<String>,
    /// Main text (capped by the parser limits).
    pub main_text: Option<String>,
    /// Structured data (JSON-LD / microdata objects).
    pub structured: Vec<serde_json::Value>,
}

/// What writing one segment did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextReport {
    /// Rows written to `text/…`.
    pub archive_rows: usize,
    /// Rows written to `ephemeral/text/…`.
    pub ephemeral_rows: usize,
}

/// The text-tier writer.
pub struct TextTier {
    pool: PgPool,
    blobs: Arc<dyn BlobStore>,
    handlers: HandlerRegistry,
    limits: Limits,
    sources: SourceCache,
    zstd_level: i32,
    lease: Duration,
}

impl std::fmt::Debug for TextTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextTier").finish_non_exhaustive()
    }
}

impl TextTier {
    /// A writer with a 10-minute claim lease.
    pub fn new(
        pool: PgPool,
        blobs: Arc<dyn BlobStore>,
        handlers: HandlerRegistry,
        limits: Limits,
        zstd_level: i32,
    ) -> Self {
        let sources = SourceCache::new(pool.clone());
        TextTier {
            pool,
            blobs,
            handlers,
            limits,
            sources,
            zstd_level,
            lease: Duration::from_secs(600),
        }
    }

    /// Replaces the source cache (tests use a zero TTL).
    pub fn with_sources(mut self, s: SourceCache) -> Self {
        self.sources = s;
        self
    }

    /// Claims up to `n` committed segments without a text object and writes them. Returns the
    /// number written. A failed segment keeps its claim until the lease runs out (then retried).
    pub async fn tick(&self, n: usize) -> Result<usize> {
        let ids = self.claim(n.max(1)).await?;
        let mut done = 0;
        for id in ids {
            match self.write_segment(&id).await {
                Ok(_) => done += 1,
                Err(e) => tracing::warn!(segment = %id, error_class = ?e.class(), error = %e,
                    "text tier: segment failed; retried after the lease"),
            }
        }
        Ok(done)
    }

    async fn claim(&self, n: usize) -> Result<Vec<String>> {
        let lease_s = self.lease.as_secs_f64();
        sqlx::query_scalar(
            "WITH cand AS (
                SELECT s.segment_id FROM segments s
                LEFT JOIN text_tier_segments t ON t.segment_id = s.segment_id
                WHERE s.purged_at IS NULL
                  AND (t.segment_id IS NULL
                       OR (t.written_at IS NULL
                           AND t.claimed_at < now() - make_interval(secs => $2)))
                ORDER BY s.committed_at, s.segment_id
                LIMIT $1)
             INSERT INTO text_tier_segments (segment_id, claimed_at)
             SELECT segment_id, now() FROM cand
             ON CONFLICT (segment_id) DO UPDATE SET claimed_at = now()
             WHERE text_tier_segments.written_at IS NULL
               AND text_tier_segments.claimed_at < now() - make_interval(secs => $2)
             RETURNING segment_id",
        )
        .bind(i64::try_from(n).unwrap_or(i64::MAX))
        .bind(lease_s)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// Writes the text object(s) of one segment and marks it written. Idempotent.
    pub async fn write_segment(&self, segment_id: &str) -> Result<TextReport> {
        let seg: Option<(String, bool)> = sqlx::query_as(
            "SELECT path, ephemeral FROM segments WHERE segment_id = $1 AND purged_at IS NULL",
        )
        .bind(segment_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        let mut rep = TextReport::default();
        if let Some((path, seg_ephemeral)) = seg {
            let sid: SegmentId = segment_id
                .parse()
                .map_err(|_| Error::Invalid("segment id".into()))?;
            let caps = capture::of_segment(&self.pool, segment_id).await?;
            let bytes = if caps.is_empty() {
                None
            } else {
                Some(self.blobs.get(&path, None).await?)
            };
            let (mut archive, mut ephemeral) = (Vec::new(), Vec::new());
            for c in &caps {
                let Some(bytes) = &bytes else { break };
                let Some((row, policy)) = self.row(c, bytes).await? else {
                    continue;
                };
                if seg_ephemeral || policy.raw == RawUse::Ephemeral {
                    ephemeral.push(row);
                } else {
                    archive.push(row);
                }
            }
            rep.archive_rows = archive.len();
            rep.ephemeral_rows = ephemeral.len();
            self.put(sid, false, &archive).await?;
            self.put(sid, true, &ephemeral).await?;
        }
        sqlx::query(
            "INSERT INTO text_tier_segments (segment_id, written_at, archive_rows, ephemeral_rows)
             VALUES ($1, now(), $2, $3)
             ON CONFLICT (segment_id) DO UPDATE SET written_at = now(),
                archive_rows = EXCLUDED.archive_rows, ephemeral_rows = EXCLUDED.ephemeral_rows",
        )
        .bind(segment_id)
        .bind(i32::try_from(rep.archive_rows).unwrap_or(i32::MAX))
        .bind(i32::try_from(rep.ephemeral_rows).unwrap_or(i32::MAX))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        tracing::debug!(
            segment = %segment_id,
            archive_rows = rep.archive_rows,
            ephemeral_rows = rep.ephemeral_rows,
            "text tier written"
        );
        Ok(rep)
    }

    async fn put(&self, sid: SegmentId, ephemeral: bool, rows: &[TextRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        match vc_store::text::put_text_tier(
            self.blobs.as_ref(),
            sid,
            ephemeral,
            rows,
            self.zstd_level,
        )
        .await
        {
            Ok(_) => Ok(()),
            // Written earlier with other bytes (e.g. a source changed since): keep that one.
            Err(Error::Conflict(_)) => {
                tracing::warn!(segment = %sid, ephemeral, "text tier object exists; kept");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// The row of one capture and its final use policy; `None` when it carries no text.
    async fn row(
        &self,
        c: &StoredCapture,
        seg: &bytes::Bytes,
    ) -> Result<Option<(TextRow, UsePolicy)>> {
        let m = &c.meta;
        if m.role != CaptureRole::Primary
            || !m.status.is_some_and(|s| (200..300).contains(&s))
            || m.revisit_of.is_some()
        {
            return Ok(None);
        }
        let Some(sid) = &m.source_id else {
            return Ok(None);
        };
        let Some(src) = self.sources.get(sid).await? else {
            return Ok(None);
        };
        if !src.spec.licence.class.ingestible() {
            return Ok(None);
        }
        let start = usize::try_from(c.offset).map_err(|_| Error::Invalid("offset".into()))?;
        let end = c
            .offset
            .checked_add(c.length)
            .and_then(|e| usize::try_from(e).ok())
            .filter(|e| *e <= seg.len())
            .ok_or_else(|| Error::Invalid("capture range outside its segment".into()))?;
        let recs = vc_store::warc::read_records(&vc_store::segment::decompress(&seg[start..end])?)?;
        let Some(body) = capture::payload_of(&recs)? else {
            return Ok(None);
        };
        let Some(parsed) = vc_parse::parse(&self.handlers, m, &body, &self.limits)? else {
            return Ok(None);
        };
        let policy = m
            .use_policy_fetch
            .meet(&UsePolicy::from_signals(&parsed.body_signals));
        let media = serde_json::to_value(m.media_type)?
            .as_str()
            .map(str::to_string)
            .unwrap_or_default();
        Ok(Some((
            TextRow {
                capture_id: m.capture_id.to_string(),
                captured_at: m.captured_at,
                url: m.url.as_str().to_string(),
                url_key: m.url_key.to_hex(),
                source_id: sid.0.clone(),
                media_type: media,
                status: m.status,
                title: parsed.title,
                lang: parsed.lang,
                main_text: parsed.main_text,
                structured: parsed.structured,
            },
            policy,
        )))
    }
}
