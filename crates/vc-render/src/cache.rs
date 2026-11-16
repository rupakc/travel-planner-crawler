//! The shared sub-resource cache (spec §17.1): content-addressed, TTL 24 h, shared across
//! renders, so most JS/CSS of a site is fetched once a day.
//!
//! Bodies are stored once per BLAKE3 digest; URLs point at digests. Only successful (200)
//! scripts and stylesheets are cached; a lookup happens **after** every policy check, so a cached
//! entry never bypasses robots, scope or the denylist. In memory, bounded by bytes (oldest URL
//! evicted first).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use vc_core::Clock;
use vc_core::fetch::Exchange;

#[derive(Debug, Clone)]
struct Entry {
    digest: [u8; 32],
    stored_at: DateTime<Utc>,
    /// The original exchange with an empty body (the body lives in `blobs`).
    meta: Exchange,
}

#[derive(Debug, Default)]
struct Inner {
    by_url: HashMap<String, Entry>,
    blobs: HashMap<[u8; 32], (Bytes, usize)>,
    bytes: u64,
}

impl Inner {
    fn unref(&mut self, digest: &[u8; 32]) {
        let gone = match self.blobs.get_mut(digest) {
            Some((body, refs)) => {
                *refs -= 1;
                (*refs == 0).then(|| body.len() as u64)
            }
            None => None,
        };
        if let Some(n) = gone {
            self.blobs.remove(digest);
            self.bytes = self.bytes.saturating_sub(n);
        }
    }

    fn remove(&mut self, url: &str) {
        if let Some(e) = self.by_url.remove(url) {
            self.unref(&e.digest);
        }
    }
}

/// The content-addressed sub-resource cache. Cheap to share (`Arc`).
#[derive(Debug)]
pub struct SubresourceCache {
    clock: Arc<dyn Clock>,
    ttl: Duration,
    max_bytes: u64,
    inner: Mutex<Inner>,
}

