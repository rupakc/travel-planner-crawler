//! The politeness oracle (spec §5.2, §12; plan Task 14).
//!
//! It reads the fake web's [`RequestLog`] plus a [`PolicySnapshot`] (what the fake web served:
//! robots.txt bodies, challenges, `Retry-After`; and the rules: delays, denylist, the one UA)
//! and reports every request that breaks the politeness contract:
//!
//! * **Overlap** — two requests of one politeness key in flight at the same time.
//! * **ShortGap** — a request started less than the delay after the previous one ended. Pages
//!   (anything that is not a render sub-resource) need `max(key min delay, robots Crawl-delay)`
//!   after the previous *page*; sub-resources need the sub-resource gap (250 ms, §17.1) after
//!   the previous request of the key.
//! * **NoRobots** — a request to an origin whose robots.txt had not been fetched yet.
//! * **Disallowed** — a path disallowed by the robots.txt most recently served to the crawler
//!   for that origin (`VoyagerBot` group, else `*`; 5xx/429 → everything disallowed). The
//!   oracle uses its own robots parser (`texting_robots`), independent of `vc-policy`.
//! * **KeyBlocked** — any request to a key after a robots.txt of the key answered with a
//!   challenge.
//! * **OriginBlocked** — any request to an origin (its robots.txt included) after its robots.txt
//!   answered 401/403. robots.txt speaks for its own origin (RFC 9309 §2.3), so other hosts of
//!   the key stay governed by their own robots.txt.
//! * **AfterPark** — a request inside a back-off window: a challenge or a 401/403 on anything but
//!   robots.txt parks the key for [`PolicySnapshot::park_ms`] (24 h); a scripted [`Backoff`]
//!   (e.g. `Retry-After`) too.
//! * **Denylisted** — a request to a denylisted host (`host`, its subdomains, or `brand.*`).
//! * **WrongUserAgent** — anything other than the one honest UA.
//!
//! The oracle is deliberately strict and must never be loosened to make a test pass.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use url::Url;
use vc_core::{PolitenessKey, ROBOTS_TOKEN, USER_AGENT};

use crate::request_log::{RequestLog, RequestLogEntry};

/// Per-key rules (overrides of the snapshot defaults).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRules {
    /// Minimum gap between pages of this key (default: [`PolicySnapshot::default_min_delay_ms`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_delay_ms: Option<u64>,
    /// Path prefixes that are render sub-resources on every origin of this key (`/` = all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subresource_paths: Vec<String>,
}

/// One robots.txt response the crawler received (recorded by the fake web).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RobotsServed {
    /// `scheme://host[:port]` (as `url::Url::origin().ascii_serialization()`).
    pub origin: String,
    /// When the response finished.
    pub at: DateTime<Utc>,
    /// HTTP status.
    pub status: u16,
    /// The body served.
    #[serde(default)]
    pub body: String,
    /// The response was a bot challenge (→ the whole key is do-not-crawl).
    #[serde(default)]
    pub challenge: bool,
}

/// A window in which a key must not be requested (park after a challenge, `Retry-After`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backoff {
    /// Politeness key.
    pub key: String,
    /// Start (the end of the response that caused it).
    pub from: DateTime<Utc>,
    /// End (exclusive).
    pub until: DateTime<Utc>,
    /// `challenge`, `retry_after`, `unauthorized`, …
    pub reason: String,
}

impl Backoff {
    /// A 24 h park of `key` starting at `from` (a challenge page).
    pub fn park(key: &str, from: DateTime<Utc>) -> Backoff {
        Backoff {
            key: key.to_string(),
            from,
            until: from + ChronoDuration::milliseconds(DEFAULT_PARK_MS as i64),
            reason: "challenge".to_string(),
        }
    }
}

const DEFAULT_MIN_DELAY_MS: u64 = 5_000;
const DEFAULT_SUBRESOURCE_GAP_MS: u64 = 250;
const DEFAULT_PARK_MS: u64 = 24 * 3_600 * 1_000;

fn default_min_delay_ms() -> u64 {
    DEFAULT_MIN_DELAY_MS
}
fn default_subresource_gap_ms() -> u64 {
    DEFAULT_SUBRESOURCE_GAP_MS
}
fn default_park_ms() -> u64 {
    DEFAULT_PARK_MS
}
fn default_ua() -> Option<String> {
    Some(USER_AGENT.to_string())
}
fn default_true() -> bool {
    true
}

