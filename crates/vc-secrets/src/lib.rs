//! # vc-secrets (Task 13, stub)
//!
//! `SecretManagerSource` (REST + ADC) and `EnvSource` (tests).

use async_trait::async_trait;
use vc_config::SecretsConfig;
use vc_core::secrets::{Secret, SecretSource};
use vc_core::{Error, Result};

/// Secrets from `VC_SECRET_<NAME>` environment variables (`-` → `_`, uppercased).
#[derive(Debug, Clone)]
pub struct EnvSource {
    /// Prefix (default `VC_SECRET_`).
    pub prefix: String,
}

#[async_trait]
impl SecretSource for EnvSource {
    async fn get(&self, name: &str) -> Result<Secret> {
        let var = format!("{}{}", self.prefix, name.replace('-', "_").to_uppercase());
        std::env::var(&var)
            .map(Secret::new)
            .map_err(|_| Error::NotFound(var))
    }
}

/// GCP Secret Manager.
#[derive(Debug, Clone)]
pub struct SecretManagerSource {
    /// Settings.
    pub config: SecretsConfig,
}

#[async_trait]
impl SecretSource for SecretManagerSource {
    async fn get(&self, _name: &str) -> Result<Secret> {
        todo!("Task 13")
    }
}
