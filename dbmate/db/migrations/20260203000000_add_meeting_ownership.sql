-- migrate:up
-- Add meeting ownership and access control columns

-- Password hash for optional meeting protection
ALTER TABLE meetings ADD COLUMN password_hash VARCHAR(255) NULL;

-- Meeting state: idle (not started), active (in progress), ended
ALTER TABLE meetings ADD COLUMN state VARCHAR(50) NOT NULL DEFAULT 'idle';
ALTER TABLE meetings ADD CONSTRAINT chk_meeting_state
    CHECK (state IN ('idle', 'active', 'ended'));

-- Attendees list as JSONB array (max 100 attendees)
ALTER TABLE meetings ADD COLUMN attendees JSONB NOT NULL DEFAULT '[]';
ALTER TABLE meetings ADD CONSTRAINT chk_attendees_max_100
    CHECK (jsonb_array_length(attendees) <= 100);

-- Index for faster state lookups
CREATE INDEX idx_meetings_state ON meetings(state);

-- migrate:down
DROP INDEX IF EXISTS idx_meetings_state;
ALTER TABLE meetings DROP CONSTRAINT IF EXISTS chk_attendees_max_100;
ALTER TABLE meetings DROP COLUMN IF EXISTS attendees;
ALTER TABLE meetings DROP CONSTRAINT IF EXISTS chk_meeting_state;
ALTER TABLE meetings DROP COLUMN IF EXISTS state;
ALTER TABLE meetings DROP COLUMN IF EXISTS password_hash;
