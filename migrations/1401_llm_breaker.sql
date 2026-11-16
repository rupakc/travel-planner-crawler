-- 1401 the spend breaker, shared by every role and remembered across restarts (owner: vc-llm).
-- A Vertex 403/429 that names billing or budget opens it for llm.breaker_h hours (§17.4);
-- every process reads it before calling Vertex, so one role's trip pauses all of them and a
-- restart does not forget it. One row (id = 1); open_until only ever moves forward.
SET lock_timeout = '5s';

CREATE TABLE llm_breaker (
    id         smallint PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    open_until timestamptz NOT NULL,
    reason     text NOT NULL DEFAULT '',
    updated_at timestamptz NOT NULL DEFAULT now()
);
