//! CDXJ index of a segment: one line per capture, `<surt> <yyyyMMddHHmmss> <json>`.
//!
//! The JSON carries the whole [`ManifestCapture`] (flattened [`vc_core::store::CaptureMeta`]
//! plus offset/length/record id), the segment file name and the record count, so that
//! `admin rebuild-captures` can rebuild `segments` and `captures` rows from GCS alone
//! (spec §5.5, disaster recovery).

use serde::{Deserialize, Serialize};
use vc_core::store::ManifestCapture;
use vc_core::{CanonicalUrl, Error, Result};

/// One CDXJ line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CdxjEntry {
    /// The capture (flattened).
    #[serde(flatten)]
    pub capture: ManifestCapture,
    /// The segment object path this capture lives in.
    pub filename: String,
    /// Number of WARC records in the capture's record group.
    pub records: u32,
}

/// The SURT form of a URL (`com,example)/path?q`), lowercase host, `www.` dropped.
pub fn surt(url: &CanonicalUrl) -> String {
    let host = url.host().to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let mut parts: Vec<&str> = host.split('.').collect();
    parts.reverse();
    let mut s = parts.join(",");
    if let Some(p) = url.as_url().port() {
        s.push_str(&format!(":{p}"));
    }
    s.push(')');
    s.push_str(&url.path_and_query());
    s
}

/// Serialises entries as CDXJ text, sorted by (surt, timestamp, capture id).
pub fn write_cdxj(entries: &[CdxjEntry]) -> Result<String> {
    let mut lines = Vec::with_capacity(entries.len());
    for e in entries {
        let key = surt(&e.capture.meta.url);
        let ts = e
            .capture
            .meta
            .captured_at
            .format("%Y%m%d%H%M%S")
            .to_string();
        let json = serde_json::to_string(e)?;
        lines.push((key, ts, e.capture.meta.capture_id, json));
    }
    lines.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
    let mut out = String::new();
    for (k, ts, _, json) in lines {
        out.push_str(&k);
        out.push(' ');
        out.push_str(&ts);
        out.push(' ');
        out.push_str(&json);
        out.push('\n');
    }
    Ok(out)
}

/// Parses CDXJ text (blank lines ignored).
pub fn parse_cdxj(text: &str) -> Result<Vec<CdxjEntry>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let json = line
            .splitn(3, ' ')
            .nth(2)
            .ok_or_else(|| Error::Invalid(format!("cdxj line {}: missing fields", i + 1)))?;
        out.push(
            serde_json::from_str(json)
                .map_err(|e| Error::Invalid(format!("cdxj line {}: {e}", i + 1)))?,
        );
    }
    Ok(out)
}
