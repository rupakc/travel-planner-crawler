//! # vc-config
//!
//! The crawler's configuration: defaults → TOML file → `VC_*` environment variables (later wins).
//! **Every setting has a default**, so an empty file and no env vars yield a working
//! development config. No secrets live here: only the *names* of Secret Manager secrets.
//!
//! ## Environment variables
//!
//! `VC_<SECTION>_<FIELD>` sets `<section>.<field>` (case-insensitive), e.g. `VC_LLM_ENABLED=true`,
//! `VC_LLM_DAILY_USD=1.0`, `VC_DATABASE_URL=postgres://…`, `VC_FRONTIER_SHARDS=[0,1]`. Deeper
//! nesting uses `__`: `VC_LLM_SHARES__SCOUT=0.05`. Aliases: `VC_PROJECT_CAP_USD` →
//! `llm.project_cap_usd`. Variables whose section is unknown (e.g. `VC_SECRET_*` for the env
//! secret backend, `VC_CONFIG`) are ignored. `VC_CONFIG` names the TOML file.
//!
//! Each crate owns one file in [`sections`] (r5 Q16); agents edit only their own.

pub mod sections;

use std::path::Path;

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

pub use sections::api::ApiConfig;
pub use sections::database::DatabaseConfig;
pub use sections::extract::ExtractConfig;
pub use sections::fetch::FetchConfig;
pub use sections::frontier::FrontierConfig;
pub use sections::index::IndexConfig;
pub use sections::kinds::KindsConfig;
pub use sections::llm::{LlmConfig, PurposeShares};
pub use sections::obs::ObsConfig;
pub use sections::parse::ParseConfig;
pub use sections::policy::PolicyConfig;
pub use sections::queue::QueueConfig;
pub use sections::render::RenderConfig;
pub use sections::secrets::{SecretBackend, SecretsConfig};
pub use sections::sources::SourcesConfig;
pub use sections::store::{BlobBackend, StoreConfig};
pub use sections::vcrawl::VcrawlConfig;

/// Section names recognised in `VC_<SECTION>_<FIELD>`.
pub const SECTIONS: &[&str] = &[
    "database", "policy", "frontier", "fetch", "render", "store", "parse", "kinds", "llm",
    "extract", "sources", "queue", "index", "api", "obs", "secrets", "vcrawl",
];

/// Env aliases: `VC_<ALIAS>` → key path.
pub const ALIASES: &[(&str, &str)] = &[("project_cap_usd", "llm.project_cap_usd")];

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A file or env value could not be parsed, or a key is unknown.
    #[error("config: {0}")]
    Load(#[from] Box<figment::Error>),
    /// A value is out of range or inconsistent.
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// The whole configuration (one field per section).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `[database]`.
    pub database: DatabaseConfig,
    /// `[policy]` (vc-policy).
    pub policy: PolicyConfig,
    /// `[frontier]` (vc-frontier).
    pub frontier: FrontierConfig,
    /// `[fetch]` (vc-fetch).
    pub fetch: FetchConfig,
    /// `[render]` (vc-render).
    pub render: RenderConfig,
    /// `[store]` (vc-store).
    pub store: StoreConfig,
    /// `[parse]` (vc-parse).
    pub parse: ParseConfig,
    /// `[kinds]` (vc-kinds).
    pub kinds: KindsConfig,
    /// `[llm]` (vc-llm).
    pub llm: LlmConfig,
    /// `[extract]` (vc-extract).
    pub extract: ExtractConfig,
    /// `[sources]` (vc-sources).
    pub sources: SourcesConfig,
    /// `[queue]` (vc-queue).
    pub queue: QueueConfig,
    /// `[index]` (vc-index).
    pub index: IndexConfig,
    /// `[api]` (vc-api).
    pub api: ApiConfig,
    /// `[obs]` (vc-obs).
    pub obs: ObsConfig,
    /// `[secrets]` (vc-secrets).
    pub secrets: SecretsConfig,
    /// `[vcrawl]` (vcrawl).
    pub vcrawl: VcrawlConfig,
}

/// Maps an env key (prefix stripped, any case) to a config key path, or `None` to ignore it.
pub fn env_key_path(key: &str) -> Option<String> {
    let k = key.to_ascii_lowercase();
    if let Some((_, path)) = ALIASES.iter().find(|(a, _)| *a == k) {
        return Some((*path).to_string());
    }
    let (section, rest) = k.split_once('_')?;
    if rest.is_empty() || !SECTIONS.contains(&section) {
        return None;
    }
    Some(format!("{section}.{}", rest.replace("__", ".")))
}

impl Config {
    /// The figment: defaults, then `toml_path` (if given and present), then `VC_*` env.
    pub fn figment(toml_path: Option<&Path>) -> Figment {
        let mut f = Figment::from(Serialized::defaults(Config::default()));
        if let Some(p) = toml_path {
            f = f.merge(Toml::file(p));
        }
        f.merge(Env::prefixed("VC_").filter_map(|k| env_key_path(k.as_str()).map(Into::into)))
    }

