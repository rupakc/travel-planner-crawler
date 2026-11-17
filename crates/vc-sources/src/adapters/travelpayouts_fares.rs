//! `travelpayouts_fares`: cached cheapest fares per route and departure month from the
//! Travelpayouts / Aviasales Data API (`api_terms`, owner's free token).
//!
//! **Not live prices.** The Data API serves prices Aviasales users found in the last 2–7 days
//! (its cache). We keep them as a time series per route-month (`fare:<ORIG>:<DEST>:<YYYY-MM>`)
//! so Voyager's pricing advisor, budget planner and fare calendar can show typical fares and
//! their trend; live, bookable fares stay on SerpAPI in the app (spec §2).
//!
//! Endpoint: `GET /aviasales/v3/grouped_prices?origin=…&destination=…&group_by=month&currency=usd`
//! (rate limit 600/min per the provider; one request per route returns the cheapest fare of every
//! upcoming month, so a route costs one call per refresh).
//!
//! Rows:
//! - the seed `…/aviasales/v3/grouped_prices` (no query) is the **route index**: it makes no
//!   request and enqueues one `adapter:travelpayouts_fares` row per committed route
//!   ([`ROUTES`], both directions), so the frontier's per-key politeness and daily budget apply
//!   to each call;
//! - a route row makes exactly one request, spooled raw before it is parsed.
//!
//! Mindful use of the free token (owner: "use it mindfully"):
//! - a process-wide daily call cap ([`CallBudget`], [`TravelpayoutsFares::daily_cap`]) on top of
//!   the source's `daily_budget` (enforced per key by the frontier);
//! - a row whose last committed capture is younger than [`TravelpayoutsFares::fresh_for`] is
//!   not refetched (the fetcher also never schedules it sooner than [`MIN_REFETCH`]);
//! - 429/503 map to `RateLimited` with the response's `Retry-After` (the fetcher backs the key
//!   off); no retry inside the adapter;
//! - every call is counted ([`CallBudget::total`], exported as
//!   `crawler_adapter_calls_total{adapter="travelpayouts_fares"}`).
//!
//! The token is read from the source's `key_secret` and sent **only** in the `X-Access-Token`
//! header, never in the URL; the header is removed from the stored request headers, so it never
//! reaches a WARC record, a log line or an error.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use vc_core::fetch::FetchError;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, enqueue_adapter_rows, fetch_raw, json, on_origin};

/// The adapter id.
pub const ID: &str = "travelpayouts_fares";

/// The header carrying the token.
pub const TOKEN_HEADER: &str = "x-access-token";

/// The query parameter the API also accepts for the token: never sent, and stripped from any
/// stored URL should one ever carry it.
pub const TOKEN_PARAM: &str = "token";

/// The endpoint path (the route index row is this path without a query).
pub const PATH: &str = "/aviasales/v3/grouped_prices";

/// The currency every route row asks for.
pub const CURRENCY: &str = "usd";

/// The fetcher never schedules a route row sooner than this after a run (cached data changes
/// slowly; the provider recommends caching responses for 24 h, we keep them for days).
pub const MIN_REFETCH: Duration = Duration::from_secs(6 * 24 * 3600);

/// Committed routes (Travelpayouts city or airport IATA codes): about 50 popular international
/// city pairs, each fetched in both directions (100 rows). Order is irrelevant.
pub const ROUTES: &[(&str, &str)] = &[
    ("NYC", "LON"),
    ("NYC", "PAR"),
    ("NYC", "ROM"),
    ("NYC", "BCN"),
    ("NYC", "MAD"),
    ("NYC", "LIS"),
    ("NYC", "DUB"),
    ("NYC", "AMS"),
    ("NYC", "TYO"),
    ("NYC", "CUN"),
    ("LAX", "TYO"),
    ("LAX", "LON"),
    ("LAX", "SEL"),
    ("SFO", "TYO"),
    ("CHI", "LON"),
    ("MIA", "MAD"),
    ("YTO", "LON"),
    ("YMQ", "PAR"),
    ("LON", "PAR"),
    ("LON", "BCN"),
    ("LON", "ROM"),
    ("LON", "ATH"),
    ("LON", "IST"),
    ("LON", "DXB"),
    ("LON", "BKK"),
    ("LON", "DEL"),
    ("LON", "SIN"),
    ("LON", "JNB"),
    ("LON", "CAI"),
    ("PAR", "ROM"),
    ("PAR", "BCN"),
    ("PAR", "LIS"),
    ("PAR", "TYO"),
    ("PAR", "BKK"),
    ("PAR", "RAK"),
    ("BER", "ROM"),
    ("FRA", "BKK"),
    ("AMS", "BCN"),
    ("MAD", "BUE"),
    ("MAD", "MEX"),
    ("LIS", "SAO"),
    ("DXB", "BOM"),
    ("DXB", "DEL"),
    ("SIN", "BKK"),
    ("SIN", "SYD"),
    ("SYD", "AKL"),
    ("SYD", "TYO"),
    ("HKG", "TYO"),
    ("SEL", "TYO"),
    ("BKK", "TYO"),
];

