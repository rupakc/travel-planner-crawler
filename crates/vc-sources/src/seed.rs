//! Seeding sources into Postgres (`sources` table, migration 0002) and their seed URLs into the
//! frontier.

use sqlx::PgPool;
use vc_core::frontier::{Frontier, NewUrl};
use vc_core::media::{FetchMode, MediaType};
use vc_core::sources::{Method, RenderMode, SourceSpec};
use vc_core::{CanonicalUrl, Error, PolitenessKey, Result};

use crate::sha256::{Sha256, hex};

/// What [`seed_sources`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeedReport {
    /// New rows.
    pub inserted: u64,
    /// Rows whose spec changed.
    pub updated: u64,
    /// Rows already up to date.
    pub unchanged: u64,
    /// Rows no longer in the directory, now disabled (only with `disable_missing`).
    pub disabled: u64,
}

/// The stable hash of a spec (SHA-256 of its JSON form, hex).
pub fn spec_hash(spec: &SourceSpec) -> Result<String> {
    let json = serde_json::to_vec(spec)?;
    Ok(hex(&Sha256::digest(&json)))
}

/// Upserts every spec into `sources` (one transaction). A row is rewritten only when its
/// `spec_hash` changed. With `disable_missing`, rows whose id is not in `specs` are set
/// `enabled = false` (never deleted: records keep their provenance).
pub async fn seed_sources(
    pool: &PgPool,
    specs: &[SourceSpec],
    disable_missing: bool,
) -> Result<SeedReport> {
    let db = |e: sqlx::Error| Error::Db(e.to_string());
    let mut tx = pool.begin().await.map_err(db)?;
    let mut rep = SeedReport::default();
    for s in specs {
        let hash = spec_hash(s)?;
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT spec_hash FROM sources WHERE source_id = $1 FOR UPDATE")
                .bind(&s.id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;
        match existing {
            Some((h,)) if h == hash => {
                rep.unchanged += 1;
                continue;
            }
            Some(_) => rep.updated += 1,
            None => rep.inserted += 1,
        }
        sqlx::query(
            "INSERT INTO sources (source_id, name, category, source_class, licence_class, licence,
                                  method, politeness_key, enabled, spec, spec_hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (source_id) DO UPDATE SET
               name = EXCLUDED.name, category = EXCLUDED.category,
               source_class = EXCLUDED.source_class, licence_class = EXCLUDED.licence_class,
               licence = EXCLUDED.licence, method = EXCLUDED.method,
               politeness_key = EXCLUDED.politeness_key, enabled = EXCLUDED.enabled,
               spec = EXCLUDED.spec, spec_hash = EXCLUDED.spec_hash, updated_at = now()",
        )
        .bind(&s.id.0)
        .bind(&s.name)
        .bind(&s.category)
        .bind(s.source_class.as_str())
        .bind(s.licence.class.as_str())
        .bind(serde_json::to_value(&s.licence)?)
        .bind(method_str(s.method))
        .bind(&s.politeness_key)
        .bind(s.enabled && s.licence.class.ingestible())
        .bind(serde_json::to_value(s)?)
        .bind(&hash)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    }
    if disable_missing {
        let ids: Vec<String> = specs.iter().map(|s| s.id.0.clone()).collect();
        let r = sqlx::query(
            "UPDATE sources SET enabled = false, updated_at = now()
             WHERE enabled AND NOT (source_id = ANY($1))",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        rep.disabled = r.rows_affected();
    }
    tx.commit().await.map_err(db)?;
    Ok(rep)
}

fn method_str(m: Method) -> &'static str {
    match m {
        Method::Html => "html",
        Method::Rss => "rss",
        Method::Sitemap => "sitemap",
        Method::Api => "api",
        Method::Dump => "dump",
    }
}

/// The frontier rows for a spec's seeds: pages (http or render), feeds, sitemaps, and
/// `adapter:<id>` rows for api/dump sources with a registered adapter. Disabled and
/// non-ingestible sources yield nothing; so do api/dump sources without an adapter.
pub fn seed_urls(spec: &SourceSpec, adapters: &[&str]) -> Result<Vec<NewUrl>> {
    if !spec.enabled || !spec.licence.class.ingestible() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut push = |u: &url::Url, mode: FetchMode, expect: Option<MediaType>| -> Result<()> {
        let url = CanonicalUrl::parse(u.as_str()).map_err(|e| Error::Invalid(e.to_string()))?;
        let key = match &spec.politeness_key {
            Some(k) => PolitenessKey(k.clone()),
            None => url.politeness_key(),
        };
        out.push(NewUrl {
            key,
            source_id: Some(spec.id.clone()),
            discovered_from: None,
            priority: 0.7,
            depth: 0,
            expect,
            fetch_mode: mode,
            not_before: None,
            url,
        });
        Ok(())
    };
    match spec.method {
        Method::Api | Method::Dump => {
            let Some(a) = spec.adapter.as_deref().filter(|a| adapters.contains(a)) else {
                return Ok(Vec::new());
            };
            for u in &spec.seeds {
                push(u, FetchMode::Adapter(a.to_string()), None)?;
            }
        }
        _ => {
            let page_mode = if spec.render == RenderMode::Always {
                FetchMode::Render
            } else {
                FetchMode::Http
            };
            for u in &spec.seeds {
                push(u, page_mode.clone(), None)?;
            }
            for u in &spec.feeds {
                push(u, FetchMode::Http, None)?;
            }
            for u in &spec.sitemaps {
                push(u, FetchMode::Http, Some(MediaType::Sitemap))?;
            }
        }
    }
    Ok(out)
}

/// Enqueues the seeds of every spec (`ON CONFLICT DO NOTHING`); returns the number of new
/// URLs.
pub async fn seed_frontier(frontier: &dyn Frontier, specs: &[SourceSpec]) -> Result<u64> {
    let ids = crate::adapters::ids();
    let mut batch = Vec::new();
    for s in specs {
        batch.extend(seed_urls(s, &ids)?);
    }
    let mut n = 0;
    for chunk in batch.chunks(500) {
        n += frontier.enqueue(chunk).await?;
    }
    Ok(n)
}
