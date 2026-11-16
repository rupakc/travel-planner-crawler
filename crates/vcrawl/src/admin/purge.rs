//! `vcrawl admin purge`: the opt-out / erasure pipeline (spec §5.6, §5.10 forums, §17.7).
//!
//! A purge is a `purge_requests` row that moves through three stages, each idempotent, so a
//! crash anywhere is repaired by running the request again (the maintenance role does):
//!
//! 1. **index** (SLA 24 h): `RecordSink::tombstone` removes matching records (and observation
//!    points) from the API and index and records a standing exclusion, matching
//!    `held_records` rows are deleted, and matching URLs leave the crawl (`url_state` →
//!    `tombstoned`, which re-discovery never revives); → `index_done`.
//! 2. **raw** (SLA 7 days): every committed capture the selector names is cut out of its
//!    segment (`vc-store` purge rewrite: new generation uploaded → one `vc_rewrite_segment`
//!    transaction → old objects deleted; GCS then drops non-current versions within its 7-day
//!    soft-delete window), its lines are removed from the segment's text-tier objects
//!    (`text/` and `ephemeral/text/`); → `raw_done`.
//!    A crash between a rewrite's commit and the deletion of the old objects is repaired by
//!    `store_garbage` (listed in the rewrite's transaction, collected on every run).
//! 3. **done** (SLA 21 days): once `raw_done_at` is older than the backup/snapshot window
//!    (WAL 14 d, snapshots 7 d) the data is gone from backups too.
//!
//! Raw scope per selector: `url`, `url_prefix`, `host`, `source` → the captures of those
//! URLs; `author` → see [`super::author`] (the name is stored only as a hash, the author's
//! records are scrubbed at the index stage, and every capture of their URLs is cut, later
//! re-captures included; for a dump that is every dump capture); `record` → index only (a
//! page's capture also holds other records; purge the URL to remove raw bytes).

use std::collections::{BTreeMap, HashSet};

use anyhow::Context as _;
use chrono::{DateTime, Duration, Utc};
use clap::Args;
use serde::Serialize;
use sqlx::{PgPool, Row};
use vc_core::index::{PurgeSelector, RecordSink, TombstoneReason};
use vc_core::store::BlobStore;
use vc_core::{CanonicalUrl, CaptureId, CaptureRef, RecordId, SegmentId, SourceId};
use vc_store::{
    PgSegmentCommitter, SegmentRewrite, collect_garbage, finish_rewrite, record_garbage,
    rewrite_segment_without,
};

use super::author;
use crate::Ctx;

/// Replacement for scrubbed author names.
pub const REMOVED: &str = "[removed]";

/// How long after the raw stage backups and snapshots still hold the data (WAL 14 d +
/// snapshots 7 d, spec §17.7).
pub const BACKUP_WINDOW_DAYS: i64 = 21;

/// What to purge (exactly one).
#[derive(Debug, Clone, Default, Args)]
#[group(required = true, multiple = false)]
pub struct SelectorArgs {
    /// One URL (canonicalised).
    #[arg(long)]
    pub url: Option<String>,
    /// Every URL starting with this canonical prefix.
    #[arg(long)]
    pub url_prefix: Option<String>,
    /// Every URL on a host.
    #[arg(long)]
    pub host: Option<String>,
    /// Every record attributed to an author display name (forum dumps; GDPR erasure).
    #[arg(long)]
    pub author: Option<String>,
    /// Every record of a source.
    #[arg(long)]
    pub source: Option<String>,
    /// One record (index only).
    #[arg(long)]
    pub record: Option<i64>,
}

/// `admin purge` arguments.
#[derive(Debug, Clone, Args)]
pub struct PurgeArgs {
    /// The selector.
    #[command(flatten)]
    pub selector: SelectorArgs,
    /// Why: `opt_out` (site or person asked), `purge` (operator), `denylist`, `robots`.
    #[arg(long, default_value = "opt_out")]
    pub reason: String,
    /// Who asked (issue number, e-mail ticket id; no personal data).
    #[arg(long, default_value = "operator")]
    pub requested_by: String,
    /// Stop after the index stage (the maintenance role finishes the raw stage).
    #[arg(long)]
    pub index_only: bool,
}

