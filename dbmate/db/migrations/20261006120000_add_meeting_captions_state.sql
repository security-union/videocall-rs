-- migrate:up
SET LOCAL lock_timeout = '5s';
ALTER TABLE meetings
    ADD COLUMN IF NOT EXISTS captions_on BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS captions_generation BIGINT NOT NULL DEFAULT 0;

-- The presence sweep reconciles captions left on; only the few rows with
-- captions on are ever candidates, so this partial index keeps that scan
-- from reading the whole table.
CREATE INDEX IF NOT EXISTS idx_meetings_captions_on
    ON meetings (id)
    WHERE captions_on;

-- migrate:down
DROP INDEX IF EXISTS idx_meetings_captions_on;
ALTER TABLE meetings
    DROP COLUMN IF EXISTS captions_generation,
    DROP COLUMN IF EXISTS captions_on;
