-- migrate:up

ALTER TABLE meetings
ADD COLUMN meeting_title VARCHAR(255) NULL,
ADD COLUMN password_hash VARCHAR(255) NULL,
ADD COLUMN waiting_room_enabled BOOLEAN DEFAULT FALSE,
ADD COLUMN meeting_status VARCHAR(20) DEFAULT 'not_started',
ADD CONSTRAINT meeting_status_check CHECK (meeting_status IN ('not_started', 'active', 'ended'));

-- migrate:down

ALTER TABLE meetings 
DROP COLUMN IF EXISTS meeting_title,
DROP COLUMN IF EXISTS password_hash,
DROP COLUMN IF EXISTS waiting_room_enabled,
DROP COLUMN IF EXISTS meeting_status;

DROP INDEX IF EXISTS idx_meetings_status;