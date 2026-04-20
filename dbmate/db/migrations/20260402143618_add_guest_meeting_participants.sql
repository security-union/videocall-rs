-- migrate:up
ALTER TABLE meetings ADD COLUMN IF NOT EXISTS allow_guests BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE meeting_participants ADD COLUMN IF NOT EXISTS is_guest BOOLEAN NOT NULL DEFAULT FALSE;

-- migrate:down
ALTER TABLE meetings DROP COLUMN IF EXISTS allow_guests;
ALTER TABLE meeting_participants DROP COLUMN IF EXISTS is_guest;
