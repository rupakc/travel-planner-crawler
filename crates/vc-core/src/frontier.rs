//! The frontier: claims, fenced leases, the global rate token, enqueue and park (spec §5.2,
//! §5.3, §17.2). Implemented by `vc-frontier` (`PgFrontier`) and `testkit::MemFrontier`.
//!
//! ## The permit
//!
//! A network request to a crawl target is only possible with a [`ClaimPermit`]. Its fields are
//! private and the only constructor, [`ClaimPermit::mint`], is `#[doc(hidden)]` and listed in
//! `clippy.toml` `disallowed-methods`: only `vc-frontier` and `testkit` may call it (with a
//! local `#[allow(clippy::disallowed_methods)]`). The permit is deliberately **not `Clone`**;
//! [`Frontier::complete`] consumes it.
//!
//! Before **every** request the permit client calls [`LeaseCheck::check`] and
//! [`Frontier::take_rate_token`] (r5 Q7, Q8). Socket, connect and read timeouts are
//! `min(own timeout, deadline − now)`.
//!
//! ## URL states (§17.2)
//!
//! `due` → `leased(epoch)` at claim → `fetched_pending` via [`Frontier::mark_fetched`] right
//! after the spool fsync (validators untouched) → `due` again when `vc_commit_segment` commits
//! the capture of the same epoch. `leased` URLs whose epoch is dead return to `due` at the next
//! claim of that key; `fetched_pending` URLs are never re-leased (spool replay completes them).
//! Other terminal states: `gone`, `tombstoned`, `disabled`.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{ErrorClass, LeaseLost, Result};
use crate::ids::{
    CaptureRef, DbTime, Epoch, OriginKey, PayloadHash, PolitenessKey, SourceId, UrlKey,
};
use crate::media::{FetchMode, MediaType};
use crate::policy::RobotsEntry;
use crate::url::CanonicalUrl;

/// Which hosts a permit covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermitScope {
    /// The claimed politeness key.
    pub key: PolitenessKey,
    /// Extra hosts grouped under this key by the source (`politeness_key` in a SourceSpec);
    /// matched exactly (lowercase).
    pub extra_hosts: Vec<String>,
}

impl PermitScope {
    /// A scope covering just the key.
    pub fn new(key: PolitenessKey) -> Self {
        PermitScope {
            key,
            extra_hosts: Vec::new(),
        }
    }

    /// Whether `url` falls under this permit's key. Third-party sub-resources are checked
    /// separately against the sub-resource allowlist ([`crate::policy::Policy`]).
    pub fn covers(&self, url: &CanonicalUrl) -> bool {
        url.politeness_key() == self.key || self.extra_hosts.iter().any(|h| h == url.host())
    }
}

#[derive(Debug, Default)]
struct PaceState {
    last_request_end: Option<Instant>,
    requests: u32,
}

/// The unforgeable right to make requests for one politeness key until a DB-time deadline.
pub struct ClaimPermit {
    key: PolitenessKey,
    epoch: Epoch,
    deadline_micros: Arc<AtomicI64>,
    scope: Arc<PermitScope>,
    delay_micros: Arc<AtomicU64>,
    pace: Arc<Mutex<PaceState>>,
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

impl ClaimPermit {
    /// **Only `vc-frontier` and `testkit` may call this** (enforced by `clippy.toml`).
    #[doc(hidden)]
    pub fn mint(
        key: PolitenessKey,
        epoch: Epoch,
        deadline: DbTime,
        scope: Arc<PermitScope>,
        delay: Duration,
    ) -> ClaimPermit {
        ClaimPermit {
            key,
            epoch,
            deadline_micros: Arc::new(AtomicI64::new(deadline.0.timestamp_micros())),
            scope,
            delay_micros: Arc::new(AtomicU64::new(micros(delay))),
            pace: Arc::new(Mutex::new(PaceState::default())),
        }
    }

    /// Moves the cached deadline forward after a successful [`Frontier::extend`]. Never moves
    /// it backwards. **Only `vc-frontier` and `testkit` may call this.**
    #[doc(hidden)]
    pub fn extend_to(&self, deadline: DbTime) {
        self.deadline_micros
            .fetch_max(deadline.0.timestamp_micros(), Ordering::SeqCst);
    }

    /// The claimed politeness key.
    pub fn key(&self) -> &PolitenessKey {
        &self.key
    }

    /// The claim epoch (fencing token).
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The current lease deadline (DB time; updated by [`Frontier::extend`]).
    pub fn deadline(&self) -> DbTime {
        let m = self.deadline_micros.load(Ordering::SeqCst);
        DbTime(DateTime::<Utc>::from_timestamp_micros(m).unwrap_or_default())
    }

    /// Time left until the deadline, relative to `now` (zero if past).
    pub fn remaining(&self, now: DateTime<Utc>) -> Duration {
        (self.deadline().0 - now).to_std().unwrap_or(Duration::ZERO)
    }

