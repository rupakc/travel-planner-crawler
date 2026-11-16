-- 1905 author purges never keep the name (owner: vcrawl 19xx). Spec §5.10, §17.7.
-- * `purge_requests.scope`: what an author purge covers (matched records, the captures that
--   produced them, their URLs), fixed at the index stage before the data is scrubbed (after
--   which the author can no longer be found); vcrawl::admin::author::AuthorScope as JSON.
-- * Author selectors are stored as `sha256:<hex>` of the display name, in `purge_requests` and
--   in the standing exclusions (`tombstones`, which vc-index hashes on re-ingest). Rows written
--   before this migration are converted. Expand-only: one nullable column, values rewritten in
--   place (vc-index matches both forms).
SET lock_timeout = '5s';

ALTER TABLE purge_requests ADD COLUMN scope jsonb;

UPDATE purge_requests
SET selector = jsonb_build_object('type', 'author', 'value',
        'sha256:' || encode(sha256(convert_to(selector->>'value', 'UTF8')), 'hex'))
WHERE selector->>'type' = 'author' AND selector->>'value' NOT LIKE 'sha256:%';

-- A plain row whose hashed twin already exists is dropped; the others are rewritten.
DELETE FROM tombstones t
WHERE t.selector_type = 'author' AND t.selector NOT LIKE 'sha256:%'
  AND EXISTS (SELECT 1 FROM tombstones h
              WHERE h.selector_type = 'author'
                AND h.selector = 'sha256:' || encode(sha256(convert_to(t.selector, 'UTF8')), 'hex'));
UPDATE tombstones
SET selector = 'sha256:' || encode(sha256(convert_to(selector, 'UTF8')), 'hex')
WHERE selector_type = 'author' AND selector NOT LIKE 'sha256:%';
