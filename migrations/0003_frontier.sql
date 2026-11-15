-- 0003 frontier (owner: vc-frontier 11xx). Spec §5.3 + §17.2 + §17.9.
SET lock_timeout = '5s';

-- An origin (scheme://host[:port]) owns robots.txt.
CREATE TABLE origins (
    origin              text PRIMARY KEY,
    key                 text NOT NULL,
    robots_state        text NOT NULL DEFAULT 'unknown' CHECK (robots_state IN
                          ('unknown', 'ok', 'not_found', 'unauthorized', 'challenge', 'unavailable')),
    robots_http_status  smallint,
    robots_body         bytea,                     -- first 500 KiB (raw copy is spooled too)
    robots_capture_id   text,
    robots_last_good    bytea,
    robots_last_good_at timestamptz,
    robots_fetched_at   timestamptz,
    robots_expires_at   timestamptz,
    tdmrep              jsonb,                     -- /.well-known/tdmrep.json, fetched with robots
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX origins_key ON origins (key);
CREATE INDEX origins_robots_due ON origins (key, robots_expires_at);

-- A politeness key owns rate, in-flight and budgets. `shard` is a leased column (§11).
CREATE TABLE politeness_keys (
    key              text PRIMARY KEY,
    shard            smallint NOT NULL DEFAULT 0,
    enabled          boolean NOT NULL DEFAULT true,
    next_allowed_at  timestamptz NOT NULL DEFAULT now(),
    delay_ms         integer NOT NULL DEFAULT 5000 CHECK (delay_ms BETWEEN 0 AND 3600000),
    min_delay_ms     integer NOT NULL DEFAULT 5000,
    crawl_delay_ms   integer,
    ewma_latency_ms  integer,
    penalty_ms       integer NOT NULL DEFAULT 0,
    parked_until     timestamptz,
    park_level       smallint NOT NULL DEFAULT 0,
    park_reason      text,
    consecutive_5xx  smallint NOT NULL DEFAULT 0,
    budget_day       integer NOT NULL DEFAULT 3000,
    used_today       integer NOT NULL DEFAULT 0,
    budget_date      date NOT NULL DEFAULT (now() AT TIME ZONE 'UTC')::date,
    updated_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX politeness_keys_ready ON politeness_keys (shard, next_allowed_at) WHERE enabled;

-- Fenced claims: completion is `UPDATE … WHERE epoch = $e`.
CREATE SEQUENCE epoch_seq AS bigint;
CREATE TABLE claims (
    key          text PRIMARY KEY REFERENCES politeness_keys (key),
    holder       text NOT NULL,
    epoch        bigint NOT NULL,
    claimed_at   timestamptz NOT NULL DEFAULT now(),
    deadline     timestamptz NOT NULL,
    completed_at timestamptz
);
CREATE UNIQUE INDEX claims_epoch ON claims (epoch);

-- Global token bucket(s), shared by every process (default 4 req/s).
CREATE TABLE rate_bucket (
    name         text PRIMARY KEY,
    tokens       double precision NOT NULL,
    capacity     double precision NOT NULL,
    refill_per_s double precision NOT NULL CHECK (refill_per_s > 0),
    updated_at   timestamptz NOT NULL DEFAULT clock_timestamp()
);
INSERT INTO rate_bucket (name, tokens, capacity, refill_per_s) VALUES ('global', 4, 4, 4);

-- URL identity: almost never updated. Hash-partitioned (16).
CREATE TABLE urls (
    url_key         bytea NOT NULL CHECK (octet_length(url_key) = 16),
    url             text NOT NULL,
    key             text NOT NULL,
    source_id       text,
    discovered_from bytea,
    depth           smallint NOT NULL DEFAULT 0,
    first_seen      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (url_key)
) PARTITION BY HASH (url_key);

-- URL crawl state. States per §17.2.
CREATE TABLE url_state (
    url_key          bytea NOT NULL,
    key              text NOT NULL,
    state            text NOT NULL DEFAULT 'due' CHECK (state IN
                       ('due', 'leased', 'fetched_pending', 'gone', 'tombstoned', 'disabled')),
    lease_epoch      bigint,
    priority         real NOT NULL DEFAULT 0.5,
    next_fetch_at    timestamptz NOT NULL DEFAULT now(),
    fetch_mode       text NOT NULL DEFAULT 'http'
                       CHECK (fetch_mode IN ('http', 'render') OR fetch_mode LIKE 'adapter:_%'),
    expect           text,                         -- media-type hint (vc_core::MediaType)
    source_id        text,
    depth            smallint NOT NULL DEFAULT 0,
    last_capture_id  text,
    last_captured_at timestamptz,
    etag             text,
    last_modified    text,
    payload_hash     bytea,
    last_status      smallint,
    change_count     integer NOT NULL DEFAULT 0,
    fetch_count      integer NOT NULL DEFAULT 0,
    gone_count       smallint NOT NULL DEFAULT 0,
    est_interval_s   integer,
    PRIMARY KEY (url_key),
    CHECK ((state IN ('leased', 'fetched_pending')) = (lease_epoch IS NOT NULL))
) PARTITION BY HASH (url_key);

DO $$
BEGIN
    FOR i IN 0..15 LOOP
        EXECUTE format(
            'CREATE TABLE urls_p%s PARTITION OF urls FOR VALUES WITH (MODULUS 16, REMAINDER %s)',
            lpad(i::text, 2, '0'), i);
        -- url_state updates touch indexed columns (not HOT): aggressive per-partition vacuum.
        EXECUTE format(
            'CREATE TABLE url_state_p%s PARTITION OF url_state '
            'FOR VALUES WITH (MODULUS 16, REMAINDER %s) '
            'WITH (autovacuum_vacuum_scale_factor = 0.02, autovacuum_analyze_scale_factor = 0.02)',
            lpad(i::text, 2, '0'), i);
    END LOOP;
END $$;

CREATE INDEX urls_key ON urls (key);
-- The claim index (§5.3).
CREATE INDEX url_state_claim ON url_state (key, next_fetch_at, priority DESC) WHERE state = 'due';
CREATE INDEX url_state_leased ON url_state (key, lease_epoch) WHERE state IN ('leased', 'fetched_pending');
