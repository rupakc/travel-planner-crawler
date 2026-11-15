-- 0008 entities, claims, redirects (owner: vc-index 15xx). Spec §5.6.
SET lock_timeout = '5s';

CREATE TABLE entities (
    entity_id     text PRIMARY KEY,                -- EntityId (ULID)
    kind          text NOT NULL,
    name          text NOT NULL,
    norm_name     text NOT NULL,
    place_ids     bigint[] NOT NULL DEFAULT '{}',
    lat           double precision,
    lon           double precision,
    licence_class text NOT NULL,                   -- merges only between compatible licences
    state         text NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'tombstoned')),
    conflict      boolean NOT NULL DEFAULT false,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX entities_kind_places ON entities USING gin (place_ids);
CREATE INDEX entities_norm_name_trgm ON entities USING gin (norm_name gin_trgm_ops);

-- Per-attribute provenance.
CREATE TABLE entity_claims (
    entity_id  text NOT NULL REFERENCES entities (entity_id),
    attribute  text NOT NULL,
    record_id  bigint NOT NULL REFERENCES records (record_id),
    version    integer NOT NULL,
    value      jsonb NOT NULL,
    confidence real NOT NULL DEFAULT 1.0,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (entity_id, attribute, record_id)
);
CREATE INDEX entity_claims_record ON entity_claims (record_id);

-- Merged ids never 404.
CREATE TABLE entity_redirects (
    old_id    text PRIMARY KEY,
    new_id    text NOT NULL REFERENCES entities (entity_id),
    merged_at timestamptz NOT NULL DEFAULT now()
);
