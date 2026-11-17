//! The go-live source allowlist (`[vcrawl] source_allowlist`, docs/runbooks/go-live.md §3).
//!
//! A ramp file (e.g. `sources/ramp-1.txt`): one source id per line, `#` starts a comment,
//! blank lines are ignored. When configured, the scheduler seeds and enqueues only these
//! sources and the fetcher holds back claimed URLs of every other source, so a fresh install
//! crawls the ramp and nothing else until the operator removes the setting.
//!
//! Fail closed: a configured file that cannot be read is an error (the scheduler pass fails,
//! the fetcher refuses to start), never "allow everything".

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use vc_config::Config;
use vc_core::{Error, Result, SourceId};

/// The source ids allowed to crawl during a ramp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAllowlist {
    path: PathBuf,
    ids: BTreeSet<String>,
}

impl SourceAllowlist {
    /// The ids in a ramp file's text.
    pub fn parse(text: &str) -> BTreeSet<String> {
        text.lines()
            .map(|l| l.split('#').next().unwrap_or(""))
            .filter_map(|l| l.split_whitespace().next())
            .map(str::to_string)
            .collect()
    }

    /// An allowlist that allows nothing (fail-closed fallback).
    pub fn empty() -> Self {
        SourceAllowlist {
            path: PathBuf::new(),
            ids: BTreeSet::new(),
        }
    }

    /// The configured allowlist: `Ok(None)` when unset or empty (no ramp).
    pub fn load(cfg: &Config) -> Result<Option<Self>> {
        let Some(raw) = cfg
            .vcrawl
            .source_allowlist
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(None);
        };
        let path = Self::resolve(cfg, raw);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            Error::Invalid(format!(
                "vcrawl.source_allowlist {}: {e} (unset it to crawl every enabled source)",
                path.display()
            ))
        })?;
        Ok(Some(SourceAllowlist {
            ids: Self::parse(&text),
            path,
        }))
    }

    fn resolve(cfg: &Config, raw: &str) -> PathBuf {
        let p = Path::new(raw);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(&cfg.sources.dir).join(p)
        }
    }

    /// Whether `source` may crawl. URLs without a source are held too.
    pub fn allows(&self, source: Option<&SourceId>) -> bool {
        source.is_some_and(|s| self.ids.contains(&s.0))
    }

    /// Whether the id `id` is listed.
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// The listed ids.
    pub fn ids(&self) -> &BTreeSet<String> {
        &self.ids
    }

    /// The file it was read from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ids_comments_and_blank_lines() {
        let ids = SourceAllowlist::parse("# head\n\na-src   # why\n  b-src\n#c-src\n");
        assert_eq!(ids.into_iter().collect::<Vec<_>>(), ["a-src", "b-src"]);
    }

    #[test]
    fn unset_or_blank_is_off_and_missing_file_fails_closed() {
        let mut cfg = Config::default();
        assert_eq!(SourceAllowlist::load(&cfg).unwrap(), None);
        cfg.vcrawl.source_allowlist = Some("  ".into());
        assert_eq!(SourceAllowlist::load(&cfg).unwrap(), None);
        cfg.vcrawl.source_allowlist = Some("/nonexistent/ramp.txt".into());
        assert!(SourceAllowlist::load(&cfg).is_err());
    }

    #[test]
    fn relative_path_resolves_against_sources_dir_and_none_source_is_held() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ramp.txt"), "a-src\n").unwrap();
        let mut cfg = Config::default();
        cfg.sources.dir = dir.path().display().to_string();
        cfg.vcrawl.source_allowlist = Some("ramp.txt".into());
        let a = SourceAllowlist::load(&cfg).unwrap().unwrap();
        assert!(a.allows(Some(&SourceId("a-src".into()))));
        assert!(!a.allows(Some(&SourceId("b-src".into()))));
        assert!(!a.allows(None));
    }

    #[test]
    fn the_repository_ramp_file_lists_ten_existing_sources() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../sources");
        let text = std::fs::read_to_string(format!("{dir}/ramp-1.txt")).unwrap();
        let ids = SourceAllowlist::parse(&text);
        assert_eq!(ids.len(), 10, "{ids:?}");
        for id in &ids {
            assert!(
                Path::new(&format!("{dir}/{id}.yaml")).exists(),
                "{id} has no spec"
            );
        }
    }
}
