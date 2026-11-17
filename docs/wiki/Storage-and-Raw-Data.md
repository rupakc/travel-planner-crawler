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
| **Text tier** | One `jsonl.zst` file per segment with the main text and structured data of each capture. | Re-extraction and the quality auditor, without reading cold WARC files. |

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

The store talks to GCS with the official client for reads and uploads, and with the GCS
JSON API for list and delete, so the same code runs against the fake GCS server in tests.

Bucket `voyager-crawler-ops` holds database backups (wal-g, kept 14 days) and LLM batch
input/output (kept 7 days). The crawler can write there but not delete; the lifecycle does.

## The crash-safe write path

| Step | What happens | If the process dies here |
|---|---|---|
| 1. Spool | The WARC record is appended to a local spool file and **fsynced** before the call returns. The append succeeds once the frame is on disk, even if the seal that follows fails (it is retried). | A half-written frame is cut off on the next open and never published. |
| 2. Mark fetched | The URL moves to `fetched_pending`. Validators are not touched. If the segment was already committed (the roller can be faster than the worker), the URL goes straight back to `due`. | The spool replay finishes it; the sweeper (below) frees it if not. |
| 3. Seal | At 128 MiB or 600 s the spool file is sealed. Its segment id and content are now fixed. | Replayed on start. |
| 4. Upload | The segment is built deterministically and uploaded with `ifGenerationMatch=0` (create only if absent), then its CDXJ. | Retry: an identical object is success; a different one is a conflict. A replayed file whose segment is already committed is deleted without uploading, so purged bytes never come back. |
| 5. Commit | `vc_commit_segment(jsonb)`: one transaction inserts the `segments` row and the `captures` rows, updates `url_state` validators **only if this capture is newer** (`last_captured_at < captured_at`), and enqueues `capture_ready`. | Idempotent by segment id: a repeat is a no-op. |
| 6. Delete | The sealed spool file is removed. | — |

Why this order matters:

- Nothing derived from a capture is visible before step 5.
- The frontier keeps the **old** ETag and Last-Modified until step 5, so a crash can never
  make us send a conditional request and get a `304` for a body we never stored.
- The "newer only" guard is not tied to the epoch. A late, orphaned or replayed segment is
  safe: it can never overwrite newer validators.

## Recovery and repair

| Problem | What handles it |
|---|---|
| A fetcher dies and a new one starts with a new instance name (every deploy) | **Spool adoption.** Each fetcher holds an exclusive file lock on its own spool directory (`fetcher-<instance>`). Every roller tick it adopts sibling `fetcher-*` directories whose lock is free (dead owner), publishes their sealed and open files and removes them once empty. Their bytes count toward backpressure until published. Processor spools are not adopted. |
| A URL stuck in `fetched_pending` (its spool was lost) | **Sweeper.** Every 10 minutes the fetchers return rows pending for more than 6 hours to `due` (`url_state.pending_since`, migration 1102). A late commit still applies its validators and capture. |
| A sealed file that is damaged, unreadable, or whose object exists with other bytes | Skipped, so newer files still publish. After 3 tries it is renamed `<file>.quarantined` and counted for an alert. A store or database outage still stops the whole tick. |
| A seal that fails after its rename | The handle on the sealed file is dropped, so later appends never change a sealed segment. |

Tests run with **failpoints** at every step (`spool::before_fsync`, `spool::before_seal`,
`spool::after_rename`, `segment::before_upload`, `segment::after_upload`,
`segment::before_commit`, `segment::after_commit`, `purge::after_commit_rewrite`) and check there is no lost capture, no duplicate and no orphan 304.

## Backpressure

If the spool grows past **2 GB**, or uploads keep failing for **15 minutes**, the fetchers
pause and the `backpressure` metric alerts.

## Text tier

The processor writes one text-tier object per committed segment (migration 1903,
4 segments per loop):

- path `text/YYYY/MM/<segment-id>.jsonl.zst`, one JSON line per primary 2xx capture with a
  stored body, in capture order: `capture_id`, `captured_at`, `url`, `url_key`,
  `source_id`, `media_type`, `status`, `title`, `lang`, `main_text`, `structured`;
- ephemeral captures (`noarchive`, TDM-reserved) and every capture of an ephemeral segment
  go to `ephemeral/text/…`, so the 30-day deletion covers them;
