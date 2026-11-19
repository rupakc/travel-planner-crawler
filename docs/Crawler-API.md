# Crawler read API (`/v1`)

The private read API of the Voyager crawler, for the Voyager backend
(`backend/app/services/crawler_client.py`, spec §13). Implemented by `crates/vc-api`.

**This contract is frozen. Changes are additive only:** new endpoints, new optional query
parameters, new response fields, new error codes. Nothing is removed or renamed, and no
field changes meaning, type or optionality without a new `/v2`. Clients must ignore fields
they do not know. The machine-readable contract is `GET /v1/openapi.json` (OpenAPI 3.1); the
snapshot test `crates/vc-api/tests/api.rs::openapi_snapshot` fails on any change to it, so
every contract change is reviewed in the diff of
`crates/vc-api/tests/snapshots/api__openapi.snap`.

## Network and TLS

- Served on the crawler VM's **static internal IP**, port **8443**, HTTPS only. The firewall
  admits TCP 8443 only from the app VM's service account (`vm-runtime`). Never exposed to the
  internet.
- The certificate is **self-signed** for the internal IP (two-year validity). The client
  **pins** it: it reads `crawler-api-tls-cert` from Secret Manager and trusts only that
  certificate (no public CA). During rotation (`bootstrap.sh --rotate-tls`) the client accepts
  the current or the next certificate.
- `/metrics` is not on this listener; it is plain HTTP on `127.0.0.1:9100` of the crawler VM.

## Authentication

Every `/v1` endpoint except `/v1/openapi.json` needs

```
Authorization: Bearer <client_id>.<secret>
```

- One token per client (row in `api_clients`; the database holds only the argon2id hash). The
  Voyager backend reads its token from Secret Manager (`CRAWLER_API_TOKEN`).
- A missing, malformed, unknown, wrong or disabled token answers `401` with
  `WWW-Authenticate: Bearer` and `{"error": "unauthorized"}`. Revoking a client takes effect
  within 60 s (verified tokens are cached that long).
- Each client has a set of **licence classes** it may receive (default: `open`,
  `share_alike`, `facts_only`, `api_terms`). Items of other classes are never returned: they
  are absent from lists and answer `404` when fetched by id.

## Rate limit

Per client, a token bucket of `rate_per_min` requests (default 600) refilled continuously.
Over the limit: `429`, `Retry-After: <seconds>` (1–60), `{"error": "rate_limited"}`. Clients
should back off for `Retry-After` and must not retry immediately.

## Errors

Every error body is

```json
{ "error": "<code>", "message": "<human-readable detail>" }
```

| Status | `error` | When |
|---|---|---|
| 400 | `bad_request` | invalid parameter, cursor or id |
| 401 | `unauthorized` | see Authentication |
| 404 | `not_found` | unknown, tombstoned, not visible to this client, or unknown route |
| 429 | `rate_limited` | see Rate limit |
| 503 | `unavailable` | not ready, or the request took longer than 10 s |
| 500 | `internal` | anything else |

Treat any 5xx, timeout or connection error as "crawler unavailable" and carry on without it
(spec §13: a missing crawler means behaviour exactly as today).

## Items

Lists and entity lookups return **items**. Every item carries `schema`, `licence`,
`provenance` and `conflict`:

```json
{
  "id": "01JCQ5Z7G8N3V2K4M6P8R0T2W4",
  "kind": "attraction",
  "schema": "attraction@1",
  "title": "Tower of London",
  "excerpt": "A historic castle on the north bank of the Thames…",
  "data": { "name": "Tower of London", "opening_hours": "…" },
  "place_ids": [2643743],
  "licence": {
    "class": "share_alike",
    "spdx": "CC-BY-SA-4.0",
    "attribution": "Wikivoyage contributors",
    "url": "https://creativecommons.org/licenses/by-sa/4.0/"
  },
  "provenance": [
    {
      "source_id": "wikivoyage",
      "url": "https://en.wikivoyage.org/wiki/London",
      "captured_at": "2026-11-01T00:00:00Z",
      "extractor": { "version": "0.1.0+1", "method": "structured",
                     "model": null, "prompt_hash": null, "run_id": null }
    }
  ],
  "conflict": false,
  "use_policy": { "index": "full", "serve": "full" },
  "updated_at": "2026-11-01T00:00:00Z",
  "score": 0.82
}
```

| Field | Meaning |
|---|---|
| `id` | Entity id (26-char ULID) or record id (decimal string) |
| `kind` / `schema` | Kind and `kind@version` of `data` (JSON schemas in `schemas/<kind>.v<N>.json`) |
| `title` | Title or headline |
| `excerpt` | At most **300 characters**, always |
| `data` | Extracted facts |
| `place_ids` | GeoNames ids |
| `licence.class` | `open`, `share_alike`, `facts_only` or `api_terms` (never `restricted`) |
| `licence.attribution` | Show it wherever the data is shown |
| `provenance[]` | One entry per contributing record; `url` is the link to show |
| `conflict` | Sources disagree on a high-stakes field (visa, advisory, emergency numbers): present with care, and say so |
| `use_policy` | What the API has already applied (below) |
| `score` | Ranking score, search only; otherwise `null` |

