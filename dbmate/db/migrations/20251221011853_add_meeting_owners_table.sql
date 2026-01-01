-- migrate:up
CREATE TABLE meeting_owners (
    id SERIAL PRIMARY KEY,
    meeting_id VARCHAR(255) NOT NULL,
    user_id VARCHAR(255) NOT NULL,
    delegated_by VARCHAR(255),
    delegated_at TIMESTAMP,
    is_active BOOLEAN DEFAULT true,
    created_at TIMESTAMP DEFAULT NOW(),
    updated_at TIMESTAMP DEFAULT NOW(),
    
    FOREIGN KEY (meeting_id) REFERENCES meetings(room_id) ON DELETE CASCADE,
    UNIQUE (meeting_id, user_id)
);


CREATE INDEX idx_meeting_owners ON meeting_owners(meeting_id, user_id);
CREATE INDEX idx_meeting_owners_user ON meeting_owners(user_id);


-- migrate:down

DROP TABLE IF EXISTS meeting_owners;