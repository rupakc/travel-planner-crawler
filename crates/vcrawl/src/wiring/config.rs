//! Loading the configuration the way the deploy kit passes it.
//!
//! On top of `vc_config` (defaults → TOML → `VC_<SECTION>_<FIELD>`), the binary understands the
//! environment names `deploy/compose.yaml` uses (`VC_SPOOL_DIR`, `VC_GCS_RAW_BUCKET`, …) and
//! never reads a `VC_*_FILE` variable as a setting: those name **secret files**
//! (`/run/secrets/*`), read with [`secret_file`] by the code that needs the secret.

use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use vc_config::{Config, ConfigError};

/// Deploy-kit environment names (prefix `VC_` stripped, lowercase) → config key paths.
pub const DEPLOY_ALIASES: &[(&str, &str)] = &[
    ("spool_dir", "store.spool_dir"),
    ("gcs_raw_bucket", "store.raw_bucket"),
    ("gcs_ops_bucket", "store.ops_bucket"),
    ("gcp_project", "llm.gcp_project"),
    ("log_format", "obs.log_format"),
    ("log_level", "obs.log_level"),
    ("api_listen", "api.bind"),
];

/// The config key path for an environment key (prefix stripped), or `None` to ignore it.
pub fn env_key(key: &str) -> Option<String> {
    let k = key.to_ascii_lowercase();
    if k.ends_with("_file") {
        return None; // a secret file path, never a setting
    }
    if let Some((_, path)) = DEPLOY_ALIASES.iter().find(|(a, _)| *a == k) {
        return Some((*path).to_string());
    }
    vc_config::env_key_path(&k)
}

/// The figment: defaults, then `toml_path` (if any), then `VC_*` env (see [`env_key`]).
pub fn figment(toml_path: Option<&Path>) -> Figment {
    let mut f = Figment::from(Serialized::defaults(Config::default()));
    if let Some(p) = toml_path {
        f = f.merge(Toml::file(p));
    }
    f.merge(Env::prefixed("VC_").filter_map(|k| env_key(k.as_str()).map(Into::into)))
}

/// Loads and validates the configuration (`$VC_CONFIG` names an optional TOML file).
pub fn load() -> Result<Config, ConfigError> {
    let path = std::env::var_os("VC_CONFIG").map(PathBuf::from);
    load_from(path.as_deref())
}

/// Like [`load`] with an explicit TOML path.
pub fn load_from(toml_path: Option<&Path>) -> Result<Config, ConfigError> {
    let mut cfg: Config = figment(toml_path).extract().map_err(Box::new)?;
    // One GCP project for everything (the deploy kit sets `VC_GCP_PROJECT` once).
    if let Ok(p) = std::env::var("VC_GCP_PROJECT")
        && !p.trim().is_empty()
    {
        cfg.secrets.project = p.trim().to_string();
    }
    cfg.validate()?;
    Ok(cfg)
}

/// Reads the secret file named by `VC_<name>_FILE` (e.g. `DATABASE_PASSWORD`), trailing
/// newline trimmed. `Ok(None)` when the variable is unset; an unreadable or empty file is an
/// error (never logged with its content).
pub fn secret_file(name: &str) -> vc_core::Result<Option<String>> {
    let var = format!("VC_{}_FILE", name.to_ascii_uppercase());
    let Some(path) = std::env::var_os(&var) else {
        return Ok(None);
    };
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| vc_core::Error::Invalid(format!("{var}: cannot read secret file: {e}")))?;
    let v = raw.trim_end_matches(['\r', '\n']).to_string();
    if v.is_empty() {
        return Err(vc_core::Error::Invalid(format!(
            "{var}: secret file is empty"
        )));
    }
    Ok(Some(v))
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Jail closures return figment::Error
mod tests {
    use super::*;

    #[test]
    fn deploy_names_map_and_files_are_ignored() {
        assert_eq!(env_key("SPOOL_DIR").as_deref(), Some("store.spool_dir"));
        assert_eq!(
            env_key("GCS_RAW_BUCKET").as_deref(),
            Some("store.raw_bucket")
        );
        assert_eq!(env_key("LOG_FORMAT").as_deref(), Some("obs.log_format"));
        assert_eq!(env_key("API_LISTEN").as_deref(), Some("api.bind"));
        assert_eq!(env_key("DATABASE_PASSWORD_FILE"), None);
        assert_eq!(env_key("API_TLS_KEY_FILE"), None);
        assert_eq!(env_key("DATABASE_URL").as_deref(), Some("database.url"));
        assert_eq!(
            env_key("PROJECT_CAP_USD").as_deref(),
            Some("llm.project_cap_usd")
        );
        assert_eq!(env_key("CONFIG"), None);
    }

    #[test]
    fn the_compose_environment_loads() {
        // Exactly the variables deploy/compose.yaml gives a role container.
        figment::Jail::expect_with(|jail| {
            jail.set_env("VC_DATABASE_URL", "postgres://vcrawl@postgres:5432/vcrawl");
            jail.set_env("VC_DATABASE_PASSWORD_FILE", "/run/secrets/pg_password");
            jail.set_env("VC_GCS_RAW_BUCKET", "raw-x");
            jail.set_env("VC_GCS_OPS_BUCKET", "ops-x");
            jail.set_env("VC_GCP_PROJECT", "proj-x");
            jail.set_env("VC_LOG_FORMAT", "pretty");
            jail.set_env("VC_LLM_ENABLED", "false");
            jail.set_env("VC_RENDER_CDP_URL", "http://chromium:9222");
            jail.set_env("VC_SPOOL_DIR", "/spool");
            jail.set_env("VC_API_LISTEN", "0.0.0.0:8443");
            jail.set_env("VC_API_TOKEN_FILE", "/run/secrets/api_token_voyager");
            let c = load_from(None).expect("compose env loads");
            assert_eq!(c.store.spool_dir, "/spool");
            assert_eq!(c.store.raw_bucket, "raw-x");
            assert_eq!(c.store.ops_bucket, "ops-x");
            assert_eq!(c.llm.gcp_project, "proj-x");
            assert_eq!(c.secrets.project, "proj-x");
            assert_eq!(c.obs.log_format, "pretty");
            assert_eq!(c.render.cdp_url, "http://chromium:9222");
            assert!(!c.llm.enabled);
            Ok(())
        });
    }

    #[test]
    fn secret_files_are_read_and_trimmed() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("pw", "s3cret\n")?;
            let p = jail.directory().join("pw");
            jail.set_env("VC_DATABASE_PASSWORD_FILE", p.display().to_string());
            assert_eq!(
                secret_file("database_password").expect("read").as_deref(),
                Some("s3cret")
            );
            assert_eq!(secret_file("api_token").expect("unset"), None);
            jail.create_file("empty", "\n")?;
            let e = jail.directory().join("empty");
            jail.set_env("VC_API_TOKEN_FILE", e.display().to_string());
            assert!(secret_file("api_token").is_err());
            Ok(())
        });
    }
}
