-- 1101 frontier: Frontier::reschedule (owner: vc-frontier). Additive.
-- A forced refetch of a URL that is leased or fetched_pending must not disturb the running
-- fetch, so it is parked in refetch_at and applied the moment the URL returns to 'due', by
-- whatever path does that (completion, lease expiry, vc_commit_segment): next_fetch_at becomes
-- least(next_fetch_at, refetch_at) and refetch_at is cleared. Politeness is untouched: claims
-- are still gated by politeness_keys (next_allowed_at, parking, budget).
SET lock_timeout = '5s';

ALTER TABLE url_state ADD COLUMN refetch_at timestamptz;

CREATE FUNCTION vc_url_state_apply_refetch() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state = 'due' THEN
        NEW.next_fetch_at := least(NEW.next_fetch_at, NEW.refetch_at);
        NEW.refetch_at := NULL;
    ELSIF NEW.state NOT IN ('leased', 'fetched_pending') THEN
        NEW.refetch_at := NULL;  -- gone/tombstoned/disabled: a pending refetch is moot
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER url_state_apply_refetch
    BEFORE UPDATE ON url_state
    FOR EACH ROW WHEN (NEW.refetch_at IS NOT NULL)
    EXECUTE FUNCTION vc_url_state_apply_refetch();
