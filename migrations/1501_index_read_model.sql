-- 1501 vc-index read model (owner: vc-index 15xx). Spec §5.6, §5.9, §5.11, §17.7, §17.9.
-- Expand-only: new nullable/defaulted columns, indexes and functions.
SET lock_timeout = '5s';

-- Records: what the reader serves without joining the raw store, plus tombstone bookkeeping.
ALTER TABLE records
    ADD COLUMN url              text,
    ADD COLUMN host             text,
    ADD COLUMN licence          jsonb,                -- {class, spdx, attribution, url}
    ADD COLUMN source_class     text,
    ADD COLUMN lat              double precision,
    ADD COLUMN lon              double precision,
    ADD COLUMN last_captured_at timestamptz,          -- monotonic guard: older captures never win
    ADD COLUMN tombstone_reason text
        CHECK (tombstone_reason IS NULL OR tombstone_reason IN
               ('opt_out', 'robots', 'denylist', 'purge', 'gone'));
CREATE INDEX records_host ON records (host);
CREATE INDEX records_url_prefix ON records (url text_pattern_ops);
CREATE INDEX records_served ON records (updated_at) WHERE state IN ('active', 'stale');

-- Entities: share_alike merges only within one licence (CC BY-SA vs ODbL).
ALTER TABLE entities
    ADD COLUMN licence_spdx text;
CREATE INDEX entities_resolve ON entities (kind, state);

-- Observations: licence, origin URL and tombstone flag (purges hide points, never rewrite them).
ALTER TABLE observations
    ADD COLUMN licence    jsonb,
    ADD COLUMN url_key    bytea,
    ADD COLUMN url        text,
    ADD COLUMN host       text,
    ADD COLUMN tombstoned boolean NOT NULL DEFAULT false;

CREATE INDEX embeddings_owner ON embeddings (owner_type, owner_id);

-- Great-circle distance in metres (no PostGIS).
CREATE FUNCTION vc_haversine_m(lat1 double precision, lon1 double precision,
                               lat2 double precision, lon2 double precision)
RETURNS double precision LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT 2 * 6371008.8 * asin(least(1, sqrt(
      power(sin(radians(lat2 - lat1) / 2), 2)
    + cos(radians(lat1)) * cos(radians(lat2)) * power(sin(radians(lon2 - lon1) / 2), 2))))
$$;
