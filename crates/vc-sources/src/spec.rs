//! Loading and validating `sources/<id>.yaml` (the library half of
//! `vcrawl admin source validate`; the network half — robots, 3 permit-gated sample fetches —
//! lives in the binary).

use std::path::Path;

use regex::Regex;
use vc_core::licence::LicenceClass;
use vc_core::policy::{DenyEntry, Denylist};
use vc_core::sources::{Method, SourceSpec};
use vc_core::{CanonicalUrl, Error, PolitenessKey, Result};

/// Kinds that only `official` sources may produce (spec §17.8).
pub const OFFICIAL_ONLY_KINDS: &[&str] = &["visa_rule", "advisory"];

/// Kinds that `official` sources, or `reference` sources corroborated by a second one, may
/// produce (spec §17.8).
pub const OFFICIAL_OR_REFERENCE_KINDS: &[&str] = &["emergency_numbers"];

/// The default per-key delay; sources may only raise it.
pub const MIN_DELAY_FLOOR_S: u32 = 5;

/// The default daily request budget; sources may only lower it.
pub const DAILY_BUDGET_CEILING: u32 = 3000;

/// Loads and validates every `<dir>/*.yaml`, sorted by id. Fails on the first file that does
/// not parse, and on any validation **error** (warnings are logged). Adapters are checked
/// against the built-in registry; no denylist is applied (use [`validate`] for that).
pub fn load_dir(dir: &Path) -> Result<Vec<SourceSpec>> {
    let ids = crate::adapters::ids();
    let mut specs = Vec::new();
    let mut errors = Vec::new();
    for (stem, spec) in read_dir(dir)? {
        let v = validate(
            &spec,
            &ValidateOpts {
                file_stem: Some(&stem),
                adapters: &ids,
                denylist: None,
            },
        );
        for w in &v.warnings {
            tracing::warn!(source = %spec.id.0, warning = %w, "source spec warning");
        }
        errors.extend(v.errors.iter().map(|e| format!("{}: {e}", spec.id.0)));
        specs.push(spec);
    }
    if !errors.is_empty() {
        return Err(Error::Invalid(errors.join("; ")));
    }
    specs.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    Ok(specs)
}

/// Parses every `<dir>/*.yaml` without validating it; returns `(file stem, spec)` pairs.
pub fn read_dir(dir: &Path) -> Result<Vec<(String, SourceSpec)>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        out.push((stem, load_file(&p)?));
    }
    Ok(out)
}

/// Parses one spec file (unknown fields are rejected).
pub fn load_file(path: &Path) -> Result<SourceSpec> {
    let text = std::fs::read_to_string(path)?;
    parse_spec(&text).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
}

/// Parses one spec from YAML text.
pub fn parse_spec(yaml: &str) -> Result<SourceSpec> {
    serde_norway::from_str(yaml).map_err(|e| Error::Invalid(e.to_string()))
}

/// What [`validate`] checks against.
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidateOpts<'a> {
    /// The file stem the spec was loaded from (must equal the id).
    pub file_stem: Option<&'a str>,
    /// Registered adapter ids.
    pub adapters: &'a [&'a str],
    /// The denylist (`policy/denylist.yaml`), if it should be applied.
    pub denylist: Option<&'a Denylist>,
}

/// The outcome of [`validate`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validation {
    /// Problems that make the spec unusable.
    pub errors: Vec<String>,
    /// Problems that make part of it inert (e.g. an api source without an adapter is not
    /// scheduled) or that should be cleaned up.
    pub warnings: Vec<String>,
}