/// Everything the oracle needs besides the log. Serialisable, so the fake web can hand it to
/// an E2E runner (`GET /__admin/snapshot`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySnapshot {
    /// The only acceptable `User-Agent` (`None` = don't check).
    #[serde(default = "default_ua")]
    pub user_agent: Option<String>,
    /// Minimum gap between pages of a key (5 s, the source `min_delay_s` floor).
    #[serde(default = "default_min_delay_ms")]
    pub default_min_delay_ms: u64,
    /// Minimum gap before a render sub-resource (250 ms, §17.1).
    #[serde(default = "default_subresource_gap_ms")]
    pub subresource_gap_ms: u64,
    /// How long a challenge or 401/403 parks a key (24 h).
    #[serde(default = "default_park_ms")]
    pub park_ms: u64,
    /// Whether a request before the origin's first robots.txt fetch is a violation.
    #[serde(default = "default_true")]
    pub require_robots_first: bool,
    /// Per-key overrides.
    #[serde(default)]
    pub keys: BTreeMap<String, KeyRules>,
    /// robots.txt responses served, in any order.
    #[serde(default)]
    pub robots: Vec<RobotsServed>,
    /// Back-off windows.
    #[serde(default)]
    pub backoffs: Vec<Backoff>,
    /// Denylisted hosts: `host` (and subdomains) or `brand.*`.
    #[serde(default)]
    pub denylist: Vec<String>,
}

impl Default for PolicySnapshot {
    fn default() -> Self {
        PolicySnapshot {
            user_agent: default_ua(),
            default_min_delay_ms: DEFAULT_MIN_DELAY_MS,
            subresource_gap_ms: DEFAULT_SUBRESOURCE_GAP_MS,
            park_ms: DEFAULT_PARK_MS,
            require_robots_first: true,
            keys: BTreeMap::new(),
            robots: Vec::new(),
            backoffs: Vec::new(),
            denylist: Vec::new(),
        }
    }
}

impl PolicySnapshot {
    /// The default rules: UA = [`USER_AGENT`], 5 s page delay, 250 ms sub-resource gap, 24 h
    /// park, robots first.
    pub fn new() -> Self {
        PolicySnapshot::default()
    }

    /// Records a robots.txt response.
    pub fn robots_served(
        mut self,
        origin: &str,
        at: DateTime<Utc>,
        status: u16,
        body: &str,
    ) -> Self {
        self.robots.push(RobotsServed {
            origin: origin_of(origin).unwrap_or_else(|| origin.to_string()),
            at,
            status,
            body: body.to_string(),
            challenge: false,
        });
        self
    }

    /// Records a robots.txt answered by a challenge page.
    pub fn robots_challenge(mut self, origin: &str, at: DateTime<Utc>) -> Self {
        self.robots.push(RobotsServed {
            origin: origin_of(origin).unwrap_or_else(|| origin.to_string()),
            at,
            status: 200,
            body: String::new(),
            challenge: true,
        });
        self
    }

    /// Adds a back-off window.
    pub fn backoff(mut self, b: Backoff) -> Self {
        self.backoffs.push(b);
        self
    }

    /// Adds a denylist pattern.
    pub fn deny(mut self, pattern: &str) -> Self {
        self.denylist.push(pattern.to_string());
        self
    }

    /// Sets the rules of one key.
    pub fn key_rules(mut self, key: &str, r: KeyRules) -> Self {
        self.keys.insert(key.to_string(), r);
        self
    }

    fn rules(&self, key: &str) -> KeyRules {
        self.keys.get(key).cloned().unwrap_or_default()
    }
}

/// The class of a violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    /// Two requests of one key in flight at once.
    Overlap,
    /// A gap shorter than the delay.
    ShortGap,
    /// No robots.txt fetched for the origin before the request.
    NoRobots,
    /// Path disallowed by the robots.txt in effect.
    Disallowed,
    /// The key is do-not-crawl (a challenge on robots.txt).
    KeyBlocked,
    /// The origin is do-not-crawl (robots.txt answered 401/403).
    OriginBlocked,
    /// Inside a park / back-off window.
    AfterPark,
    /// Denylisted host.
    Denylisted,
    /// Not the one honest UA.
    WrongUserAgent,
}

/// One violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    /// Class.
    pub kind: ViolationKind,
    /// Politeness key.
    pub key: String,
    /// The offending request's URL.
    pub url: String,
    /// The offending request's start.
    pub at: DateTime<Utc>,
    /// Human-readable detail.
    pub detail: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} key={} at={} url={}: {}",
            self.kind,
            self.key,
            self.at.to_rfc3339(),
            self.url,
            self.detail
        )
    }
}