    /// Loads and validates. `toml_path = None` uses `$VC_CONFIG` if set.
    pub fn load(toml_path: Option<&Path>) -> Result<Config, ConfigError> {
        let env_path = std::env::var_os("VC_CONFIG").map(std::path::PathBuf::from);
        let path = toml_path.map(Path::to_path_buf).or(env_path);
        let cfg: Config = Config::figment(path.as_deref())
            .extract()
            .map_err(Box::new)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Checks ranges and cross-field rules. Startup refuses an invalid config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |m: &str| Err(ConfigError::Invalid(m.to_string()));
        let f = &self.frontier;
        if !(1..=5).contains(&f.max_urls_per_claim) {
            return bad("frontier.max_urls_per_claim must be 1..=5");
        }
        if f.global_rate_per_s <= 0.0 || f.global_rate_burst < 1.0 {
            return bad("frontier.global_rate_per_s must be > 0 and burst >= 1");
        }
        if f.min_delay_s == 0 || f.max_delay_s > 3600 || f.min_delay_s > f.max_delay_s {
            return bad("frontier delays must satisfy 0 < min_delay_s <= max_delay_s <= 3600");
        }
        if f.shards.is_empty() {
            return bad("frontier.shards must not be empty");
        }
        if self.fetch.max_redirects > 5 {
            return bad("fetch.max_redirects must be <= 5");
        }
        if self.render.max_tabs == 0 || self.render.max_tabs > 2 {
            return bad("render.max_tabs must be 1..=2");
        }
        if self.api.excerpt_chars > 300 || self.extract.excerpt_chars > 300 {
            return bad("excerpts are capped at 300 characters");
        }
        let l = &self.llm;
        if l.daily_usd < 0.0 || l.monthly_usd < 0.0 || l.daily_usd > l.monthly_usd {
            return bad("llm caps must satisfy 0 <= daily_usd <= monthly_usd");
        }
        if (l.shares.sum() - 1.0).abs() > 1e-6 {
            return bad("llm.shares must sum to 1");
        }
        if l.enabled {
            match l.project_cap_usd {
                None => {
                    return bad("llm.enabled requires VC_PROJECT_CAP_USD (llm.project_cap_usd)");
                }
                Some(cap) if l.monthly_usd > 0.5 * cap => {
                    return bad("llm.monthly_usd must be <= 50% of llm.project_cap_usd");
                }
                Some(_) => {}
            }
        }
        if self.queue.max_attempts == 0 {
            return bad("queue.max_attempts must be >= 1");
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Jail closures return figment::Error
mod tests {
    use super::*;
    use figment::Jail;

    #[test]
    fn defaults_are_valid_and_llm_is_off() {
        let c = Config::default();
        c.validate().unwrap();
        assert!(!c.llm.enabled);
        assert_eq!(c.frontier.max_urls_per_claim, 5);
        assert_eq!(c.llm.model, "gemini-3.1-flash-lite");
        assert_eq!(c.llm.monthly_usd, 40.0);
        assert_eq!(c.llm.daily_usd, 1.5);
    }

    #[test]
    fn env_key_mapping() {
        assert_eq!(
            env_key_path("LLM_DAILY_USD").as_deref(),
            Some("llm.daily_usd")
        );
        assert_eq!(
            env_key_path("PROJECT_CAP_USD").as_deref(),
            Some("llm.project_cap_usd")
        );
        assert_eq!(
            env_key_path("LLM_SHARES__SCOUT").as_deref(),
            Some("llm.shares.scout")
        );
        assert_eq!(env_key_path("SECRET_CRAWLER_PG_PASSWORD"), None);
        assert_eq!(env_key_path("CONFIG"), None);
    }

    #[test]
    fn toml_then_env_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "vc.toml",
                "[llm]\nmonthly_usd = 20.0\n[frontier]\nshards = [1, 2]\n",
            )?;
            jail.set_env("VC_LLM_DAILY_USD", "0.5");
            jail.set_env("VC_DATABASE_URL", "postgres://x@db/vc");
            jail.set_env("VC_SECRET_CRAWLER_PG_PASSWORD", "ignored");
            jail.set_env("VC_LLM_SHARES__SCOUT", "0.04");
            let c = Config::load(Some(Path::new("vc.toml"))).expect("load");
            assert_eq!(c.llm.monthly_usd, 20.0);
            assert_eq!(c.llm.daily_usd, 0.5);
            assert_eq!(c.frontier.shards, vec![1, 2]);
            assert_eq!(c.database.url, "postgres://x@db/vc");
            Ok(())
        });
    }

    #[test]
    fn unknown_keys_are_rejected() {
        Jail::expect_with(|jail| {
            jail.create_file("vc.toml", "[fetch]\nuser_agent = \"Mozilla/5.0\"\n")?;
            assert!(Config::load(Some(Path::new("vc.toml"))).is_err());
            Ok(())
        });
    }

    #[test]
    fn llm_enabled_requires_project_cap_and_half_rule() {
        Jail::expect_with(|jail| {
            jail.set_env("VC_LLM_ENABLED", "true");
            assert!(Config::load(None).is_err(), "no project cap");
            jail.set_env("VC_PROJECT_CAP_USD", "60");
            assert!(Config::load(None).is_err(), "40 > 50% of 60");
            jail.set_env("VC_PROJECT_CAP_USD", "100");
            let c = Config::load(None).expect("40 <= 50% of 100");
            assert!(c.llm.enabled);
            Ok(())
        });
    }
}