/// The state of one request after processing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PurgeOutcome {
    /// `purge_requests.id`.
    pub id: i64,
    /// `pending` | `index_done` | `raw_done` | `done`.
    pub state: String,
    /// Records tombstoned by this run.
    pub records_tombstoned: u64,
    /// Captures cut out of segments by this run.
    pub captures_removed: u64,
    /// Segments rewritten (or dropped) by this run.
    pub segments_rewritten: u64,
    /// Stored rows scrubbed of the author name by this run.
    pub rows_scrubbed: u64,
}

impl SelectorArgs {
    /// The selector.
    pub fn selector(&self) -> anyhow::Result<PurgeSelector> {
        Ok(if let Some(u) = &self.url {
            PurgeSelector::Url(CanonicalUrl::parse(u).context("--url")?.key())
        } else if let Some(p) = &self.url_prefix {
            PurgeSelector::UrlPrefix(p.clone())
        } else if let Some(h) = &self.host {
            PurgeSelector::Host(h.trim_end_matches('.').to_ascii_lowercase())
        } else if let Some(a) = &self.author {
            anyhow::ensure!(!a.trim().is_empty(), "--author is empty");
            PurgeSelector::Author(a.clone())
        } else if let Some(s) = &self.source {
            PurgeSelector::Source(SourceId(s.clone()))
        } else if let Some(r) = self.record {
            PurgeSelector::Record(RecordId(r))
        } else {
            anyhow::bail!("one selector is required")
        })
    }
}

/// Parses `--reason`.
pub fn parse_reason(s: &str) -> anyhow::Result<TombstoneReason> {
    Ok(match s {
        "opt_out" => TombstoneReason::OptOut,
        "purge" => TombstoneReason::Purge,
        "denylist" => TombstoneReason::Denylist,
        "robots" => TombstoneReason::Robots,
        other => anyhow::bail!("--reason {other:?}: expected opt_out|purge|denylist|robots"),
    })
}

fn reason_str(r: TombstoneReason) -> &'static str {
    match r {
        TombstoneReason::OptOut => "opt_out",
        TombstoneReason::Robots => "robots",
        TombstoneReason::Denylist => "denylist",
        TombstoneReason::Purge => "purge",
        TombstoneReason::Gone => "gone",
    }
}

/// Runs the command: records the request and processes it now.
pub async fn run(ctx: &Ctx, a: PurgeArgs) -> anyhow::Result<serde_json::Value> {
    let sel = a.selector.selector()?;
    let reason = parse_reason(&a.reason)?;
    let pool = crate::wiring::db::ops_pool(&ctx.cfg, 4).await?;
    let id = request_purge(&pool, &sel, reason, &a.requested_by).await?;
    let out = if a.index_only {
        run_index_stage(&pool, id).await?
    } else {
        let store = vc_store::blob_store_from_config(&ctx.cfg.store).await?;
        process_purge(&pool, store.as_ref(), id, Utc::now()).await?
    };
    Ok(serde_json::to_value(out)?)
}

/// Inserts a `purge_requests` row and returns its id.
pub async fn request_purge(
    pool: &PgPool,
    sel: &PurgeSelector,
    reason: TombstoneReason,
    requested_by: &str,
) -> anyhow::Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO purge_requests (selector, reason, requested_by) VALUES ($1, $2, $3)
         RETURNING id",
    )
    .bind(serde_json::to_value(author::stored_selector(sel))?)
    .bind(reason_str(reason))
    .bind(requested_by)
    .fetch_one(pool)
    .await?;
    tracing::info!(
        purge_id = id,
        reason = reason_str(reason),
        "purge requested"
    );
    Ok(id)
}

struct Request {
    selector: PurgeSelector,
    reason: TombstoneReason,
    state: String,
    raw_done_at: Option<DateTime<Utc>>,
}

