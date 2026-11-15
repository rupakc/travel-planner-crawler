-- 0011 operations: purge, tombstones, audits, digest, heartbeats, partitions (owner: ops 18xx).
SET lock_timeout = '5s';

CREATE TABLE purge_requests (
    id            bigserial PRIMARY KEY,
    selector      jsonb NOT NULL,                  -- vc_core::index::PurgeSelector
    reason        text NOT NULL,
    requested_by  text NOT NULL,
    state         text NOT NULL DEFAULT 'pending'
                    CHECK (state IN ('pending', 'index_done', 'raw_done', 'done')),
    requested_at  timestamptz NOT NULL DEFAULT now(),
    index_done_at timestamptz,                     -- SLA: 24 h
    raw_done_at   timestamptz,                     -- SLA: 7 days
    done_at       timestamptz                      -- SLA: 21 days (backups, snapshots)
);

-- Standing exclusions, consulted on (re-)ingest.
CREATE TABLE tombstones (
    id            bigserial PRIMARY KEY,
    selector_type text NOT NULL CHECK (selector_type IN
                    ('url', 'url_prefix', 'host', 'author', 'source', 'record')),
    selector      text NOT NULL,
    reason        text NOT NULL CHECK (reason IN ('opt_out', 'robots', 'denylist', 'purge', 'gone')),
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (selector_type, selector)
);

CREATE TABLE audits (
    audit_id   bigserial PRIMARY KEY,
    record_id  bigint NOT NULL,
    version    integer NOT NULL,
    source_id  text NOT NULL,
    verdict    text NOT NULL CHECK (verdict IN ('ok', 'minor', 'wrong', 'unsupported')),
    score      real NOT NULL,
    notes      jsonb NOT NULL DEFAULT '{}',
    model      text,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audits_source ON audits (source_id, created_at);

CREATE TABLE crawl_digest (
    day        date PRIMARY KEY,
    digest     jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    emailed_at timestamptz
);

CREATE TABLE heartbeats (
    role      text NOT NULL,
    instance  text NOT NULL,
    last_beat timestamptz NOT NULL DEFAULT now(),
    progress  jsonb NOT NULL DEFAULT '{}',
    PRIMARY KEY (role, instance)
);

-- Creates monthly partitions for the current month and `months_ahead` future months of
-- captures (captured_at), record_versions (created_at) and observations (observed_at).
-- Rows already sitting in a DEFAULT partition for a new range are moved into it, so the call
-- is safe at any time. Returns the number of partitions created. Idempotent.
CREATE FUNCTION vc_ensure_partitions(months_ahead integer DEFAULT 2) RETURNS integer
LANGUAGE plpgsql AS $$
DECLARE
    spec    record;
    m       integer;
    lo      timestamptz;
    hi      timestamptz;
    part    text;
    created integer := 0;
BEGIN
    IF months_ahead < 0 OR months_ahead > 24 THEN
        RAISE EXCEPTION 'months_ahead must be between 0 and 24';
    END IF;
    FOR spec IN SELECT * FROM (VALUES
            ('captures', 'captured_at'),
            ('record_versions', 'created_at'),
            ('observations', 'observed_at')) AS t(tbl, col) LOOP
        FOR m IN 0..months_ahead LOOP
            lo := vc_month_start(now()) + make_interval(months => m);
            hi := lo + interval '1 month';
            part := format('%s_%s', spec.tbl, to_char(lo AT TIME ZONE 'UTC', 'YYYYMM'));
            CONTINUE WHEN to_regclass(part) IS NOT NULL;
            EXECUTE format('CREATE TABLE %I (LIKE %I INCLUDING DEFAULTS INCLUDING CONSTRAINTS)',
                           part, spec.tbl);
            EXECUTE format(
                'WITH moved AS (DELETE FROM %I WHERE %I >= %L AND %I < %L RETURNING *) '
                'INSERT INTO %I SELECT * FROM moved',
                spec.tbl || '_default', spec.col, lo, spec.col, hi, part);
            EXECUTE format('ALTER TABLE %I ATTACH PARTITION %I FOR VALUES FROM (%L) TO (%L)',
                           spec.tbl, part, lo, hi);
            created := created + 1;
        END LOOP;
    END LOOP;
    RETURN created;
END $$;

SELECT vc_ensure_partitions(2);
