//! Object-path routing (spec §5.5, §17.7).
//!
//! | what | archive | ephemeral (noarchive / TDM-reserved) |
//! |---|---|---|
//! | segment | `segments/YYYY/MM/DD/<id>.warc.zst` | `ephemeral/YYYY/MM/DD/<id>.warc.zst` |
//! | CDXJ | `cdxj/YYYY/MM/DD/<id>.cdxj` | `ephemeral/cdxj/YYYY/MM/DD/<id>.cdxj` |
//! | text tier | `text/YYYY/MM/<id>.jsonl.zst` | `ephemeral/text/YYYY/MM/<id>.jsonl.zst` |
//!
//! Ephemeral material never leaves the `ephemeral/` prefix, so the 30-day lifecycle rule covers
//! every byte derived from it. A purge rewrite adds a generation suffix (`<id>.r<N>.warc.zst`).

use vc_core::SegmentId;
use vc_core::store::{PREFIX_CDXJ, PREFIX_EPHEMERAL, PREFIX_SEGMENTS, PREFIX_TEXT};

/// The `(segment path, cdxj path)` of a segment. The date comes from the segment ULID.
pub fn segment_paths(id: SegmentId, ephemeral: bool) -> (String, String) {
    segment_paths_gen(id, ephemeral, 0)
}

/// Like [`segment_paths`] for rewrite generation `gen` (0 = original).
pub fn segment_paths_gen(id: SegmentId, ephemeral: bool, generation: u32) -> (String, String) {
    let day = id.datetime().format("%Y/%m/%d");
    let suffix = if generation == 0 {
        String::new()
    } else {
        format!(".r{generation}")
    };
    if ephemeral {
        (
            format!("{PREFIX_EPHEMERAL}{day}/{id}{suffix}.warc.zst"),
            format!("{PREFIX_EPHEMERAL}{PREFIX_CDXJ}{day}/{id}{suffix}.cdxj"),
        )
    } else {
        (
            format!("{PREFIX_SEGMENTS}{day}/{id}{suffix}.warc.zst"),
            format!("{PREFIX_CDXJ}{day}/{id}{suffix}.cdxj"),
        )
    }
}

/// The text-tier object of a segment.
pub fn text_path(id: SegmentId, ephemeral: bool) -> String {
    let month = id.datetime().format("%Y/%m");
    if ephemeral {
        format!("{PREFIX_EPHEMERAL}{PREFIX_TEXT}{month}/{id}.jsonl.zst")
    } else {
        format!("{PREFIX_TEXT}{month}/{id}.jsonl.zst")
    }
}

/// The rewrite generation encoded in a segment path (`….r3.warc.zst` → 3; none → 0).
pub fn rewrite_generation(path: &str) -> u32 {
    let stem = path.strip_suffix(".warc.zst").unwrap_or(path);
    stem.rsplit_once(".r")
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0)
}

/// Whether a path is under the ephemeral prefix.
pub fn is_ephemeral(path: &str) -> bool {
    path.starts_with(PREFIX_EPHEMERAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_by_prefix() {
        let id: SegmentId = "01JD0000000000000000000000".parse().expect("ulid");
        let (s, c) = segment_paths(id, false);
        assert!(
            s.starts_with("segments/") && s.ends_with(".warc.zst"),
            "{s}"
        );
        assert!(c.starts_with("cdxj/") && c.ends_with(".cdxj"), "{c}");
        let (s, c) = segment_paths(id, true);
        assert!(s.starts_with("ephemeral/") && c.starts_with("ephemeral/cdxj/"));
        assert!(text_path(id, false).starts_with("text/"));
        assert!(text_path(id, true).starts_with("ephemeral/text/"));
        let (r, _) = segment_paths_gen(id, false, 2);
        assert_eq!(rewrite_generation(&r), 2);
        assert_eq!(rewrite_generation(&s), 0);
        assert!(is_ephemeral(&s));
    }
}
