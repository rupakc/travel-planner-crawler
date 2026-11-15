//! # vc-secrets
//!
//! [`SecretSource`] implementations:
//! - [`SecretManagerSource`]: GCP Secret Manager over REST
//!   (`GET {endpoint}/v1/projects/{project}/secrets/{name}/versions/latest:access`) with an
//!   Application Default Credentials token (the VM's `crawler-runtime` service account). The
//!   payload's CRC32C is verified.
//! - [`EnvSource`]: `VC_SECRET_<NAME>` environment variables (tests, development).
//!
//! Secret values and tokens never appear in `Debug` output, errors or logs. Secret Manager is
//! not a crawl target, so no `ClaimPermit` is involved.

pub mod base64;

/// CRC32C (Castagnoli), as Secret Manager's `dataCrc32c` uses. Bitwise: secrets are tiny.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::OnceCell;
use vc_config::{SecretBackend, SecretsConfig};
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

/// The secret source selected by `secrets.backend`.
pub fn from_config(config: &SecretsConfig) -> Arc<dyn SecretSource> {
    match config.backend {
        SecretBackend::Env => Arc::new(EnvSource {
            prefix: config.env_prefix.clone(),
        }),
        SecretBackend::SecretManager => Arc::new(SecretManagerSource::new(config.clone())),
    }
}

/// An OAuth access-token source for Secret Manager calls.
#[async_trait]
pub trait TokenProvider: Send + Sync + std::fmt::Debug {
    /// A current access token. Never log it.
    async fn token(&self) -> Result<String>;
}

/// A fixed token (tests). `Debug` is redacted.
#[derive(Clone)]
pub struct StaticToken(String);

impl StaticToken {
    /// Wraps a token.
    pub fn new(token: impl Into<String>) -> Self {
        StaticToken(token.into())
    }
}

impl std::fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StaticToken(<redacted>)")
    }
}

#[async_trait]
impl TokenProvider for StaticToken {
    async fn token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}

const CLOUD_PLATFORM: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Application Default Credentials (`google-cloud-auth`), resolved on first use and cached
/// (the library refreshes the token itself).
#[derive(Debug, Default)]
pub struct AdcTokens {
    creds: OnceCell<google_cloud_auth::credentials::AccessTokenCredentials>,
}

#[async_trait]
impl TokenProvider for AdcTokens {
    async fn token(&self) -> Result<String> {
        let creds = self
            .creds
            .get_or_try_init(|| async {
                google_cloud_auth::credentials::Builder::default()
                    .with_scopes([CLOUD_PLATFORM])
                    .build_access_token_credentials()
                    .map_err(|e| Error::Invalid(format!("application default credentials: {e}")))
            })
            .await?;
        let t = creds.access_token().await.map_err(|e| {
            Error::Io(std::io::Error::other(format!("access token: {e}")))
        })?;
        Ok(t.token)
    }
}

/// GCP Secret Manager (REST + ADC).
#[derive(Debug, Clone)]
pub struct SecretManagerSource {
    config: SecretsConfig,
    http: reqwest::Client,
    tokens: Arc<dyn TokenProvider>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessResponse {
    payload: Payload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    data: String,
    #[serde(default)]
    data_crc32c: Option<String>,
}

/// Secret ids are `[A-Za-z0-9_-]{1,255}` (Secret Manager's own rule); anything else could
/// change the request path.
pub fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl SecretManagerSource {
    /// A source using Application Default Credentials.
    pub fn new(config: SecretsConfig) -> Self {
        Self::with_tokens(config, Arc::new(AdcTokens::default()))
    }

    /// A source with an explicit token provider (tests).
    pub fn with_tokens(config: SecretsConfig, tokens: Arc<dyn TokenProvider>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_s.max(1)))
            .build()
            .unwrap_or_default();
        SecretManagerSource {
            config,
            http,
            tokens,
        }
    }

    /// The settings.
    pub fn config(&self) -> &SecretsConfig {
        &self.config
    }
}

#[async_trait]
impl SecretSource for SecretManagerSource {
    async fn get(&self, name: &str) -> Result<Secret> {
        if !valid_secret_name(name) {
            return Err(Error::Invalid(format!(
                "secret name must match [A-Za-z0-9_-]{{1,255}} (got {} chars)",
                name.len()
            )));
        }
        if !valid_secret_name(&self.config.project) {
            return Err(Error::Invalid("secrets.project is not a valid id".into()));
        }
        let url = format!(
            "{}/v1/projects/{}/secrets/{name}/versions/latest:access",
            self.config.endpoint.trim_end_matches('/'),
            self.config.project
        );
        let token = self.tokens.token().await?;
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                Error::Io(std::io::Error::other(format!(
                    "secret manager request for {name} failed: {}",
                    e.without_url()
                )))
            })?;
        let status = resp.status();
        match status.as_u16() {
            200 => {}
            404 => return Err(Error::NotFound(format!("secret {name}"))),
            401 | 403 => {
                return Err(Error::Policy(format!(
                    "secret {name}: access denied ({status})"
                )));
            }
            400..=499 => {
                return Err(Error::Invalid(format!("secret {name}: HTTP {status}")));
            }
            _ => {
                return Err(Error::Io(std::io::Error::other(format!(
                    "secret {name}: HTTP {status}"
                ))));
            }
        }
        let body: AccessResponse = resp.json().await.map_err(|_| {
            Error::Invalid(format!("secret {name}: malformed access response"))
        })?;
        let data = base64::decode(&body.payload.data)
            .ok_or_else(|| Error::Invalid(format!("secret {name}: payload is not base64")))?;
        if let Some(want) = body.payload.data_crc32c {
            let ok = want
                .parse::<u64>()
                .is_ok_and(|w| w == u64::from(crc32c(&data)));
            if !ok {
                return Err(Error::Invalid(format!(
                    "secret {name}: payload checksum mismatch"
                )));
            }
        }
        let value = String::from_utf8(data)
            .map_err(|_| Error::Invalid(format!("secret {name}: payload is not UTF-8")))?;
        Ok(Secret::new(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn names() {
        assert!(valid_secret_name("crawler-api-tls-key"));
        assert!(valid_secret_name("A_b-9"));
        assert!(!valid_secret_name("a.b"));
        assert!(!valid_secret_name("a%2Fb"));
    }

    #[test]
    fn static_token_is_redacted() {
        assert_eq!(
            format!("{:?}", StaticToken::new("t0k")),
            "StaticToken(<redacted>)"
        );
    }
}
