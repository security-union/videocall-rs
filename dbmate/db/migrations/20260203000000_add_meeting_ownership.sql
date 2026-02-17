-- migrate:up
-- Add meeting ownership and access control columns
-- Note: password_hash is already added by 20251221011540_add_meeting_protection_fields.sql

-- Meeting state: idle (not started), active (in progress), ended
ALTER TABLE meetings ADD COLUMN IF NOT EXISTS state VARCHAR(50) NOT NULL DEFAULT 'idle';
ALTER TABLE meetings ADD CONSTRAINT IF NOT EXISTS chk_meeting_state
    CHECK (state IN ('idle', 'active', 'ended'));

-- Attendees list as JSONB array (max 100 attendees)
ALTER TABLE meetings ADD COLUMN IF NOT EXISTS attendees JSONB NOT NULL DEFAULT '[]';
ALTER TABLE meetings ADD CONSTRAINT IF NOT EXISTS chk_attendees_max_100
    CHECK (jsonb_array_length(attendees) <= 100);

-- Index for faster state lookups
CREATE INDEX IF NOT EXISTS idx_meetings_state ON meetings(state);

-- migrate:down
DROP INDEX IF EXISTS idx_meetings_state;
ALTER TABLE meetings DROP CONSTRAINT IF EXISTS chk_attendees_max_100;
ALTER TABLE meetings DROP COLUMN IF EXISTS attendees;
ALTER TABLE meetings DROP CONSTRAINT IF EXISTS chk_meeting_state;
ALTER TABLE meetings DROP COLUMN IF EXISTS state;