impl Validation {
    /// No errors.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Validates one spec (static checks only; no network).
pub fn validate(spec: &SourceSpec, opts: &ValidateOpts<'_>) -> Validation {
    let mut v = Validation::default();
    let id = spec.id.0.as_str();
    let id_ok = id.len() >= 2
        && id.len() <= 63
        && id
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !id_ok {
        v.errors
            .push(format!("id {id:?} must match ^[a-z0-9][a-z0-9-]{{1,62}}$"));
    }
    if let Some(stem) = opts.file_stem
        && stem != id
    {
        v.errors
            .push(format!("id {id:?} differs from file name {stem:?}"));
    }
    if spec.name.trim().is_empty() {
        v.errors.push("name is empty".into());
    }
    if spec.licence.attribution.trim().is_empty() {
        v.errors.push("licence.attribution is empty".into());
    }
    // Relevance: the app sections it serves and why; news/blogs must be travel-focused.
    v.errors.extend(crate::relevance::check(spec));
    if spec.licence.class == LicenceClass::Restricted && spec.enabled {
        v.errors
            .push("restricted licence: the source must be enabled: false".into());
    }
    if spec.licence.class == LicenceClass::ApiTerms && spec.enabled {
        if spec.max_retention_days.is_none() {
            v.warnings
                .push("api_terms source without max_retention_days".into());
        }
        if spec.display_requirements.is_none() {
            v.warnings
                .push("api_terms source without display_requirements".into());
        }
    }

    // Method ↔ adapter / URL lists.
    match spec.method {
        Method::Api | Method::Dump => match &spec.adapter {
            Some(a) if !opts.adapters.contains(&a.as_str()) => {
                v.errors.push(format!("adapter {a:?} is not registered"));
            }
            Some(_) => {
                if spec.seeds.is_empty() {
                    v.errors.push("adapter source without seeds".into());
                }
            }
            None if spec.enabled => v
                .warnings
                .push("api/dump source without an adapter: never scheduled".into()),
            None => {}
        },
        m => {
            if spec.adapter.is_some() {
                v.errors.push(format!("adapter set on a {m:?} source"));
            }
            let (list, name) = match m {
                Method::Html => (&spec.seeds, "seeds"),
                Method::Rss => (&spec.feeds, "feeds"),
                _ => (&spec.sitemaps, "sitemaps"),
            };
            if list.is_empty() && spec.enabled {
                v.errors.push(format!("{m:?} source without {name}"));
            }
        }
    }

    // URLs: http(s), canonicalisable, not denylisted.
    for u in spec.seeds.iter().chain(&spec.sitemaps).chain(&spec.feeds) {
        match CanonicalUrl::parse(u.as_str()) {
            Err(e) => v.errors.push(format!("bad URL {u}: {e}")),
            Ok(c) => {
                if let Some(dl) = opts.denylist
                    && spec.enabled
                    && let Some(e) = deny_match(dl, &c)
                {
                    v.errors
                        .push(format!("{u} is denylisted ({})", e.reason.trim()));
                }
            }
        }
    }
    for (name, list) in [("include", &spec.include), ("exclude", &spec.exclude)] {
        for r in list {
            if let Err(e) = Regex::new(r) {
                v.errors.push(format!("{name} regex {r:?}: {e}"));
            }
        }
    }

    // Politeness: sources can only make us slower.
    if spec.min_delay_s < MIN_DELAY_FLOOR_S {
        v.errors.push(format!(
            "min_delay_s {} is below the floor {MIN_DELAY_FLOOR_S}",
            spec.min_delay_s
        ));
    }
    if spec.min_delay_s > 3600 {
        v.errors.push("min_delay_s above 3600".into());
    }
    if spec.daily_budget > DAILY_BUDGET_CEILING {
        v.errors.push(format!(
            "daily_budget {} exceeds the ceiling {DAILY_BUDGET_CEILING}",
            spec.daily_budget
        ));
    }
    if let Some(k) = &spec.politeness_key
        && (k.is_empty() || k.contains('/') || k.contains(' ') || k.to_ascii_lowercase() != *k)
    {
        v.errors
            .push(format!("politeness_key {k:?} is not a lowercase host"));
    }

    // Keys.
    if spec.needs_key && spec.key_secret.is_none() {
        v.errors.push("needs_key without key_secret".into());
    }

    // Kinds: high-stakes kinds by source class (spec §17.8); duplicates.
    let class = spec.source_class.as_str();
    for k in spec.kinds.iter().chain(&spec.llm_kinds) {
        let k = &*k.0;
        if OFFICIAL_ONLY_KINDS.contains(&k) && class != "official" {
            v.errors
                .push(format!("kind {k} needs an official source (is {class})"));
        }
        if OFFICIAL_OR_REFERENCE_KINDS.contains(&k) && !matches!(class, "official" | "reference") {
            v.errors.push(format!(
                "kind {k} needs an official or reference source (is {class})"
            ));
        }
    }
    for (name, list) in [("kinds", &spec.kinds), ("llm_kinds", &spec.llm_kinds)] {
        let mut seen = std::collections::BTreeSet::new();
        for k in list {
            if !seen.insert(&*k.0) {
                v.warnings.push(format!("{name}: duplicate {}", k.0));
            }
        }
    }
    v
}

/// The denylist entry covering `url`, if any (the same rules as `vc-policy`: host and
/// subdomains; `<brand>.*` under any public suffix; `path_prefix`; `except_hosts`).
pub fn deny_match<'a>(dl: &'a Denylist, url: &CanonicalUrl) -> Option<&'a DenyEntry> {
    let host = url.host().to_ascii_lowercase();
    let under = |h: &str, base: &str| h == base || h.ends_with(&format!(".{base}"));
    dl.entries.iter().find(|e| {
        let host_hit = match e.brand() {
            Some(brand) => {
                let reg = PolitenessKey::for_host(&host).0;
                reg.split('.').next() == Some(brand) && reg.contains('.')
            }
            None => under(&host, &e.host.to_ascii_lowercase()),
        };
        host_hit
            && !e
                .except_hosts
                .iter()
                .any(|x| under(&host, &x.to_ascii_lowercase()))
            && e.path_prefix
                .as_deref()
                .is_none_or(|p| url.as_url().path().starts_with(p))
    })
}

/// Parses `policy/denylist.yaml`.
pub fn load_denylist(path: &Path) -> Result<Denylist> {
    let text = std::fs::read_to_string(path)?;
    serde_norway::from_str(&text).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
}
