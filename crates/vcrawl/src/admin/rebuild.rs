//! `vcrawl admin rebuild-captures`: rebuilds `segments` and `captures` rows from the `.cdxj`
//! objects in the raw bucket (spec §5.5: the disaster-recovery path for rows newer than the
//! last backup; also the equivalence check of the restore drill, §12, §17.9).
//!
//! * Lists `cdxj/` and `ephemeral/cdxj/`; for each segment id only the highest purge-rewrite
//!   generation (`<id>.r<N>.cdxj`) counts, so a crash between a rewrite's commit and the
//!   deletion of the old objects never resurrects purged captures.
//! * The segment's size is the end of its last frame; its crc32c is recomputed from the object
//!   itself (which also proves the object exists and matches the index).
//! * Rows are inserted with the same column mapping as `vc_commit_segment`, `ON CONFLICT DO
//!   NOTHING`, without touching `url_state` or the work queue: rebuilt rows are history, not
//!   new captures.
//! * Purges are honoured: objects listed in `store_garbage` (old generations a crash left
//!   behind) are skipped, a segment whose row is purged or names another generation gets no
//!   capture rows from this object, and captures matching a standing exclusion (`tombstones`:
//!   url, url_prefix, host, source) are never inserted, even after a restore lost the rest.
//! * Columns the processor sets later (`use_policy_final`, `text_ref`) are not in the CDXJ and
//!   stay NULL; `committed_at` / `rewritten_at` are the rebuild time.

use std::collections::BTreeMap;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Serialize;
use sqlx::PgPool;
use vc_core::store::{BlobStore, PREFIX_CDXJ, PREFIX_EPHEMERAL, SegmentManifest};
use vc_core::{Error, SegmentId};
use vc_store::cdxj::CdxjEntry;
use vc_store::paths::is_ephemeral;

use crate::Ctx;

/// `admin rebuild-captures` arguments.
#[derive(Debug, Clone, Default, Args)]
pub struct RebuildArgs {
    /// Report what would be inserted without writing.
    #[arg(long)]
    pub dry_run: bool,
    /// Compare the CDXJ-derived rows with the database instead of inserting (restore drill).
    #[arg(long, conflicts_with = "dry_run")]
    pub verify: bool,
    /// Only segments whose CDXJ path starts with this (e.g. `cdxj/2026/11/`).
    #[arg(long)]
    pub prefix: Option<String>,
}

/// What a rebuild did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RebuildReport {
    /// Segments found in the bucket (latest generation each).
    pub segments_seen: u64,
    /// Segments rows inserted.
    pub segments_inserted: u64,
    /// Captures rows inserted.
    pub captures_inserted: u64,
    /// Older rewrite generations ignored.
    pub superseded_generations: u64,
    /// CDXJ files whose segment object is gone (purged): skipped.
    pub missing_objects: Vec<String>,
    /// Segments skipped because a purge made their object obsolete (`store_garbage`, a purged
    /// row, or a row naming another generation).
    pub purged_skipped: Vec<String>,
    /// Verify mode: differences between the bucket and the database.
    pub mismatches: Vec<String>,
}

/// One segment as reconstructed from its CDXJ.
#[derive(Debug, Clone)]
pub struct RebuiltSegment {
    /// The manifest (same shape as the `vc_commit_segment` argument).
    pub manifest: SegmentManifest,
    /// Purge-rewrite generation (0 = original).
    pub generation: u32,
}

/// Runs the command.
pub async fn run(ctx: &Ctx, a: RebuildArgs) -> anyhow::Result<serde_json::Value> {
    let pool = crate::wiring::db::ops_pool(&ctx.cfg, 4).await?;
    let store = vc_store::blob_store_from_config(&ctx.cfg.store).await?;
    let rep = rebuild_captures(&pool, store.as_ref(), &a).await?;
    Ok(serde_json::to_value(rep)?)
}

/// `(segment id, generation)` of a CDXJ object path (`…/<ulid>[.r<N>].cdxj`).
pub fn parse_cdxj_path(path: &str) -> Option<(SegmentId, u32)> {
    let file = path.rsplit('/').next()?.strip_suffix(".cdxj")?;
    let (id, generation) = match file.split_once('.') {
        None => (file, 0),
        Some((id, g)) => (id, g.strip_prefix('r')?.parse().ok()?),
    };
    Some((id.parse().ok()?, generation))
}

