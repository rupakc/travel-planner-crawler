//! `[sources]`: source specs and adapters (owner: vc-sources).

use serde::{Deserialize, Serialize};

/// Source settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourcesConfig {
    /// Directory of `<id>.yaml` specs.
    pub dir: String,
    /// Dumps run in this UTC hour window `[start, end)`.
    pub dump_window_utc: (u8, u8),
    /// Dump stream cap per adapter run, bytes.
    pub dump_max_bytes: u64,
    /// Cap on one GTFS feed download, bytes (`gtfs_static`): a feed whose `Content-Length`
    /// is larger is skipped with a report, and a stream that grows past it is cut.
    pub gtfs_max_bytes: u64,
    /// Extend the lease every this many seconds while streaming a dump.
    pub dump_extend_every_s: u64,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        SourcesConfig {
            dir: "sources".into(),
            dump_window_utc: (2, 5),
            dump_max_bytes: 2 * 1024 * 1024 * 1024,
            gtfs_max_bytes: 600 * 1024 * 1024,
            dump_extend_every_s: 60,
        }
    }
}
