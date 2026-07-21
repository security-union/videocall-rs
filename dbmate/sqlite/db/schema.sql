CREATE TABLE IF NOT EXISTS "schema_migrations" (version varchar(128) primary key);
CREATE TABLE oauth_requests (
    pkce_challenge TEXT,
    pkce_verifier TEXT,
    csrf_state TEXT UNIQUE,
    return_to TEXT,
    nonce TEXT
);
CREATE TABLE users (
    email TEXT PRIMARY KEY CHECK (length(email) <= 255),
    access_token TEXT,
    refresh_token TEXT,
    name TEXT,
    created_at TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    last_login TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);
CREATE TABLE meetings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id TEXT NOT NULL CHECK (length(room_id) <= 255),
    started_at TEXT NOT NULL,
    ended_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    deleted_at TEXT,
    creator_id TEXT CHECK (creator_id IS NULL OR length(creator_id) <= 255),
    password_hash TEXT,
    state TEXT NOT NULL DEFAULT 'idle' CHECK (state IN ('idle', 'active', 'ended')),
    attendees TEXT NOT NULL DEFAULT '[]'
        CHECK (json_type(attendees) = 'array' AND json_array_length(attendees) <= 100),
    host_display_name TEXT CHECK (host_display_name IS NULL OR length(host_display_name) <= 255),
    waiting_room_enabled INTEGER NOT NULL DEFAULT 1
);
CREATE UNIQUE INDEX idx_meetings_room_id_unique_active
    ON meetings(room_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_meetings_room_id ON meetings(room_id);
CREATE INDEX idx_meetings_creator_id ON meetings(creator_id);
CREATE INDEX idx_meetings_state ON meetings(state);
CREATE TABLE meeting_participants (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL CHECK (length(user_id) <= 255),
    status TEXT NOT NULL DEFAULT 'waiting' CHECK (status IN ('waiting', 'admitted', 'rejected', 'left')),
    is_host INTEGER NOT NULL DEFAULT 0,
    is_required INTEGER NOT NULL DEFAULT 0,
    joined_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    admitted_at TEXT,
    left_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    display_name TEXT CHECK (display_name IS NULL OR length(display_name) <= 255),
    UNIQUE (meeting_id, user_id)
);
CREATE INDEX idx_meeting_participants_meeting_id
    ON meeting_participants(meeting_id);
CREATE INDEX idx_meeting_participants_user_id
    ON meeting_participants(user_id);
CREATE INDEX idx_meeting_participants_status
    ON meeting_participants(status);
-- Dbmate schema migrations
INSERT INTO "schema_migrations" (version) VALUES
  ('20220807000000');