    /// Clamps a timeout to the lease: `min(own, deadline − now)`.
    pub fn clamp_timeout(&self, own: Duration, now: DateTime<Utc>) -> Duration {
        own.min(self.remaining(now))
    }

    /// The permit scope.
    pub fn scope(&self) -> &PermitScope {
        &self.scope
    }

    /// The per-request delay for this key: max(robots crawl-delay, source min delay,
    /// 10 × EWMA latency, penalty), capped at 1 h.
    pub fn delay(&self) -> Duration {
        Duration::from_micros(self.delay_micros.load(Ordering::SeqCst))
    }

    /// Raises the per-request delay for the rest of this claim (e.g. a `Crawl-delay` learned
    /// from the robots.txt fetched under this permit). Never lowers it, so anyone may call it.
    pub fn raise_delay(&self, d: Duration) {
        self.delay_micros.fetch_max(micros(d), Ordering::SeqCst);
    }

    /// When the last request under this permit ended (for pacing), if any.
    pub fn last_request_end(&self) -> Option<Instant> {
        self.pace.lock().map(|p| p.last_request_end).unwrap_or(None)
    }

    /// Records the end of a request (called by the permit client after each request).
    pub fn record_request_end(&self, at: Instant) {
        if let Ok(mut p) = self.pace.lock() {
            p.last_request_end = Some(at);
            p.requests = p.requests.saturating_add(1);
        }
    }

    /// Requests made under this permit so far.
    pub fn requests_made(&self) -> u32 {
        self.pace.lock().map(|p| p.requests).unwrap_or(0)
    }

    /// A cheap, cloneable handle sharing this permit's deadline and pacing state. A body
    /// stream holds it so that a [`Frontier::extend`] made **after** the stream was opened is
    /// seen by the stream, and so that it can record when the body ended
    /// ([`DeadlineHandle::record_request_end`]) for pacing. The handle cannot move the deadline
    /// and cannot make requests: it is not a permit.
    pub fn deadline_handle(&self) -> DeadlineHandle {
        DeadlineHandle {
            deadline_micros: Arc::clone(&self.deadline_micros),
            pace: Arc::clone(&self.pace),
        }
    }

    /// The `LeaseLost` error for this permit.
    pub fn lease_lost(&self, reason: crate::error::LeaseLostReason) -> LeaseLost {
        LeaseLost {
            key: self.key.0.clone(),
            epoch: self.epoch.0,
            reason,
        }
    }
}

impl std::fmt::Debug for ClaimPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimPermit")
            .field("key", &self.key)
            .field("epoch", &self.epoch)
            .field("deadline", &self.deadline())
            .field("delay", &self.delay())
            .finish_non_exhaustive()
    }
}

/// A read-only view of a [`ClaimPermit`]'s deadline (shared, so lease extensions are visible)
/// plus the ability to record the end of a request for pacing. Obtained from
/// [`ClaimPermit::deadline_handle`]; it carries no right to make requests.
#[derive(Clone)]
pub struct DeadlineHandle {
    deadline_micros: Arc<AtomicI64>,
    pace: Arc<Mutex<PaceState>>,
}

impl DeadlineHandle {
    /// The permit's current lease deadline (DB time), including any later extension.
    pub fn deadline(&self) -> DbTime {
        let m = self.deadline_micros.load(Ordering::SeqCst);
        DbTime(DateTime::<Utc>::from_timestamp_micros(m).unwrap_or_default())
    }

    /// Time left until the deadline, relative to `now` (zero if past).
    pub fn remaining(&self, now: DateTime<Utc>) -> Duration {
        (self.deadline().0 - now).to_std().unwrap_or(Duration::ZERO)
    }

    /// Clamps a timeout to the lease: `min(own, deadline − now)`.
    pub fn clamp_timeout(&self, own: Duration, now: DateTime<Utc>) -> Duration {
        own.min(self.remaining(now))
    }

    /// Records the end of a request body on the permit's pacing state (the permit's
    /// [`ClaimPermit::last_request_end`] moves to `at`, never backwards). Unlike
    /// [`ClaimPermit::record_request_end`] it does not count a new request.
    pub fn record_request_end(&self, at: Instant) {
        if let Ok(mut p) = self.pace.lock() {
            p.last_request_end = Some(p.last_request_end.map_or(at, |prev| prev.max(at)));
        }
    }

    /// When the last request under the permit ended, if any.
    pub fn last_request_end(&self) -> Option<Instant> {
        self.pace.lock().map(|p| p.last_request_end).unwrap_or(None)
    }
}

impl std::fmt::Debug for DeadlineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeadlineHandle")
            .field("deadline", &self.deadline())
            .finish_non_exhaustive()
    }
}

