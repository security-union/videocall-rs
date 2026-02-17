-- migrate:up
-- Meeting participants table for wait room and admission tracking

CREATE TABLE IF NOT EXISTS meeting_participants (
    id SERIAL PRIMARY KEY,
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    email VARCHAR(255) NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'waiting',
    is_host BOOLEAN NOT NULL DEFAULT FALSE,
    is_required BOOLEAN NOT NULL DEFAULT FALSE,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    admitted_at TIMESTAMPTZ NULL,
    left_at TIMESTAMPTZ NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT chk_participant_status
        CHECK (status IN ('waiting', 'admitted', 'rejected', 'left')),
    CONSTRAINT uq_meeting_participant
        UNIQUE (meeting_id, email)
);

-- Indexes for efficient lookups
CREATE INDEX idx_meeting_participants_meeting_id ON meeting_participants(meeting_id);
CREATE INDEX idx_meeting_participants_email ON meeting_participants(email);
CREATE INDEX idx_meeting_participants_status ON meeting_participants(status);

-- Trigger for updated_at
CREATE TRIGGER update_meeting_participants_updated_at
BEFORE UPDATE ON meeting_participants
FOR EACH ROW
EXECUTE FUNCTION update_updated_at_column();

-- migrate:down
DROP TRIGGER IF EXISTS update_meeting_participants_updated_at ON meeting_participants;
DROP TABLE IF EXISTS meeting_participants;
