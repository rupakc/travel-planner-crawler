# Crate: vc-frontier

**Purpose.** `PgFrontier`: the Postgres frontier and the heart of politeness. It hands out
claims, fences leases, gives out global rate tokens, enqueues URLs, parks keys and estimates
recrawl times. It is the **only** crate (besides testkit) allowed to mint a `ClaimPermit`.

Path: `crates/vc-frontier`. Dependency: `sqlx`. Migration block `11xx` (tables are in
`0003_frontier.sql`).

## Key types and functions

| Item | What it does |
|---|---|
| `PgFrontier` | implements `Frontier` and `LeaseCheck` |
| `claim(shard, max_urls)` | one ready key with `FOR UPDATE SKIP LOCKED` (not parked, under budget, `next_allowed_at ≤ now()`, no live claim); a new epoch from `epoch_seq`; up to 5 due URLs leased by priority, all in one transaction |
| `check(permit)` | lease still current: epoch matches and database `now()` < deadline; any doubt is `LeaseLost` |
| `take_rate_token(permit)` | lease check + one token from the `rate_bucket 'global'` row in one statement; waits for refill |
| `extend(permit, by)` | epoch-fenced deadline extension (long dumps call this between chunks) |
| `mark_fetched(permit, url, next_fetch_at)` | `leased` to `fetched_pending` after the spool fsync |
| `complete(permit, outcome)` | applies URL results, updates EWMA, delay, back-off or park; releases the lease; schedules the key |
| `enqueue(batch)` | batched `INSERT … ON CONFLICT DO NOTHING`, sorted to avoid deadlocks; creates missing keys and origins |
| `park(key, reason)` | the park ladder |
| `robots(origin)`, `store_robots(permit, entry)` | the robots.txt cache; storing is lease-checked |
| `schedule::delay_ms`, `ewma`, `park_for`, `next_key_state` | pure scheduling math |
| `estimator::change_rate`, `next_interval` | the Cho and Garcia-Molina change-rate estimator |

## Rules

- **Serialisation point:** the key row. Each claim sets `next_allowed_at = deadline + delay`,
  so a holder that dies still leaves `max(deadline, last request end + delay)` before the
  next claim.
- **Database time only.** No local clocks for leases.
- **Lock order** (deadlock-free): `politeness_keys` row, then `claims` row, then `url_state`
  rows.
- **Delay:** `max(Crawl-delay, min delay, 10 × EWMA, penalty) ≤ 1 h`.
- **Parks:** 24 h, doubling, up to 30 days; 5 consecutive server errors park 1 hour; 429/503
  back off by `Retry-After` or doubling up to 24 h.
- `leased` URLs with a dead epoch return to `due` at the next claim; `fetched_pending` URLs
  are never re-leased.

## Config

`[frontier]`: shards, URLs per claim, per-URL budget, global rate and burst, delays, EWMA,
daily budget, park ladder, server-error threshold, enqueue batch, holder. See
[Configuration](Configuration).

## Tests

`tests/pg.rs` on a real Postgres: `two_claimers_never_share_a_key`,
`racing_claimers_take_each_key_once`, `stale_epoch_completion_rejected`,
`lease_expiry_returns_leased_urls_to_due`, `fetched_pending_never_released`,
`rate_token_global_across_pools`, `park_ladder_doubles_to_30d`, `delay_respects_crawl_delay`,
`budget_exhaustion_blocks_key`, `enqueue_dedupes`, `concurrent_enqueue_inserts_once`,
`extend_moves_deadline_and_holds_off_next_claim`, `rate_limited_backs_off`,
`server_errors_park_one_hour_after_threshold`, `park_signal_and_park_revoke_live_lease`,
`robots_roundtrip_is_lease_checked`, `estimate_next_fetch_uses_change_history`,
`database_failure_fails_closed`.

## Limits

One Postgres. The `shard` column allows several fetcher VMs later, each claiming its own
shards.
