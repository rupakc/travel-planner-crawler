-- 1908 admin decisions on a source survive re-seeding (owner: vcrawl 19xx; go-live).
-- `vcrawl admin source disable` sets `enabled_override = false`, `approve` sets it true; the
-- scheduler's seed upsert never writes it and applies it over the spec's `enabled` (a
-- restricted licence still wins). NULL = follow the spec. Expand-only.
SET lock_timeout = '5s';

ALTER TABLE sources ADD COLUMN enabled_override boolean;