async fn load(pool: &PgPool, id: i64) -> anyhow::Result<Request> {
    let row = sqlx::query(
        "SELECT selector, reason, state, raw_done_at FROM purge_requests WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .with_context(|| format!("purge request {id} not found"))?;
    Ok(Request {
        selector: serde_json::from_value(row.get("selector"))?,
        reason: parse_reason(row.get::<String, _>("reason").as_str())?,
        state: row.get("state"),
        raw_done_at: row.get("raw_done_at"),
    })
}

fn outcome(id: i64, state: &str) -> PurgeOutcome {
    PurgeOutcome {
        id,
        state: state.to_string(),
        records_tombstoned: 0,
        captures_removed: 0,
        segments_rewritten: 0,
        rows_scrubbed: 0,
    }
}

/// Stage 1 only (tombstone).
pub async fn run_index_stage(pool: &PgPool, id: i64) -> anyhow::Result<PurgeOutcome> {
    let r = load(pool, id).await?;
    let mut out = outcome(id, &r.state);
    if r.state == "pending" {
        (out.records_tombstoned, out.rows_scrubbed) = index_stage(pool, id, &r).await?;
        out.state = "index_done".into();
    }
    Ok(out)
}

/// Returns `(records tombstoned, rows scrubbed)`.
async fn index_stage(pool: &PgPool, id: i64, r: &Request) -> anyhow::Result<(u64, u64)> {
    let (n, scrubbed) = match &r.selector {
        PurgeSelector::Author(a) => {
            author::index_stage(pool, id, a, r.reason, reason_str(r.reason)).await?
        }
        sel => (
            vc_index::PgIndex::new(pool.clone())
                .tombstone(sel, r.reason)
                .await?,
            0,
        ),
    };
    let held = purge_held(pool, &r.selector).await?;
    if held > 0 {
        tracing::info!(purge_id = id, held, "purge: held records dropped");
    }
    let stopped = stop_crawling(pool, &r.selector).await?;
    if stopped > 0 {
        tracing::info!(purge_id = id, urls = stopped, "purge: URLs left the crawl");
    }
    sqlx::query(
        "UPDATE purge_requests SET state = 'index_done', index_done_at = now()
         WHERE id = $1 AND state = 'pending'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok((n, scrubbed))
}

/// Advances one request as far as it can go at `now`.
pub async fn process_purge(
    pool: &PgPool,
    store: &dyn BlobStore,
    id: i64,
    now: DateTime<Utc>,
) -> anyhow::Result<PurgeOutcome> {
    let mut r = load(pool, id).await?;
    let mut out = outcome(id, &r.state);
    if r.state == "pending" {
        (out.records_tombstoned, out.rows_scrubbed) = index_stage(pool, id, &r).await?;
        r.state = "index_done".into();
    }
    if r.state == "index_done" {
        let (caps, segs) = raw_stage(pool, store, id, &r.selector).await?;
        out.captures_removed = caps;
        out.segments_rewritten = segs;
        sqlx::query(
            "UPDATE purge_requests SET state = 'raw_done', raw_done_at = $2
             WHERE id = $1 AND state = 'index_done'",
        )
        .bind(id)
        .bind(now)
        .execute(pool)
        .await?;
        r.state = "raw_done".into();
        r.raw_done_at = Some(now);
    }
    if r.state == "raw_done"
        && r.raw_done_at
            .is_some_and(|t| t + Duration::days(BACKUP_WINDOW_DAYS) <= now)
    {
        sqlx::query(
            "UPDATE purge_requests SET state = 'done', done_at = $2
             WHERE id = $1 AND state = 'raw_done'",
        )
        .bind(id)
        .bind(now)
        .execute(pool)
        .await?;
        r.state = "done".into();
    }
    out.state = r.state;
    tracing::info!(
        purge_id = id,
        state = %out.state,
        records = out.records_tombstoned,
        captures = out.captures_removed,
        segments = out.segments_rewritten,
        "purge processed"
    );
    Ok(out)
}

/// Puts the URLs a selector names into the terminal `tombstoned` state (a leased one too: its
/// fetch's completion is fenced on the lease epoch and leaves it alone), so they are never
/// fetched again; `Frontier::enqueue` keeps an existing row. Returns the URLs changed.
pub async fn stop_crawling(pool: &PgPool, sel: &PurgeSelector) -> anyhow::Result<u64> {
    let (pred, val) = match sel {
        PurgeSelector::Url(k) => ("u.url_key = decode($1, 'hex')".to_string(), k.to_hex()),
        PurgeSelector::UrlPrefix(p) => ("starts_with(x.url, $1)".to_string(), p.clone()),
        PurgeSelector::Host(h) => (
            "lower(substring(x.url from '^[a-zA-Z][a-zA-Z0-9+.-]*://([^/:?#]+)')) = $1".to_string(),
            h.clone(),
        ),
        PurgeSelector::Source(s) => (
            "(u.source_id = $1 OR x.source_id = $1)".to_string(),
            s.0.clone(),
        ),
        PurgeSelector::Author(_) | PurgeSelector::Record(_) => return Ok(0),
    };
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE url_state u SET state = 'tombstoned', lease_epoch = NULL
         FROM urls x
         WHERE x.url_key = u.url_key AND u.state <> 'tombstoned' AND {pred}"
    )))
    .bind(&val)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Advances every unfinished request (the maintenance role), after deleting leftover purge
