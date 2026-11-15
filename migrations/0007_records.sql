-- 0007 records, versions, observations, extraction runs (owner: vc-index 15xx). Spec §5.6.
SET lock_timeout = '5s';

CREATE TABLE extraction_runs (
    run_id       text PRIMARY KEY,                 -- ExtractionRunId (ULID)
    kind         text NOT NULL,
    schema_ver   integer NOT NULL,
    prompt_hash  text,
    model        text,
    scope        jsonb NOT NULL DEFAULT '{}',
    status       text NOT NULL DEFAULT 'canary'
                   CHECK (status IN ('canary', 'rolling', 'done', 'aborted')),
    canary_stats jsonb,
    created_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz
);

-- One record per (url, kind, record_key).
CREATE TABLE records (
    record_id         bigserial PRIMARY KEY,
    kind              text NOT NULL,
    source_id         text NOT NULL,
    url_key           bytea NOT NULL,
    record_key        text NOT NULL,
    state             text NOT NULL DEFAULT 'active'
                        CHECK (state IN ('active', 'stale', 'gone', 'tombstoned')),
    current_version   integer NOT NULL DEFAULT 0,
    current_data_hash bytea,
    licence_class     text NOT NULL,
    title             text,
    place_ids         bigint[] NOT NULL DEFAULT '{}',
    quality           real NOT NULL DEFAULT 0.5,
    conflict          boolean NOT NULL DEFAULT false,
    first_seen        timestamptz NOT NULL DEFAULT now(),
    last_seen         timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (url_key, kind, record_key)
);
CREATE INDEX records_kind_places ON records USING gin (place_ids);
CREATE INDEX records_source ON records (source_id);
CREATE INDEX records_state ON records (kind, state);

-- Versions: a new row only when data_hash changes. Range-partitioned by month of created_at.
CREATE TABLE record_versions (
    record_id         bigint NOT NULL,
    version           integer NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    capture_id        text NOT NULL,
    captured_at       timestamptz NOT NULL,
    data              jsonb NOT NULL,
    data_hash         bytea NOT NULL,
    kind_schema       text NOT NULL,               -- kind@ver
    extractor_ver     text NOT NULL,
    method            text NOT NULL CHECK (method IN ('structured', 'adapter', 'llm')),
    model_id          text,
    prompt_hash       text,
    extraction_run_id text,
    quality           real NOT NULL DEFAULT 0.5,
    lang              text,
    place_ids         bigint[] NOT NULL DEFAULT '{}',
    licence_class     text NOT NULL,
    attribution       text,
    use_policy        jsonb NOT NULL,
    conflict          boolean NOT NULL DEFAULT false,
    title             text,
    excerpt           text CHECK (excerpt IS NULL OR char_length(excerpt) <= 300),
    valid_from        timestamptz NOT NULL DEFAULT now(),
    superseded_at     timestamptz,
    PRIMARY KEY (record_id, version, created_at)
) PARTITION BY RANGE (created_at);
CREATE TABLE record_versions_default PARTITION OF record_versions DEFAULT;
CREATE INDEX record_versions_capture ON record_versions (capture_id);
CREATE INDEX record_versions_current ON record_versions (record_id) WHERE superseded_at IS NULL;

-- Time series (fx_rate, weather_normal, advisory levels, …). Range-partitioned by month.
CREATE TABLE observations (
    series_key  text NOT NULL,
    observed_at timestamptz NOT NULL,
    source_id   text NOT NULL,
    value       jsonb NOT NULL,
    capture_id  text NOT NULL,
    captured_at timestamptz NOT NULL,
    record_id   bigint,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (series_key, observed_at, source_id)
) PARTITION BY RANGE (observed_at);
CREATE TABLE observations_default PARTITION OF observations DEFAULT;