/// Lists the latest-generation CDXJ object of every segment under `prefix` (or everywhere).
/// Returns `(segment id → (generation, path))` and the number of superseded generations.
pub async fn latest_cdxj(
    store: &dyn BlobStore,
    prefix: Option<&str>,
) -> anyhow::Result<(BTreeMap<SegmentId, (u32, String)>, u64)> {
    let mut paths = store.list(PREFIX_CDXJ).await?;
    paths.extend(
        store
            .list(&format!("{PREFIX_EPHEMERAL}{PREFIX_CDXJ}"))
            .await?,
    );
    let mut latest: BTreeMap<SegmentId, (u32, String)> = BTreeMap::new();
    let mut superseded = 0;
    for p in paths {
        if prefix.is_some_and(|pre| !p.starts_with(pre)) {
            continue;
        }
        let Some((id, generation)) = parse_cdxj_path(&p) else {
            tracing::warn!(path = %p, "rebuild: not a segment cdxj, skipped");
            continue;
        };
        match latest.get(&id) {
            Some((g, _)) if *g >= generation => superseded += 1,
            Some(_) => {
                superseded += 1;
                latest.insert(id, (generation, p));
            }
            None => {
                latest.insert(id, (generation, p));
            }
        }
    }
    Ok((latest, superseded))
}

/// Reconstructs one segment's manifest from its CDXJ and object. `Ok(None)` when the segment
/// object no longer exists (a fully purged segment whose index outlived it).
pub async fn segment_from_cdxj(
    store: &dyn BlobStore,
    id: SegmentId,
    generation: u32,
    cdxj_path: &str,
) -> anyhow::Result<Option<RebuiltSegment>> {
    let entries: Vec<CdxjEntry> = vc_store::read_cdxj(store, cdxj_path).await?;
    let Some(first) = entries.first() else {
        return Ok(None);
    };
    let path = first.filename.clone();
    anyhow::ensure!(
        entries.iter().all(|e| e.filename == path),
        "{cdxj_path}: entries name different segment files"
    );
    let object = match store.get(&path, None).await {
        Ok(b) => b,
        Err(Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e).context(format!("read {path}")),
    };
    let bytes = entries
        .iter()
        .map(|e| e.capture.offset + e.capture.length)
        .max()
        .unwrap_or(0);
    anyhow::ensure!(
        bytes == object.len() as u64,
        "{path}: object is {} bytes, its index ends at {bytes}",
        object.len()
    );
    let mut captures: Vec<_> = entries.iter().map(|e| e.capture.clone()).collect();
    captures.sort_by_key(|c| c.offset);
    Ok(Some(RebuiltSegment {
        manifest: SegmentManifest {
            segment_id: id,
            ephemeral: is_ephemeral(&path),
            path,
            cdxj_path: cdxj_path.to_string(),
            bytes,
            crc32c: crc32c::crc32c(&object),
            record_count: entries.iter().map(|e| e.records).sum(),
            created_at: id.datetime(),
            captures,
        },
        generation,
    }))
}

/// Rebuilds (or with `verify`, compares) every segment found in the bucket.
pub async fn rebuild_captures(
    pool: &PgPool,
    store: &dyn BlobStore,
    a: &RebuildArgs,
) -> anyhow::Result<RebuildReport> {
    let (latest, superseded) = latest_cdxj(store, a.prefix.as_deref()).await?;
    let mut rep = RebuildReport {
        superseded_generations: superseded,
        ..Default::default()
    };
    for (id, (generation, cdxj)) in latest {
        let Some(seg) = segment_from_cdxj(store, id, generation, &cdxj).await? else {
            rep.missing_objects.push(cdxj);
            continue;
        };
        rep.segments_seen += 1;
        if obsolete(pool, &seg).await? {
            rep.purged_skipped.push(seg.manifest.path.clone());
            continue;
        }
        if a.verify {
            rep.mismatches.extend(diff_segment(pool, &seg).await?);
        } else if a.dry_run {
            rep.captures_inserted += seg.manifest.captures.len() as u64;
        } else {
            let (s, c) = insert_segment(pool, &seg).await?;
            rep.segments_inserted += s;
            rep.captures_inserted += c;
        }
    }
    tracing::info!(
        segments = rep.segments_seen,
        inserted = rep.captures_inserted,
        mismatches = rep.mismatches.len(),
        "rebuild-captures done"
    );
    Ok(rep)
}

/// Whether a purge made this object obsolete: it is listed as garbage, or the segment's row is
/// purged or points at another generation.
async fn obsolete(pool: &PgPool, seg: &RebuiltSegment) -> anyhow::Result<bool> {
    let m = &seg.manifest;
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM store_garbage WHERE path = $2 OR path = $3)
             OR EXISTS (SELECT 1 FROM segments WHERE segment_id = $1
                        AND (purged_at IS NOT NULL OR path <> $2))",
    )
    .bind(m.segment_id.to_string())
    .bind(&m.path)
    .bind(&m.cdxj_path)
    .fetch_one(pool)
    .await?)
}

