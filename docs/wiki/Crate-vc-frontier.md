# Crate: vc-frontier

**Purpose.** `PgFrontier`: the Postgres frontier and the heart of politeness. It hands out
claims, fences leases, gives out global rate tokens, enqueues URLs, parks keys and estimates
recrawl times. It is the **only** crate (besides testkit) allowed to mint a `ClaimPermit`.

Path: `crates/vc-frontier`. Dependency: `sqlx`. Migration block `11xx` (tables are in
`0003_frontier.sql`; `1101` adds `reschedule`, `1102` adds `url_state.pending_since`).
Status: wired; the `scheduler` and `fetcher` roles of [vcrawl](Crate-vcrawl) use it.

## Key types and functions

| Item | What it does |
|---|---|
| `PgFrontier` | implements `Frontier` and `LeaseCheck` |
| `claim(shard, max_urls)` | one ready key with `FOR UPDATE SKIP LOCKED` (not parked, under budget, `next_allowed_at ≤ now()`, no live claim); a new epoch from `epoch_seq`; up to 5 due URLs leased by priority, all in one transaction |
| `check(permit)` | lease still current: epoch matches and database `now()` < deadline; any doubt is `LeaseLost` |
| `take_rate_token(permit)` | lease check + one token from the `rate_bucket 'global'` row in one statement; waits for refill |
| `extend(permit, by)` | epoch-fenced deadline extension (long dumps call this between chunks) |
| `mark_fetched(permit, url, next_fetch_at)` | `leased` to `fetched_pending` after the spool fsync and stamps `pending_since`; if the segment holding the capture is already committed, the URL goes straight to `due` (closes a commit-before-mark race) |
| `sweep_stale_pending(older_than)` | returns `fetched_pending` rows older than `older_than` to `due`: their spool frame was lost, so no commit will release them. The fetcher runs it with 6 h. A late commit still records validators and the capture |
| `borrow_subresource(key)`, `subresource_done(key)` | a render sub-request to another politeness key (an allowlisted CDN) borrows that key through Postgres: granted only if the key is ready, not parked and has no live claim; its `next_allowed_at` moves on by its delay, and again from the end of the request. Holds across processes |
| `complete(permit, outcome)` | applies URL results, updates EWMA, delay, back-off or park; releases the lease; schedules the key |
| `enqueue(batch)` | batched `INSERT … ON CONFLICT DO NOTHING`, sorted to avoid deadlocks; creates missing keys and origins |
| `park(key, reason)` | the park ladder |
| `reschedule(url, at)` | forces a refetch of one URL no later than `at` (for example after an upstream change signal); only the URL's own schedule moves, so the key's delay, parks, budget and rate tokens still apply (trigger in migration `1101`) |
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
- **Parks:** 24 h, doubling, up to 30 days; 5 consecutive server errors park 1 hour.
- **429/503 back-off:** penalty = `max(Retry-After, 2 × previous penalty, 2 × delay)`, capped
  at 24 h. A short `Retry-After` never cancels a back-off already earned.
- **Crawl-delay mid-claim:** `ClaimPermit::raise_delay` (in vc-core) lets the fetcher raise the
  permit's delay as soon as it reads a `Crawl-delay` from robots.txt fetched under that
  permit. The delay only ever goes up, so later URLs, redirects and sub-requests of the same
  claim are paced by it.
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
`database_failure_fails_closed`, and the `reschedule_*` tests.
`tests/pending_recovery.rs`: `mark_fetched_after_commit_goes_straight_to_due`,
`mark_fetched_without_commit_stays_pending`, `sweeper_returns_stale_pending_rows_to_due`.
`tests/subresource_borrow.rs`: `borrow_honours_and_advances_the_third_party_key`. Unit test
`repeated_429_short_retry_after_keeps_doubling` in `schedule.rs`.

## Limits

One Postgres. The `shard` column allows several fetcher VMs later, each claiming its own
shards.
