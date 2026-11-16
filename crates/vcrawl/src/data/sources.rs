//! Source specs as stored in `sources.spec` (seeded by the scheduler), cached briefly so a
//! busy processor does not re-read them per capture.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use sqlx::PgPool;
use vc_core::sources::SourceSpec;
use vc_core::{CanonicalUrl, Error, Result, SourceId};

/// How long a loaded spec is reused.
pub const SOURCE_TTL: Duration = Duration::from_secs(60);

/// A source as the processor needs it.
#[derive(Debug, Clone)]
pub struct LoadedSource {
    /// The spec.
    pub spec: SourceSpec,
    /// `sources.enabled`.
    pub enabled: bool,
    /// Compiled include regexes (empty = everything on the seeds' hosts).
    pub include: Vec<Regex>,
    /// Compiled exclude regexes.
    pub exclude: Vec<Regex>,
    /// Hosts of the seeds, sitemaps and feeds (the default crawl scope).
    pub hosts: Vec<String>,
}

impl LoadedSource {
    /// Builds the derived fields. Invalid regexes are refused (the validator rejects them
    /// before a spec is seeded, so this only guards hand-edited rows).
    pub fn new(spec: SourceSpec, enabled: bool) -> Result<Self> {
        let compile = |v: &[String]| -> Result<Vec<Regex>> {
            v.iter()
                .map(|r| Regex::new(r).map_err(|e| Error::Invalid(format!("source regex: {e}"))))
                .collect()
        };
        let include = compile(&spec.include)?;
        let exclude = compile(&spec.exclude)?;
        let mut hosts: Vec<String> = spec
            .seeds
            .iter()
            .chain(&spec.sitemaps)
            .chain(&spec.feeds)
            .filter_map(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .collect();
        hosts.sort();
        hosts.dedup();
        Ok(LoadedSource {
            spec,
            enabled,
            include,
            exclude,
            hosts,
        })
    }

    /// Whether a discovered link belongs to this source's crawl scope: never excluded; then
    /// any include regex, or (no includes) a seed host.
    pub fn in_scope(&self, url: &CanonicalUrl) -> bool {
        let s = url.as_str();
        if self.exclude.iter().any(|r| r.is_match(s)) {
            return false;
        }
        if !self.include.is_empty() {
            return self.include.iter().any(|r| r.is_match(s));
        }
        let h = url.host();
        self.hosts.iter().any(|x| x.eq_ignore_ascii_case(h))
    }
}

type Entries = HashMap<SourceId, (Instant, Option<Arc<LoadedSource>>)>;

/// A TTL cache over `sources`.
#[derive(Clone)]
pub struct SourceCache {
    pool: PgPool,
    ttl: Duration,
    cache: Arc<Mutex<Entries>>,
}

impl std::fmt::Debug for SourceCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceCache").finish_non_exhaustive()
    }
}

impl SourceCache {
    /// A cache over `pool` with [`SOURCE_TTL`].
    pub fn new(pool: PgPool) -> Self {
        Self::with_ttl(pool, SOURCE_TTL)
    }

    /// A cache with a custom TTL (tests use zero).
    pub fn with_ttl(pool: PgPool, ttl: Duration) -> Self {
        SourceCache {
            pool,
            ttl,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The source, if it exists.
    pub async fn get(&self, id: &SourceId) -> Result<Option<Arc<LoadedSource>>> {
        if let Some((at, v)) = self.lock()?.get(id)
            && at.elapsed() < self.ttl
        {
            return Ok(v.clone());
        }
        let row: Option<(serde_json::Value, bool)> =
            sqlx::query_as("SELECT spec, enabled FROM sources WHERE source_id = $1")
                .bind(&id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| Error::Db(e.to_string()))?;
        let v = match row {
            Some((spec, enabled)) => {
                let spec: SourceSpec = serde_json::from_value(spec)?;
                Some(Arc::new(LoadedSource::new(spec, enabled)?))
            }
            None => None,
        };
        self.lock()?.insert(id.clone(), (Instant::now(), v.clone()));
        Ok(v)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Entries>> {
        self.cache
            .lock()
            .map_err(|_| Error::Invalid("source cache poisoned".into()))
    }
}
