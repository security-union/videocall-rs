-- migrate:up
SET LOCAL lock_timeout = '5s';
ALTER TABLE meetings ADD COLUMN IF NOT EXISTS host_left_end_due_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_meetings_host_left_end_due_at
    ON meetings (host_left_end_due_at)
    WHERE host_left_end_due_at IS NOT NULL;

-- migrate:down
DROP INDEX IF EXISTS idx_meetings_host_left_end_due_at;
ALTER TABLE meetings DROP COLUMN IF EXISTS host_left_end_due_at;
