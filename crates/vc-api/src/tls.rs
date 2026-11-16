//! TLS and serving (spec §17.5): a self-signed certificate for the static internal IP, pinned
//! by the client. The certificate and key come from the files the host's `deploy.sh` installs
//! in [`SECRETS_DIR`] (it installs only a matched pair, so a half-done `--rotate-tls` keeps
//! the old one), else from a [`SecretSource`] (`crawler-api-tls-cert`, `crawler-api-tls-key`);
//! a key that does not match its certificate is refused either way. The API listens with TLS
//! on `bind`, and `/metrics` listens in plain HTTP on `metrics_bind` (localhost).

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use vc_config::ApiConfig;
use vc_core::secrets::SecretSource;
use vc_core::{Error, Result};

use crate::{ApiState, metrics_router, router};

/// Builds the rustls config from PEM bytes.
pub async fn tls_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<RustlsConfig> {
    if cert_pem.is_empty() || key_pem.is_empty() {
        return Err(Error::Invalid("tls: empty certificate or key".into()));
    }
    RustlsConfig::from_pem(cert_pem.to_vec(), key_pem.to_vec())
        .await
        .map_err(|e| Error::Invalid(format!("tls: {e}")))
}

/// Where compose mounts the pair `deploy.sh` installed (`api_tls_cert`, `api_tls_key`).
pub const SECRETS_DIR: &str = "/run/secrets";
/// The certificate file name in [`SECRETS_DIR`].
pub const CERT_FILE: &str = "api_tls_cert";
/// The key file name in [`SECRETS_DIR`].
pub const KEY_FILE: &str = "api_tls_key";

/// Loads the API's certificate and key: the matched pair in [`SECRETS_DIR`] when both files
/// are there and usable, else the secrets named in `cfg` from `secrets`.
pub async fn tls_from_secrets(secrets: &dyn SecretSource, cfg: &ApiConfig) -> Result<RustlsConfig> {
    tls_from_dir_or_secrets(Path::new(SECRETS_DIR), secrets, cfg).await
}

/// [`tls_from_secrets`] with the files looked up in `dir`. A pair in `dir` that cannot be
/// used (unreadable, invalid, key not matching the certificate) is logged and skipped; the
/// Secret Manager pair must then match itself, or the API does not start (fail closed).
pub async fn tls_from_dir_or_secrets(
    dir: &Path,
    secrets: &dyn SecretSource,
    cfg: &ApiConfig,
) -> Result<RustlsConfig> {
    let read = |name: &str| std::fs::read(dir.join(name)).ok().filter(|b| !b.is_empty());
    if let (Some(cert), Some(key)) = (read(CERT_FILE), read(KEY_FILE)) {
        match tls_from_pem(&cert, &key).await {
            Ok(c) => {
                tracing::info!(dir = %dir.display(), "api TLS: the installed key/certificate pair");
                return Ok(c);
            }
            Err(e) => tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "api TLS: installed pair unusable; trying Secret Manager"
            ),
        }
    }
    let cert = secrets.get(&cfg.tls_cert_secret).await?;
    let key = secrets.get(&cfg.tls_key_secret).await?;
    tls_from_pem(cert.expose().as_bytes(), key.expose().as_bytes()).await
}

fn addr(s: &str) -> Result<SocketAddr> {
    s.parse()
        .map_err(|_| Error::Invalid(format!("listen address {s:?}")))
}

