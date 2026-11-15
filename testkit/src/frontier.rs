//! In-memory `Frontier` + `LeaseCheck` with the real fencing semantics (epochs, deadlines,
//! `fetched_pending`), driven by a [`vc_core::Clock`]. Simplifications vs `PgFrontier`: no
//! shards, no budgets, no global rate limiting (tokens are only counted), and
//! `next_allowed_at = now + delay` at completion.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use vc_core::error::{LeaseLost, LeaseLostReason};
use vc_core::frontier::{
    Claim, ClaimPermit, Frontier, KeyOutcome, KeySignal, LeaseCheck, LeasedUrl, NewUrl, ParkReason,
    PermitScope, UrlResult, Validators,
};
use vc_core::media::{FetchMode, MediaType};
use vc_core::policy::RobotsEntry;
use vc_core::{
    CanonicalUrl, CaptureRef, Clock, DbTime, Epoch, Error, MAX_URLS_PER_CLAIM, OriginKey,
    PayloadHash, PolitenessKey, Result, SourceId, UrlKey,
};

/// URL states (mirror of `url_state.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlState {
    /// Ready to be claimed when `next_fetch_at ≤ now`.
    Due,
    /// Leased by a claim.
    Leased,
    /// Spooled; waiting for the segment commit.
    FetchedPending,
    /// Gone (404/410).
    Gone,
    /// Tombstoned.
    Tombstoned,
    /// Disabled (robots/denylist).
    Disabled,
}

#[derive(Debug, Clone)]
struct UrlRow {
    url: CanonicalUrl,
    key: PolitenessKey,
    state: UrlState,
    lease_epoch: Option<Epoch>,
    next_fetch_at: DateTime<Utc>,
    priority: f32,
    depth: u8,
    source_id: Option<SourceId>,
    expect: Option<MediaType>,
    fetch_mode: FetchMode,
    validators: Validators,
    payload_hash: Option<PayloadHash>,
    last_capture: Option<CaptureRef>,
}

#[derive(Debug, Clone)]
struct KeyRow {
    next_allowed_at: DateTime<Utc>,
    parked_until: Option<DateTime<Utc>>,
    park_reason: Option<ParkReason>,
    park_level: u32,
    claim: Option<(Epoch, DateTime<Utc>)>,
}

#[derive(Debug, Default)]
struct Inner {
    keys: BTreeMap<PolitenessKey, KeyRow>,
    urls: HashMap<UrlKey, UrlRow>,
    robots: HashMap<OriginKey, RobotsEntry>,
    next_epoch: i64,
    rate_tokens: u64,
    completions: Vec<(PolitenessKey, KeyOutcome)>,
}

/// An in-memory frontier.
#[derive(Debug)]
pub struct MemFrontier {
    clock: Arc<dyn Clock>,
    per_url_budget: Duration,
    delay: Duration,
    inner: Mutex<Inner>,
}

