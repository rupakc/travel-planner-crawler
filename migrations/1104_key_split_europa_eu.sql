-- 1104 key splits: europa.eu is keyed per host (owner: vc-frontier 11xx; policy/key_splits.yaml).
-- From this release `PolitenessKey::for_host` keys every host under a split registrable domain
-- on its own (www.ecb.europa.eu, taxation-customs.ec.europa.eu, ...), so a 403 from one EU body
-- no longer parks the others. Rows written before carry the old key `europa.eu`; this re-keys
-- them. Expand-only (no schema change), idempotent: a second run finds nothing left to move.
--
-- vc_split_politeness_key(domain, park_host) for a registrable domain `domain`:
-- * one politeness_keys row per host seen under it (urls.url, origins.origin), copied from the
--   `domain` row (shard, delay, min delay, Crawl-delay, EWMA, penalty, budget and today's use,
--   next_allowed_at: never laxer than before), but **unparked** (park level 0) — except
--   `park_host`, the host whose answer caused the current park, and any host whose robots.txt
--   answered a challenge (a robots challenge blocks the whole key): those get the `domain`
--   row's parked_until / park_level / park_reason (the later park wins if one exists).
-- * urls.key, url_state.key (every state: a lease under `domain` can no longer complete, fails
--   closed, and the host key's next claim returns leased rows to due; parked keys have no live
--   lease anyway) and origins.key move to the host key. captures.key is history: unchanged.
-- * the `domain` row stays, parked as it was, with enabled = false: the previous image (which
--   still keys hosts by `domain` until it is rolled) can recreate rows under `domain`, but can
--   never claim or borrow that key again, so nothing slips past a host park in the mixed
--   window. A later run of this function moves any such stragglers to their hosts.
SET lock_timeout = '5s';

CREATE OR REPLACE FUNCTION vc_split_politeness_key(domain text, park_host text)
RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
    o politeness_keys%ROWTYPE;
BEGIN
    -- Lock order as in the frontier: politeness_keys row first.
    SELECT * INTO o FROM politeness_keys WHERE key = domain FOR UPDATE;

    CREATE TEMP TABLE IF NOT EXISTS _vc_split_hosts (host text PRIMARY KEY) ON COMMIT DROP;
    DELETE FROM _vc_split_hosts;
    INSERT INTO _vc_split_hosts (host)
    SELECT DISTINCT h FROM (
        SELECT lower(substring(url FROM '^[A-Za-z][A-Za-z0-9+.-]*://([^/:?#]+)')) AS h
        FROM urls WHERE key = domain
        UNION
        SELECT lower(substring(origin FROM '^[A-Za-z][A-Za-z0-9+.-]*://([^/:?#]+)'))
        FROM origins WHERE key = domain
    ) s
    WHERE h IS NOT NULL AND h <> domain AND h LIKE '%.' || domain;

    IF EXISTS (SELECT 1 FROM _vc_split_hosts) THEN
        INSERT INTO politeness_keys (key, shard, enabled, next_allowed_at, delay_ms, min_delay_ms,
                                     crawl_delay_ms, ewma_latency_ms, penalty_ms, budget_day,
                                     used_today, budget_date)
        SELECT s.host,
               coalesce(o.shard, 0), true,
               coalesce(o.next_allowed_at, now()),
               coalesce(o.delay_ms, 5000), coalesce(o.min_delay_ms, 5000),
               o.crawl_delay_ms, o.ewma_latency_ms, coalesce(o.penalty_ms, 0),
               coalesce(o.budget_day, 3000), coalesce(o.used_today, 0),
               coalesce(o.budget_date, (now() AT TIME ZONE 'UTC')::date)
        FROM _vc_split_hosts s
        ORDER BY s.host
        ON CONFLICT (key) DO NOTHING;

        -- The park goes only where it belongs.
        IF o.key IS NOT NULL AND (o.parked_until IS NOT NULL OR o.park_level > 0) THEN
            UPDATE politeness_keys k
            SET parked_until = CASE
                    WHEN k.parked_until IS NULL OR k.parked_until < o.parked_until
                        THEN o.parked_until ELSE k.parked_until END,
                park_level   = greatest(k.park_level, o.park_level),
                park_reason  = CASE
                    WHEN k.parked_until IS NULL OR k.parked_until < o.parked_until
                        THEN o.park_reason ELSE k.park_reason END,
                updated_at   = now()
            WHERE k.key IN (
                SELECT host FROM _vc_split_hosts
                WHERE host = park_host
                   OR EXISTS (SELECT 1 FROM origins g
                              WHERE g.key = domain AND g.robots_state = 'challenge'
                                AND lower(substring(g.origin FROM '^[A-Za-z][A-Za-z0-9+.-]*://([^/:?#]+)'))
                                    = _vc_split_hosts.host));
        END IF;

        UPDATE url_state u
        SET key = s.host
        FROM urls x JOIN _vc_split_hosts s
             ON s.host = lower(substring(x.url FROM '^[A-Za-z][A-Za-z0-9+.-]*://([^/:?#]+)'))
        WHERE u.key = domain AND x.url_key = u.url_key;

        UPDATE urls x
        SET key = s.host
        FROM _vc_split_hosts s
        WHERE x.key = domain
          AND s.host = lower(substring(x.url FROM '^[A-Za-z][A-Za-z0-9+.-]*://([^/:?#]+)'));

        UPDATE origins g
        SET key = s.host
        FROM _vc_split_hosts s
        WHERE g.key = domain
          AND s.host = lower(substring(g.origin FROM '^[A-Za-z][A-Za-z0-9+.-]*://([^/:?#]+)'));
    END IF;

    -- The old key is never claimed again (its park, if any, stays as the record of it).
    UPDATE politeness_keys SET enabled = false, updated_at = now()
    WHERE key = domain AND enabled;
END $$;

-- 2026-11-17 23:30 UTC: taxation-customs.ec.europa.eu answered 403 and parked europa.eu.
SELECT vc_split_politeness_key('europa.eu', 'taxation-customs.ec.europa.eu');