/// Inserts one segment and its captures (one transaction). Returns `(segments, captures)`
/// inserted; existing rows are left alone.
pub async fn insert_segment(pool: &PgPool, seg: &RebuiltSegment) -> anyhow::Result<(u64, u64)> {
    let m = serde_json::to_value(&seg.manifest)?;
    let mut tx = pool.begin().await?;
    let s = sqlx::query(
        "INSERT INTO segments (segment_id, path, cdxj_path, bytes, crc32c, record_count,
                               ephemeral, created_at, rewritten_at)
         VALUES ($1->>'segment_id', $1->>'path', $1->>'cdxj_path', ($1->>'bytes')::bigint,
                 ($1->>'crc32c')::bigint, ($1->>'record_count')::integer,
                 coalesce(($1->>'ephemeral')::boolean, false), ($1->>'created_at')::timestamptz,
                 CASE WHEN $2 > 0 THEN now() END)
         ON CONFLICT (segment_id) DO NOTHING",
    )
    .bind(&m)
    .bind(i64::from(seg.generation))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // The same mapping as vc_commit_segment's captures insert (migration 0005).
    let c = sqlx::query(
        "INSERT INTO captures (capture_id, captured_at, segment_id, warc_offset, warc_length,
                               warc_record_id, url_key, url, key, source_id, role, fetch_mode,
                               epoch, orphan_epoch, status, media_type, payload_hash, payload_len,
                               etag, last_modified, revisit_of_id, revisit_of_at, use_signals,
                               use_policy_fetch, raw_expires_at)
         SELECT c->>'capture_id', (c->>'captured_at')::timestamptz, $1->>'segment_id',
                (c->>'offset')::bigint, (c->>'length')::bigint, c->>'warc_record_id',
                decode(c->>'url_key', 'hex'), c->>'url', c->>'key', c->>'source_id',
                c->>'role', c->>'fetch_mode', (c->>'epoch')::bigint,
                coalesce((c->>'orphan_epoch')::boolean, false), (c->>'status')::smallint,
                c->>'media_type',
                CASE WHEN c->>'payload_hash' IS NULL THEN NULL
                     ELSE decode(c->>'payload_hash', 'hex') END,
                coalesce((c->>'payload_len')::bigint, 0), c->'validators'->>'etag',
                c->'validators'->>'last_modified', c->'revisit_of'->>'capture_id',
                (c->'revisit_of'->>'captured_at')::timestamptz,
                coalesce(c->'use_signals', '{}'::jsonb), c->'use_policy_fetch',
                (c->>'raw_expires_at')::timestamptz
         FROM jsonb_array_elements(coalesce($1->'captures', '[]'::jsonb)) AS x(c)
         WHERE NOT EXISTS (
             SELECT 1 FROM tombstones t
             WHERE (t.selector_type = 'url' AND t.selector = c->>'url_key')
                OR (t.selector_type = 'url_prefix' AND starts_with(c->>'url', t.selector))
                OR (t.selector_type = 'host' AND t.selector = lower(substring(c->>'url'
                        from '^[a-zA-Z][a-zA-Z0-9+.-]*://([^/:?#]+)')))
                OR (t.selector_type = 'source' AND t.selector = c->>'source_id'))
         ON CONFLICT DO NOTHING",
    )
    .bind(&m)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok((s, c))
}

/// Differences between a rebuilt segment and its database rows (restore-drill equivalence).
pub async fn diff_segment(pool: &PgPool, seg: &RebuiltSegment) -> anyhow::Result<Vec<String>> {
    let m = &seg.manifest;
    let id = m.segment_id.to_string();
    let mut out = Vec::new();
    let row: Option<(String, String, i64, i64, i32, bool)> = sqlx::query_as(
        "SELECT path, cdxj_path, bytes, crc32c, record_count, ephemeral
         FROM segments WHERE segment_id = $1",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await?;
    let want = (
        m.path.clone(),
        m.cdxj_path.clone(),
        m.bytes as i64,
        i64::from(m.crc32c),
        m.record_count as i32,
        m.ephemeral,
    );
    match row {
        None => out.push(format!("segment {id}: missing row")),
        Some(r) if r != want => out.push(format!("segment {id}: row differs from the bucket")),
        Some(_) => {}
    }
    let rows: Vec<(String, DateTime<Utc>, i64, i64)> = sqlx::query_as(
        "SELECT capture_id, captured_at, warc_offset, warc_length
         FROM captures WHERE segment_id = $1 ORDER BY warc_offset",
    )
    .bind(&id)
    .fetch_all(pool)
    .await?;
    let want: Vec<(String, DateTime<Utc>, i64, i64)> = m
        .captures
        .iter()
        .map(|c| {
            (
                c.meta.capture_id.to_string(),
                c.meta.captured_at,
                c.offset as i64,
                c.length as i64,
            )
        })
        .collect();
    if rows != want {
        out.push(format!(
            "segment {id}: {} capture rows, bucket has {} (or offsets differ)",
            rows.len(),
            want.len()
        ));
    }
    Ok(out)
}
