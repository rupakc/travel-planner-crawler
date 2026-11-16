//! Loading the configuration.
//!
//! `vc_config::Config::load` (defaults → `$VC_CONFIG` TOML → `VC_<SECTION>_<FIELD>`) is the
//! one loader; `deploy/compose.yaml` uses only real setting names. `VC_*_FILE` variables name
//! **secret files** (`/run/secrets/*`), never settings: [`secret_file`] reads them where the
//! secret is needed. `VC_*` names that no setting reads are reported at startup
//! ([`vc_config::unmapped_env_keys`]) so a typo in the deploy kit is visible.

use vc_config::{Config, ConfigError};

/// Loads and validates the configuration; warns (stderr, before tracing is set up) about
/// `VC_*` variables that no setting reads.
pub fn load() -> Result<Config, ConfigError> {
    let keys: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();
    for k in vc_config::unmapped_env_keys(keys.iter().map(String::as_str)) {
        eprintln!("vcrawl: environment variable {k} is not a setting (ignored)");
    }
    Config::load(None)
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
    fn the_compose_environment_loads() {
        // The variables deploy/compose.yaml gives a role container.
        figment::Jail::expect_with(|jail| {
            jail.set_env("VC_DATABASE_URL", "postgres://vcrawl@postgres:5432/vcrawl");
            jail.set_env("VC_DATABASE_PASSWORD_FILE", "/run/secrets/pg_password");
            jail.set_env("VC_STORE_RAW_BUCKET", "raw-x");
            jail.set_env("VC_STORE_SPOOL_DIR", "/spool");
            jail.set_env("VC_OBS_LOG_FORMAT", "pretty");
            jail.set_env("VC_RENDER_CDP_URL", "http://chromium:9222");
            let c = load().expect("compose env loads");
            assert_eq!(c.store.spool_dir, "/spool");
            assert_eq!(c.store.raw_bucket, "raw-x");
            assert_eq!(c.obs.log_format, "pretty");
            assert_eq!(c.render.cdp_url, "http://chromium:9222");
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
