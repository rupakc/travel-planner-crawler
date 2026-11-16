-- 1901 held reference records (owner: vcrawl 19xx). Spec §5.7 step 4, §17.8.
-- High-stakes records from `reference` sources that wait for a second independent source to
-- agree (`emergency_numbers`). Shared by every processor instance and kept across restarts;
-- never served. Rows expire after 7 days (the processor ignores and deletes older ones).
SET lock_timeout = '5s';

CREATE TABLE held_records (
    kind        text NOT NULL,
    source_id   text NOT NULL,
    record_key  text NOT NULL,
    record      jsonb NOT NULL,                    -- vc_core::records::ExtractedRecord
    provenance  jsonb NOT NULL,                    -- vc_core::records::Provenance
    captured_at timestamptz NOT NULL,              -- a newer capture replaces an older one
    held_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, source_id, record_key)
);
CREATE INDEX held_records_held_at ON held_records (held_at);
