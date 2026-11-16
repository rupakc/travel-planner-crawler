-- 1904 purge garbage and frontier hygiene for removed captures (owner: vcrawl 19xx; the
-- function is vc-store's `vc_rewrite_segment`, replaced in place with the same signature).
-- Spec §5.5, §17.7. Expand-only: one new table, one function body replaced compatibly (the
-- previous image's callers pass the same jsonb and get the same boolean).
--
-- * `store_garbage`: object paths a committed purge rewrite made obsolete (the old generation,
--   and uncommitted generations the rewrite stepped over). Written in the rewrite's own
--   transaction, so a crash between the commit and the object deletion never leaves purged
--   bytes in the bucket forever: later runs delete every listed object that no `segments` row
--   still names, then the row. `admin rebuild-captures` never resurrects a listed object.
-- * Removed captures release their URL's validators: when the capture that `url_state` takes
--   its etag / last-modified / payload hash / last capture from is gone (or the stored body
--   behind them is), those columns are cleared in the same transaction, so the next fetch is
--   unconditional and stores a body instead of a 304 revisit of bytes that no longer exist.
SET lock_timeout = '5s';

CREATE TABLE store_garbage (
    path        text PRIMARY KEY,
    segment_id  text NOT NULL,
    reason      text NOT NULL CHECK (reason IN ('rewritten', 'abandoned', 'unreferenced')),
    recorded_at timestamptz NOT NULL DEFAULT now()
);

CREATE OR REPLACE FUNCTION vc_rewrite_segment(r jsonb) RETURNS boolean
LANGUAGE plpgsql AS $$
DECLARE
    n    integer;
    k    jsonb;
    keys bytea[];
    ids  text[];
BEGIN
    UPDATE segments SET
        path         = coalesce(r->>'new_path', path),
        cdxj_path    = coalesce(r->>'new_cdxj_path', cdxj_path),
        bytes        = (r->>'bytes')::bigint,
        crc32c       = (r->>'crc32c')::bigint,
        record_count = (r->>'record_count')::integer,
        rewritten_at = now(),
        purged_at    = CASE WHEN r->>'new_path' IS NULL THEN now() ELSE purged_at END
    WHERE segment_id = r->>'segment_id'
      AND path = r->>'old_path';
    GET DIAGNOSTICS n = ROW_COUNT;
    IF n = 0 THEN
        RETURN false;
    END IF;

    INSERT INTO store_garbage (path, segment_id, reason)
    SELECT p, r->>'segment_id', 'rewritten'
    FROM unnest(ARRAY[r->>'old_path', r->>'old_cdxj_path']) AS p
    WHERE p IS NOT NULL
    ON CONFLICT (path) DO NOTHING;
    INSERT INTO store_garbage (path, segment_id, reason)
    SELECT a, r->>'segment_id', 'abandoned'
    FROM jsonb_array_elements_text(coalesce(r->'abandoned', '[]'::jsonb)) AS a
    ON CONFLICT (path) DO NOTHING;

    WITH gone AS (
        DELETE FROM captures c
        USING jsonb_array_elements(coalesce(r->'removed', '[]'::jsonb)) x
        WHERE c.capture_id = x.value->>'capture_id'
          AND c.captured_at = (x.value->>'captured_at')::timestamptz
          AND c.segment_id = r->>'segment_id'
        RETURNING c.capture_id, c.url_key, c.role
    )
    SELECT array_agg(DISTINCT url_key) FILTER (WHERE role = 'primary'), array_agg(capture_id)
    INTO keys, ids
    FROM gone;

    FOR k IN SELECT value FROM jsonb_array_elements(coalesce(r->'kept', '[]'::jsonb)) LOOP
        UPDATE captures SET
            warc_offset = (k->>'offset')::bigint,
            warc_length = (k->>'length')::bigint
        WHERE capture_id = k->>'capture_id'
          AND captured_at = (k->>'captured_at')::timestamptz
          AND segment_id = r->>'segment_id';
    END LOOP;

    -- Validators survive only while the body they vouch for is still stored: the last capture
    -- is still there, is not a revisit of a removed capture, and some remaining 2xx capture of
    -- the URL holds a body with that payload hash.
    IF keys IS NOT NULL THEN
        UPDATE url_state u SET
            etag            = NULL,
            last_modified   = NULL,
            payload_hash    = NULL,
            last_capture_id = NULL
        WHERE u.url_key = ANY (keys)
          AND (u.etag IS NOT NULL OR u.last_modified IS NOT NULL OR u.payload_hash IS NOT NULL
               OR u.last_capture_id IS NOT NULL)
          AND (u.last_capture_id = ANY (ids)
               OR EXISTS (SELECT 1 FROM captures l
                          WHERE l.capture_id = u.last_capture_id
                            AND l.revisit_of_id = ANY (ids))
               OR (u.payload_hash IS NOT NULL AND NOT EXISTS (SELECT 1 FROM captures b
                              WHERE b.url_key = u.url_key
                                AND b.payload_hash = u.payload_hash
                                AND b.revisit_of_id IS NULL
                                AND b.status BETWEEN 200 AND 299)));
    END IF;
    RETURN true;
END $$;