impl MemFrontier {
    /// A frontier on `clock` with a 240 s per-URL budget and a 5 s delay.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        MemFrontier {
            clock,
            per_url_budget: Duration::from_secs(240),
            delay: Duration::from_secs(5),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Sets the per-key delay (permit delay and completion spacing).
    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }

    /// Sets the per-URL lease budget.
    pub fn with_per_url_budget(mut self, d: Duration) -> Self {
        self.per_url_budget = d;
        self
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("mem frontier lock")
    }

    /// The state of a URL.
    pub fn url_state(&self, k: &UrlKey) -> Option<UrlState> {
        self.lock().urls.get(k).map(|u| u.state)
    }

    /// `next_fetch_at` of a URL.
    pub fn next_fetch_at(&self, k: &UrlKey) -> Option<DateTime<Utc>> {
        self.lock().urls.get(k).map(|u| u.next_fetch_at)
    }

    /// When a key is parked until, if parked.
    pub fn parked_until(&self, key: &PolitenessKey) -> Option<DateTime<Utc>> {
        self.lock().keys.get(key).and_then(|k| k.parked_until)
    }

    /// Rate tokens taken so far.
    pub fn rate_tokens_taken(&self) -> u64 {
        self.lock().rate_tokens
    }

    /// Accepted completions, in order.
    pub fn completions(&self) -> Vec<(PolitenessKey, KeyOutcome)> {
        self.lock().completions.clone()
    }

    /// Simulates `vc_commit_segment` for one primary capture: updates validators when newer
    /// than the last commit, and flips `fetched_pending → due` when `epoch` matches.
    pub fn commit_capture(
        &self,
        url: &UrlKey,
        epoch: Epoch,
        capture: CaptureRef,
        validators: Validators,
        payload_hash: Option<PayloadHash>,
    ) {
        let mut g = self.lock();
        if let Some(u) = g.urls.get_mut(url) {
            let newer = u
                .last_capture
                .is_none_or(|c| c.captured_at < capture.captured_at);
            if newer {
                u.last_capture = Some(capture);
                u.validators = validators;
                if payload_hash.is_some() {
                    u.payload_hash = payload_hash;
                }
            }
            if u.state == UrlState::FetchedPending && u.lease_epoch == Some(epoch) {
                u.state = UrlState::Due;
                u.lease_epoch = None;
            }
        }
    }

    fn verify(&self, g: &Inner, p: &ClaimPermit) -> std::result::Result<(), LeaseLost> {
        match g.keys.get(p.key()).and_then(|k| k.claim) {
            Some((e, deadline)) if e == p.epoch() => {
                if self.now() < deadline {
                    Ok(())
                } else {
                    Err(p.lease_lost(LeaseLostReason::Expired))
                }
            }
            _ => Err(p.lease_lost(LeaseLostReason::Superseded)),
        }
    }

    fn park_locked(&self, g: &mut Inner, key: &PolitenessKey, reason: ParkReason) {
        let now = self.now();
        let row = g.keys.entry(key.clone()).or_insert_with(|| KeyRow {
            next_allowed_at: now,
            parked_until: None,
            park_reason: None,
            park_level: 0,
            claim: None,
        });
        let dur = if reason == ParkReason::ServerErrors {
            chrono::Duration::hours(1)
        } else {
            let hours = 24i64.saturating_mul(1i64 << row.park_level.min(5));
            row.park_level = row.park_level.saturating_add(1);
            chrono::Duration::hours(hours.min(30 * 24))
        };
        row.parked_until = Some(now + dur);
        row.park_reason = Some(reason);
    }
}

#[async_trait]
impl LeaseCheck for MemFrontier {
    async fn check(&self, p: &ClaimPermit) -> std::result::Result<(), LeaseLost> {
        let g = self.lock();
        self.verify(&g, p)
    }
}