/// garbage (old objects of rewrites that crashed before their deletion).
pub async fn process_pending(
    pool: &PgPool,
    store: &dyn BlobStore,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<PurgeOutcome>> {
    if let Err(e) = collect_garbage(pool, store).await {
        tracing::warn!(error = %e, "purge: garbage collection failed; retried later");
    }
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM purge_requests WHERE state <> 'done' ORDER BY id")
            .fetch_all(pool)
            .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        match process_purge(pool, store, id, now).await {
            Ok(o) => out.push(o),
            Err(e) => tracing::warn!(purge_id = id, error = %e, "purge step failed; retried later"),
        }
    }
    Ok(out)
}

/// Deletes the `held_records` rows (reference records waiting for a second source, never
/// served) that the selector names. Returns the rows deleted.
pub async fn purge_held(pool: &PgPool, sel: &PurgeSelector) -> anyhow::Result<u64> {
    let (pred, val) = match sel {
        PurgeSelector::Url(k) => ("h.provenance->>'url_key' = $1".to_string(), k.to_hex()),
        PurgeSelector::UrlPrefix(p) => (
            "starts_with(h.provenance->>'url', $1)".to_string(),
            p.clone(),
        ),
        PurgeSelector::Host(h) => (
            "lower(substring(h.provenance->>'url' from '^[a-zA-Z][a-zA-Z0-9+.-]*://([^/:?#]+)')) = $1"
                .to_string(),
            h.clone(),
        ),
        PurgeSelector::Source(s) => ("h.source_id = $1".to_string(), s.0.clone()),
        PurgeSelector::Author(a) => (author::author_sql("h.record"), author::digest(a)),
        PurgeSelector::Record(_) => return Ok(0),
    };
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM held_records h WHERE {pred}"
    )))
    .bind(&val)
    .execute(pool)
    .await?
    .rows_affected())
}

/// One committed capture to cut out.
#[derive(Debug, Clone)]
pub struct DoomedCapture {
    /// Capture.
    pub capture: CaptureRef,
    /// Its segment.
    pub segment_id: String,
    /// Segment path.
    pub path: String,
    /// CDXJ path.
    pub cdxj_path: String,
}

