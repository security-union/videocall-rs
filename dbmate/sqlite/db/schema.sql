CREATE TABLE IF NOT EXISTS "schema_migrations" (version varchar(128) primary key);
CREATE TABLE oauth_requests (
    pkce_challenge TEXT,
    pkce_verifier  TEXT,
    csrf_state     TEXT,
    return_to      TEXT,
    nonce          TEXT CHECK (nonce IS NULL OR length(nonce) <= 255)
);
CREATE TABLE users (
    email         TEXT PRIMARY KEY CHECK (length(email) <= 255),
    access_token  TEXT,
    refresh_token TEXT,
    name          TEXT,
    created_at    TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now')),
    last_login    TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
);
CREATE TABLE meetings (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id              TEXT NOT NULL CHECK (length(room_id) <= 255),
    started_at           TEXT NOT NULL,
    ended_at             TEXT,
    created_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    deleted_at           TEXT,
    creator_id           TEXT CHECK (creator_id IS NULL OR length(creator_id) <= 255),
    password_hash        TEXT CHECK (password_hash IS NULL OR length(password_hash) <= 255),
    state                TEXT NOT NULL DEFAULT 'idle'
                         CHECK (state IN ('idle', 'active', 'ended')),
    attendees            TEXT NOT NULL DEFAULT '[]'
                         CHECK (json_array_length(attendees) <= 100),
    host_display_name    TEXT CHECK (host_display_name IS NULL OR length(host_display_name) <= 255),
    waiting_room_enabled BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE INDEX idx_meetings_room_id ON meetings(room_id);
CREATE INDEX idx_meetings_creator_id ON meetings(creator_id);
CREATE INDEX idx_meetings_state ON meetings(state);
CREATE UNIQUE INDEX idx_meetings_room_id_unique_active
    ON meetings(room_id)
    WHERE deleted_at IS NULL;
CREATE TABLE meeting_participants (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    meeting_id   INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    user_id      TEXT NOT NULL CHECK (length(user_id) <= 255),
    status       TEXT NOT NULL DEFAULT 'waiting'
                 CHECK (status IN ('waiting', 'admitted', 'rejected', 'left')),
    is_host      BOOLEAN NOT NULL DEFAULT FALSE,
    is_required  BOOLEAN NOT NULL DEFAULT FALSE,
    joined_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    admitted_at  TEXT,
    left_at      TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    display_name TEXT CHECK (display_name IS NULL OR length(display_name) <= 255),

    CONSTRAINT uq_meeting_participant_user UNIQUE (meeting_id, user_id)
);
CREATE INDEX idx_meeting_participants_meeting_id ON meeting_participants(meeting_id);
CREATE INDEX idx_meeting_participants_user_id ON meeting_participants(user_id);
CREATE INDEX idx_meeting_participants_status ON meeting_participants(status);
-- Dbmate schema migrations
INSERT INTO "schema_migrations" (version) VALUES
  ('20260307000001');
