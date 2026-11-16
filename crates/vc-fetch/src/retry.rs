//! `Retry-After` (RFC 9110 §10.2.3): delay-seconds or an HTTP-date.

use std::time::Duration;

use chrono::{DateTime, Utc};
use http::HeaderMap;

/// Parses a `Retry-After` value relative to `now`: a non-negative integer number of seconds, or
/// an HTTP-date (a date in the past gives zero). Anything else is `None`.
pub fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let v = value.trim();
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        return v.parse::<u64>().ok().map(Duration::from_secs);
    }
    let at: DateTime<Utc> = httpdate::parse_http_date(v).ok()?.into();
    Some((at - now).to_std().unwrap_or(Duration::ZERO))
}

/// The parsed `Retry-After` header of a response, if present and valid.
pub fn retry_after(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    let v = headers.get(http::header::RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(v, now)
}
