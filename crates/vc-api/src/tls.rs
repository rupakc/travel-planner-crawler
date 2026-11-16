//! TLS and serving (spec §17.5): a self-signed certificate for the static internal IP, pinned
//! by the client. The certificate and key come from a [`SecretSource`]
//! (`crawler-api-tls-cert`, `crawler-api-tls-key`); the API listens with TLS on `bind`, and
//! `/metrics` listens in plain HTTP on `metrics_bind` (localhost).

use std::future::Future;
use std::net::SocketAddr;
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

/// Loads the certificate and key named in `cfg` from `secrets`.
pub async fn tls_from_secrets(secrets: &dyn SecretSource, cfg: &ApiConfig) -> Result<RustlsConfig> {
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

    #[test]
    fn listen_addresses_parse() {
        let cfg = ApiConfig::default();
        assert!(addr(&cfg.bind).is_ok());
        assert!(addr(&cfg.metrics_bind).expect("addr").ip().is_loopback());
        assert!(addr("nope").is_err());
    }
}
