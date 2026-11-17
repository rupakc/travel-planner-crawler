//! `vcrawl admin client add|list|revoke` against the api role's `/v1`: a registered token gets
//! 200, a revoked one 401; only the argon2 hash is stored and nothing prints the token.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::StatusCode;
use testkit::vc_api_fixtures::MemPlaces;
use vc_core::secrets::{Secret, SecretSource};
use vcrawl::admin::client;

const TOKEN: &str = "voyager.0123456789abcdef-ZYX_w";

/// A secret source holding one secret (stands in for Secret Manager).
struct OneSecret(&'static str, &'static str);

#[async_trait::async_trait]
impl SecretSource for OneSecret {
    async fn get(&self, name: &str) -> vc_core::Result<Secret> {
        if name == self.0 {
            Ok(Secret::new(format!("{}\n", self.1)))
        } else {
            Err(vc_core::Error::NotFound(name.into()))
        }
    }
}

async fn serve(pool: sqlx::PgPool) -> String {
    let state = vcrawl::data::api::state(
        pool,
        vc_config::ApiConfig::default(),
        vc_config::IndexConfig::default(),
        Arc::new(MemPlaces::default()),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = vc_api::router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn stats(base: &str, token: &str) -> StatusCode {
    reqwest::Client::new()
        .get(format!("{base}/v1/stats"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn added_client_gets_200_and_revoked_gets_401() {
    let pool = testkit::pg().await;
    // The token comes from Secret Manager (`crawler-api-token-voyager`).
    let secrets = OneSecret("crawler-api-token-voyager", TOKEN);
    let token = client::token_from(&secrets, "voyager").await.unwrap();
    let out = client::add(&pool, "voyager", &token).await.unwrap();
    assert_eq!(out["enabled"], true);
    assert_eq!(out["rotated"], false);
    assert!(!out.to_string().contains("0123456789abcdef"), "{out}");

    let hash: String =
        sqlx::query_scalar("SELECT token_hash FROM api_clients WHERE client_id = 'voyager'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(hash.starts_with("$argon2id$") && !hash.contains("0123456789abcdef"));
    let rows = client::list(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!serde_json::to_string(&rows).unwrap().contains("argon2"));

    let base = serve(pool.clone()).await;
    assert_eq!(stats(&base, TOKEN).await, StatusCode::OK);
    assert_eq!(
        stats(&base, "voyager.wrong-wrong-wrong-wrong").await,
        StatusCode::UNAUTHORIZED
    );

    client::revoke(&pool, "voyager").await.unwrap();
    // A fresh API process (no 60 s verification cache) refuses the revoked token.
    let base = serve(pool.clone()).await;
    assert_eq!(stats(&base, TOKEN).await, StatusCode::UNAUTHORIZED);
    assert!(!client::list(&pool).await.unwrap()[0].enabled);

    // Re-adding rotates and re-enables.
    let out = client::add(&pool, "voyager", &Secret::new(TOKEN))
        .await
        .unwrap();
    assert_eq!(out["rotated"], true);
    let base = serve(pool.clone()).await;
    assert_eq!(stats(&base, TOKEN).await, StatusCode::OK);
}

#[tokio::test]
async fn add_refuses_tokens_without_the_client_prefix() {
    let pool = testkit::pg().await;
    for bad in [
        "0123456789abcdef0123",
        "other.0123456789abcdef",
        "voyager.short",
        "voyager.",
    ] {
        let e = client::add(&pool, "voyager", &Secret::new(bad))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            bad == "voyager." || !e.contains(bad),
            "the error echoes the token: {e}"
        );
    }
    assert!(client::list(&pool).await.unwrap().is_empty());
    assert!(client::revoke(&pool, "nobody").await.is_err());
}