### What the API enforces before an item leaves

- **`facts_only`** (blogs, news): facts + headline + excerpt (≤ 300 characters) + link only.
  The title and every string in `data` are cut to 300 characters and full-text keys (`text`,
  `body`, `content`, `html`, `full_text`, `article_body`, `articleBody`) are removed.
- **`share_alike`**: never merged with other licences; keep `licence.attribution` with the
  data and pass the same licence on to anything derived from it.
- **`use_policy.index = title_and_link`** or **`serve = link_only`**: `data` is `{}` and
  `excerpt` is `null`; show the title and the provenance link only.
- **`serve = facts_only`**: `excerpt` is `null`.
- Tombstoned (opted-out, purged, robots-disallowed) data is never returned.

When passing items to an LLM (spec §17.8), wrap them as **untrusted reference data**, with
their licence and conflict flags.

## Endpoints

All endpoints are `GET`. Page sizes default to 20 and are capped at 100 (`max_page`);
`limit` above the cap is clamped, a non-numeric `limit` is `400`.

### `GET /v1/search`

Hybrid search (full text + vectors, × quality × freshness) over entities and records.

| Parameter | |
|---|---|
| `q` | Free text |
| `place` | Exact GeoNames id |
| `kind` | Comma-separated kinds (e.g. `attraction,event`) |
| `lat`, `lon`, `radius_km` | Radius search; all three or none (`radius_km` in (0, 20000]) |
| `updated_since` | RFC 3339; only items updated since |
| `limit` | Page size |
| `cursor` | `next_cursor` from the previous page |

Response: `{"items": [Item…], "next_cursor": "<opaque>" | null}`.

### `GET /v1/entities`

Same parameters and response as `/v1/search`; for listing entities by place and kind.

### `GET /v1/entities/{id}`

`{"item": Item, "claims": [{"attribute", "value", "record_id", "version", "confidence"}]}`;
`claims` is the per-attribute provenance of the entity.

- A merged entity answers **`301`** with `Location: /v1/entities/<new id>` and
  `{"redirect_to": "<new id>", "location": "/v1/entities/<new id>"}`. Follow it and update
  any stored id. Merged ids never `404`.
- Malformed id: `400`. Unknown, tombstoned or not visible: `404`.

### `GET /v1/records/{id}/versions`

Versions of a record, newest first: `{"items": [RecordVersion…], "next_cursor"}`, where a
record version is `record_id`, `version`, `schema`, `data`, `data_hash`, `captured_at`,
`extractor`, `licence_class`, `use_policy`, `valid_from`, `superseded_at` (`null` = current).
Parameters: `limit`, `cursor`. The same licence rules apply.

### `GET /v1/articles`

Articles (headline + ≤ 300-character excerpt + facts + link). Same parameters as
`/v1/search`, except that `kind` is ignored (always `article`).

### `GET /v1/observations`

Time series (FX rates, weather normals, advisory levels).

| Parameter | |
|---|---|
| `series` | **Required**, e.g. `fx:USD:EUR` |
| `from` | RFC 3339, inclusive; default `to` − 30 days |
| `to` | RFC 3339, exclusive; default now |
| `limit` | Max points |

Response: `{"items": [{"series_key", "observed_at", "value", "source_id", "licence"}]}`.

### `GET /v1/carbon/factors`

Published emission factors (kind `emission_factor`, added 2026-11-19) for the trip carbon
footprint, newest factor-set `year` first, then by id. Same item shape and licence shaping as
`/v1/search`. Every filter is an exact match on the record's field except `activity` (a
substring of the slug); an invalid value is `400` (never a wildcard).

| Parameter | |
|---|---|
| `category` | `flights`, `rail`, `bus`, `coach`, `ferry`, `car`, `taxi`, `motorbike`, `accommodation`, `food`, `activity`, `electricity`, `fuel`, `other` |
| `country` | ISO 3166-1 alpha-2 `country_code` (case-insensitive) |
| `region` | `region_code`: eGRID subregion (`CAMX`) or US state (`CA`) |
| `year` | factor-set year |
| `gas_basis` | `co2`, `co2e`, `co2e_with_rf`, `co2e_without_rf` |
| `activity` | the `activity` slug contains this snake_case text (`long_haul`) |
| `source` | source id (`od-desnz-ghg-factors`) |
| `limit`, `cursor` | page size; `next_cursor` of the previous page |

Response: `{"items": [Item], "next_cursor"}`.

### `GET /v1/carbon/grid`

