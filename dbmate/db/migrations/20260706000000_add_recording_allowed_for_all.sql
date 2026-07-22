-- migrate:up
ALTER TABLE meetings ADD COLUMN IF NOT EXISTS recording_allowed_for_all BOOLEAN NOT NULL DEFAULT FALSE;

-- migrate:down
ALTER TABLE meetings DROP COLUMN IF EXISTS recording_allowed_for_all;
