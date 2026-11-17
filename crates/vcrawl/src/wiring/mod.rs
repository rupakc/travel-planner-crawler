//! Shared constructors for the roles (Task 15): configuration, the DB pool, the frontier, the
//! permit client and renderer, the raw store (blob store, spool, roller), sources and
//! adapters, observability, the daily raw-bytes cap and the LLM startup probe.
//!
//! Every role builds what it needs from [`crate::Ctx`] with these functions; nothing here
//! starts work on its own.

pub mod allowlist;
pub mod config;
pub mod db;
pub mod fetch;
pub mod llm;
pub mod obs;
pub mod rawcap;
pub mod signals;
pub mod sources;
pub mod store;

use std::sync::Arc;

use sqlx::PgPool;
use vc_config::Config;
use vc_frontier::PgFrontier;

/// The instance name: `[vcrawl] instance`, else `$HOSTNAME`, else `local`.
pub fn instance_name(cfg: &Config) -> String {
    cfg.vcrawl
        .instance
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "local".into())
}

/// The Postgres frontier; the claim holder is `<role>@<instance>:<pid>` unless configured.
pub fn frontier(cfg: &Config, pool: PgPool, role: &str, instance: &str) -> Arc<PgFrontier> {
    let mut fc = cfg.frontier.clone();
    if fc.holder.is_none() {
        fc.holder = Some(format!("{role}@{instance}:{}", std::process::id()));
    }
    Arc::new(PgFrontier::new(pool, fc))
}
