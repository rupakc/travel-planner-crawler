# Crate: vc-store

**Purpose.** The raw store: WARC/1.1 writer and reader, CDXJ, the fsynced spool, the segment
roller, blob stores for GCS and local disk, the segment committer (`vc_commit_segment`), the
text tier and the purge rewrite. It makes "raw first" true.

Path: `crates/vc-store`. Dependencies: `warc`, `zstd`, `google-cloud-storage`, `crc32c`,
`sha2`, `fail`. Migration block `12xx` (`1201_store_rewrite.sql`); the commit function is in
`0005_queue.sql`.

## Modules and key items

| Module | Key items | What it does |
|---|---|---|
| `spool` | `Spool`, `SpoolOptions`, `SealedSpool`, `SpoolEntry` | append a frame and **fsync** before returning; a torn frame is truncated on open; sealing fixes a segment's id and content |
| `segment` | `SegmentRoller`, `build_segment`, `compress_frame`, `upload`, `read_segment`, `read_capture` | roll at 128 MiB or 600 s; build deterministically; upload with `ifGenerationMatch=0`; `tick` replays sealed files on start |
| `commit` | `PgSegmentCommitter` | calls `vc_commit_segment(jsonb)` once per segment; `commit_rewrite` for purges |
| `warc` | `encode`, `read_records`, `WarcReader`, `parse_http`, `payload_hash`, `dedupe_payload`, `verify_payload_digest` | the only WARC serialiser; decoded bodies; revisit records; deterministic record ids |
| `cdxj` | `CdxjEntry`, `write_cdxj`, `parse_cdxj`, `surt` | one line per capture with the full manifest entry, so rows can be rebuilt from GCS |
| `text` | `encode_text_tier`, `decode_text_tier`, `put_text_tier` | `text/YYYY/MM/<id>.jsonl.zst` |
| `paths` | `segment_paths`, `text_path`, `is_ephemeral`, `rewrite_generation` | object routing, including `ephemeral/` and `<id>.r<N>` generations |
| `gcs`, `fs` | `GcsBlobStore`, `GcsOptions`, `FsBlobStore`, `blob_store_from_config` | `BlobStore` implementations |
| `purge` | `rewrite_segment_without`, `finish_rewrite`, `SegmentRewrite` | rewrite a segment without some captures, crash-consistently |

WARC headers added by the crawler: `WARC-Voyager-Original-Encoding`,
`WARC-Voyager-Capture-Id`, `WARC-Voyager-Redirect`.

## Failpoints

With the `failpoints` feature (on in this crate's tests): `spool::before_fsync`,
`spool::before_seal`, `segment::before_upload`, `segment::after_upload`,
`segment::before_commit`, `segment::after_commit`.

## Config

`[store]`: spool directory, segment size and age, backend (`gcs` or `fs`), buckets,
backpressure thresholds (2 GiB, 900 s), zstd level (9), ephemeral days (30), GCS endpoint
override for fake-gcs-server.

## Tests

- `tests/crash.rs`: `spool_replay_after_crash`, `unacknowledged_append_is_never_published`,
  `roll_limits_and_backpressure`, `no_capture_visible_before_commit`.
- `tests/warc.rs`: property round trip, `warc_reader_never_panics`,
  `reader_rejects_lying_lengths`, resource and metadata records, `cdxj_roundtrip`,
  `revisit_on_same_payload`.
- `tests/blob.rs`, `tests/gcs.rs`: `upload_idempotent_same_bytes`, `upload_mismatch_errors`,
  file-store semantics, GCS against fake-gcs-server.
- `tests/purge.rs`, `tests/pg_purge.rs`: the rewrite removes a record and keeps others;
  rows are updated once.
- Fuzz target `fuzz/fuzz_targets/fuzz_warc_reader.rs`.

More: [Storage and Raw Data](Storage-and-Raw-Data).
