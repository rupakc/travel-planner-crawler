-- 1701 adapter resume cursors (owner: sources 17xx). An API adapter that stops at its page cap
-- reports `AdapterReport { complete: false, cursor }`; the cursor is kept here per leased
-- `adapter:<id>` row so the next run (after a restart too) continues the stream instead of
-- starting over. A complete run deletes its row; the scheduler prunes rows of URLs that left
-- the crawl and rows nobody resumed for 30 days. Cursors are adapter-defined positions
-- (query name + offset, a redacted page path), never page content or credentials.
SET lock_timeout = '5s';

CREATE TABLE adapter_cursors (
    url_key    bytea PRIMARY KEY CHECK (octet_length(url_key) = 16),
    adapter    text NOT NULL,
    cursor     text NOT NULL CHECK (length(cursor) <= 2048),
    updated_at timestamptz NOT NULL DEFAULT now()
);
