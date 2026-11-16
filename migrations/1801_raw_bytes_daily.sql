-- 1801 daily raw-bytes counter (owner: ops 18xx). Spec §5.2: an overall cap on decoded bytes
-- spooled per UTC day (fetch.daily_raw_bytes_cap, default 3 GB), shared by every fetcher
-- process. One row per day (DB time). It replaces the `raw_bytes:YYYY-MM-DD` rows the first
-- image kept in `rate_bucket`; their totals are carried over so today's count survives the
-- deploy. The old rows are left alone (the previous image may still write them until it is
-- rolled away; the scheduler prunes them like the new rows).
SET lock_timeout = '5s';

CREATE TABLE raw_bytes_daily (
    day        date PRIMARY KEY,
    bytes      bigint NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

INSERT INTO raw_bytes_daily (day, bytes)
SELECT to_date(substr(name, 11), 'YYYY-MM-DD'), greatest(tokens, 0)::bigint
FROM rate_bucket
WHERE name ~ '^raw_bytes:[0-9]{4}-[0-9]{2}-[0-9]{2}$'
ON CONFLICT (day) DO NOTHING;