/// The committed captures a selector names (`None`: the selector has no raw scope).
pub async fn captures_for(
    pool: &PgPool,
    sel: &PurgeSelector,
) -> anyhow::Result<Option<Vec<DoomedCapture>>> {
    let (pred, val) = match sel {
        PurgeSelector::Url(k) => ("c.url_key = decode($1, 'hex')".to_string(), k.to_hex()),
        PurgeSelector::UrlPrefix(p) => ("starts_with(c.url, $1)".to_string(), p.clone()),
        PurgeSelector::Host(h) => (
            "lower(substring(c.url from '^[a-zA-Z][a-zA-Z0-9+.-]*://([^/:?#]+)')) = $1".to_string(),
            h.clone(),
        ),
        PurgeSelector::Source(s) => ("c.source_id = $1".to_string(), s.0.clone()),
        PurgeSelector::Author(a) => {
            let sc = author::find_scope(pool, a).await?;
            return Ok(Some(author::captures(pool, &sc).await?));
        }
        PurgeSelector::Record(_) => return Ok(None),
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT c.capture_id, c.captured_at, s.segment_id, s.path, s.cdxj_path
         FROM captures c JOIN segments s ON s.segment_id = c.segment_id
         WHERE s.purged_at IS NULL AND {pred}
         ORDER BY s.segment_id, c.capture_id"
    )))
    .bind(&val)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(DoomedCapture {
                capture: CaptureRef {
                    capture_id: r.get::<String, _>("capture_id").parse()?,
                    captured_at: r.get("captured_at"),
                },
                segment_id: r.get("segment_id"),
                path: r.get("path"),
                cdxj_path: r.get("cdxj_path"),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(Some)
}

async fn raw_stage(
    pool: &PgPool,
    store: &dyn BlobStore,
    id: i64,
    sel: &PurgeSelector,
) -> anyhow::Result<(u64, u64)> {
    let doomed = match sel {
        // The scope fixed at the index stage (the scrubbed data no longer names the author).
        PurgeSelector::Author(a) => {
            author::captures(pool, &author::scope(pool, id, a).await?).await?
        }
        sel => match captures_for(pool, sel).await? {
            Some(d) => d,
            None => return Ok((0, 0)),
        },
    };
    remove_captures(pool, store, &doomed).await
}

/// Cuts captures out of their segments (one rewrite per segment). Returns `(captures,
/// segments)` removed / rewritten.
pub async fn remove_captures(
    pool: &PgPool,
    store: &dyn BlobStore,
    doomed: &[DoomedCapture],
) -> anyhow::Result<(u64, u64)> {
    let mut by_seg: BTreeMap<&str, Vec<&DoomedCapture>> = BTreeMap::new();
    for d in doomed {
        by_seg.entry(d.segment_id.as_str()).or_default().push(d);
    }
    let committer = PgSegmentCommitter::new(pool.clone());
    let (mut caps, mut segs) = (0, 0);
    for (seg, list) in by_seg {
        let remove: HashSet<CaptureId> = list.iter().map(|d| d.capture.capture_id).collect();
        let (path, cdxj) = (&list[0].path, &list[0].cdxj_path);
        let rw = match rewrite_segment_without(store, path, cdxj, &remove).await {
            Ok(rw) => rw,
            // The objects are already gone (the ephemeral lifecycle rule): every row of the
            // segment points at nothing, so all of them go and the segment is marked purged.
            Err(vc_core::Error::NotFound(_)) => SegmentRewrite {
                segment_id: seg.parse()?,
                old_path: path.clone(),
                old_cdxj_path: cdxj.clone(),
                new_path: None,
                new_cdxj_path: None,
                bytes: 0,
                crc32c: 0,
                record_count: 0,
                kept: vec![],
                removed: segment_captures(pool, seg).await?,
                abandoned: vec![],
            },
            Err(e) => return Err(e.into()),
        };
        let sid: SegmentId = seg.parse()?;
        if committer.commit_rewrite(&rw).await? {
            caps += rw.removed.len() as u64;
            segs += 1;
            fail::fail_point!("purge::after_commit_rewrite", |_| Err(anyhow::anyhow!(
                "failpoint purge::after_commit_rewrite"
            )));
            finish_rewrite(store, &rw).await?;
        } else {
            // Another rewrite of this segment committed first: ours is garbage (never live).
            let ours: Vec<String> = [rw.new_path.clone(), rw.new_cdxj_path.clone()]
                .into_iter()
                .flatten()
                .chain(rw.abandoned.iter().cloned())
                .collect();
            if !ours.is_empty() {
                record_garbage(pool, sid, &ours, "unreferenced").await?;
            }
        }
        // Noarchive captures of an archive segment have their text under `ephemeral/text/`.
        purge_text_tier(store, sid, false, &remove).await?;
        purge_text_tier(store, sid, true, &remove).await?;
    }
    collect_garbage(pool, store).await?;
    Ok((caps, segs))
}

async fn segment_captures(pool: &PgPool, segment: &str) -> anyhow::Result<Vec<CaptureRef>> {
    let rows: Vec<(String, DateTime<Utc>)> =
        sqlx::query_as("SELECT capture_id, captured_at FROM captures WHERE segment_id = $1")
            .bind(segment)
            .fetch_all(pool)
            .await?;
    rows.into_iter()
        .map(|(id, at)| {
            Ok(CaptureRef {
                capture_id: id.parse()?,
                captured_at: at,
            })
        })
        .collect()
}

/// Removes the rows of `remove` from a segment's text-tier object (delete + re-create; the
/// object is create-if-absent by design, and re-extraction tolerates a missing one).
pub async fn purge_text_tier(
    store: &dyn BlobStore,
    segment: SegmentId,
    ephemeral: bool,
    remove: &HashSet<CaptureId>,
) -> anyhow::Result<()> {
    let path = vc_store::paths::text_path(segment, ephemeral);
    let bytes = match store.get(&path, None).await {
        Ok(b) => b,
        Err(vc_core::Error::NotFound(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let rows: Vec<serde_json::Value> = vc_store::text::decode_text_tier(&bytes)?;
    let removed = |v: &serde_json::Value| {
        v.get("capture_id")
            .and_then(|c| c.as_str())
            .and_then(|c| c.parse::<CaptureId>().ok())
            .is_some_and(|c| remove.contains(&c))
    };
    if !rows.iter().any(removed) {
        return Ok(());
    }
    let kept: Vec<_> = rows.into_iter().filter(|v| !removed(v)).collect();
    store.delete(&path).await?;
    if !kept.is_empty() {
        let b = vc_store::text::encode_text_tier(&kept, 3)?;
        vc_store::segment::upload(store, &path, b).await?;
    }
    Ok(())
}