/// Serves the API (TLS, `cfg.bind`) and `/metrics` (plain, `cfg.metrics_bind`) until
/// `shutdown` resolves; both listeners then drain for up to 5 s and stop together.
pub async fn serve(
    state: ApiState,
    tls: RustlsConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let api_addr = addr(&state.config.bind)?;
    let metrics_addr = addr(&state.config.metrics_bind)?;
    let api = router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let metrics = metrics_router(&state).into_make_service_with_connect_info::<SocketAddr>();
    let (ah, mh): (Handle<SocketAddr>, Handle<SocketAddr>) = (Handle::new(), Handle::new());
    let (ah2, mh2) = (ah.clone(), mh.clone());
    tokio::spawn(async move {
        shutdown.await;
        ah2.graceful_shutdown(Some(Duration::from_secs(5)));
        mh2.graceful_shutdown(Some(Duration::from_secs(5)));
    });
    tracing::info!(%api_addr, %metrics_addr, "vc-api listening");
    let api_srv = axum_server::bind_rustls(api_addr, tls)
        .handle(ah)
        .serve(api);
    let met_srv = axum_server::bind(metrics_addr).handle(mh).serve(metrics);
    let (a, m) = tokio::join!(api_srv, met_srv);
    a?;
    m?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use vc_core::secrets::Secret;

    use super::*;

    struct NoSecrets;
    #[async_trait]
    impl SecretSource for NoSecrets {
        async fn get(&self, name: &str) -> Result<Secret> {
            Err(Error::NotFound(name.into()))
        }
    }

    struct Junk;
    #[async_trait]
    impl SecretSource for Junk {
        async fn get(&self, _name: &str) -> Result<Secret> {
            Ok(Secret::new("not pem"))
        }
    }

    #[tokio::test]
    async fn missing_or_invalid_tls_material_fails_closed() {
        let cfg = ApiConfig::default();
        assert!(matches!(
            tls_from_secrets(&NoSecrets, &cfg).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            tls_from_secrets(&Junk, &cfg).await,
            Err(Error::Invalid(_))
        ));
        assert!(tls_from_pem(b"", b"").await.is_err());
    }

    const A_CRT: &str = include_str!("../tests/fixtures/tls/a.crt");
    const A_KEY: &str = include_str!("../tests/fixtures/tls/a.key");
    const B_CRT: &str = include_str!("../tests/fixtures/tls/b.crt");
    const B_KEY: &str = include_str!("../tests/fixtures/tls/b.key");

    /// Secret Manager mid-rotation: the new key, the old certificate.
    struct Pair(&'static str, &'static str);
    #[async_trait]
    impl SecretSource for Pair {
        async fn get(&self, name: &str) -> Result<Secret> {
            Ok(Secret::new(if name.ends_with("cert") {
                self.0
            } else {
                self.1
            }))
        }
    }

    /// A fresh directory under the system temp dir (no tempfile dependency here).
    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "vc-api-tls-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir_with(cert: &str, key: &str) -> TmpDir {
        let d = TmpDir::new();
        std::fs::write(d.path().join(CERT_FILE), cert).unwrap();
        std::fs::write(d.path().join(KEY_FILE), key).unwrap();
        d
    }

    #[tokio::test]
    async fn mismatched_pair_is_refused() {
        assert!(
            tls_from_pem(A_CRT.as_bytes(), A_KEY.as_bytes())
                .await
                .is_ok()
        );
        assert!(matches!(
            tls_from_pem(A_CRT.as_bytes(), B_KEY.as_bytes()).await,
            Err(Error::Invalid(_))
        ));
    }

    /// Go-live review 16: `--rotate-tls` adds the key version first. Reading the latest
    /// versions then gives a mismatched pair, and the api crash-looped. The installed files
    /// (deploy.sh installs matched pairs only) win.
    #[tokio::test]
    async fn installed_pair_wins_over_a_half_rotated_secret_manager() {
        let cfg = ApiConfig::default();
        let half_rotated = Pair(A_CRT, B_KEY);
        let empty = TmpDir::new();
        assert!(
            tls_from_dir_or_secrets(empty.path(), &half_rotated, &cfg)
                .await
                .is_err()
        );
        let d = dir_with(A_CRT, A_KEY);
        assert!(
            tls_from_dir_or_secrets(d.path(), &half_rotated, &cfg)
                .await
                .is_ok()
        );
        // no files at all, or a broken pair on disk: Secret Manager, if it matches
        assert!(
            tls_from_dir_or_secrets(empty.path(), &Pair(B_CRT, B_KEY), &cfg)
                .await
                .is_ok()
        );
        let broken = dir_with(A_CRT, B_KEY);
        assert!(
            tls_from_dir_or_secrets(broken.path(), &Pair(B_CRT, B_KEY), &cfg)
                .await
                .is_ok()
        );
        // nothing usable anywhere: fail closed
        assert!(
            tls_from_dir_or_secrets(broken.path(), &half_rotated, &cfg)
                .await
                .is_err()
        );
        assert!(
            tls_from_dir_or_secrets(empty.path(), &NoSecrets, &cfg)
                .await
                .is_err()
        );
    }

    #[test]
    fn listen_addresses_parse() {
        let cfg = ApiConfig::default();
        assert!(addr(&cfg.bind).is_ok());
        assert!(addr(&cfg.metrics_bind).expect("addr").ip().is_loopback());
        assert!(addr("nope").is_err());
    }
}
