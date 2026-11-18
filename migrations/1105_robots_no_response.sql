-- 1105 robots.txt that gets no response at all (owner: vc-frontier 11xx).
-- Owner decision 2026-11-18: a robots.txt request with no response (connect/read timeout,
-- connection refused/reset/closed before a status line, TLS handshake failure, DNS failure)
-- is stored as `no_response` and treated like a missing robots.txt (allow all; the last good
-- copy still governs if one exists), with a 6 h TTL. Any HTTP answer keeps its meaning
-- (5xx/429 stay `unavailable` = disallow). Expand-only: the CHECK only gains a value. The
-- previous image reads an unknown state as "no cached entry" and fails closed.
SET lock_timeout = '5s';

ALTER TABLE origins DROP CONSTRAINT IF EXISTS origins_robots_state_check;
ALTER TABLE origins ADD CONSTRAINT origins_robots_state_check CHECK (robots_state IN
    ('unknown', 'ok', 'not_found', 'unauthorized', 'challenge', 'unavailable', 'no_response'))
    NOT VALID;
ALTER TABLE origins VALIDATE CONSTRAINT origins_robots_state_check;