Grid carbon intensity of one country (kind `grid_carbon_intensity`, added 2026-11-19): every
series `grid_ci:<country>:<period>:<basis>` and every source, newest first. A shortcut over
`/v1/observations` for clients that do not know which series exist.

| Parameter | |
|---|---|
| `country` | **Required**, ISO 3166-1 alpha-2 |
| `period` | `annual` or `monthly` (default both) |
| `basis` | `lifecycle` or `direct` (default both) |
| `from` | RFC 3339, inclusive; default 2000-01-01 |
| `to` | RFC 3339, exclusive; default now |
| `limit` | Max points |

Response: `{"items": [{"series_key", "observed_at", "value", "source_id", "licence"}]}`
(newest first; `value` holds `intensity_g_per_kwh`, `country_code`, `period`, `basis`, …).

### `GET /v1/carbon/occupancy`

Published average vehicle occupancies (kind `occupancy`, persons per vehicle, added 2026-11-19)
for the trip carbon footprint: they turn a per-vehicle factor into a per-passenger one. Newest
`year` first, then by id; same item shape and licence shaping as `/v1/search` and
`/v1/carbon/factors`. Every filter is an exact match on the record's field; an invalid value is
`400` (never a wildcard).

| Parameter | |
|---|---|
| `mode` | `car`, `taxi`, `bus`, `coach`, `bus_coach`, `rail`, `light_rail`, `ferry`, `motorcycle`, `van`, `all_vehicles`, `other` |
| `country` | ISO 3166-1 alpha-2 `country_code` (case-insensitive) |
| `year` | the year the figure is for |
| `trip_purpose` | `all`, `work`, `commuting`, `leisure`, `other` |
| `basis` | `per_vehicle_km` (distance-weighted) or `per_trip` |
| `source` | source id (`od-dft-tag-databook`) |
| `limit`, `cursor` | page size; `next_cursor` of the previous page |

Response: `{"items": [Item], "next_cursor"}`. An item's `data` holds `mode`, `mode_label`,
`trip_purpose`, `time_period`, `geography`, `country_code`, `value`, `unit`
(`persons per vehicle`), `basis`, `includes_driver`, `year`, `publisher`, `dataset` and
`as_of`; the source and licence are in `provenance` and `licence`.

### `GET /v1/places/resolve`

Resolves a place name to GeoNames candidates, best first.

| Parameter | |
|---|---|
| `name` | **Required**, 1–200 characters |
| `country` | ISO 3166-1 alpha-2 hint |
| `lat`, `lon` | A nearby point (both or neither) |
| `lang` | Language of `name` |

Response: `{"items": [{"geoname_id", "name", "country", "admin1", "lat", "lon", "score"}]}`
(empty when unresolved).

### `GET /v1/changes`

The change feed, for clients that keep a local copy.

| Parameter | |
|---|---|
| `since` | `next` from the previous page; absent = from the beginning |
| `kinds` | Comma-separated kinds; absent = all |
| `limit` | Page size |

Response:

```json
{
  "changes": [
    { "cursor": "7340032-981", "owner_type": "record", "owner_id": "981",
      "op": "upsert", "kind": "event", "at": "2026-11-15T10:00:00Z" }
  ],
  "next": "7340032-981"
}
```

- `owner_type`: `record`, `entity`, `observation`, `source`. `op`: `upsert` (re-fetch it),
  `tombstone` (delete any copy within 24 h: the opt-out SLA), `redirect` (entity merged:
  re-fetch; the old id answers 301).
- **No skips:** the cursor is `(xid, seq)` and only changes of transactions older than every
  still-open transaction are served, so a client that always passes `since = next` sees every
  committed change exactly once, in order, even when transactions commit out of order.
- `next` is the last change returned, or the `since` you sent when the page is empty. Store
  it durably and poll again later; an empty page means "caught up".
- Cursors are opaque strings: do not parse or construct them. A malformed `since` is `400`.

### `GET /v1/sources`

`{"items": [{"source_id", "name", "category", "source_class", "licence", "method",
"enabled"}]}`, by id. `restricted` sources are not listed.

### `GET /v1/stats`

`{"sources_enabled", "records_active", "entities_active", "changes_seq_max",
"generated_at"}`; cached up to 60 s.

### `GET /v1/openapi.json`

The OpenAPI 3.1 document. No authentication.

### `GET /healthz`, `GET /readyz`

Probes, no authentication. `/healthz`: `{"status": "ok"}` while the process runs. `/readyz`:
`{"status": "ready"}` when the database answers, else `503`.

## Compatibility checklist (for changes to vc-api)

- Additive only: a new field is optional for clients (they ignore unknown fields); a new
  parameter is optional with a default that preserves today's behaviour.
- Update `crates/vc-api/tests/snapshots/api__openapi.snap` and this page in the same PR.
- A breaking change needs `/v2` beside `/v1`, a Voyager release that uses it, and only then
  the removal of the `/v1` piece.
