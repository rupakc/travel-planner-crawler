//! Sources: the spec directory, the adapter registry and the secret source.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use vc_config::Config;
use vc_core::secrets::SecretSource;
use vc_core::sources::{SourceAdapter, SourceSpec};
use vc_core::{Result, SourceId};
use vc_sources::adapters::ResumableAdapter;

/// The loaded source specs, by id.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    by_id: HashMap<String, Arc<SourceSpec>>,
}

impl Sources {
    /// Indexes `specs` by id.
    pub fn new(specs: Vec<SourceSpec>) -> Self {
        Sources {
            by_id: specs
                .into_iter()
                .map(|s| (s.id.0.clone(), Arc::new(s)))
                .collect(),
        }
    }

    /// Loads and validates `<sources.dir>/*.yaml`.
    pub fn load(cfg: &Config) -> Result<Self> {
        Ok(Sources::new(vc_sources::load_dir(Path::new(
            &cfg.sources.dir,
        ))?))
    }

    /// The spec of `id`.
    pub fn get(&self, id: &SourceId) -> Option<Arc<SourceSpec>> {
        self.by_id.get(&id.0).cloned()
    }

    /// Every spec.
    pub fn all(&self) -> Vec<Arc<SourceSpec>> {
        let mut v: Vec<_> = self.by_id.values().cloned().collect();
        v.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        v
    }
}

/// The built-in adapters (dump settings from `[sources]`), by id.
pub fn adapters(cfg: &Config) -> HashMap<&'static str, Arc<dyn SourceAdapter>> {
    vc_sources::adapters::registry_with(&cfg.sources)
        .into_iter()
        .map(|a| {
            let a: Arc<dyn SourceAdapter> = Arc::from(a);
            (a.id(), a)
        })
        .collect()
}

/// The adapter to run for one leased row of `id`. An author-keeping dump (`se_travel_dump`,
/// see [`crate::admin::author::AUTHOR_DUMP_ADAPTERS`]) gets the purged authors' hashes from
/// `tombstones`, loaded now, so a re-ingested dump never stores a purged author's posts (final
/// review B3). Without a pool every adapter runs as registered. A failed load fails the run
/// (it is retried), never runs the dump unfiltered.
pub async fn adapter_for_run(
    cfg: &Config,
    pool: Option<&sqlx::PgPool>,
    id: &str,
    registered: Arc<dyn SourceAdapter>,
) -> Result<Arc<dyn SourceAdapter>> {
    use vc_sources::adapters::se_travel_dump::SeTravelDump;
    let Some(pool) = pool else {
        return Ok(registered);
    };
    if id != SeTravelDump::DEFAULT.id() {
        return Ok(registered);
    }
    let authors: Vec<String> =
        sqlx::query_scalar("SELECT selector FROM tombstones WHERE selector_type = 'author'")
            .fetch_all(pool)
            .await
            .map_err(|e| vc_core::Error::Db(e.to_string()))?;
    let digests = authors.iter().map(|a| crate::admin::author::digest(a));
    Ok(Arc::new(
        SeTravelDump::new(vc_sources::adapters::DumpSettings::from_config(
            &cfg.sources,
        ))
        .with_excluded_authors(digests),
    ))
}

/// The built-in adapters that resume a paged stream from a cursor, by id.
pub fn resumable_adapters() -> HashMap<&'static str, Arc<dyn ResumableAdapter>> {
    vc_sources::adapters::resumable()
        .into_iter()
        .map(|a| (a.id(), a))
        .collect()
}

/// The configured secret source (`[secrets] backend`).
pub fn secrets(cfg: &Config) -> Arc<dyn SecretSource> {
    vc_secrets::from_config(&cfg.secrets)
}