#[async_trait]
impl Frontier for MemFrontier {
    async fn claim(&self, _shard: &[i16], max_urls: usize) -> Result<Option<Claim>> {
        if max_urls == 0 || max_urls > MAX_URLS_PER_CLAIM {
            return Err(Error::Invalid(format!(
                "max_urls must be 1..={MAX_URLS_PER_CLAIM}"
            )));
        }
        let now = self.now();
        let mut g = self.lock();
        let mut ready: Vec<(DateTime<Utc>, PolitenessKey)> = g
            .keys
            .iter()
            .filter(|(_, k)| k.next_allowed_at <= now)
            .filter(|(_, k)| k.parked_until.is_none_or(|t| t <= now))
            .filter(|(_, k)| k.claim.is_none_or(|(_, d)| d <= now))
            .map(|(key, k)| (k.next_allowed_at, key.clone()))
            .collect();
        ready.sort();
        for (_, key) in ready {
            // Leases of dead epochs return to due.
            let live = g
                .keys
                .get(&key)
                .and_then(|k| k.claim)
                .filter(|(_, d)| *d > now);
            for u in g.urls.values_mut().filter(|u| u.key == key) {
                if u.state == UrlState::Leased && live.is_none_or(|(e, _)| u.lease_epoch != Some(e))
                {
                    u.state = UrlState::Due;
                    u.lease_epoch = None;
                }
            }
            let mut due: Vec<(UrlKey, f32, DateTime<Utc>)> = g
                .urls
                .iter()
                .filter(|(_, u)| u.key == key && u.state == UrlState::Due && u.next_fetch_at <= now)
                .map(|(k, u)| (*k, u.priority, u.next_fetch_at))
                .collect();
            if due.is_empty() {
                continue;
            }
            due.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));
            due.truncate(max_urls);
            g.next_epoch += 1;
            let epoch = Epoch(g.next_epoch);
            let n = u32::try_from(due.len()).unwrap_or(u32::MAX);
            let budget = chrono::Duration::from_std(self.per_url_budget * n)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let deadline = now + budget;
            if let Some(k) = g.keys.get_mut(&key) {
                k.claim = Some((epoch, deadline));
            }
            let mut urls = Vec::with_capacity(due.len());
            let mut robots_due: Vec<OriginKey> = Vec::new();
            for (uk, _, _) in &due {
                let u = g.urls.get_mut(uk).expect("url exists");
                u.state = UrlState::Leased;
                u.lease_epoch = Some(epoch);
                let origin = u.url.origin();
                urls.push(LeasedUrl {
                    url_key: *uk,
                    url: u.url.clone(),
                    source_id: u.source_id.clone(),
                    priority: u.priority,
                    depth: u.depth,
                    expect: u.expect,
                    fetch_mode: u.fetch_mode.clone(),
                    validators: u.validators.clone(),
                    last_payload_hash: u.payload_hash,
                    last_capture: u.last_capture,
                });
                if !robots_due.contains(&origin) {
                    robots_due.push(origin);
                }
            }
            robots_due.retain(|o| g.robots.get(o).is_none_or(|r| !r.is_fresh(now)));
            #[allow(clippy::disallowed_methods)] // testkit is allowed to mint permits
            let permit = ClaimPermit::mint(
                key.clone(),
                epoch,
                DbTime(deadline),
                Arc::new(PermitScope::new(key.clone())),
                self.delay,
            );
            return Ok(Some(Claim {
                permit,
                urls,
                robots_due,
            }));
        }
        Ok(None)
    }

    async fn extend(&self, p: &ClaimPermit, by: Duration) -> std::result::Result<(), LeaseLost> {
        let mut g = self.lock();
        self.verify(&g, p)?;
        let by = chrono::Duration::from_std(by).unwrap_or_else(|_| chrono::Duration::zero());
        let k = g.keys.get_mut(p.key()).expect("verified key exists");
        if let Some((e, d)) = k.claim {
            let nd = d + by;
            k.claim = Some((e, nd));
            #[allow(clippy::disallowed_methods)] // testkit is allowed to move deadlines
            p.extend_to(DbTime(nd));
        }
        Ok(())
    }

    async fn take_rate_token(&self, p: &ClaimPermit) -> std::result::Result<(), LeaseLost> {
        let mut g = self.lock();
        self.verify(&g, p)?;
        g.rate_tokens += 1;
        Ok(())
    }

    async fn mark_fetched(
        &self,
        p: &ClaimPermit,
        url: &UrlKey,
        next_fetch_at: DateTime<Utc>,
    ) -> std::result::Result<(), LeaseLost> {
        let mut g = self.lock();
        self.verify(&g, p)?;
        match g.urls.get_mut(url) {
            Some(u) if u.state == UrlState::Leased && u.lease_epoch == Some(p.epoch()) => {
                u.state = UrlState::FetchedPending;
                u.next_fetch_at = next_fetch_at;
                Ok(())
            }
            _ => Err(p.lease_lost(LeaseLostReason::Superseded)),
        }
    }

    async fn complete(
        &self,
        p: ClaimPermit,
        outcome: KeyOutcome,
    ) -> std::result::Result<(), LeaseLost> {
        let now = self.now();
        let mut g = self.lock();
        let current = g.keys.get(p.key()).and_then(|k| k.claim).map(|(e, _)| e);
        if current != Some(p.epoch()) {
            return Err(p.lease_lost(LeaseLostReason::Superseded));
        }
        for o in &outcome.urls {
            if let Some(u) = g.urls.get_mut(&o.url_key) {
                if u.lease_epoch != Some(p.epoch()) {
                    continue;
                }
                match &o.result {
                    UrlResult::Fetched => {}
                    UrlResult::NotAttempted => {
                        u.state = UrlState::Due;
                        u.lease_epoch = None;
                    }
                    UrlResult::Retry { at } | UrlResult::Failed { retry_at: at, .. } => {
                        u.state = UrlState::Due;
                        u.lease_epoch = None;
                        u.next_fetch_at = *at;
                    }
                    UrlResult::Gone => {
                        u.state = UrlState::Gone;
                        u.lease_epoch = None;
                    }
                    UrlResult::Disallowed => {
                        u.state = UrlState::Disabled;
                        u.lease_epoch = None;
                    }
                }
            }
        }
        // Leased URLs not mentioned go back to due.
        for u in g.urls.values_mut() {
            if u.state == UrlState::Leased && u.lease_epoch == Some(p.epoch()) {
                u.state = UrlState::Due;
                u.lease_epoch = None;
            }
        }
        let delay =
            chrono::Duration::from_std(p.delay()).unwrap_or_else(|_| chrono::Duration::zero());
        if let Some(k) = g.keys.get_mut(p.key()) {
            k.claim = None;
            k.next_allowed_at = now + delay;
        }
        if let KeySignal::Park(reason) = &outcome.signal {
            self.park_locked(&mut g, p.key(), reason.clone());
        }
        g.completions.push((p.key().clone(), outcome));
        Ok(())
    }

    async fn enqueue(&self, batch: &[NewUrl]) -> Result<u64> {
        let now = self.now();
        let mut g = self.lock();
        let mut n = 0;
        for nu in batch {
            let uk = nu.url.key();
            if g.urls.contains_key(&uk) {
                continue;
            }
            g.keys.entry(nu.key.clone()).or_insert_with(|| KeyRow {
                next_allowed_at: now,
                parked_until: None,
                park_reason: None,
                park_level: 0,
                claim: None,
            });
            g.urls.insert(
                uk,
                UrlRow {
                    url: nu.url.clone(),
                    key: nu.key.clone(),
                    state: UrlState::Due,
                    lease_epoch: None,
                    next_fetch_at: nu.not_before.unwrap_or(now),
                    priority: nu.priority,
                    depth: nu.depth,
                    source_id: nu.source_id.clone(),
                    expect: nu.expect,
                    fetch_mode: nu.fetch_mode.clone(),
                    validators: Validators::default(),
                    payload_hash: None,
                    last_capture: None,
                },
            );
            n += 1;
        }
        Ok(n)
    }

    async fn park(&self, key: &PolitenessKey, reason: ParkReason) -> Result<()> {
        let mut g = self.lock();
        self.park_locked(&mut g, key, reason);
        Ok(())
    }

    async fn robots(&self, origin: &OriginKey) -> Result<Option<RobotsEntry>> {
        Ok(self.lock().robots.get(origin).cloned())
    }

    async fn store_robots(&self, p: &ClaimPermit, entry: RobotsEntry) -> Result<()> {
        let mut g = self.lock();
        self.verify(&g, p)?;
        g.robots.insert(entry.origin.clone(), entry);
        Ok(())
    }
}
