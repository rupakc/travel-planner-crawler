-- 1907 render re-queue (owner: vcrawl 19xx). Spec §5.4 (`render: auto`).
-- The processor switches a URL whose HTTP capture vc-parse flags `needs_render` to
-- fetch_mode 'render' and makes it due; this column keeps the SHA-256 of the body that
-- triggered it, so one content hash re-queues a URL at most once. NULL = never requested.
-- Expand-only.
SET lock_timeout = '5s';

ALTER TABLE url_state ADD COLUMN render_requested_hash bytea;
