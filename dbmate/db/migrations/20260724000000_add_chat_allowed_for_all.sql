-- migrate:up
ALTER TABLE meetings ADD COLUMN IF NOT EXISTS chat_allowed_for_all BOOLEAN NOT NULL DEFAULT TRUE;

-- migrate:down
ALTER TABLE meetings DROP COLUMN IF EXISTS chat_allowed_for_all;
