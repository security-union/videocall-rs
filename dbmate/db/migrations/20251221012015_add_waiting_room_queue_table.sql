-- migrate:up
CREATE TABLE waiting_room_queue (
    id SERIAL PRIMARY KEY,
    meeting_id VARCHAR(255) NOT NULL,
    user_id VARCHAR(255) NOT NULL,
    user_name VARCHAR(255),
    joined_at TIMESTAMP DEFAULT NOW(),
    status VARCHAR(20) DEFAULT 'waiting',
    approved_by VARCHAR(255),
    approved_at TIMESTAMP,
    rejection_reason TEXT,
    created_at TIMESTAMP DEFAULT NOW(),
    updated_at TIMESTAMP DEFAULT NOW(),
    
    FOREIGN KEY (meeting_id) REFERENCES meetings(room_id) ON DELETE CASCADE,
    CHECK (status IN ('waiting', 'approved', 'rejected', 'left'))
);

CREATE INDEX idx_waiting_room_meeting ON waiting_room_queue(meeting_id);
CREATE INDEX idx_waiting_room_status ON waiting_room_queue(meeting_id, status);
CREATE INDEX idx_waiting_room_user ON waiting_room_queue(user_id);

-- migrate:down

DROP TABLE IF EXISTS waiting_room_queue;