/// `scheme://host[:port]` of a URL string.
pub(crate) fn origin_of(u: &str) -> Option<String> {
    Url::parse(u).ok().map(|u| u.origin().ascii_serialization())
}

fn host_of(u: &str) -> Option<String> {
    Url::parse(u)
        .ok()?
        .host_str()
        .map(|h| h.to_ascii_lowercase())
}

fn path_and_query(u: &Url) -> String {
    match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    }
}

fn is_robots(u: &Url) -> bool {
    u.path() == "/robots.txt"
}

/// Whether `host` matches a denylist pattern (`host`, subdomains of it, or `brand.*` = the
/// brand label directly under any public suffix).
pub fn denylisted(host: &str, pattern: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let pattern = pattern.trim().to_ascii_lowercase();
    if let Some(brand) = pattern.strip_suffix(".*") {
        let reg = PolitenessKey::for_host(&host).0;
        return reg.split('.').next() == Some(brand) && reg.contains('.');
    }
    host == pattern || host.ends_with(&format!(".{pattern}"))
}

enum RobotsState {
    AllowAll,
    DisallowAll(u16),
    KeyBlocked(&'static str),
    OriginBlocked(u16),
    Rules(Box<texting_robots::Robot>),
}

fn robots_state(r: &RobotsServed) -> RobotsState {
    if r.challenge {
        return RobotsState::KeyBlocked("challenge");
    }
    match r.status {
        200..=299 => match texting_robots::Robot::new(ROBOTS_TOKEN, r.body.as_bytes()) {
            Ok(robot) => RobotsState::Rules(Box::new(robot)),
            // An unparseable body: be strict.
            Err(_) => RobotsState::DisallowAll(r.status),
        },
        401 | 403 => RobotsState::OriginBlocked(r.status),
        429 | 500..=599 => RobotsState::DisallowAll(r.status),
        // 3xx left unresolved by the crawler, 404/410 and other 4xx: allow all.
        _ => RobotsState::AllowAll,
    }
}

/// Every violation in `log` (empty = polite). Ordered by key, then time.
pub fn check_polite(log: &RequestLog, snap: &PolicySnapshot) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut robots: Vec<&RobotsServed> = snap.robots.iter().collect();
    robots.sort_by_key(|r| r.at);
    let robots_for = |origin: &str, at: DateTime<Utc>| -> Option<&RobotsServed> {
        robots
            .iter()
            .rev()
            .find(|r| r.origin == origin && r.at <= at)
            .copied()
    };

    // Origin blocks from robots 401/403: (origin, since, status), never ending.
    let origin_blocks: Vec<(&str, DateTime<Utc>, u16)> = robots
        .iter()
        .filter_map(|r| match robots_state(r) {
            RobotsState::OriginBlocked(st) => Some((r.origin.as_str(), r.at, st)),
            _ => None,
        })
        .collect();
    // Key blocks from robots, as back-off windows that never end.
    let mut windows: Vec<Backoff> = snap.backoffs.clone();
    for r in &robots {
        if let RobotsState::KeyBlocked(why) = robots_state(r) {
            let host = host_of(&r.origin).unwrap_or_default();
            windows.push(Backoff {
                key: PolitenessKey::for_host(&host).0,
                from: r.at,
                until: DateTime::<Utc>::MAX_UTC,
                reason: format!("robots_{why}"),
            });
        }
    }

