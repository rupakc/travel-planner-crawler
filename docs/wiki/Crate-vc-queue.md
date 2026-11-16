# Crate: vc-queue

**Purpose.** `PgWorkQueue`: the Postgres work queue between roles (`work_items` table,
migration `0005_queue.sql`). Implements `vc_core::queue::WorkQueue`. Migration block `13xx`.

Path: `crates/vc-queue`. Dependency: `sqlx`.

## Work kinds

`capture_ready` (a committed capture to process, enqueued by `vc_commit_segment`),
`extract`, `embed`, `triage`, `audit`, `purge`. Payloads hold **ids only**, never content.

## Behaviour

| Feature | How |
|---|---|
| At-least-once | `take` leases ready items with `FOR UPDATE SKIP LOCKED` (concurrent takers never share an item) and counts the attempt. An item whose lease expires without an ack is delivered again. |
| Back-off | `nack` makes an item ready again after `max(retry_in, backoff(attempts))`; the back-off doubles from 30 s up to 6 h. A caller's `retry_in` (such as `Retry-After`) is honoured. |
| Dead letters | after 8 attempts (`MAX_ATTEMPTS`), or when the lease expires after the last attempt (a poison item that crashes its worker never gets to nack), the item becomes `dead`. |
| Dedupe | enqueue goes through the SQL function `vc_enqueue`, which other transactions use too; a `dedupe_key` is unique among pending (ready or leased) items of a kind, and free again once the item is done or dead. |

Acks and nacks are **not fenced** (the trait carries only the id). A worker that outlived its
lease may ack an item that another worker re-leased. Handlers are idempotent, so this is
harmless.

## Key items

`PgWorkQueue::new`, `backoff`, `nack_with_error`, `dead_letters`, `requeue_dead`, `stats`
(`QueueStat`), `prune_done` (deletes done items after 7 days), `parse_kind`.

## Config

`[queue]`: lease (300 s), max attempts (8), back-off base and max, done retention.

## Tests

`tests/queue.rs`: `queue_at_least_once_redelivery`, `dead_letter_after_8`,
`dead_letter_after_8_expired_leases`, `dedupe_key`, `take_respects_kind_limit_and_backoff`,
`concurrent_takers_never_share_an_item`, `sql_enqueue_from_vc_commit_segment_is_taken`,
`prune_and_stats_and_requeue_dead`, `backoff_is_exponential_and_capped`.

## Limits

Postgres only today; Pub/Sub can replace it behind the same trait at larger scale.
