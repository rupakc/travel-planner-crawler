//! `MemFrontier` (real leases) with a settable robots cache and a log of enqueued URLs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use testkit::{FakeClock, MemFrontier};
use vc_core::error::LeaseLost;
use vc_core::frontier::{Claim, ClaimPermit, Frontier, KeyOutcome, LeaseCheck, NewUrl, ParkReason};
use vc_core::policy::{RobotsEntry, RobotsStatus};
use vc_core::{CanonicalUrl, Clock, OriginKey, PolitenessKey, Result, UrlKey};

pub struct TestFrontier {
    pub inner: MemFrontier,
    pub clock: FakeClock,
    robots: Mutex<HashMap<OriginKey, RobotsEntry>>,
    enqueued: Mutex<Vec<NewUrl>>,
}

impl TestFrontier {
    pub fn new(clock: FakeClock) -> Arc<Self> {
        Arc::new(TestFrontier {
            inner: MemFrontier::new(Arc::new(clock.clone())).with_delay(Duration::from_secs(1)),
            clock,
            robots: Mutex::new(HashMap::new()),
            enqueued: Mutex::new(Vec::new()),
        })
    }

    fn entry(&self, origin: &str, status: RobotsStatus, body: Option<&str>) -> RobotsEntry {
        let now: DateTime<Utc> = self.clock.now();
        RobotsEntry {
            origin: CanonicalUrl::parse(origin).unwrap().origin(),
            status,
            http_status: Some(if status == RobotsStatus::Ok { 200 } else { 404 }),
            body: body.map(|b| Bytes::from(b.to_string())),
            last_good: None,
            fetched_at: now,
            expires_at: now + chrono::Duration::days(1000),
        }
    }

    /// robots.txt of `origin` is a 404 (allow all), fresh for a long time.
    pub fn allow_all(&self, origin: &str) {
        let e = self.entry(origin, RobotsStatus::NotFound, None);
        self.robots.lock().unwrap().insert(e.origin.clone(), e);
    }

    /// robots.txt of `origin` has this body.
    pub fn robots_txt(&self, origin: &str, body: &str) {
        let e = self.entry(origin, RobotsStatus::Ok, Some(body));
        self.robots.lock().unwrap().insert(e.origin.clone(), e);
    }

    /// robots.txt of `origin` is cached but expired.
    pub fn stale_robots(&self, origin: &str) {
        let mut e = self.entry(origin, RobotsStatus::NotFound, None);
        e.expires_at = self.clock.now() - chrono::Duration::seconds(1);
        self.robots.lock().unwrap().insert(e.origin.clone(), e);
    }

    pub fn enqueued(&self) -> Vec<String> {
        self.enqueued
            .lock()
            .unwrap()
            .iter()
            .map(|n| n.url.as_str().to_string())
            .collect()
    }

    /// Enqueues `page` and claims its key: a real permit from the in-memory frontier.
    pub async fn permit(&self, page: &str) -> ClaimPermit {
        let url = CanonicalUrl::parse(page).unwrap();
        self.inner.enqueue(&[NewUrl::new(url)]).await.unwrap();
        let claim = self
            .inner
            .claim(&[], 1)
            .await
            .unwrap()
            .expect("a claimable key");
        claim.permit
    }
}

#[async_trait]
impl LeaseCheck for TestFrontier {
    async fn check(&self, p: &ClaimPermit) -> std::result::Result<(), LeaseLost> {
        self.inner.check(p).await
    }
}

#[async_trait]
impl Frontier for TestFrontier {
    async fn claim(&self, shard: &[i16], max_urls: usize) -> Result<Option<Claim>> {
        self.inner.claim(shard, max_urls).await
    }
    async fn extend(&self, p: &ClaimPermit, by: Duration) -> std::result::Result<(), LeaseLost> {
        self.inner.extend(p, by).await
    }
    async fn take_rate_token(&self, p: &ClaimPermit) -> std::result::Result<(), LeaseLost> {
        self.inner.take_rate_token(p).await
    }
    async fn mark_fetched(
        &self,
        p: &ClaimPermit,
        url: &UrlKey,
        next_fetch_at: DateTime<Utc>,
    ) -> std::result::Result<(), LeaseLost> {
        self.inner.mark_fetched(p, url, next_fetch_at).await
    }
    async fn complete(&self, p: ClaimPermit, o: KeyOutcome) -> std::result::Result<(), LeaseLost> {
        self.inner.complete(p, o).await
    }
    async fn enqueue(&self, batch: &[NewUrl]) -> Result<u64> {
        self.enqueued.lock().unwrap().extend(batch.iter().cloned());
        Ok(batch.len() as u64)
    }
    async fn park(&self, key: &PolitenessKey, reason: ParkReason) -> Result<()> {
        self.inner.park(key, reason).await
    }
    async fn robots(&self, origin: &OriginKey) -> Result<Option<RobotsEntry>> {
        Ok(self.robots.lock().unwrap().get(origin).cloned())
    }
    async fn store_robots(&self, _p: &ClaimPermit, entry: RobotsEntry) -> Result<()> {
        self.robots
            .lock()
            .unwrap()
            .insert(entry.origin.clone(), entry);
        Ok(())
    }
}
