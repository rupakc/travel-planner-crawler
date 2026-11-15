//! Secret lookup. Implemented by `vc-secrets` (`SecretManagerSource`, `EnvSource`).

use async_trait::async_trait;

use crate::error::Result;

/// A secret value. `Debug` and `Display` are redacted; call [`Secret::expose`] deliberately.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a value.
    pub fn new(v: impl Into<String>) -> Self {
        Secret(v.into())
    }

    /// The value. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Reads secrets by name (e.g. `crawler-pg-password`).
#[async_trait]
pub trait SecretSource: Send + Sync {
    /// The latest version of a secret. Missing → [`crate::Error::NotFound`].
    async fn get(&self, name: &str) -> Result<Secret>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn secret_is_redacted() {
        let s = super::Secret::new("hunter2");
        assert_eq!(format!("{s:?} {s}"), "Secret(<redacted>) <redacted>");
        assert_eq!(s.expose(), "hunter2");
    }
}
