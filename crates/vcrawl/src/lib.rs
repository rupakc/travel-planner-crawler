//! # vcrawl
//!
//! The single binary: `run` (several roles in one process, [`run`]), roles `scheduler`, `fetcher`, `processor`, `embedder`, `api`,
//! `maintenance`, plus `admin`, `migrate` and `health`. Only this crate depends on
//! implementations. Layout (so parallel work does not collide):
//! `roles/<role>.rs`, `admin/<group>.rs`, `agentic/*` (Task 17), `migrate.rs`, `health.rs`,
//! shared wiring in `ctx.rs`.

pub mod admin;
pub mod agentic;
pub mod ctx;
pub mod data;
pub mod health;
pub mod migrate;
pub mod obs_jobs;
pub mod roles;
pub mod run;
pub mod wiring;

pub use ctx::Ctx;

/// Selects aws-lc-rs as the process-wide rustls crypto provider. Dependencies enable
/// both `ring` and `aws-lc-rs`, and rustls refuses to guess: without this every TLS
/// client (GCS, Secret Manager, Vertex) panics on first use. Idempotent.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

#[cfg(test)]
mod crypto_provider_tests {
    #[test]
    fn crypto_provider_is_installed() {
        super::install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        super::install_crypto_provider(); // second call is a no-op
    }
}
