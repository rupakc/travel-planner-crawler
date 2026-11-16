-- 1103 frontier: url_state points at the last capture that STORED A BODY (owner: vc-frontier;
-- vc_commit_segment is redefined here, same contract as 0005 otherwise). Additive.
--
-- Before: every primary capture (304 revisits and identical-payload revisits included) moved
-- url_state.last_capture_id, so each unchanged fetch grew a revisit chain C0 <- R1 <- R2 <- ...
-- that readers had to walk. Now:
--   * last_capture_id + the new body_captured_at move only for a capture that holds a body:
--     not a revisit, a 2xx status, a payload hash present. 304s and identical-payload 200s keep
--     pointing at the stored body, so every revisit refers to it directly (chains stay one hop);
--   * last_captured_at keeps meaning "last primary fetch committed" (the monotonic guard below,
--     the FCDO change feed's "already have this version");
--   * validators (etag, last_modified), payload_hash, change_count, last_status, fetch_count
--     and next_fetch_at are unchanged from 0005: a 304 still refreshes the validators it
--     carries, so conditional requests keep being sent.
-- The frontier lease reads (last_capture_id, body_captured_at) as the CaptureRef revisits and
-- dedupe refer to.
SET lock_timeout = '5s';

ALTER TABLE url_state ADD COLUMN body_captured_at timestamptz;  -- captured_at of last_capture_id

-- Clearing last_capture_id (e.g. vc_rewrite_segment's validator release) clears its timestamp.
CREATE FUNCTION vc_url_state_clear_body_ref() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.body_captured_at := NULL;
    RETURN NEW;
END $$;

CREATE TRIGGER url_state_clear_body_ref
    BEFORE UPDATE ON url_state
    FOR EACH ROW WHEN (NEW.last_capture_id IS NULL AND NEW.body_captured_at IS NOT NULL)
    EXECUTE FUNCTION vc_url_state_clear_body_ref();

-- Backfill: resolve each pointer through its revisit chain (bounded) to the capture holding the
-- body. A chain that cannot be resolved keeps its pointer with the timestamp it was paired with.
WITH RECURSIVE walk(url_key, capture_id, captured_at, revisit_of_id, revisit_of_at, hops) AS (
    SELECT u.url_key, c.capture_id, c.captured_at, c.revisit_of_id, c.revisit_of_at, 0
    FROM url_state u
    JOIN captures c ON c.capture_id = u.last_capture_id AND c.captured_at = u.last_captured_at
    WHERE u.last_capture_id IS NOT NULL
  UNION ALL
    SELECT w.url_key, c.capture_id, c.captured_at, c.revisit_of_id, c.revisit_of_at, w.hops + 1
    FROM walk w
    JOIN captures c ON c.capture_id = w.revisit_of_id AND c.captured_at = w.revisit_of_at
    WHERE w.revisit_of_id IS NOT NULL AND w.hops < 64
),
body AS (
    SELECT DISTINCT ON (url_key) url_key, capture_id, captured_at
    FROM walk
    WHERE revisit_of_id IS NULL
    ORDER BY url_key, hops
)
UPDATE url_state u SET last_capture_id = b.capture_id, body_captured_at = b.captured_at
FROM body b
WHERE u.url_key = b.url_key;

UPDATE url_state SET body_captured_at = last_captured_at
WHERE last_capture_id IS NOT NULL AND body_captured_at IS NULL;

CREATE OR REPLACE FUNCTION vc_commit_segment(m jsonb) RETURNS boolean
LANGUAGE plpgsql AS $$
DECLARE
    c        jsonb;
    n        integer;
    ts       timestamptz;
    uk       bytea;
    st       smallint;
    ph       bytea;
    validish boolean;
    has_body boolean;
BEGIN
    INSERT INTO segments (segment_id, path, cdxj_path, bytes, crc32c, record_count, ephemeral,
                          created_at)
    VALUES (m->>'segment_id', m->>'path', m->>'cdxj_path', (m->>'bytes')::bigint,
            (m->>'crc32c')::bigint, (m->>'record_count')::integer,
            coalesce((m->>'ephemeral')::boolean, false), (m->>'created_at')::timestamptz)
    ON CONFLICT (segment_id) DO NOTHING;
    GET DIAGNOSTICS n = ROW_COUNT;
    IF n = 0 THEN
        RETURN false;
    END IF;

    FOR c IN SELECT value FROM jsonb_array_elements(coalesce(m->'captures', '[]'::jsonb)) LOOP
        ts := (c->>'captured_at')::timestamptz;
        uk := decode(c->>'url_key', 'hex');
        st := (c->>'status')::smallint;
        ph := CASE WHEN c->>'payload_hash' IS NULL THEN NULL
                   ELSE decode(c->>'payload_hash', 'hex') END;

        INSERT INTO captures (capture_id, captured_at, segment_id, warc_offset, warc_length,
                              warc_record_id, url_key, url, key, source_id, role, fetch_mode,
                              epoch, orphan_epoch, status, media_type, payload_hash, payload_len,
                              etag, last_modified, revisit_of_id, revisit_of_at, use_signals,
                              use_policy_fetch, raw_expires_at)
        VALUES (c->>'capture_id', ts, m->>'segment_id', (c->>'offset')::bigint,
                (c->>'length')::bigint, c->>'warc_record_id', uk, c->>'url', c->>'key',
                c->>'source_id', c->>'role', c->>'fetch_mode', (c->>'epoch')::bigint,
                coalesce((c->>'orphan_epoch')::boolean, false), st, c->>'media_type', ph,
                coalesce((c->>'payload_len')::bigint, 0), c->'validators'->>'etag',
                c->'validators'->>'last_modified', c->'revisit_of'->>'capture_id',
                (c->'revisit_of'->>'captured_at')::timestamptz,
                coalesce(c->'use_signals', '{}'::jsonb), c->'use_policy_fetch',
                (c->>'raw_expires_at')::timestamptz)
        ON CONFLICT DO NOTHING;

        IF c->>'role' = 'primary' THEN
            validish := st IS NOT NULL AND (st BETWEEN 200 AND 299 OR st = 304);
            has_body := st IS NOT NULL AND st BETWEEN 200 AND 299 AND ph IS NOT NULL
                        AND c->'revisit_of'->>'capture_id' IS NULL;
            UPDATE url_state u SET
                last_capture_id  = CASE WHEN has_body THEN c->>'capture_id'
                                        ELSE u.last_capture_id END,
                body_captured_at = CASE WHEN has_body THEN ts ELSE u.body_captured_at END,
                last_captured_at = ts,
                last_status      = st,
                fetch_count      = u.fetch_count + 1,
                etag             = CASE WHEN NOT validish THEN u.etag
                                        WHEN st = 304 THEN coalesce(c->'validators'->>'etag', u.etag)
                                        ELSE c->'validators'->>'etag' END,
                last_modified    = CASE WHEN NOT validish THEN u.last_modified
                                        WHEN st = 304 THEN coalesce(c->'validators'->>'last_modified', u.last_modified)
                                        ELSE c->'validators'->>'last_modified' END,
                payload_hash     = CASE WHEN validish THEN coalesce(ph, u.payload_hash)
                                        ELSE u.payload_hash END,
                change_count     = u.change_count + CASE
                                        WHEN validish AND ph IS NOT NULL
                                             AND u.payload_hash IS NOT NULL
                                             AND ph <> u.payload_hash THEN 1 ELSE 0 END,
                next_fetch_at    = coalesce((c->>'next_fetch_at')::timestamptz, u.next_fetch_at)
            WHERE u.url_key = uk
              AND (u.last_captured_at IS NULL OR u.last_captured_at < ts);

            UPDATE url_state u SET state = 'due', lease_epoch = NULL
            WHERE u.url_key = uk
              AND u.state = 'fetched_pending'
              AND u.lease_epoch = (c->>'epoch')::bigint;

            PERFORM vc_enqueue('capture_ready',
                               jsonb_build_object('capture_id', c->>'capture_id',
                                                  'captured_at', c->>'captured_at'),
                               c->>'capture_id');
        END IF;
    END LOOP;
    RETURN true;
END $$;
