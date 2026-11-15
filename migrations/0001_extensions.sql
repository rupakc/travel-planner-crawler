-- 0001 extensions. Expand-only (spec §17.6); every migration starts with a lock timeout.
SET lock_timeout = '5s';

CREATE EXTENSION IF NOT EXISTS vector;   -- pgvector: embeddings (halfvec + HNSW)
CREATE EXTENSION IF NOT EXISTS pg_trgm;  -- entity name similarity

-- First day of the month containing ts (UTC). Used by partition maintenance.
CREATE OR REPLACE FUNCTION vc_month_start(ts timestamptz) RETURNS timestamptz
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT date_trunc('month', ts AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
$$;
