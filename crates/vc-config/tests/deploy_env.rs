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
        "VC_LLM_MONTHLY_USD",
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
        // The owner's monthly cap reaches the roles; unset, it is the setting's own default.
        assert_eq!(env["VC_LLM_MONTHLY_USD"], "30");
        assert_eq!(
            c.llm.monthly_usd,
            vc_config::LlmConfig::default().monthly_usd
        );
        Ok(())
    });
}

/// The top-level services of `deploy/compose.yaml`: (name, body lines).
fn compose_services() -> Vec<(String, Vec<String>)> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/compose.yaml");
    let text = std::fs::read_to_string(path).expect("deploy/compose.yaml");
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut on = false;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.is_empty() && !line.starts_with('#') {
            on = line == "services:";
            continue;
        }
        if !on {
            continue;
        }
        if let Some(name) = line
            .strip_prefix("  ")
            .and_then(|l| l.strip_suffix(':'))
            .filter(|n| !n.starts_with(' ') && !n.starts_with('#'))
        {
            out.push((name.to_string(), Vec::new()));
        } else if let Some((_, body)) = out.last_mut() {
            body.push(line.to_string());
        }
    }
    out
}

#[test]
fn every_role_gets_the_owner_llm_settings() {
    // A role that sets its own `environment:` must merge the shared role environment, or
    // the owner's VC_LLM_ENABLED / VC_PROJECT_CAP_USD / VC_LLM_MONTHLY_USD never reach it
    // (host.local.env, infrastructure/README.md).
    let services = compose_services();
    let roles: Vec<_> = services
        .iter()
        .filter(|(_, b)| b.iter().any(|l| l.trim() == "<<: *role"))
        .collect();
    // One container runs every role (`vcrawl run`, the 4 GB Spot VM).
    let vcrawl = roles
        .iter()
        .find(|(n, _)| n == "vcrawl")
        .expect("vcrawl is a role service");
    assert!(
        vcrawl.1.iter().any(|l| l.trim() == r#"command: ["run"]"#),
        "vcrawl does not run every role: {:?}",
        vcrawl.1
    );
    for (name, body) in roles {
        if body.iter().any(|l| l.trim() == "environment:") {
            assert!(
                body.iter().any(|l| l.trim() == "<<: *role-env"),
                "{name} overrides environment without <<: *role-env"
            );
        }
    }
    // …and those three live in that shared environment (the `x-role` anchor).
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/compose.yaml");
    let text = std::fs::read_to_string(path).expect("deploy/compose.yaml");
    let anchor: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.starts_with("x-role:"))
        .skip(1)
        .take_while(|l| l.is_empty() || l.starts_with(' '))
        .collect();
    for k in ["VC_LLM_ENABLED", "VC_PROJECT_CAP_USD", "VC_LLM_MONTHLY_USD"] {
        assert!(
            anchor
                .iter()
                .any(|l| l.trim_start().starts_with(&format!("{k}: ${{{k}:-"))),
            "{k} is not passed from host.local.env in the shared role environment"
        );
    }
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

/// `512m`, `1g`, `1536m` → bytes.
fn mem_bytes(v: &str) -> u64 {
    let v = v.trim();
    let (n, unit) = v.split_at(v.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(v.len()));
    let n: u64 = n.parse().unwrap_or_else(|_| panic!("memory limit {v:?}"));
    n * match unit.to_ascii_lowercase().as_str() {
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "" | "b" => 1,
        u => panic!("unit {u:?} in {v:?}"),
    }
}

#[test]
fn memory_limits_fit_the_small_vm() {
    // Spot e2-medium: 4 GB. Every service (the one-shot migrate included) has a limit, and
    // the limits leave ≥ 0.6 GB to the host OS, Docker and the Ops Agent.
    let mut total = 0;
    for (name, body) in compose_services() {
        let limit = body
            .iter()
            .find_map(|l| {
                let i = l.find("memory:")?;
                let rest = l[i + "memory:".len()..].trim();
                Some(rest.split([' ', '}']).next().unwrap_or("").to_string())
            })
            .unwrap_or_else(|| panic!("{name} has no memory limit"));
        total += mem_bytes(&limit);
    }
    assert!(
        total <= 3_400_000_000,
        "memory limits sum to {total} bytes (> 3.4 GB)"
    );
}
