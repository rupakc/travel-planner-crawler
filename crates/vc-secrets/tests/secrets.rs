//! vc-secrets: Secret Manager over REST (wiremock) and the env source.

use std::sync::Arc;

use serde_json::json;
use vc_config::{SecretBackend, SecretsConfig};
use vc_core::Error;
use vc_core::secrets::SecretSource;
use vc_secrets::{EnvSource, SecretManagerSource, StaticToken};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PATH: &str =
    "/v1/projects/voyager-prod-ohzkl/secrets/crawler-pg-password/versions/latest:access";

async fn source(server: &MockServer) -> SecretManagerSource {
    let config = SecretsConfig {
        endpoint: server.uri(),
        ..SecretsConfig::default()
    };
    SecretManagerSource::with_tokens(config, Arc::new(StaticToken::new("test-token")))
}

fn payload(value: &[u8]) -> serde_json::Value {
    json!({
        "name": "projects/123/secrets/crawler-pg-password/versions/3",
        "payload": {
            "data": vc_secrets::base64::encode(value),
            "dataCrc32c": vc_secrets::crc32c(value).to_string(),
        }
    })
}

#[tokio::test]
async fn secret_manager_reads_latest_version() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PATH))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(payload(b"hunter2\n")))
        .expect(1)
        .mount(&server)
        .await;
    let s = source(&server).await;
    let secret = s.get("crawler-pg-password").await.unwrap();
    assert_eq!(secret.expose(), "hunter2\n");
    // Neither the source nor the secret prints the value or the token.
    let dbg = format!("{s:?} {secret:?}");
    assert!(
        !dbg.contains("hunter2") && !dbg.contains("test-token"),
        "{dbg}"
    );
}

#[tokio::test]
async fn secret_manager_maps_errors() {
    let server = MockServer::start().await;
    for (name, status) in [("missing", 404u16), ("forbidden", 403), ("broken", 503)] {
        Mock::given(method("GET"))
            .and(path(format!(
                "/v1/projects/voyager-prod-ohzkl/secrets/{name}/versions/latest:access"
            )))
            .respond_with(ResponseTemplate::new(status).set_body_string("secret-ish body"))
            .mount(&server)
            .await;
    }
    let s = source(&server).await;
    assert!(matches!(s.get("missing").await, Err(Error::NotFound(_))));
    assert!(matches!(s.get("forbidden").await, Err(Error::Policy(_))));
    let e = s.get("broken").await.unwrap_err();
    assert_eq!(e.class(), vc_core::ErrorClass::Transient, "{e:?}");
    assert!(!e.to_string().contains("secret-ish"));
}

#[tokio::test]
async fn secret_manager_rejects_bad_checksum_and_payload() {
    let server = MockServer::start().await;
    let mut bad = payload(b"value");
    bad["payload"]["dataCrc32c"] = json!("12345");
    Mock::given(method("GET"))
        .and(path(PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(bad))
        .mount(&server)
        .await;
    let s = source(&server).await;
    let e = s.get("crawler-pg-password").await.unwrap_err();
    assert!(e.to_string().contains("checksum"), "{e}");
    assert!(!e.to_string().contains("value"));
}

#[tokio::test]
async fn secret_names_are_validated_before_any_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(payload(b"x")))
        .expect(0)
        .mount(&server)
        .await;
    let s = source(&server).await;
    for name in [
        "",
        "../crawler-pg-password",
        "a/b",
        "a?b",
        "a b",
        &"x".repeat(256),
    ] {
        assert!(
            matches!(s.get(name).await, Err(Error::Invalid(_))),
            "{name}"
        );
    }
}

#[tokio::test]
async fn env_source_reads_prefixed_uppercased_names() {
    // `PATH` is always set; the env backend maps `-` to `_` and uppercases.
    let s = EnvSource {
        prefix: String::new(),
    };
    assert_eq!(
        s.get("path").await.unwrap().expose(),
        std::env::var("PATH").unwrap()
    );
    let s = EnvSource {
        prefix: "VC_SECRET_".into(),
    };
    assert!(matches!(
        s.get("crawler-no-such-secret").await,
        Err(Error::NotFound(v)) if v == "VC_SECRET_CRAWLER_NO_SUCH_SECRET"
    ));
}

#[tokio::test]
async fn from_config_picks_the_backend() {
    let env = vc_secrets::from_config(&SecretsConfig {
        backend: SecretBackend::Env,
        env_prefix: String::new(),
        ..SecretsConfig::default()
    });
    assert!(env.get("path").await.is_ok());
    // The Secret Manager backend builds without credentials (they're resolved on first use).
    let _sm = vc_secrets::from_config(&SecretsConfig::default());
}
