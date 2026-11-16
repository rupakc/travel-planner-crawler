//! Per-client bearer tokens (spec §5.11).
//!
//! A token is `<client_id>.<secret>`; `api_clients.token_hash` holds the argon2id PHC string
//! of the whole token. The client id selects the row, argon2 verifies the token. Verified
//! tokens are cached in memory for [`CACHE_TTL`] so argon2 runs about once a minute per
//! client; disabling a client therefore takes effect within that time. Tokens are never
//! logged.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::Semaphore;
use vc_core::licence::LicenceClass;
use vc_core::secrets::Secret;
use vc_core::{Error, Result};

use crate::error::ApiError;

/// How long a verified token stays cached.
pub const CACHE_TTL: Duration = Duration::from_secs(60);

/// Concurrent argon2 verifications (bounds CPU spent on bad tokens).
const MAX_VERIFY: usize = 4;

/// One `api_clients` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredClient {
    /// Id (no `.`; it is the token prefix).
    pub client_id: String,
    /// Display name.
    pub name: String,
    /// argon2id PHC string of the whole token.
    pub token_hash: String,
    /// Licence classes this client may receive.
    pub licence_classes: Vec<LicenceClass>,
    /// Requests per minute.
    pub rate_per_min: u32,
    /// Disabled clients are refused.
    pub enabled: bool,
}

/// Where clients live (`api_clients`; [`crate::pg::PgApiStore`] in production).
#[async_trait]
pub trait ClientStore: Send + Sync {
    /// The client row, if any.
    async fn get(&self, client_id: &str) -> Result<Option<StoredClient>>;
    /// Records a successful authentication (`last_used_at`).
    async fn touch(&self, client_id: &str) -> Result<()>;
    /// Readiness check.
    async fn ping(&self) -> Result<()>;
}

/// An in-memory [`ClientStore`] (tests, local runs).
#[derive(Default)]
pub struct MemClientStore {
    clients: Mutex<HashMap<String, StoredClient>>,
}

impl MemClientStore {
    /// A store holding `clients`.
    pub fn new(clients: Vec<StoredClient>) -> Self {
        MemClientStore {
            clients: Mutex::new(
                clients
                    .into_iter()
                    .map(|c| (c.client_id.clone(), c))
                    .collect(),
            ),
        }
    }

    /// Inserts or replaces a client.
    pub fn upsert(&self, c: StoredClient) {
        self.lock().insert(c.client_id.clone(), c);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, StoredClient>> {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl ClientStore for MemClientStore {
    async fn get(&self, client_id: &str) -> Result<Option<StoredClient>> {
        Ok(self.lock().get(client_id).cloned())
    }
    async fn touch(&self, _client_id: &str) -> Result<()> {
        Ok(())
    }
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
}

/// Hashes a token with the production argon2id parameters (19 MiB, t = 2, p = 1).
pub fn hash_token(token: &str) -> Result<String> {
    hash_token_with(token, Params::default())
}

/// Hashes a token with explicit argon2id parameters (verification reads them from the hash).
pub fn hash_token_with(token: &str, params: Params) -> Result<String> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(token.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| Error::Invalid(format!("token hash: {e}")))
}

/// Whether `token` matches the PHC string `hash` (false on a malformed hash).
pub fn verify_token(token: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(token.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Splits `<client_id>.<secret>`.
pub fn split_token(token: &str) -> Option<(&str, &str)> {
    let (id, secret) = token.split_once('.')?;
    (!id.is_empty() && !secret.is_empty()).then_some((id, secret))
}

/// The authenticated caller, available to handlers as a request extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// Id.
    pub client_id: String,
    /// Licence classes it may receive.
    pub licence_classes: Vec<LicenceClass>,
    /// Requests per minute.
    pub rate_per_min: u32,
}

struct CacheEntry {
    token: Secret,
    client: Client,
    until: Instant,
}

/// Verifies bearer tokens, with a short cache.
pub(crate) struct Authenticator {
    cache: Mutex<HashMap<String, CacheEntry>>,
    verify: Semaphore,
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

impl Authenticator {
    pub(crate) fn new() -> Self {
        Authenticator {
            cache: Mutex::new(HashMap::new()),
            verify: Semaphore::new(MAX_VERIFY),
        }
    }

    fn cached(&self, id: &str, token: &str) -> Option<Client> {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let e = cache.get(id)?;
        (e.until > Instant::now() && ct_eq(e.token.expose().as_bytes(), token.as_bytes()))
            .then(|| e.client.clone())
    }

    /// Authenticates a request.
    pub(crate) async fn authenticate(
        &self,
        store: &dyn ClientStore,
        headers: &HeaderMap,
    ) -> std::result::Result<Client, ApiError> {
        let token = bearer(headers).ok_or(ApiError::Unauthorized)?;
        let (id, _) = split_token(token).ok_or(ApiError::Unauthorized)?;
        if let Some(c) = self.cached(id, token) {
            return Ok(c);
        }
        let stored = store
            .get(id)
            .await
            .map_err(|e| {
                tracing::error!(class = ?e.class(), "client lookup failed");
                ApiError::Unavailable
            })?
            .filter(|c| c.enabled)
            .ok_or(ApiError::Unauthorized)?;
        let ok = {
            let _permit = self
                .verify
                .acquire()
                .await
                .map_err(|_| ApiError::Internal)?;
            let (t, h) = (token.to_string(), stored.token_hash.clone());
            tokio::task::spawn_blocking(move || verify_token(&t, &h))
                .await
                .map_err(|_| ApiError::Internal)?
        };
        if !ok {
            tracing::info!(client_id = %stored.client_id, "bearer token rejected");
            return Err(ApiError::Unauthorized);
        }
        let client = Client {
            client_id: stored.client_id.clone(),
            licence_classes: stored.licence_classes.clone(),
            rate_per_min: stored.rate_per_min,
        };
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                stored.client_id.clone(),
                CacheEntry {
                    token: Secret::new(token),
                    client: client.clone(),
                    until: Instant::now() + CACHE_TTL,
                },
            );
        if let Err(e) = store.touch(&stored.client_id).await {
            tracing::warn!(class = ?e.class(), "api client touch failed");
        }
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap() -> Params {
        Params::new(8, 1, 1, None).expect("params")
    }

    #[test]
    fn hash_and_verify() {
        let h = hash_token_with("a.b", cheap()).expect("hash");
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_token("a.b", &h));
        assert!(!verify_token("a.c", &h));
        assert!(!verify_token("a.b", "not a phc string"));
    }

    #[test]
    fn production_params_are_owasp_defaults() {
        let p = Params::default();
        assert_eq!((p.m_cost(), p.t_cost(), p.p_cost()), (19 * 1024, 2, 1));
    }

    #[test]
    fn token_parsing() {
        assert_eq!(split_token("id.sec.ret"), Some(("id", "sec.ret")));
        assert_eq!(split_token(".x"), None);
        assert_eq!(split_token("x."), None);
        assert_eq!(split_token("x"), None);
        let mut h = HeaderMap::new();
        h.insert("authorization", "bearer  a.b ".parse().expect("hv"));
        assert_eq!(bearer(&h), Some("a.b"));
        h.insert("authorization", "Basic YTpi".parse().expect("hv"));
        assert_eq!(bearer(&h), None);
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
