//! `[secrets]`: secret source (owner: vc-secrets).

use serde::{Deserialize, Serialize};

/// Secret backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretBackend {
    /// GCP Secret Manager (ADC).
    SecretManager,
    /// Environment variables `VC_SECRET_<NAME>` (tests, development).
    Env,
}

/// Secret settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsConfig {
    /// Backend.
    pub backend: SecretBackend,
    /// GCP project of the secrets.
    pub project: String,
    /// Env prefix for the `env` backend (`crawler-pg-password` → `VC_SECRET_CRAWLER_PG_PASSWORD`).
    pub env_prefix: String,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        SecretsConfig {
            backend: SecretBackend::SecretManager,
            project: "voyager-prod-ohzkl".into(),
            env_prefix: "VC_SECRET_".into(),
        }
    }
}
