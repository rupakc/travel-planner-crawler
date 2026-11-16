-- 1201 store: purge rewrite of a committed segment (owner: vc-store). Spec §17.7.
-- The new segment object is uploaded first (new path, generation suffix .r<N>); this function
-- then switches the segments row to it, fixes the offsets of the kept captures and deletes the
-- purged capture rows, in ONE transaction; the old objects are deleted afterwards. The update
-- is fenced on the old path, so a replay after a crash is a no-op (returns false).
-- Argument: vc_store::purge::SegmentRewrite serialised with serde_json.
SET lock_timeout = '5s';

CREATE FUNCTION vc_rewrite_segment(r jsonb) RETURNS boolean
LANGUAGE plpgsql AS $$
DECLARE
    n integer;
    k jsonb;
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

    DELETE FROM captures c
    USING jsonb_array_elements(coalesce(r->'removed', '[]'::jsonb)) x
    WHERE c.capture_id = x.value->>'capture_id'
      AND c.captured_at = (x.value->>'captured_at')::timestamptz
      AND c.segment_id = r->>'segment_id';

    FOR k IN SELECT value FROM jsonb_array_elements(coalesce(r->'kept', '[]'::jsonb)) LOOP
        UPDATE captures SET
            warc_offset = (k->>'offset')::bigint,
            warc_length = (k->>'length')::bigint
        WHERE capture_id = k->>'capture_id'
          AND captured_at = (k->>'captured_at')::timestamptz
          AND segment_id = r->>'segment_id';
    END LOOP;
    RETURN true;
END $$;
