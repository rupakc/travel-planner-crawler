-- 0004 raw store (owner: vc-store 12xx). Spec §5.5, §17.7. No FKs into partitioned tables.
SET lock_timeout = '5s';

CREATE TABLE segments (
    segment_id   text PRIMARY KEY,                 -- ULID
    path         text NOT NULL UNIQUE,             -- segments/… or ephemeral/…
    cdxj_path    text NOT NULL,
    bytes        bigint NOT NULL,
    crc32c       bigint NOT NULL,
    record_count integer NOT NULL,
    ephemeral    boolean NOT NULL DEFAULT false,
    created_at   timestamptz NOT NULL,
    committed_at timestamptz NOT NULL DEFAULT now(),
    rewritten_at timestamptz,                      -- purge rewrite
    purged_at    timestamptz
);

-- One row per WARC record, range-partitioned by month (partitions: vc_ensure_partitions).
CREATE TABLE captures (
    capture_id       text NOT NULL,                -- ULID
    captured_at      timestamptz NOT NULL,
    segment_id       text NOT NULL,
    warc_offset      bigint NOT NULL,
    warc_length      bigint NOT NULL,
    warc_record_id   text NOT NULL,
    url_key          bytea NOT NULL,
    url              text NOT NULL,
    key              text NOT NULL,
    source_id        text,
    role             text NOT NULL CHECK (role IN ('primary', 'subresource', 'robots', 'manifest')),
    fetch_mode       text NOT NULL,
    epoch            bigint NOT NULL,
    orphan_epoch     boolean NOT NULL DEFAULT false,
    status           smallint,
    media_type       text NOT NULL,
    payload_hash     bytea,
    payload_len      bigint NOT NULL DEFAULT 0,
    etag             text,
    last_modified    text,
    revisit_of_id    text,
    revisit_of_at    timestamptz,
    use_signals      jsonb NOT NULL DEFAULT '{}',
    use_policy_fetch jsonb NOT NULL,
    use_policy_final jsonb,                        -- set by the processor (meet; only restricts)
    raw_expires_at   timestamptz,                  -- noarchive / TDM: purge raw after this
    text_ref         text,                         -- text/YYYY/MM/<segment>.jsonl.zst
    PRIMARY KEY (capture_id, captured_at)
) PARTITION BY RANGE (captured_at);
CREATE TABLE captures_default PARTITION OF captures DEFAULT;

CREATE INDEX captures_url ON captures (url_key, captured_at DESC);
CREATE INDEX captures_segment ON captures (segment_id);
CREATE INDEX captures_raw_expiry ON captures (raw_expires_at) WHERE raw_expires_at IS NOT NULL;
