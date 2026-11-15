-- 0005 work queue and the commit contract (owners: vc-queue 13xx; vc_commit_segment: vc-store).
-- r5 Q10: the cross-crate commit is SQL. `vc_core::store::SegmentManifest` serialised with
-- serde_json IS the argument of vc_commit_segment.
SET lock_timeout = '5s';

CREATE TABLE work_items (
    work_id      bigserial PRIMARY KEY,
    kind         text NOT NULL CHECK (kind IN
                   ('capture_ready', 'extract', 'embed', 'triage', 'audit', 'purge')),
    payload      jsonb NOT NULL,                   -- ids only, never page content
    dedupe_key   text,
    state        text NOT NULL DEFAULT 'ready' CHECK (state IN ('ready', 'leased', 'done', 'dead')),
    attempts     integer NOT NULL DEFAULT 0,
    available_at timestamptz NOT NULL DEFAULT now(),
    leased_until timestamptz,
    last_error   text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX work_items_ready ON work_items (kind, available_at) WHERE state = 'ready';
CREATE INDEX work_items_leased ON work_items (kind, leased_until) WHERE state = 'leased';
CREATE UNIQUE INDEX work_items_dedupe ON work_items (kind, dedupe_key)
    WHERE dedupe_key IS NOT NULL AND state IN ('ready', 'leased');

-- Enqueue work; returns the new id, or NULL when an item with the same dedupe key is pending.
CREATE FUNCTION vc_enqueue(p_kind text, p_payload jsonb, p_dedupe_key text DEFAULT NULL,
                           p_available_at timestamptz DEFAULT now())
RETURNS bigint LANGUAGE sql AS $$
    INSERT INTO work_items (kind, payload, dedupe_key, available_at)
    VALUES (p_kind, p_payload, p_dedupe_key, p_available_at)
    ON CONFLICT (kind, dedupe_key) WHERE dedupe_key IS NOT NULL AND state IN ('ready', 'leased')
    DO NOTHING
    RETURNING work_id
$$;

-- Commit one uploaded segment in ONE transaction (spec §5.5 step 3, §17.2):
--   * insert the segments row (idempotent by segment_id: a repeat returns false, no-op);
--   * insert every capture;
--   * for primary captures: update url_state validators/payload hash ONLY when the capture is
--     newer than url_state.last_captured_at (monotonic guard, NOT epoch-gated, so late, orphan
--     and replayed segments are safe), flip fetched_pending -> due when the capture's epoch is
--     the URL's pending epoch, and enqueue capture_ready (dedupe = capture_id).
-- Validators and payload hash are taken only from 2xx and 304 captures; a 304 keeps the old
-- values where the capture has none.
CREATE FUNCTION vc_commit_segment(m jsonb) RETURNS boolean
LANGUAGE plpgsql AS $$
DECLARE
    c        jsonb;
    n        integer;
    ts       timestamptz;
    uk       bytea;
    st       smallint;
    ph       bytea;
    validish boolean;
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
            UPDATE url_state u SET
                last_capture_id  = c->>'capture_id',
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