/// HTTP validators for a conditional GET.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Validators {
    /// `ETag`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// `Last-Modified`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

impl Validators {
    /// Whether there is nothing to send.
    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }
}

/// A URL leased by a claim.
#[derive(Debug, Clone)]
pub struct LeasedUrl {
    /// The url key.
    pub url_key: UrlKey,
    /// The canonical URL.
    pub url: CanonicalUrl,
    /// The source that discovered or owns it.
    pub source_id: Option<SourceId>,
    /// Priority (higher first).
    pub priority: f32,
    /// Link depth from a seed.
    pub depth: u8,
    /// Media-type hint set by the enqueuer (`url_state.expect`).
    pub expect: Option<MediaType>,
    /// How to fetch it.
    pub fetch_mode: FetchMode,
    /// Validators from the last **committed** capture (§5.5: never ahead of stored bodies).
    pub validators: Validators,
    /// Payload hash of the last committed capture (for revisit records).
    pub last_payload_hash: Option<PayloadHash>,
    /// The last committed capture (for revisit records).
    pub last_capture: Option<CaptureRef>,
}

/// A successful claim.
#[derive(Debug)]
pub struct Claim {
    /// The permit.
    pub permit: ClaimPermit,
    /// Up to [`crate::MAX_URLS_PER_CLAIM`] leased URLs, fetched sequentially with the delay.
    pub urls: Vec<LeasedUrl>,
    /// Origins of this key whose robots.txt is missing or expired: fetch these **first**, under
    /// the same permit.
    pub robots_due: Vec<OriginKey>,
}

/// A URL to add to the frontier.
#[derive(Debug, Clone)]
pub struct NewUrl {
    /// The canonical URL.
    pub url: CanonicalUrl,
    /// Its politeness key (default: [`CanonicalUrl::politeness_key`]).
    pub key: PolitenessKey,
    /// The owning source.
    pub source_id: Option<SourceId>,
    /// The page it was discovered on.
    pub discovered_from: Option<UrlKey>,
    /// Priority in `[0, 1]` (higher first).
    pub priority: f32,
    /// Depth from a seed.
    pub depth: u8,
    /// Media-type hint.
    pub expect: Option<MediaType>,
    /// Fetch mode.
    pub fetch_mode: FetchMode,
    /// Do not fetch before this time.
    pub not_before: Option<DateTime<Utc>>,
}

impl NewUrl {
    /// A URL with default settings (own key, priority 0.5, depth 0, HTTP).
    pub fn new(url: CanonicalUrl) -> Self {
        let key = url.politeness_key();
        NewUrl {
            url,
            key,
            source_id: None,
            discovered_from: None,
            priority: 0.5,
            depth: 0,
            expect: None,
            fetch_mode: FetchMode::Http,
            not_before: None,
        }
    }
}

/// Why a key is parked (spec §5.2 back-off). Park ladder: 24 h, doubling, up to 30 days.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum ParkReason {
    /// A challenge page / interstitial.
    Challenge,
    /// HTTP 401 or 403.
    Unauthorized {
        /// The status.
        status: u16,
    },
    /// robots.txt answered with a challenge page.
    RobotsBlocked,
    /// Five consecutive 5xx (parked 1 h).
    ServerErrors,
    /// Parked by an operator.
    Manual {
        /// Free-form note (no page content).
        note: String,
    },
}

/// What happened to the key during a claim, reported at completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySignal {
    /// Nothing special.
    Normal,
    /// 429/503 seen: honour `Retry-After`, else double the delay (up to 24 h).
    RateLimited {
        /// Parsed `Retry-After`.
        retry_after: Option<Duration>,
    },
    /// A run of 5xx (the frontier parks after 5 consecutive).
    ServerErrors {
        /// Consecutive 5xx in this claim.
        consecutive: u32,
    },
    /// Park the key.
    Park(ParkReason),
}

/// The result for one leased URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlResult {
    /// Spooled and marked `fetched_pending` (the commit returns it to `due`).
    Fetched,
    /// Not attempted (lease ran out, key parked): back to `due` unchanged.
    NotAttempted,
    /// Retry later (e.g. 429 for this URL).
    Retry {
        /// Not before.
        at: DateTime<Utc>,
    },
    /// 404/410 seen (the frontier marks `gone` after two in a row).
    Gone,
    /// Disallowed by robots or the denylist: `disabled`.
    Disallowed,
    /// Failed otherwise.
    Failed {
        /// Error class.
        class: ErrorClass,
        /// Not before.
        retry_at: DateTime<Utc>,
    },
}

/// The outcome for one leased URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlOutcome {
    /// The URL.
    pub url_key: UrlKey,
    /// What happened.
    pub result: UrlResult,
}