- revisits, non-2xx, non-primary captures and unknown or `restricted` sources are skipped;
- each segment is claimed with a 10-minute lease in `text_tier_segments`, and objects are
  create-if-absent, so a rerun or a second processor changes nothing.

The quality auditor reads page text from here (see [Agentic Loops](Agentic-Loops)).

## Dumps

Large dumps (Wikivoyage, Stack Exchange Travel, OurAirports) are **streamed**, not stored
whole. The adapter filters the stream as it reads and writes:

- a `dump_manifest` `metadata` record naming the dump exactly (URL, size, sha256, date,
  filter version);
- the kept rows as `resource` records.

The full dump is a public, versioned artefact, so the manifest is enough to find it again.
Dumps run in a nightly window (02:00–05:00 UTC) under the daily raw-bytes cap (3 GiB a
day across all fetchers, counted in the `raw_bytes_daily` table, migration 1801).

Some adapters stop at a page cap per run. Two of them, `wikidata_sparql` and
`ticketmaster_discovery`, save a **resume cursor** in `adapter_cursors` (migration 1701),
so the next run (due again after 60 s, still under the key's politeness) continues where
the last stopped, even after a restart. A complete run deletes the cursor. The scheduler
prunes cursors of rows that left the crawl or were not resumed for 30 days.

## Purge

A purge (opt-out, erasure request, denylist addition) is started with `vcrawl admin purge`
and finished by the maintenance role's `purge` job (every 15 minutes). Each request in
`purge_requests` moves through two stages: **index** (records, held records, cached LLM
outputs) then **raw** (segments and text tier).

The raw stage rewrites the affected segments without the removed captures:

1. upload the new segment and CDXJ under a new generation path `<id>.r<N>`
   (idempotent). A generation left by a crashed rewrite is stepped over, not retried
   forever;
2. one transaction (`vc_rewrite_segment`) switches the `segments` row, fixes the kept
   captures' offsets, deletes the purged capture rows, and lists the old objects in the
   **`store_garbage`** table (migration 1904);
3. delete every garbage object that no live segment names. The purge job retries this, so a
   crash between commit and delete never leaves purged bytes behind.

The same steps also:

- remove the purged captures' lines from the `text/` and `ephemeral/text/` tier objects
  (an emptied object is deleted);
- **release validators**: when a removed capture backed a URL's ETag, Last-Modified or
  payload hash, they are cleared, so the next fetch is unconditional and stores a body.
  Ephemeral expiry does the same;
- take purged URLs **out of the crawl**: a URL, prefix, host or source purge sets the
  matching `url_state` rows to `tombstoned`. URLs under that host first found after the
  purge are still fetched; the index keeps their records out.

`admin rebuild-captures` skips garbage objects, purged segments and old generations, and
never re-inserts captures that a URL, prefix, host or source tombstone covers.

**Author erasure** (`admin purge --author`, for forum posts) is scoped:

- only records that name the person **as an author** are tombstoned, and in their stored
  versions the person's own posts are blanked and the name is cut as a whole word only
  ("Jordanson" or the country Jordan are untouched);
- the raw scope is fixed first (migration 1905, `purge_requests.scope`): every capture of
  those records' URLs, plus the dump chunks holding the person's posts;
- the name is stored only as `sha256:<hex>`, never in plain text.

A later dump can bring the posts back into raw storage; re-run the purge to cut them
again. Kept captures are copied byte for byte, not recompressed. Together with the 14-day
backup expiry and 7-day snapshots, data is gone everywhere within 21 days.

## Disaster recovery

- Postgres streams its WAL to GCS with **wal-g** and takes a base backup daily at 03:15 UTC.
- `admin rebuild-captures` can rebuild the `segments` and `captures` rows from the CDXJ
  files in GCS alone, for anything newer than the last backup.
- wal-g runs with `WALG_PREVENT_WAL_OVERWRITE=true`: a retried upload of a WAL segment
  that is already stored exits cleanly instead of wedging archiving.
- Weekly, the maintenance role rebuilds the newest 50 segments' rows from their CDXJ in
  memory and compares them with the database (`restore_drill_ok` metric).
- Weekly (Sundays 05:30 UTC), the host's light restore drill (`deploy/host/restore-drill.sh`) restores the
  latest base backup plus WAL into a throwaway container and runs sanity queries. The
  monthly full drill on a Spot VM is not wired yet. See
  [Operations and Deploy](Operations-and-Deploy).

Code: [vc-store](Crate-vc-store).