    for (key, entries) in log.by_key() {
        let rules = snap.rules(&key);
        let min_delay = rules.min_delay_ms.unwrap_or(snap.default_min_delay_ms);
        let mut key_windows: Vec<Backoff> =
            windows.iter().filter(|w| w.key == key).cloned().collect();
        // 401/403 on any response but robots.txt parks the key (detect → park); on robots.txt it
        // blocks that origin for good (`origin_blocks`).
        for e in &entries {
            if matches!(e.status, 401 | 403) && !Url::parse(&e.url).is_ok_and(|u| is_robots(&u)) {
                key_windows.push(Backoff {
                    key: key.clone(),
                    from: e.ended_at,
                    until: e.ended_at + ChronoDuration::milliseconds(snap.park_ms as i64),
                    reason: format!("http_{}", e.status),
                });
            }
        }

        let mut latest_end: Option<DateTime<Utc>> = None;
        let mut last_page_end: Option<DateTime<Utc>> = None;
        for e in &entries {
            let mut push = |kind: ViolationKind, detail: String| {
                out.push(Violation {
                    kind,
                    key: key.clone(),
                    url: e.url.clone(),
                    at: e.started_at,
                    detail,
                });
            };

            if let Some(ua) = &snap.user_agent
                && &e.ua != ua
            {
                push(ViolationKind::WrongUserAgent, format!("ua {:?}", e.ua));
            }

            let parsed = Url::parse(&e.url);
            let host = parsed
                .as_ref()
                .ok()
                .and_then(|u| u.host_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if let Some(p) = snap.denylist.iter().find(|p| denylisted(&host, p)) {
                push(ViolationKind::Denylisted, format!("matches denylist {p:?}"));
            }

            for w in &key_windows {
                if e.started_at > w.from && e.started_at < w.until {
                    let kind = if w.reason.starts_with("robots_") {
                        ViolationKind::KeyBlocked
                    } else {
                        ViolationKind::AfterPark
                    };
                    push(kind, format!("{} since {}", w.reason, w.from.to_rfc3339()));
                    break;
                }
            }

            if let Some(end) = latest_end
                && e.started_at < end
            {
                push(
                    ViolationKind::Overlap,
                    format!(
                        "started before the previous request ended ({})",
                        end.to_rfc3339()
                    ),
                );
            }

            let Ok(u) = parsed else {
                latest_end = latest_end.max(Some(e.ended_at));
                continue;
            };
            let pq = path_and_query(&u);
            let sub = rules
                .subresource_paths
                .iter()
                .any(|p| u.path().starts_with(p.as_str()));
            let origin = u.origin().ascii_serialization();
            if let Some((_, since, st)) = origin_blocks
                .iter()
                .find(|(o, since, _)| *o == origin && e.started_at > *since)
            {
                push(
                    ViolationKind::OriginBlocked,
                    format!("robots.txt answered {st} at {}", since.to_rfc3339()),
                );
            }
            let in_effect = robots_for(&origin, e.started_at);
            let crawl_delay_ms = in_effect
                .and_then(|r| match robots_state(r) {
                    RobotsState::Rules(robot) => robot.delay,
                    _ => None,
                })
                .map(|d| (f64::from(d) * 1000.0) as u64)
                .unwrap_or(0);

            // Gaps.
            let (need, since) = if sub {
                (snap.subresource_gap_ms, latest_end)
            } else {
                (min_delay.max(crawl_delay_ms), last_page_end)
            };
            if let Some(prev) = since {
                let gap = (e.started_at - prev).num_milliseconds();
                if gap >= 0 && (gap as u64) < need {
                    push(ViolationKind::ShortGap, format!("gap {gap} ms < {need} ms"));
                }
            }

            // robots.txt.
            if !is_robots(&u) {
                match in_effect {
                    None if snap.require_robots_first => push(
                        ViolationKind::NoRobots,
                        format!("no robots.txt fetched for {origin} before this request"),
                    ),
                    None => {}
                    Some(r) => match robots_state(r) {
                        RobotsState::AllowAll
                        | RobotsState::KeyBlocked(_)
                        | RobotsState::OriginBlocked(_) => {}
                        RobotsState::DisallowAll(st) => push(
                            ViolationKind::Disallowed,
                            format!("robots.txt answered {st}: everything is disallowed"),
                        ),
                        RobotsState::Rules(robot) => {
                            if !robot.allowed(&pq) {
                                push(
                                    ViolationKind::Disallowed,
                                    format!(
                                        "{pq} disallowed by robots.txt served at {}",
                                        r.at.to_rfc3339()
                                    ),
                                );
                            }
                        }
                    },
                }
            }

            latest_end = latest_end.max(Some(e.ended_at));
            if !sub {
                last_page_end = last_page_end.max(Some(e.ended_at));
            }
        }
    }
    out
}

/// Panics with a readable report if `log` breaks the politeness contract.
#[track_caller]
pub fn assert_polite(log: &RequestLog, snap: &PolicySnapshot) {
    let v = check_polite(log, snap);
    if !v.is_empty() {
        let mut msg = format!(
            "politeness oracle: {} violation(s) in {} request(s)\n",
            v.len(),
            log.len()
        );
        for x in v.iter().take(50) {
            msg.push_str(&format!("  {x}\n"));
        }
        if v.len() > 50 {
            msg.push_str(&format!("  … and {} more\n", v.len() - 50));
        }
        panic!("{msg}");
    }
}

/// The entries of `log` for one key (convenience for test diagnostics).
pub fn entries_for(log: &RequestLog, key: &str) -> Vec<RequestLogEntry> {
    log.by_key().remove(key).unwrap_or_default()
}
