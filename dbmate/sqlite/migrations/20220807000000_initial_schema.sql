-- migrate:up

-- OAuth request storage for PKCE/CSRF flows
CREATE TABLE IF NOT EXISTS oauth_requests (
    pkce_challenge TEXT,
    pkce_verifier TEXT,
    csrf_state TEXT,
    return_to TEXT,
    nonce TEXT
);

-- User accounts with OAuth tokens
CREATE TABLE IF NOT EXISTS users (
    email TEXT PRIMARY KEY,
    access_token TEXT,
    refresh_token TEXT,
    name TEXT,
    created_at TEXT DEFAULT (datetime('now')),
    last_login TEXT DEFAULT (datetime('now'))
);

-- Meetings
CREATE TABLE IF NOT EXISTS meetings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    deleted_at TEXT,
    creator_id TEXT,
    password_hash TEXT,
    state TEXT NOT NULL DEFAULT 'idle' CHECK (state IN ('idle', 'active', 'ended')),
    attendees TEXT NOT NULL DEFAULT '[]',
    host_display_name TEXT,
    waiting_room_enabled INTEGER NOT NULL DEFAULT 1
);

-- Partial unique index: only one active (non-deleted) meeting per room_id
CREATE UNIQUE INDEX IF NOT EXISTS idx_meetings_room_id_unique_active
    ON meetings(room_id) WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_meetings_room_id ON meetings(room_id);
CREATE INDEX IF NOT EXISTS idx_meetings_creator_id ON meetings(creator_id);
CREATE INDEX IF NOT EXISTS idx_meetings_state ON meetings(state);

-- Auto-update updated_at on meetings
CREATE TRIGGER IF NOT EXISTS update_meetings_updated_at
    AFTER UPDATE ON meetings
    FOR EACH ROW
BEGIN
    UPDATE meetings SET updated_at = datetime('now') WHERE id = NEW.id;
END;

-- Meeting participants (waiting room, admission tracking)
CREATE TABLE IF NOT EXISTS meeting_participants (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'waiting' CHECK (status IN ('waiting', 'admitted', 'rejected', 'left')),
    is_host INTEGER NOT NULL DEFAULT 0,
    is_required INTEGER NOT NULL DEFAULT 0,
    joined_at TEXT NOT NULL DEFAULT (datetime('now')),
    admitted_at TEXT,
    left_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    display_name TEXT,
    UNIQUE (meeting_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_meeting_participants_meeting_id
    ON meeting_participants(meeting_id);
CREATE INDEX IF NOT EXISTS idx_meeting_participants_user_id
    ON meeting_participants(user_id);
CREATE INDEX IF NOT EXISTS idx_meeting_participants_status
    ON meeting_participants(status);

-- Auto-update updated_at on meeting_participants
CREATE TRIGGER IF NOT EXISTS update_meeting_participants_updated_at
    AFTER UPDATE ON meeting_participants
    FOR EACH ROW
BEGIN
    UPDATE meeting_participants SET updated_at = datetime('now') WHERE id = NEW.id;
END;

-- migrate:down
DROP TRIGGER IF EXISTS update_meeting_participants_updated_at;
DROP TABLE IF EXISTS meeting_participants;
DROP TRIGGER IF EXISTS update_meetings_updated_at;
DROP TABLE IF EXISTS meetings;
DROP TABLE IF EXISTS users;
DROP TABLE IF EXISTS oauth_requests;
