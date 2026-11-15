//! # vc-frontier (Task 2, stub)
//!
//! `PgFrontier`: the Postgres frontier (claims with epoch fencing, the global DB token bucket,
//! `mark_fetched`, park ladder, batched enqueue, recrawl estimator). The only crate (besides
//! testkit) allowed to call `ClaimPermit::mint`. Migration block 11xx.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use vc_config::FrontierConfig;
use vc_core::frontier::{Claim, ClaimPermit, Frontier, KeyOutcome, LeaseCheck, NewUrl, ParkReason};
use vc_core::policy::RobotsEntry;
use vc_core::{LeaseLost, OriginKey, PolitenessKey, Result, UrlKey};

/// The Postgres frontier.
#[derive(Debug, Clone)]
pub struct PgFrontier {
    /// Pool.
    pub pool: PgPool,
    /// Settings.
    pub config: FrontierConfig,
}

impl PgFrontier {
    /// Creates a frontier.
    pub fn new(pool: PgPool, config: FrontierConfig) -> Self {
        PgFrontier { pool, config }
    }
}

#[async_trait]
impl LeaseCheck for PgFrontier {
    async fn check(&self, _p: &ClaimPermit) -> Result<(), LeaseLost> {
        todo!("Task 2")
    }
}

#[async_trait]
impl Frontier for PgFrontier {
    async fn claim(&self, _shard: &[i16], _max_urls: usize) -> Result<Option<Claim>> {
        todo!("Task 2")
    }
    async fn extend(&self, _p: &ClaimPermit, _by: Duration) -> Result<(), LeaseLost> {
        todo!("Task 2")
    }
    async fn take_rate_token(&self, _p: &ClaimPermit) -> Result<(), LeaseLost> {
        todo!("Task 2")
    }
    async fn mark_fetched(
        &self,
        _p: &ClaimPermit,
        _url: &UrlKey,
        _next: DateTime<Utc>,
    ) -> Result<(), LeaseLost> {
        todo!("Task 2")
    }
    async fn complete(&self, _p: ClaimPermit, _o: KeyOutcome) -> Result<(), LeaseLost> {
        todo!("Task 2")
    }
    async fn enqueue(&self, _batch: &[NewUrl]) -> Result<u64> {
        todo!("Task 2")
    }
    async fn park(&self, _key: &PolitenessKey, _reason: ParkReason) -> Result<()> {
        todo!("Task 2")
    }
    async fn robots(&self, _origin: &OriginKey) -> Result<Option<RobotsEntry>> {
        todo!("Task 2")
    }
    async fn store_robots(&self, _p: &ClaimPermit, _entry: RobotsEntry) -> Result<()> {
        todo!("Task 2")
    }
}