/// Every route row's query, both directions, deduplicated and sorted.
pub fn route_pairs() -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&str, &str)> = ROUTES.iter().flat_map(|&(a, b)| [(a, b), (b, a)]).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The path and query of one route row.
pub fn route_path(origin: &str, destination: &str) -> String {
    format!("{PATH}?currency={CURRENCY}&destination={destination}&group_by=month&origin={origin}")
}

fn iata(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// What a leased row is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// The route index (no query): enqueue the route rows, no request.
    Index,
    /// One route: origin, destination (IATA).
    Route(String, String),
}

/// Classifies a leased URL; anything but the index or a well-formed route row (only the
/// parameters [`route_path`] writes, no token) is refused.
pub fn row(url: &CanonicalUrl) -> Result<Row> {
    let u = url.as_url();
    if u.path() != PATH {
        return Err(Error::Invalid("travelpayouts: unknown endpoint".into()));
    }
    if u.query().is_none_or(str::is_empty) {
        return Ok(Row::Index);
    }
    let (mut origin, mut destination, mut group, mut currency) = (None, None, None, None);
    for (k, v) in u.query_pairs() {
        let slot = match &*k {
            "origin" => &mut origin,
            "destination" => &mut destination,
            "group_by" => &mut group,
            "currency" => &mut currency,
            _ => return Err(Error::Invalid("travelpayouts: unexpected parameter".into())),
        };
        if slot.replace(v.into_owned()).is_some() {
            return Err(Error::Invalid("travelpayouts: repeated parameter".into()));
        }
    }
    match (origin, destination, group.as_deref(), currency.as_deref()) {
        (Some(o), Some(d), Some("month"), Some(CURRENCY)) if iata(&o) && iata(&d) && o != d => {
            Ok(Row::Route(o, d))
        }
        _ => Err(Error::Invalid("travelpayouts: bad route row".into())),
    }
}

// ---------------------------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------------------------

/// The cheapest cached fare of one departure month.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonthFare {
    /// `YYYY-MM`.
    pub month: String,
    /// Origin as queried (city or airport code).
    pub origin: Option<String>,
    /// Destination as queried.
    pub destination: Option<String>,
    /// Departure airport.
    pub origin_airport: Option<String>,
    /// Arrival airport.
    pub destination_airport: Option<String>,
    /// Price in the response currency.
    pub price: f64,
    /// Marketing airline IATA code.
    pub airline: Option<String>,
    /// Flight number.
    pub flight_number: Option<String>,
    /// Departure timestamp as published.
    pub departure_at: Option<String>,
    /// Return timestamp as published (round trips).
    pub return_at: Option<String>,
    /// Stops out.
    pub transfers: Option<u32>,
    /// Stops back.
    pub return_transfers: Option<u32>,
    /// Duration in minutes.
    pub duration: Option<u32>,
    /// When the cache found it, if given.
    pub found_at: Option<String>,
    /// Aviasales search path (`/search/…`), if given.
    pub link: Option<String>,
}

/// A parsed `grouped_prices` response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupedPrices {
    /// Response currency (upper case), if stated.
    pub currency: Option<String>,
    /// One entry per month, sorted by month.
    pub months: Vec<MonthFare>,
}

#[derive(Deserialize)]
struct Doc {
    success: bool,
    #[serde(default)]
    data: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    currency: Option<String>,
}

#[derive(Deserialize)]
struct RawFare {
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    destination: Option<String>,
    #[serde(default)]
    origin_airport: Option<String>,
    #[serde(default)]
    destination_airport: Option<String>,
    price: f64,
    #[serde(default)]
    airline: Option<String>,
    #[serde(default)]
    flight_number: Option<serde_json::Value>,
    #[serde(default)]
    departure_at: Option<String>,
    #[serde(default)]
    return_at: Option<String>,
    #[serde(default)]
    transfers: Option<u32>,
    #[serde(default)]
    return_transfers: Option<u32>,
    #[serde(default)]
    duration: Option<u32>,
    #[serde(default)]
    found_at: Option<String>,
    #[serde(default)]
    link: Option<String>,
}

