-- migrate:up
CREATE TABLE IF NOT EXISTS session_participants (
    id SERIAL PRIMARY KEY,
    room_id VARCHAR(255) NOT NULL,
    user_id VARCHAR(255) NOT NULL,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    left_at TIMESTAMPTZ NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(room_id, user_id)
);

-- Index for efficient counting by room
CREATE INDEX idx_session_participants_room_id ON session_participants(room_id);
CREATE INDEX idx_session_participants_active ON session_participants(room_id) WHERE left_at IS NULL;

-- migrate:down
DROP TABLE IF EXISTS session_participants;

