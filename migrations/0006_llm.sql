-- 0006 LLM budget, ledger, batch jobs, cache (owner: vc-llm 14xx). Spec §5.8, §17.3, §17.4.
SET lock_timeout = '5s';

-- One row per day ('YYYY-MM-DD') and per month ('YYYY-MM'). Reservations update both rows in
-- one statement (§17.4). Amounts are USD with 6 decimals (vc_core::llm::Usd is micro-USD).
CREATE TABLE llm_budget (
    period   text PRIMARY KEY CHECK (period ~ '^\d{4}-\d{2}(-\d{2})?$'),
    cap      numeric(14, 6) NOT NULL CHECK (cap >= 0),
    reserved numeric(14, 6) NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    spent    numeric(14, 6) NOT NULL DEFAULT 0 CHECK (spent >= 0)
);

-- Per-purpose spend within a month (purpose sub-shares of the monthly cap).
CREATE TABLE llm_purpose_budget (
    period   text NOT NULL CHECK (period ~ '^\d{4}-\d{2}$'),
    purpose  text NOT NULL CHECK (purpose IN
               ('extraction', 'embedding', 'triage', 'auditor', 'scout', 'place')),
    cap      numeric(14, 6) NOT NULL CHECK (cap >= 0),
    reserved numeric(14, 6) NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    spent    numeric(14, 6) NOT NULL DEFAULT 0 CHECK (spent >= 0),
    PRIMARY KEY (period, purpose)
);

CREATE TABLE llm_ledger (
    id                  text PRIMARY KEY,          -- ReservationId (ULID)
    day                 date NOT NULL,
    month               text NOT NULL,
    purpose             text NOT NULL CHECK (purpose IN
                          ('extraction', 'embedding', 'triage', 'auditor', 'scout', 'place')),
    model               text,
    est_usd             numeric(14, 6) NOT NULL,
    actual_usd          numeric(14, 6),
    input_tokens        bigint,
    output_tokens       bigint,
    thinking_tokens     bigint,
    cached_input_tokens bigint,
    batch               boolean NOT NULL DEFAULT false,
    status              text NOT NULL DEFAULT 'reserved'
                          CHECK (status IN ('reserved', 'settled', 'expired', 'released')),
    created_at          timestamptz NOT NULL DEFAULT now(),
    settled_at          timestamptz
);
CREATE INDEX llm_ledger_open ON llm_ledger (created_at) WHERE status = 'reserved' AND NOT batch;

-- Batch prediction lifecycle (§17.3).
CREATE TABLE batch_jobs (
    id              text PRIMARY KEY,              -- BatchJobId (ULID); display name + label
    state           text NOT NULL DEFAULT 'prepared' CHECK (state IN
                      ('prepared', 'submitted', 'succeeded', 'failed', 'ingested')),
    purpose         text NOT NULL,
    model           text NOT NULL,
    input_uri       text NOT NULL,
    output_uri      text,
    vertex_job_name text UNIQUE,
    reservation_id  text NOT NULL,
    reserved_usd    numeric(14, 6) NOT NULL,
    request_count   integer NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    submitted_at    timestamptz,
    finished_at     timestamptz,
    ingested_at     timestamptz,
    error           text
);
CREATE INDEX batch_jobs_open ON batch_jobs (state) WHERE state IN ('prepared', 'submitted', 'succeeded');

-- Identical inputs are never paid twice.
CREATE TABLE llm_cache (
    model       text NOT NULL,
    prompt_hash text NOT NULL,
    input_hash  text NOT NULL,
    output      jsonb NOT NULL,                    -- LlmResponse without usage cost
    created_at  timestamptz NOT NULL DEFAULT now(),
    hits        integer NOT NULL DEFAULT 0,
    PRIMARY KEY (model, prompt_hash, input_hash)
);
