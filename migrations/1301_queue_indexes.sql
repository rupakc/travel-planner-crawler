-- 1301 queue (owner: vc-queue): indexes for pruning done items and listing dead letters.
SET lock_timeout = '5s';

CREATE INDEX IF NOT EXISTS work_items_done ON work_items (updated_at) WHERE state = 'done';
CREATE INDEX IF NOT EXISTS work_items_dead ON work_items (kind, updated_at) WHERE state = 'dead';