fn is_month(k: &str) -> bool {
    let b = k.as_bytes();
    b.len() == 7
        && b[4] == b'-'
        && b[..4].iter().chain(&b[5..]).all(u8::is_ascii_digit)
        && matches!(
            &k[5..],
            "01" | "02" | "03" | "04" | "05" | "06" | "07" | "08" | "09" | "10" | "11" | "12"
        )
}

/// Parses a `grouped_prices?group_by=month` response. `success: false` is an error; entries
/// keyed by anything but `YYYY-MM`, or without a finite non-negative price, are skipped.
pub fn parse(body: &[u8]) -> Result<GroupedPrices> {
    let d: Doc = json(body)?;
    if !d.success {
        return Err(Error::Invalid("travelpayouts: success=false".into()));
    }
    let mut months = Vec::new();
    for (k, v) in d.data.unwrap_or_default() {
        if !is_month(&k) {
            continue;
        }
        let Ok(f) = serde_json::from_value::<RawFare>(v) else {
            continue;
        };
        if !(f.price.is_finite() && f.price >= 0.0) {
            continue;
        }
        months.push(MonthFare {
            month: k,
            origin: f.origin,
            destination: f.destination,
            origin_airport: f.origin_airport,
            destination_airport: f.destination_airport,
            price: f.price,
            airline: f.airline,
            flight_number: f.flight_number.and_then(|n| match n {
                serde_json::Value::String(s) => Some(s),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            }),
            departure_at: f.departure_at,
            return_at: f.return_at,
            transfers: f.transfers,
            return_transfers: f.return_transfers,
            duration: f.duration,
            found_at: f.found_at,
            link: f.link,
        });
    }
    months.sort_by(|a, b| a.month.cmp(&b.month));
    Ok(GroupedPrices {
        currency: d.currency.map(|c| c.trim().to_ascii_uppercase()),
        months,
    })
}

// ---------------------------------------------------------------------------------------------
// Call budget
// ---------------------------------------------------------------------------------------------

/// A process-wide count of the calls this adapter sent: per UTC day (the cap) and in total
/// (the metric). The frontier's per-key `daily_budget` is the cross-process authority; this is
/// the adapter's own guard on top of it.
#[derive(Debug)]
pub struct CallBudget {
    day: Mutex<(i32, u32)>,
    total: AtomicU64,
}

impl CallBudget {
    /// An empty budget.
    pub const fn new() -> Self {
        CallBudget {
            day: Mutex::new((i32::MIN, 0)),
            total: AtomicU64::new(0),
        }
    }

