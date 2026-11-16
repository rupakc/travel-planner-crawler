# Read API

The crawler serves its data to the Voyager backend through a private, read-only HTTP API
under `/v1`. The full, frozen contract is
[`crates/vc-api/docs/Crawler-API.md`](https://github.com/rupakc/travel-planner-crawler/blob/main/crates/vc-api/docs/Crawler-API.md);
this page is a short guide.

## Who can reach it

- Only the Voyager app VM. The API listens on the crawler VM's **static internal IP**, port
  **8443**, HTTPS only. The firewall admits TCP 8443 only from the app VM's service account
  (`vm-runtime`). It is never exposed to the internet.
- TLS uses a **self-signed certificate** for the internal IP (2-year validity). The client
  pins it by reading `crawler-api-tls-cert` from Secret Manager. During rotation it accepts
  the current or the next certificate.
- `/metrics` is on a separate plain-HTTP listener, `127.0.0.1:9100`, never on the API port.

## Authentication and limits

```
Authorization: Bearer <client_id>.<secret>
```

- One token per client, stored as an argon2id hash in `api_clients`. Revoking a client takes
  effect within 60 s.
- Each client has a set of licence classes it may receive (default: `open`, `share_alike`,
  `facts_only`, `api_terms`). Other items are invisible to it (absent from lists, `404` by id).
- Per-client rate limit: 600 requests per minute (token bucket). Over it: `429` with
  `Retry-After`.
- Request timeout 10 s (`503 unavailable` after that).

## Endpoints

All are `GET`. Lists default to 20 items, capped at 100.

| Endpoint | What it returns |
|---|---|
| `/v1/search?q=&place=&kind=&lat=&lon=&radius_km=&updated_since=&limit=&cursor=` | hybrid search over entities and records |
| `/v1/entities` | the same, for listing by place and kind |
| `/v1/entities/{id}` | one entity with per-attribute claims; a merged id answers `301` to the new id |
| `/v1/records/{id}/versions` | version history of a record, newest first |
| `/v1/articles` | articles: headline, ≤ 300-character excerpt, facts, link |
| `/v1/observations?series=&from=&to=` | a time series, such as `fx:USD:EUR` |
| `/v1/places/resolve?name=&country=&lat=&lon=&lang=` | GeoNames candidates for a place name |
| `/v1/changes?since=&kinds=&limit=` | the change feed (`upsert`, `tombstone`, `redirect`) |
| `/v1/sources` | the enabled, non-restricted sources |
| `/v1/stats` | counts, cached up to 60 s |
| `/v1/openapi.json` | the OpenAPI 3.1 document (no auth) |
| `/healthz`, `/readyz` | probes (no auth); `/readyz` is `503` when the database does not answer |

## Every item carries its context

| Field | Meaning |
|---|---|
| `id`, `kind`, `schema` | identity and `kind@version` of `data` |
| `title`, `excerpt` | excerpt is **at most 300 characters**, always |
| `data` | the extracted facts |
| `place_ids` | GeoNames ids |
| `licence` | `class`, `spdx`, `attribution`, `url`; show the attribution wherever you show the data |
| `provenance[]` | source id, URL to link to, capture time, extractor (method, model, prompt hash, run) |
| `conflict` | sources disagree on a high-stakes field; present with care and say so |
| `use_policy` | what the API already removed because of opt-out signals |
| `score` | ranking score in search results |

What the API enforces before an item leaves:

- `facts_only`: title and every string in `data` cut to 300 characters; full-text keys
  (`text`, `body`, `content`, `html`, `full_text`, `article_body`, `articleBody`) removed.
- `use_policy.index = title_and_link` or `serve = link_only`: `data` is `{}` and `excerpt`
  is null.
- `serve = facts_only`: `excerpt` is null.
- Tombstoned data (opt-out, purge, robots) is never returned.

When Voyager passes items to its own LLM, it wraps them as **untrusted reference data**, with
the licence and conflict flags.

## How search ranks

1. Filter: exact `place` id, or a radius; kinds; licence classes allowed for the client;
   `updated_since`.
2. Rank two ways: Postgres full-text rank (`ts_rank`) and vector similarity (pgvector HNSW,
   when a query embedder is configured; otherwise text only).
3. Combine with **reciprocal rank fusion** (constant 60), then multiply by quality and by
   freshness (half-life 90 days).

## The change feed, without gaps

A client that keeps a local copy polls `/v1/changes?since=<next>`. The cursor is
`(xid, seq)`, and only changes from transactions older than every still-open transaction
are served. So a client that always passes `since = next` sees every committed change exactly
once, in order. A `tombstone` means "delete your copy within 24 hours" (the opt-out SLA).

## Errors

`{"error": "<code>", "message": "…"}` with `400 bad_request`, `401 unauthorized`,
`404 not_found`, `429 rate_limited`, `503 unavailable`, `500 internal`. Voyager treats any
5xx, timeout or connection error as "crawler unavailable" and carries on exactly as before.

## Compatibility rule

The contract is **additive only**: new endpoints, optional parameters, new fields and error
codes. Nothing is removed or renamed under `/v1`. Clients must ignore unknown fields. An
insta snapshot of `/v1/openapi.json` fails the build on any change, so every contract change
shows up in review.

## Voyager side

> **Status:** not started. Voyager's backend will get `CRAWLER_API_URL`, `CRAWLER_API_TOKEN`
> and `CRAWLER_GROUNDING_ENABLED` (default false), and a `crawler_client.py` with a 2 s
> timeout and a circuit breaker. A missing crawler means behaviour exactly as today. It
> ships in the main repo with its own design note.

Code: [vc-api](Crate-vc-api), [vc-index](Crate-vc-index).
