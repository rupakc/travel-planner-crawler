-- 1102 frontier: when a URL entered fetched_pending (owner: vc-frontier). Additive.
-- A fetched_pending row whose spool frame is lost (an old instance's spool never replayed, a
-- crash before upload) would otherwise wait for a commit that never comes; the sweeper
-- (`PgFrontier::sweep_stale_pending`) returns rows pending longer than a window to 'due'.
SET lock_timeout = '5s';

ALTER TABLE url_state ADD COLUMN pending_since timestamptz;

-- Rows already pending get the window from now.
UPDATE url_state SET pending_since = now() WHERE state = 'fetched_pending';
