//! `[store]`: spool, segments, blob store (owner: vc-store).

use serde::{Deserialize, Serialize};

/// Which blob store backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobBackend {
    /// Google Cloud Storage.
    Gcs,
    /// Local filesystem (tests, development).
    Fs,
}

/// Raw-store settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    /// Spool root (each role instance writes `spool/<role>-<instance>/`).
    pub spool_dir: String,
    /// Roll a segment at this many bytes (128 MiB).
    pub segment_max_bytes: u64,
    /// Roll a segment after this many seconds (600).
    pub segment_max_age_s: u64,
    /// Backend.
    pub backend: BlobBackend,
    /// Raw bucket (segments, ephemeral, text, cdxj).
    pub raw_bucket: String,
    /// Ops bucket (backups, exports, batch I/O).
    pub ops_bucket: String,
    /// Root directory for the `fs` backend.
    pub fs_root: String,
    /// Pause fetchers when the spool exceeds this many bytes (2 GB).
    pub spool_backpressure_bytes: u64,
    /// Pause fetchers when uploads fail for this long, seconds (900).
    pub upload_failure_pause_s: u64,
    /// zstd level for segments.
    pub zstd_level: i32,
    /// Ephemeral (noarchive/TDM) retention, days (30).
    pub ephemeral_days: u32,
    /// GCS endpoint override (`http://host:port` for fake-gcs-server in E2E); empty = Google.
    pub gcs_endpoint: String,
    /// Use anonymous GCS credentials (emulators only); default false = ADC.
    pub gcs_anonymous: bool,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            spool_dir: "/var/lib/voyager-crawler/spool".into(),
            segment_max_bytes: 128 * 1024 * 1024,
            segment_max_age_s: 600,
            backend: BlobBackend::Gcs,
            raw_bucket: "voyager-crawler-raw".into(),
            ops_bucket: "voyager-crawler-ops".into(),
            fs_root: "/var/lib/voyager-crawler/blobs".into(),
            spool_backpressure_bytes: 2 * 1024 * 1024 * 1024,
            upload_failure_pause_s: 900,
            zstd_level: 9,
            ephemeral_days: 30,
            gcs_endpoint: String::new(),
            gcs_anonymous: false,
        }
    }
}
