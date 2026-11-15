-- 0010 API clients and the change feed (owner: vc-api 16xx; changes written by vc-index).
SET lock_timeout = '5s';

CREATE TABLE api_clients (
    client_id       text PRIMARY KEY,
    name            text NOT NULL,
    token_hash      text NOT NULL,                 -- argon2 PHC string
    licence_classes text[] NOT NULL DEFAULT
                      '{open,share_alike,facts_only,api_terms}',
    rate_per_min    integer NOT NULL DEFAULT 600,
    enabled         boolean NOT NULL DEFAULT true,
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_used_at    timestamptz
);

-- Change feed (§17.9): serve only xid < pg_snapshot_xmin(pg_current_snapshot()); cursor (xid, seq).
CREATE TABLE changes (
    seq        bigserial PRIMARY KEY,
    xid        xid8 NOT NULL DEFAULT pg_current_xact_id(),
    owner_type text NOT NULL CHECK (owner_type IN ('record', 'entity', 'observation', 'source')),
    owner_id   text NOT NULL,
    op         text NOT NULL CHECK (op IN ('upsert', 'tombstone', 'redirect')),
    kind       text,
    at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX changes_cursor ON changes (xid, seq);
