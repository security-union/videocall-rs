-- migrate:up

-- Table to store pre-registered attendees for a meeting
-- Attendees are stored when meeting is created (optional list, up to 100)
CREATE TABLE IF NOT EXISTS meeting_attendees (
    id SERIAL PRIMARY KEY,
    meeting_id VARCHAR(255) NOT NULL,
    user_id VARCHAR(255) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    FOREIGN KEY (meeting_id) REFERENCES meetings(room_id) ON DELETE CASCADE,
    UNIQUE (meeting_id, user_id)
);

-- Index for faster lookups
CREATE INDEX IF NOT EXISTS idx_meeting_attendees_meeting_id ON meeting_attendees(meeting_id);
CREATE INDEX IF NOT EXISTS idx_meeting_attendees_user_id ON meeting_attendees(user_id);

-- migrate:down

DROP TABLE IF EXISTS meeting_attendees;
