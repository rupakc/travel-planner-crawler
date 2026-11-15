//! The extracted-text tier (spec §5.5): one `jsonl.zst` object per segment under `text/`
//! (Standard storage, read by re-extraction instead of Coldline WARCs). Ephemeral segments'
//! text goes under `ephemeral/text/`.

use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use vc_core::store::{BlobStore, PutOutcome};
use vc_core::{Error, Result, SegmentId};

use crate::paths::text_path;
use crate::segment::{decompress, upload};

/// Encodes rows as zstd-compressed JSON lines.
pub fn encode_text_tier<T: Serialize>(rows: &[T], zstd_level: i32) -> Result<Bytes> {
    let mut jsonl = Vec::new();
    for r in rows {
        serde_json::to_writer(&mut jsonl, r)?;
        jsonl.push(b'\n');
    }
    zstd::bulk::compress(&jsonl, zstd_level)
        .map(Bytes::from)
        .map_err(|e| Error::Storage(format!("zstd: {e}")))
}

/// Decodes a text-tier object.
pub fn decode_text_tier<T: DeserializeOwned>(bytes: &[u8]) -> Result<Vec<T>> {
    let raw = decompress(bytes)?;
    raw.split(|&c| c == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).map_err(Error::from))
        .collect()
}

/// Writes a segment's text-tier object (create-if-absent) and returns its path.
pub async fn put_text_tier<T: Serialize>(
    store: &dyn BlobStore,
    segment_id: SegmentId,
    ephemeral: bool,
    rows: &[T],
    zstd_level: i32,
) -> Result<(String, PutOutcome)> {
    let path = text_path(segment_id, ephemeral);
    let b = encode_text_tier(rows, zstd_level)?;
    let o = upload(store, &path, b).await?;
    Ok((path, o))
}
