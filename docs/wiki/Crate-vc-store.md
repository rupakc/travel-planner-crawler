# Crate: vc-store

**Purpose.** The raw store: WARC/1.1 writer and reader, CDXJ, the fsynced spool, the segment
roller, blob stores for GCS and local disk, the segment committer (`vc_commit_segment`), the
text tier and the purge rewrite. It makes "raw first" true.

Path: `crates/vc-store`. Dependencies: `warc`, `zstd`, `google-cloud-storage`, `crc32c`,
`sha2`, `fail`, `reqwest` (GCS JSON API). Migration block `12xx` (`1201_store_rewrite.sql`);
the commit function is in `0005_queue.sql`; the purge garbage table `store_garbage` is
`1904_purge_garbage.sql` (vcrawl block). Status: wired; the fetcher runs the spool and roller,
maintenance and `admin purge` run the rewrite and garbage collection.

## Modules and key items

| Module | Key items | What it does |
|---|---|---|
| `spool` | `Spool`, `SpoolOptions`, `SealedSpool`, `SpoolEntry` | append a frame and **fsync** before returning; a torn frame is truncated on open; sealing fixes a segment's id and content. An append returns `Ok` once its frame is durable, even if the seal after it fails (the next `seal_due` retries); a sealed file is never appended to again |
| `segment` | `SegmentRoller`, `CommitProbe`, `build_segment`, `upload`, `read_segment`, `read_capture`, `read_cdxj` | roll at 128 MiB or 600 s; build deterministically; upload with `ifGenerationMatch=0`; `tick` replays sealed files on start. With a `CommitProbe`, a sealed file whose segment is already committed is deleted without re-uploading (a purge may have removed bytes since). A file that fails on its own (damaged, or a different object exists) is skipped so newer files still publish, and renamed `<file>.quarantined` after 3 tries (`QUARANTINE_AFTER`; counted by `quarantined_total()`) |
| `commit` | `PgSegmentCommitter` | calls `vc_commit_segment(jsonb)` once per segment; `commit_rewrite` for purges; implements `CommitProbe` |
| `warc` | `encode`, `read_records`, `WarcReader`, `parse_http`, `payload_hash`, `dedupe_payload`, `verify_payload_digest` | the only WARC serialiser; decoded bodies; revisit records; deterministic record ids |
| `cdxj` | `CdxjEntry`, `write_cdxj`, `parse_cdxj`, `surt` | one line per capture with the full manifest entry, so rows can be rebuilt from GCS |
| `text` | `encode_text_tier`, `decode_text_tier`, `put_text_tier` | `text/YYYY/MM/<id>.jsonl.zst` |
| `paths` | `segment_paths`, `text_path`, `is_ephemeral`, `rewrite_generation` | object routing, including `ephemeral/` and `<id>.r<N>` generations |
| `gcs`, `fs` | `GcsBlobStore`, `GcsOptions`, `FsBlobStore`, `blob_store_from_config` | `BlobStore` implementations. `GcsBlobStore::list` and `delete` use the GCS JSON API (`/storage/v1/b/{bucket}/o`, paged, a 404 on delete is fine), so they also work against fake-gcs-server |
| `purge` | `rewrite_segment_without`, `finish_rewrite`, `SegmentRewrite`, `record_garbage`, `collect_garbage` | rewrite a segment without some captures, crash-consistently. A rewrite steps over stale uncommitted generations (listed in `SegmentRewrite.abandoned`). The old generation and abandoned ones are recorded in `store_garbage` in the rewrite's transaction; `collect_garbage` deletes every listed object no live segment names, so a crash between commit and delete never leaves purged bytes behind |

The same rewrite transaction (`vc_rewrite_segment`) clears a URL's validators (`etag`,
`last_modified`, `payload_hash`, `last_capture_id`) when the capture they vouch for is removed
by expiry or purge. The next fetch is then unconditional and stores a body.

WARC headers added by the crawler: `WARC-Voyager-Original-Encoding`,
`WARC-Voyager-Capture-Id`, `WARC-Voyager-Redirect`.

## Failpoints

With the `failpoints` feature (on in this crate's tests): `spool::before_fsync`,
`spool::before_seal`, `spool::after_rename`, `segment::before_upload`, `segment::after_upload`,
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
- `tests/blob.rs`, `tests/gcs.rs` also: `gcs_list_delete` (prefixes, odd names, paging,
  idempotent delete), `gcs_list_missing_bucket_errors`.
- `tests/purge.rs`, `tests/pg_purge.rs`: the rewrite removes a record and keeps others;
  rows are updated once.
- `tests/robust.rs`: `append_is_ok_when_the_seal_after_a_durable_frame_fails`,
  `sealed_content_is_fixed_even_if_the_seal_fails_late`,
  `bad_sealed_file_is_quarantined_and_newer_ones_publish`,
  `replay_after_commit_does_not_reupload_purged_bytes`,
  `rewrite_uses_the_next_free_generation_after_a_stale_one`.
- Fuzz target `fuzz/fuzz_targets/fuzz_warc_reader.rs`, run nightly by `fuzz.yml`.

More: [Storage and Raw Data](Storage-and-Raw-Data).
