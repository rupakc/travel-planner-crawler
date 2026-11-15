//! Identities. All ids serialise to stable, human-readable strings (hex for hashes, ULID text
//! for ULIDs), which is also how they are stored in Postgres (`bytea` for hashes, `text` for
//! ULIDs) and in JSON payloads such as the segment manifest.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::Error;

// ---------------------------------------------------------------------------------------------
// Hash ids
// ---------------------------------------------------------------------------------------------

/// 128-bit identity of a canonical URL: the first 16 bytes of `blake3(canonical URL)`.
///
/// Stored as `bytea` (16 bytes) in `urls.url_key`; serialised as 32 lowercase hex chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UrlKey(pub [u8; 16]);

impl UrlKey {
    /// Hashes `canonical_url`, which **must already be canonical** (use
    /// [`crate::CanonicalUrl::key`] when you hold a parsed URL).
    pub fn of(canonical_url: &str) -> Self {
        let h = blake3::hash(canonical_url.as_bytes());
        let mut out = [0u8; 16];
        out.copy_from_slice(&h.as_bytes()[..16]);
        UrlKey(out)
    }

    /// The raw 16 bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Builds a key from a 16-byte slice (e.g. a `bytea` column).
    pub fn from_slice(b: &[u8]) -> Result<Self, Error> {
        let arr: [u8; 16] = b
            .try_into()
            .map_err(|_| Error::Invalid(format!("url key must be 16 bytes, got {}", b.len())))?;
        Ok(UrlKey(arr))
    }

    /// Lowercase hex (32 chars).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for UrlKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for UrlKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UrlKey({})", self.to_hex())
    }
}

impl FromStr for UrlKey {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        let v = hex::decode(s).map_err(|e| Error::Invalid(format!("url key hex: {e}")))?;
        UrlKey::from_slice(&v)
    }
}

/// SHA-256 of a **decoded** response body (the WARC payload digest, r5 Q2).
///
/// Stored as `bytea` (32 bytes); serialised as 64 lowercase hex chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PayloadHash(pub [u8; 32]);

impl PayloadHash {
    /// The raw 32 bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex (64 chars).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The `WARC-Payload-Digest` value written by our WARC writer: `sha256:<lowercase hex>`.
    pub fn warc_digest(&self) -> String {
        format!("sha256:{}", self.to_hex())
    }
}

impl fmt::Display for PayloadHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for PayloadHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PayloadHash({})", self.to_hex())
    }
}

impl FromStr for PayloadHash {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        let v = hex::decode(s).map_err(|e| Error::Invalid(format!("payload hash hex: {e}")))?;
        let arr: [u8; 32] = v
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invalid("payload hash must be 32 bytes".into()))?;
        Ok(PayloadHash(arr))
    }
}

macro_rules! hex_serde {
    ($t:ty) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_hex())
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}
hex_serde!(UrlKey);
hex_serde!(PayloadHash);

// ---------------------------------------------------------------------------------------------
// ULID ids
// ---------------------------------------------------------------------------------------------

macro_rules! ulid_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub ulid::Ulid);

        impl $name {
            /// A new id for the current time (monotonic within the process is not guaranteed;
            /// ids are unique, and ordered by creation millisecond).
            pub fn new() -> Self {
                $name(ulid::Ulid::generate())
            }

            /// An id for a given time (random low bits).
            pub fn at(t: DateTime<Utc>) -> Self {
                let st = std::time::SystemTime::from(t);
                $name(ulid::Ulid::from_datetime(st))
            }

            /// The creation time encoded in the id.
            pub fn datetime(&self) -> DateTime<Utc> {
                DateTime::<Utc>::from_timestamp_millis(
                    i64::try_from(self.0.timestamp_ms()).unwrap_or(i64::MAX),
                )
                .unwrap_or_default()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                ulid::Ulid::from_string(s)
                    .map($name)
                    .map_err(|e| Error::Invalid(format!("{}: {e}", stringify!($name))))
            }
        }
    };
}

ulid_id!(
    /// One captured record (response, revisit, resource or metadata) in a WARC segment.
    /// Stored as `text` (26-char ULID) in `captures.capture_id`.
    CaptureId
);
ulid_id!(
    /// A WARC segment (`segments/YYYY/MM/DD/<SegmentId>.warc.zst`).
    SegmentId
);
ulid_id!(
    /// A canonical, cross-source entity (`entities.entity_id`).
    EntityId
);
ulid_id!(
    /// An extraction run (`extraction_runs.run_id`).
    ExtractionRunId
);
ulid_id!(
    /// An LLM budget reservation (`llm_ledger.id`).
    ReservationId
);
ulid_id!(
    /// A Vertex batch-prediction job row (`batch_jobs.id`).
    BatchJobId
);

