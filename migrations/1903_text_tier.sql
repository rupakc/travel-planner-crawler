-- 1903 extracted-text tier bookkeeping (owner: vcrawl 19xx). Spec §5.5 "Extracted text tier".
-- One row per segment whose `text/YYYY/MM/<segment>.jsonl.zst` object was written (or is being
-- written: `claimed_at` is a lease, so a crashed writer's segment is picked up again). The
-- objects are create-if-absent, so a repeated write is harmless.
SET lock_timeout = '5s';

CREATE TABLE text_tier_segments (
    segment_id     text PRIMARY KEY,
    claimed_at     timestamptz NOT NULL DEFAULT now(),
    written_at     timestamptz,
    archive_rows   integer,
    ephemeral_rows integer
);
CREATE INDEX text_tier_segments_pending ON text_tier_segments (claimed_at) WHERE written_at IS NULL;
