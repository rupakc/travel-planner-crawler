-- 1902 stale records (owner: vcrawl 19xx). Spec §5.6 curation states.
-- A record that a newer capture of its page no longer contains is marked `stale` (still
-- served, never deleted) with the capture that found it missing; it is re-activated (and these
-- columns cleared) when a later capture contains it again. Expand-only: nullable columns.
SET lock_timeout = '5s';

ALTER TABLE records
    ADD COLUMN stale_since       timestamptz,
    ADD COLUMN stale_capture_id  text,
    ADD COLUMN stale_captured_at timestamptz;
