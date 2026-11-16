-- 1502 vc-index: the kind of an observation series point (owner: vc-index 15xx). Spec §5.6, §5.9.
-- Tombstone `changes` rows of observations carry the kind, so kind-filtered change-feed clients
-- see them. Expand-only: one nullable column (older rows stay NULL; the writer falls back to
-- the series' last upsert change).
SET lock_timeout = '5s';

ALTER TABLE observations ADD COLUMN kind text;
