# Crate: vc-core

**Purpose.** Shared ids, value types and **every trait between crates**. Each library crate
depends only on `vc-core` and `vc-config`, and implements one or more of these traits. Only
the `vcrawl` binary depends on the implementations. `vc-core` is **frozen**: changes go
through the controller as additive, batched PRs.

Path: `crates/vc-core`. Main dependencies: `url`, `psl`, `blake3`, `ulid`, `chrono`, `serde`.

## Modules

| Module | Holds | Implemented by |
|---|---|---|
| `ids`, `url`, `media` | identities, canonical URLs, media types | here |
| `usepolicy`, `licence` | opt-out signals to allowed uses; licence and source classes | here (the matrix), [vc-policy](Crate-vc-policy) |
| `policy` | `Policy` trait: robots, denylist, SSRF, challenge detection | [vc-policy](Crate-vc-policy) |
| `frontier` | `Frontier`, `LeaseCheck`, `ClaimPermit`, `Claim`, outcomes | [vc-frontier](Crate-vc-frontier) |
| `fetch` | `PermitClient`, `FetchRequest`, `Exchange`, `FetchError` | [vc-fetch](Crate-vc-fetch) |
| `render` | `Renderer`, `RenderBudget`, `BlockReason`, `RenderOutcome` | [vc-render](Crate-vc-render) |
| `store` | `BlobStore`, `CaptureSink`, `SegmentCommitter`, `SpoolRecord`, `SegmentManifest` | [vc-store](Crate-vc-store) |
| `parse` | `ContentHandler`, `HandlerRegistry`, `Parsed`, `Link`, `Limits` | [vc-parse](Crate-vc-parse) |
| `kinds` | `KindRegistry`, `KindDef`, `KindStorage` | [vc-kinds](Crate-vc-kinds) |
| `llm` | `LlmClient`, `Embedder`, `BudgetLedger`, `BatchRunner`, `Purpose`, `Usd` | [vc-llm](Crate-vc-llm) |
| `extract`, `records` | `PlaceResolver`; `ExtractedRecord`, `Provenance` | [vc-extract](Crate-vc-extract) |
| `sources` | `SourceSpec`, `SourceAdapter`, `Cadence`, `Method`, `RenderMode` | [vc-sources](Crate-vc-sources) |
| `queue` | `WorkQueue`, `WorkKind`, `WorkItem` | [vc-queue](Crate-vc-queue) |
| `index` | `RecordSink`, `IndexReader`, views, change cursor | [vc-index](Crate-vc-index) |
| `secrets` | `SecretSource`, `Secret` | [vc-secrets](Crate-vc-secrets) |
| `clock`, `error` | `Clock`, `SystemClock`; `Error`, `ErrorClass`, `LeaseLost` | here |

## Key constants

| Constant | Value |
|---|---|
| `ROBOTS_TOKEN` | `VoyagerBot` |
| `USER_AGENT` | `VoyagerBot/<crate version> (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)` |
| `MAX_URLS_PER_CLAIM` | 5 |
| `records::MAX_EXCERPT_CHARS` | 300 |
| `queue::MAX_ATTEMPTS` | 8 |
| `store::PREFIX_*` | `segments/`, `ephemeral/`, `text/`, `cdxj/` |

## Key types

- **`UrlKey`**: 16 bytes, blake3 of the canonical URL. `PayloadHash`: 32 bytes (sha256).
- **ULID ids**: `CaptureId`, `SegmentId`, `ExtractionRunId`, `BatchJobId` and others.
- **String ids**: `OriginKey`, `PolitenessKey`, `SourceId`, `RecordKey`, `KindId`, `ModelId`.
- **`Epoch`**, `WorkId`, `RecordId`, `DbTime` (a time read from the database, the only clock
  for leases).
- **`CanonicalUrl`**: the only accepted URL type for crawling (rules below).
- **`ClaimPermit`**: proof of a live claim. Its constructor `mint` and `extend_to` are
  forbidden outside `vc-frontier` and testkit by `clippy.toml`.
- **`UsePolicy`**: `{raw, index, llm, embed, serve, train}` with a `meet` operation (the
  minimum of each dimension), `ALLOW_ALL` and `DENY_ALL`. Signals can only restrict.
- **`LicenceClass`** (`open`, `share_alike`, `facts_only`, `api_terms`, `restricted`) and
  **`SourceClass`** (`official`, `reference`, `editorial`, `community`, `commercial_api`).
- **`MediaType`**: `html`, `rss`, `atom`, `sitemap`, `sitemap_text`, `json`, `csv`, `xml`,
  `text`, `pdf` (no handler in v1), `other`.
- **`CaptureRole`**: `primary` (only these update `url_state`), `subresource`, `robots`,
  `manifest`.
- **`WorkKind`**: `capture_ready`, `extract`, `embed`, `triage`, `audit`, `purge`.
- **`ErrorClass`**: lets callers tell transient from permanent errors.

## URL canonicalisation

Idempotent (`canon(canon(u)) == canon(u)`, a property test) and conservative:

1. only `http`/`https` with a host; scheme and host lowercased; IDNs to punycode; IPv4
   forms normalised; trailing dot and default port removed;
2. user info and fragment removed;
3. path: dot segments resolved, empty path becomes `/`, unreserved escapes decoded, other
   escapes uppercased;
4. query: empty pairs and tracking parameters (`utm_*` and a fixed list such as `gclid`,
   `fbclid`) removed, escapes normalised, pairs sorted by name; empty query removed.

## Tests

- `tests/contracts.rs`: `canonical_url_is_idempotent` (property test),
  `use_policy_meet_only_restricts`, `url_key_is_16_bytes_and_stable`, source-spec field and
  enum spellings.
- `tests/source_yaml.rs`: every `sources/*.yaml` and the denylist parse into the core types.
- A unit test pins the User-Agent shape (starts with `VoyagerBot/`, ends with the BOT.md link).

## Limits

Frozen after P0. No hard-coded entity kinds (they come from data; see [vc-kinds](Crate-vc-kinds)).
