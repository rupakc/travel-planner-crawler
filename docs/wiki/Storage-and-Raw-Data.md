# Storage and Raw Data

**Raw first** means: every response is stored durably in GCS **before** anything built from
it becomes visible. Raw data is never edited (except by a purge), so any record can be
traced back to the exact bytes it came from, and everything can be rebuilt.

## Formats

| Format | What it is | Used for |
|---|---|---|
| **WARC/1.1** | The standard web-archive format (ISO 28500). One file holds many records: `request`, `response`, `revisit`, `resource`, `metadata`. | All raw captures. |
| **zstd** | A fast compression format. Each WARC record is compressed as its own frame. | Segment files (`.warc.zst`). A reader can jump to one record. |
| **CDXJ** | A text index: one line per capture, `<SURT url> <timestamp> <json>`. | Finding a record in a segment; rebuilding the database from GCS alone. |
| **Text tier** | One `jsonl.zst` file per segment with the main text and structured data of each capture. | Re-extraction, without reading cold WARC files. |

How a WARC record is written (`vc-store::warc`, the only serialiser):

- Bodies are stored **decoded**. `Content-Encoding` is dropped, `Content-Length` is rewritten,
  and the original encoding is kept in `WARC-Voyager-Original-Encoding`.
  `WARC-Payload-Digest` is `sha256:<hex>` of the decoded body.
- A response becomes a `request` + `response` pair. A 304 becomes a `revisit` with profile
  `server-not-modified`. A body identical to the URL's previous one becomes a `revisit` with
  profile `identical-payload-digest`, pointing to the earlier record.
- A rendered page's final DOM is a `resource` record. A dump manifest is a `metadata` record.
- Record ids come from the capture id, so writing the same capture twice gives the same
  bytes. This keeps uploads idempotent.
- The reader treats stored bytes as untrusted: every length is checked, bad input is an
  error, never a crash. It is fuzzed.

## Where things live in GCS

Bucket `voyager-crawler-raw`:

| What | Normal path | Ephemeral path (`noarchive` or TDM-reserved) |
|---|---|---|
| Segment | `segments/YYYY/MM/DD/<id>.warc.zst` | `ephemeral/YYYY/MM/DD/<id>.warc.zst` |
| CDXJ | `cdxj/YYYY/MM/DD/<id>.cdxj` | `ephemeral/cdxj/YYYY/MM/DD/<id>.cdxj` |
| Text tier | `text/YYYY/MM/<id>.jsonl.zst` | `ephemeral/text/YYYY/MM/<id>.jsonl.zst` |

`<id>` is the segment's ULID (a sortable unique id). A purge rewrite adds a generation:
`<id>.r<N>.warc.zst`.

Bucket lifecycle:

| Rule | Effect |
|---|---|
| `segments/` older than 90 days | moved to Coldline (cheaper, slower) |
| `cdxj/`, `text/` | stay in Standard |
| `ephemeral/` | deleted after 30 days |
| non-current object versions | deleted after 7 days |
| soft delete | 7 days |
| retention lock | **none**, so purges are possible |

Bucket `voyager-crawler-ops` holds database backups (wal-g, kept 14 days) and LLM batch
input/output (kept 7 days). The crawler can write there but not delete; the lifecycle does.

## The crash-safe write path

| Step | What happens | If the process dies here |
|---|---|---|
| 1. Spool | The WARC record is appended to a local spool file and **fsynced** before the call returns. | A half-written frame is cut off on the next open and never published. |
| 2. Mark fetched | The URL moves to `fetched_pending`. Validators are not touched. | The spool replay finishes it. |
| 3. Seal | At 128 MiB or 600 s the spool file is sealed. Its segment id and content are now fixed. | Replayed on start. |
| 4. Upload | The segment is built deterministically and uploaded with `ifGenerationMatch=0`, then its CDXJ. | Retry: an identical object is success; a different one is a conflict. |
| 5. Commit | `vc_commit_segment(jsonb)`: one transaction inserts the `segments` row and the `captures` rows, updates `url_state` validators **only if this capture is newer** (`last_captured_at < captured_at`), and enqueues `capture_ready`. | Idempotent by segment id: a repeat is a no-op. |
| 6. Delete | The sealed spool file is removed. | — |

Why this order matters:

- Nothing derived from a capture is visible before step 5.
- The frontier keeps the **old** ETag and Last-Modified until step 5, so a crash can never
  make us send a conditional request and get a `304` for a body we never stored.
- The "newer only" guard is not tied to the epoch. A late, orphaned or replayed segment is
  safe: it can never overwrite newer validators.

Tests run with **failpoints** at every step (`spool::before_fsync`, `spool::before_seal`,
`segment::before_upload`, `segment::after_upload`, `segment::before_commit`,
`segment::after_commit`) and check there is no lost capture, no duplicate and no orphan 304.

## Backpressure

If the spool grows past **2 GB**, or uploads keep failing for **15 minutes**, the fetchers
pause and the `backpressure` metric alerts.

## Dumps

Large dumps (Wikivoyage, Stack Exchange Travel, OurAirports) are **streamed**, not stored
whole. The adapter filters the stream as it reads and writes:

- a `dump_manifest` `metadata` record naming the dump exactly (URL, size, sha256, date,
  filter version);
- the kept rows as `resource` records.

The full dump is a public, versioned artefact, so the manifest is enough to find it again.
Dumps run in a nightly window (02:00–05:00 UTC) under the daily raw-bytes cap.

## Purge

A purge (opt-out, erasure request, denylist addition) rewrites the affected segments
without the removed captures:

1. upload the new segment and CDXJ under a new generation path (idempotent);
2. one transaction switches the `segments` row, fixes the kept captures' offsets and
   deletes the purged capture rows;
3. delete the old objects. GCS drops the non-current versions after the soft-delete window.

Kept records are copied byte for byte, not recompressed. Together with the 14-day backup
expiry and 7-day snapshots, data is gone everywhere within 21 days.

## Disaster recovery

- Postgres streams its WAL to GCS with **wal-g** and takes a base backup daily at 03:15 UTC.
- `admin rebuild-captures` can rebuild the `segments` and `captures` rows from the CDXJ
  files in GCS alone, for anything newer than the last backup.
- A weekly light restore drill and a monthly full drill on a throwaway Spot VM check that
  backups really restore. See [Operations and Deploy](Operations-and-Deploy).

> **Status:** the store library, failpoints, purge rewrite and CDXJ are built and tested.
> The `admin rebuild-captures` and `admin purge` commands and the drill runner are part of
> the `vcrawl` wiring that is in progress.

Code: [vc-store](Crate-vc-store).