/// Everything the frontier needs at completion to schedule the key again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOutcome {
    /// Per-URL results (every leased URL should appear; missing ones count as `NotAttempted`).
    pub urls: Vec<UrlOutcome>,
    /// Key-level signal.
    pub signal: KeySignal,
    /// Requests made under the permit (robots and sub-requests included).
    pub requests: u32,
    /// Mean response latency observed (feeds the EWMA).
    pub mean_latency: Option<Duration>,
    /// `Crawl-delay` from robots, if known.
    pub crawl_delay: Option<Duration>,
}

impl KeyOutcome {
    /// An outcome with no URLs and a normal signal.
    pub fn normal() -> Self {
        KeyOutcome {
            urls: Vec::new(),
            signal: KeySignal::Normal,
            requests: 0,
            mean_latency: None,
            crawl_delay: None,
        }
    }
}

/// Liveness of a permit, checked before **every** request: one indexed query comparing DB
/// `now()` with the deadline and the epoch.
#[async_trait]
pub trait LeaseCheck: Send + Sync {
    /// `Ok` while the claim with this epoch is current and DB `now() < deadline`. Any failure
    /// to verify is `LeaseLost` (fail closed).
    async fn check(&self, p: &ClaimPermit) -> Result<(), LeaseLost>;
}

/// The frontier.
#[async_trait]
pub trait Frontier: LeaseCheck + Send + Sync {
    /// Claims one ready key in `shard` (not parked, under budget, `next_allowed_at ≤ now()`),
    /// with a new epoch, leasing up to `max_urls` (≤ [`crate::MAX_URLS_PER_CLAIM`]) due URLs.
    /// The deadline is `max_urls × per-URL budget`. `Ok(None)` when nothing is ready.
    async fn claim(&self, shard: &[i16], max_urls: usize) -> Result<Option<Claim>>;

    /// Extends the lease by `by` (epoch-fenced) and moves the permit's cached deadline
    /// ([`ClaimPermit::extend_to`]). Long dumps call this between chunks.
    async fn extend(&self, p: &ClaimPermit, by: Duration) -> Result<(), LeaseLost>;

    /// Checks the lease and takes one token from the global DB token bucket (default 4/s across
    /// all processes), waiting for one if needed. Called before **every** request.
    async fn take_rate_token(&self, p: &ClaimPermit) -> Result<(), LeaseLost>;

    /// `leased(epoch)` → `fetched_pending` for one URL, setting `next_fetch_at` (epoch-fenced;
    /// validators untouched). Called right after the spool record is fsynced.
    async fn mark_fetched(
        &self,
        p: &ClaimPermit,
        url: &UrlKey,
        next_fetch_at: DateTime<Utc>,
    ) -> Result<(), LeaseLost>;

    /// Completes the claim (epoch-fenced): applies per-URL results, updates delay/EWMA/back-off
    /// or parks, and sets `next_allowed_at = max(deadline, last request end + delay)`.
    async fn complete(&self, p: ClaimPermit, outcome: KeyOutcome) -> Result<(), LeaseLost>;

    /// Inserts URLs (batched, `ON CONFLICT DO NOTHING`); creates missing keys/origins. Returns
    /// the number of new URLs.
    async fn enqueue(&self, batch: &[NewUrl]) -> Result<u64>;

    /// Parks a key (ladder 24 h → ×2 → 30 d; `ServerErrors` = 1 h).
    async fn park(&self, key: &PolitenessKey, reason: ParkReason) -> Result<()>;

    /// The cached robots.txt entry for an origin, if any (fresh or not).
    async fn robots(&self, origin: &OriginKey) -> Result<Option<RobotsEntry>>;

    /// Stores a freshly fetched robots.txt entry (the raw response must already be spooled).
    /// Lease-checked: only the permit holder for the origin's key may store it.
    async fn store_robots(&self, p: &ClaimPermit, entry: RobotsEntry) -> Result<()>;

    /// Forces a refetch of one URL no later than `at` (e.g. after an upstream change-feed
    /// signal). Never bypasses politeness: only the URL's own schedule moves; the key's
    /// `next_allowed_at`, parking, budget and rate tokens still gate the claim.
    ///
    /// * `due`: `next_fetch_at` becomes `at` (earlier or later).
    /// * `leased` / `fetched_pending`: the running fetch is not disturbed; when the URL next
    ///   returns to `due` (completion or segment commit), its `next_fetch_at` is at most `at`.
    /// * `gone` / `tombstoned` / `disabled`, or an unknown key: nothing changes, `Ok(false)`.
    ///
    /// Returns whether the URL was rescheduled. The default implementation refuses with
    /// [`crate::Error::Invalid`] (frontiers that do not support it).
    async fn reschedule(&self, url: &UrlKey, at: DateTime<Utc>) -> Result<bool> {
        let _ = (url, at);
        Err(crate::Error::Invalid(
            "reschedule is not supported by this frontier".into(),
        ))
    }
}