    /// Takes one call for the UTC day of `now`, if fewer than `cap` were taken that day.
    pub fn try_take(&self, now: DateTime<Utc>, cap: u32) -> bool {
        let today = now.date_naive().num_days_from_ce();
        let mut d = self.day.lock().unwrap_or_else(|e| e.into_inner());
        if d.0 != today {
            *d = (today, 0);
        }
        if d.1 >= cap {
            return false;
        }
        d.1 += 1;
        self.total.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Calls taken on the UTC day of `now`.
    pub fn used_on(&self, now: DateTime<Utc>) -> u32 {
        let d = self.day.lock().unwrap_or_else(|e| e.into_inner());
        if d.0 == now.date_naive().num_days_from_ce() {
            d.1
        } else {
            0
        }
    }

    /// Every call taken since the process started.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

impl Default for CallBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide budget of the registered adapter.
pub static CALLS: CallBudget = CallBudget::new();

/// Time until the next UTC midnight (the cap resets then).
fn until_next_utc_day(now: DateTime<Utc>) -> Duration {
    let next = (now.date_naive() + chrono::Days::new(1))
        .and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc());
    next.and_then(|t| (t - now).to_std().ok())
        .unwrap_or(Duration::from_secs(3600))
}

// ---------------------------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------------------------

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct TravelpayoutsFares {
    /// Calls per UTC day (process-wide, through `budget`).
    pub daily_cap: u32,
    /// A route row whose last committed capture is younger than this is not refetched.
    pub fresh_for: Duration,
    /// Where calls are counted.
    pub budget: &'static CallBudget,
}

impl TravelpayoutsFares {
    /// 150 calls per day (the source's `daily_budget` is 200 per key), fresh for 20 h.
    pub const DEFAULT: TravelpayoutsFares = TravelpayoutsFares {
        daily_cap: 150,
        fresh_for: Duration::from_secs(20 * 3600),
        budget: &CALLS,
    };
}

impl Default for TravelpayoutsFares {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[async_trait]
impl SourceAdapter for TravelpayoutsFares {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let url = ctx.url.url.clone();
        match row(&url)? {
            Row::Index => {
                let mut urls = Vec::new();
                for (o, d) in route_pairs() {
                    urls.push(on_origin(&url, &route_path(o, d))?);
                }
                enqueue_adapter_rows(ctx, ID, urls, 0.5, &mut rep).await?;
            }
            Row::Route(..) => {
                let now = ctx.clock.now();
                if let Some(last) = &ctx.url.last_capture
                    && (now - last.captured_at)
                        .to_std()
                        .is_ok_and(|age| age < self.fresh_for)
                {
                    tracing::info!(adapter = ID, "fresh; not refetched");
                    rep.complete = true;
                    return Ok(rep);
                }
                if !self.budget.try_take(now, self.daily_cap) {
                    tracing::warn!(adapter = ID, cap = self.daily_cap, "daily call cap reached");
                    return Err(Error::Fetch(FetchError::RateLimited {
                        status: 429,
                        retry_after: Some(until_next_utc_day(now)),
                    }));
                }
                let secret = ctx.source.key_secret.clone().ok_or_else(|| {
                    Error::Invalid("travelpayouts: source has no key_secret".into())
                })?;
                let secrets = ctx.secrets;
                let token = secrets.get(&secret).await?;
                let opts = Fetch {
                    accept: Some("application/json"),
                    hint: Some(MediaType::Json),
                    redact: &[TOKEN_PARAM],
                    secret_header: Some((TOKEN_HEADER, &token)),
                };
                let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
                check_status(&ex)?;
                let p = parse(&ex.body)?;
                tracing::info!(adapter = ID, months = p.months.len(), "parsed");
            }
        }
        rep.complete = true;
        Ok(rep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn routes_are_iata_and_both_ways() {
        let p = route_pairs();
        assert_eq!(p.len(), 2 * ROUTES.len(), "no duplicate pairs");
        assert!(p.iter().all(|(o, d)| iata(o) && iata(d) && o != d));
        for (o, d) in &p {
            assert!(p.contains(&(*d, *o)));
        }
    }

    #[test]
    fn rows_are_classified_and_unknown_shapes_refused() {
        let u =
            |s: &str| CanonicalUrl::parse(&format!("https://api.travelpayouts.com{s}")).unwrap();
        assert_eq!(row(&u(PATH)).unwrap(), Row::Index);
        assert_eq!(
            row(&u(&route_path("NYC", "LON"))).unwrap(),
            Row::Route("NYC".into(), "LON".into())
        );
        for bad in [
            format!("{PATH}?origin=NYC&destination=LON&group_by=month&currency=usd&token=x"),
            format!("{PATH}?origin=NYC&destination=LON&group_by=departure_at&currency=usd"),
            format!("{PATH}?origin=nyc&destination=LON&group_by=month&currency=usd"),
            format!("{PATH}?origin=NYC&destination=NYC&group_by=month&currency=usd"),
            "/v2/prices/latest".to_string(),
        ] {
            assert!(row(&u(&bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn budget_caps_per_utc_day_and_counts_total() {
        let b = CallBudget::new();
        let d1 = Utc.with_ymd_and_hms(2026, 11, 17, 23, 0, 0).unwrap();
        assert!(b.try_take(d1, 2));
        assert!(b.try_take(d1, 2));
        assert!(!b.try_take(d1, 2));
        assert_eq!((b.used_on(d1), b.total()), (2, 2));
        let d2 = d1 + chrono::Duration::hours(2);
        assert!(b.try_take(d2, 2));
        assert_eq!((b.used_on(d2), b.total()), (1, 3));
        assert_eq!(until_next_utc_day(d1), Duration::from_secs(3600));
    }

    #[test]
    fn month_keys() {
        assert!(is_month("2027-02"));
        assert!(!is_month("2027-13"));
        assert!(!is_month("2027-02-01"));
    }
}
