-- 1906 page-level noarchive expiry (owner: vcrawl 19xx). Spec §5.2 UsePolicy, §5.5, §17.7.
-- A page's meta robots `noarchive` is only known after parsing: the processor records it in
-- `captures.use_policy_final` (`raw: ephemeral`) for a capture that was spooled as an archive
-- capture. The maintenance role gives such captures `raw_expires_at` and its hourly expiry
-- job cuts them out of their segments. This partial index holds only the captures still
-- waiting for that routing, so the hourly lookup never scans the table. Expand-only.
SET lock_timeout = '5s';

CREATE INDEX captures_noarchive_unrouted ON captures (captured_at)
    WHERE raw_expires_at IS NULL AND (use_policy_final->>'raw') = 'ephemeral';