impl SubresourceCache {
    /// A cache with `ttl` and a cap of `max_bytes` stored body bytes.
    pub fn new(clock: Arc<dyn Clock>, ttl: Duration, max_bytes: u64) -> Self {
        SubresourceCache {
            clock,
            ttl,
            max_bytes,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fresh(&self, stored_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        let ttl = chrono::Duration::from_std(self.ttl).unwrap_or(chrono::Duration::MAX);
        now < stored_at + ttl
    }

    /// The cached exchange for `url`, if present and younger than the TTL.
    pub fn get(&self, url: &str) -> Option<Exchange> {
        let now = self.clock.now();
        let mut g = self.lock();
        let e = g.by_url.get(url)?.clone();
        if !self.fresh(e.stored_at, now) {
            g.remove(url);
            return None;
        }
        let body = g.blobs.get(&e.digest)?.0.clone();
        let mut ex = e.meta;
        ex.body = body;
        Some(ex)
    }

    /// Stores a successful exchange under `url` (ignored unless the status is 200 and the body
    /// fits the cache).
    pub fn put(&self, url: &str, ex: &Exchange) {
        if ex.status != http::StatusCode::OK || ex.body.len() as u64 > self.max_bytes {
            return;
        }
        let digest = *blake3::hash(&ex.body).as_bytes();
        let now = self.clock.now();
        let mut g = self.lock();
        g.remove(url);
        match g.blobs.get_mut(&digest) {
            Some((_, refs)) => *refs += 1,
            None => {
                g.bytes += ex.body.len() as u64;
                g.blobs.insert(digest, (ex.body.clone(), 1));
            }
        }
        let mut meta = ex.clone();
        meta.body = Bytes::new();
        g.by_url.insert(
            url.to_string(),
            Entry {
                digest,
                stored_at: now,
                meta,
            },
        );
        // Evict expired entries, then the oldest, until under the cap.
        let expired: Vec<String> = g
            .by_url
            .iter()
            .filter(|(_, e)| !self.fresh(e.stored_at, now))
            .map(|(u, _)| u.clone())
            .collect();
        for u in expired {
            g.remove(&u);
        }
        while g.bytes > self.max_bytes {
            let oldest = g
                .by_url
                .iter()
                .min_by_key(|(_, e)| e.stored_at)
                .map(|(u, _)| u.clone());
            match oldest {
                Some(u) => g.remove(&u),
                None => break,
            }
        }
    }

    /// Stored body bytes (each distinct body counted once).
    pub fn stored_bytes(&self) -> u64 {
        self.lock().bytes
    }

    /// Number of cached URLs.
    pub fn len(&self) -> usize {
        self.lock().by_url.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, Method, StatusCode};
    use std::sync::Mutex as StdMutex;
    use vc_core::CanonicalUrl;
    use vc_core::fetch::RequestPurpose;

    #[derive(Debug)]
    struct TestClock(StdMutex<DateTime<Utc>>);
    impl Clock for TestClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    fn ex(url: &str, status: u16, body: &str) -> Exchange {
        Exchange {
            url: CanonicalUrl::parse(url).unwrap(),
            redirects: Vec::new(),
            method: Method::GET,
            request_headers: HeaderMap::new(),
            status: StatusCode::from_u16(status).unwrap(),
            response_headers: HeaderMap::new(),
            body: Bytes::from(body.to_string()),
            original_encoding: None,
            truncated: false,
            peer_ip: None,
            started_at: Utc::now(),
            ended_at: Utc::now(),
            purpose: RequestPurpose::SubResource,
        }
    }

    fn cache(max: u64) -> (Arc<TestClock>, SubresourceCache) {
        let c = Arc::new(TestClock(StdMutex::new(Utc::now())));
        (
            c.clone(),
            SubresourceCache::new(c, Duration::from_secs(24 * 3600), max),
        )
    }

    #[test]
    fn identical_bodies_stored_once() {
        let (_, c) = cache(1 << 20);
        c.put(
            "https://a.test/x.js",
            &ex("https://a.test/x.js", 200, "lib"),
        );
        c.put(
            "https://b.test/y.js",
            &ex("https://b.test/y.js", 200, "lib"),
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c.stored_bytes(), 3);
        assert_eq!(c.get("https://b.test/y.js").unwrap().body, "lib");
    }

    #[test]
    fn only_200_cached() {
        let (_, c) = cache(1 << 20);
        c.put("https://a.test/x.js", &ex("https://a.test/x.js", 404, "nf"));
        assert!(c.get("https://a.test/x.js").is_none());
    }

    #[test]
    fn ttl_expires() {
        let (clock, c) = cache(1 << 20);
        c.put(
            "https://a.test/x.js",
            &ex("https://a.test/x.js", 200, "lib"),
        );
        *clock.0.lock().unwrap() += chrono::Duration::hours(23);
        assert!(c.get("https://a.test/x.js").is_some());
        *clock.0.lock().unwrap() += chrono::Duration::hours(2);
        assert!(c.get("https://a.test/x.js").is_none());
        assert_eq!(c.stored_bytes(), 0);
    }

    #[test]
    fn evicts_oldest_over_cap() {
        let (clock, c) = cache(10);
        c.put(
            "https://a.test/1.js",
            &ex("https://a.test/1.js", 200, "aaaaaa"),
        );
        *clock.0.lock().unwrap() += chrono::Duration::seconds(1);
        c.put(
            "https://a.test/2.js",
            &ex("https://a.test/2.js", 200, "bbbbbb"),
        );
        assert!(c.get("https://a.test/1.js").is_none());
        assert!(c.get("https://a.test/2.js").is_some());
        assert!(c.stored_bytes() <= 10);
        // Larger than the whole cache: not stored.
        c.put(
            "https://a.test/3.js",
            &ex("https://a.test/3.js", 200, "ccccccccccccc"),
        );
        assert!(c.get("https://a.test/3.js").is_none());
    }

    #[test]
    fn replacing_a_url_releases_the_old_body() {
        let (_, c) = cache(1 << 20);
        c.put("https://a.test/x.js", &ex("https://a.test/x.js", 200, "v1"));
        c.put(
            "https://a.test/x.js",
            &ex("https://a.test/x.js", 200, "v2-new"),
        );
        assert_eq!(c.stored_bytes(), 6);
        assert_eq!(c.get("https://a.test/x.js").unwrap().body, "v2-new");
    }
}
