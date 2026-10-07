-- migrate:up
-- Server-registered in-app recordings (issue #2856).
CREATE TABLE IF NOT EXISTS meeting_recording_leases (
    recording_id UUID PRIMARY KEY,
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    user_id VARCHAR(255) NOT NULL,
    attempt_id UUID NOT NULL,
    secret_hash BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    renewed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at TIMESTAMPTZ,
    UNIQUE (meeting_id, user_id)
);

CREATE TABLE IF NOT EXISTS meeting_recording_state (
    meeting_id INTEGER PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
    version BIGINT NOT NULL,
    changed_at TIMESTAMPTZ NOT NULL,
    reg_window_start TIMESTAMPTZ NOT NULL,
    reg_window_count INTEGER NOT NULL,
    reg_prev_count INTEGER NOT NULL
);

-- migrate:down
DROP TABLE IF EXISTS meeting_recording_state;
DROP TABLE IF EXISTS meeting_recording_leases;
