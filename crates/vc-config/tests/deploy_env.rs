//! `Config::load` with exactly the environment `deploy/compose.yaml` gives each container.
//! Any `VC_*` variable there that is neither a setting nor a declared secret file fails.
#![allow(clippy::result_large_err)] // figment::Jail closures return figment::Error

use std::collections::BTreeMap;

use figment::Jail;
use vc_config::{Config, unmapped_env_keys};

/// Every service's `VC_*` environment from `deploy/compose.yaml`, `${X:-default}` resolved to
/// its default. Line-based: the compose file writes one `KEY: value` per line under
/// `environment:` (anchors merged in by `<<: *role-env`).
fn compose_env() -> BTreeMap<String, String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/compose.yaml");
    let text = std::fs::read_to_string(path).expect("deploy/compose.yaml");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        let k = k.trim();
        if !k.starts_with("VC_")
            || !k
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            continue;
        }
        let mut v = v.trim().trim_matches('"').to_string();
        if let Some(inner) = v.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
            v = inner
                .split_once(":-")
                .map_or(String::new(), |(_, d)| d.to_string());
        }
        out.insert(k.to_string(), v);
    }
    out
}

#[test]
fn compose_environment_is_complete_and_known() {
    let env = compose_env();
    for want in [
        "VC_DATABASE_URL",
        "VC_STORE_RAW_BUCKET",
        "VC_STORE_SPOOL_DIR",
        "VC_API_BIND",
    ] {
        assert!(
            env.contains_key(want),
            "{want} missing from deploy/compose.yaml"
        );
    }
    let unmapped = unmapped_env_keys(env.keys().map(String::as_str));
    assert!(
        unmapped.is_empty(),
        "deploy/compose.yaml sets VC_* variables no setting reads: {unmapped:?}"
    );
}

#[test]
fn compose_environment_loads() {
    let env = compose_env();
    Jail::expect_with(|jail| {
        for (k, v) in &env {
            jail.set_env(k, v);
        }
        let c = Config::load(None).expect("the compose environment loads");
        assert_eq!(c.database.url, env["VC_DATABASE_URL"]);
        assert_eq!(c.store.raw_bucket, env["VC_STORE_RAW_BUCKET"]);
        assert_eq!(c.store.ops_bucket, env["VC_STORE_OPS_BUCKET"]);
        assert_eq!(c.store.spool_dir, env["VC_STORE_SPOOL_DIR"]);
        assert_eq!(c.llm.gcp_project, env["VC_LLM_GCP_PROJECT"]);
        assert_eq!(c.secrets.project, env["VC_SECRETS_PROJECT"]);
        assert_eq!(c.obs.log_format, env["VC_OBS_LOG_FORMAT"]);
        assert_eq!(c.render.cdp_url, env["VC_RENDER_CDP_URL"]);
        assert_eq!(c.api.bind, env["VC_API_BIND"]);
        assert!(!c.llm.enabled);
        assert_eq!(c.llm.project_cap_usd, Some(0.0));
        Ok(())
    });
}

#[test]
fn unmapped_keys_are_reported() {
    let keys = [
        "VC_API_LISTEN",                 // valid section, no such field
        "VC_GCS_RAW_BUCKET",             // unknown section: would be silently ignored
        "VC_SPOOL_DIR",                  // ditto
        "VC_API_TOKEN_FILE",             // a secret file nothing reads
        "VC_LLM_ENABLED",                // fine
        "VC_DATABASE_PASSWORD_FILE",     // declared secret file
        "VC_PROJECT_CAP_USD",            // alias
        "VC_CONFIG",                     // the TOML path
        "VC_SECRET_CRAWLER_PG_PASSWORD", // env secret backend
        "VC_LLM_SHARES__SCOUT",          // nested
        "VC_FETCH_USER_AGENT",           // no such field
    ];
    assert_eq!(
        unmapped_env_keys(keys),
        vec![
            "VC_API_LISTEN",
            "VC_GCS_RAW_BUCKET",
            "VC_SPOOL_DIR",
            "VC_API_TOKEN_FILE",
            "VC_FETCH_USER_AGENT"
        ]
    );
}

#[test]
fn secret_file_variables_never_break_load() {
    Jail::expect_with(|jail| {
        jail.set_env("VC_DATABASE_PASSWORD_FILE", "/run/secrets/pg_password");
        Config::load(None).expect("a secret file path is not a setting");
        Ok(())
    });
}