/// A reference to a capture in a month-partitioned table: `(capture_id, captured_at)`.
///
/// There are no foreign keys into partitioned tables (spec §17.6); references carry both parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CaptureRef {
    /// The capture id.
    pub capture_id: CaptureId,
    /// When it was captured (the partition key).
    pub captured_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------------------------
// String / integer ids
// ---------------------------------------------------------------------------------------------

/// An origin: `scheme://host[:port]` (ASCII serialisation, default port omitted). Owns
/// robots.txt (spec §5.2).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OriginKey(pub String);

/// A politeness key: by default the registrable domain (PSL) of a host; a source may group
/// several domains under one key. Owns rate, in-flight and budgets (spec §5.2).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PolitenessKey(pub String);

impl PolitenessKey {
    /// The default key for a host: its registrable domain per the Public Suffix List, or the
    /// host itself (lowercased, without a trailing dot) for IPs, `localhost` and unknown
    /// suffixes.
    pub fn for_host(host: &str) -> Self {
        let h = host.trim_end_matches('.').to_ascii_lowercase();
        if h.parse::<std::net::IpAddr>().is_ok() || h.starts_with('[') {
            return PolitenessKey(h);
        }
        match psl::domain_str(&h) {
            Some(d) => PolitenessKey(d.to_string()),
            None => PolitenessKey(h),
        }
    }
}

/// A source id (`sources/<id>.yaml`, `sources.source_id`).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub String);

/// A stable per-page record key (spec §5.6): the JSON-LD `@id` or URL anchor, else a hash of
/// (kind, normalised name, date/geo). See [`RecordKey::from_parts`].
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordKey(pub String);

impl RecordKey {
    /// A deterministic key from parts: each part is trimmed, lowercased and whitespace-
    /// collapsed, then `h:` + the first 16 bytes of blake3 over the `\u{1f}`-joined parts, hex.
    pub fn from_parts(parts: &[&str]) -> Self {
        let norm: Vec<String> = parts
            .iter()
            .map(|p| {
                p.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase()
            })
            .collect();
        let h = blake3::hash(norm.join("\u{1f}").as_bytes());
        RecordKey(format!("h:{}", hex::encode(&h.as_bytes()[..16])))
    }
}

/// A kind id such as `event` or `visa_rule` (data-driven; there is no kinds enum, r5 Q15).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KindId(pub Arc<str>);

impl KindId {
    /// Creates a kind id.
    pub fn new(s: &str) -> Self {
        KindId(Arc::from(s))
    }
}

/// A model id such as `gemini-3.1-flash-lite` or `text-embedding-005`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub Arc<str>);

impl ModelId {
    /// Creates a model id.
    pub fn new(s: &str) -> Self {
        ModelId(Arc::from(s))
    }
}

/// A claim epoch: strictly increasing per database (`epoch_seq`); used to fence completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Epoch(pub i64);

/// A work item id (`work_items.work_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkId(pub i64);

/// A record id (`records.record_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordId(pub i64);

/// A timestamp that was read from Postgres `now()` (lease deadlines are DB time, never local
/// clocks, spec §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DbTime(pub DateTime<Utc>);

macro_rules! string_id_impls {
    ($($t:ident),*) => {$(
        impl $t {
            /// The id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($t), &*self.0)
            }
        }
        impl From<&str> for $t {
            fn from(s: &str) -> Self {
                $t(s.into())
            }
        }
    )*};
}
string_id_impls!(
    OriginKey,
    PolitenessKey,
    SourceId,
    RecordKey,
    KindId,
    ModelId
);

macro_rules! int_id_display {
    ($($t:ident),*) => {$(
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    )*};
}
int_id_display!(Epoch, WorkId, RecordId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn politeness_key_uses_registrable_domain() {
        assert_eq!(PolitenessKey::for_host("www.gov.uk").0, "www.gov.uk");
        assert_eq!(
            PolitenessKey::for_host("en.wikivoyage.org").0,
            "wikivoyage.org"
        );
        assert_eq!(
            PolitenessKey::for_host("A.B.Example.CO.UK.").0,
            "example.co.uk"
        );
        assert_eq!(PolitenessKey::for_host("127.0.0.1").0, "127.0.0.1");
    }

    #[test]
    fn ulid_ids_roundtrip_through_text() {
        let id = CaptureId::new();
        let s = id.to_string();
        assert_eq!(s.len(), 26);
        assert_eq!(s.parse::<CaptureId>().unwrap(), id);
        let j = serde_json::to_string(&id).unwrap();
        assert_eq!(j, format!("\"{s}\""));
    }

    #[test]
    fn record_key_normalises_parts() {
        assert_eq!(
            RecordKey::from_parts(&["event", "  Jazz   Night "]),
            RecordKey::from_parts(&["event", "jazz night"])
        );
        assert!(RecordKey::from_parts(&["a"]).0.starts_with("h:"));
    }
}
