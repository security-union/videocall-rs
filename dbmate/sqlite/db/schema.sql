CREATE TABLE IF NOT EXISTS "schema_migrations" (version varchar(128) primary key);
CREATE TABLE oauth_requests (
    pkce_challenge TEXT,
    pkce_verifier TEXT,
    csrf_state TEXT UNIQUE,
    return_to TEXT,
    nonce TEXT
, created_at TEXT, browser_binding_hash TEXT);
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
CREATE INDEX idx_oauth_requests_created_at ON oauth_requests(created_at);
CREATE TABLE cli_auth_requests (
    id                          TEXT PRIMARY KEY CHECK (length(id) = 22),
    code_challenge              TEXT NOT NULL CHECK (length(code_challenge) BETWEEN 43 AND 128),
    redirect_kind               TEXT NOT NULL CHECK (redirect_kind IN ('loopback','remote')),
    redirect_host               TEXT CHECK (redirect_host IS NULL OR redirect_host = '127.0.0.1'),
    redirect_port               INTEGER CHECK (redirect_port IS NULL OR redirect_port BETWEEN 1024 AND 65535),
    client_state                TEXT NOT NULL CHECK (length(client_state) BETWEEN 16 AND 128),
    public_key                  TEXT NOT NULL CHECK (length(public_key) = 43),
    device_name                 TEXT NOT NULL CHECK (length(device_name) BETWEEN 1 AND 64),
    requester_ip                TEXT,
    -- Key for the per-client cap on open requests: the IPv4 address or the
    -- IPv6 /64. NULL when the address is infrastructure (loopback, private,
    -- missing) and the per-client cap does not apply.
    requester_cap_key           TEXT,
    created_at                  TEXT NOT NULL,
    expires_at                  TEXT NOT NULL,
    approval_nonce_hash         TEXT,
    approval_nonce_user_id      TEXT,
    approval_nonce_expires_at   TEXT,
    decided_at                  TEXT,
    decision                    TEXT CHECK (decision IS NULL OR decision IN ('approved','denied')),
    user_id                     TEXT,
    user_name                   TEXT,
    code_hash                   TEXT UNIQUE,
    code_expires_at             TEXT,
    consumed_at                 TEXT,
    CHECK ((redirect_kind = 'loopback') = (redirect_host IS NOT NULL AND redirect_port IS NOT NULL)),
    CHECK (decision IS NULL OR (decided_at IS NOT NULL AND user_id IS NOT NULL)),
    CHECK (code_hash IS NULL OR (decision = 'approved' AND code_expires_at IS NOT NULL))
);
CREATE INDEX idx_cli_auth_requests_expires_at ON cli_auth_requests(expires_at);
CREATE INDEX idx_cli_auth_requests_cap_key ON cli_auth_requests(requester_cap_key, expires_at)
    WHERE decided_at IS NULL;
CREATE TABLE devices (
    id                          TEXT PRIMARY KEY CHECK (length(id) = 22),
    public_key                  TEXT NOT NULL UNIQUE CHECK (length(public_key) = 43),
    fingerprint                 TEXT NOT NULL CHECK (length(fingerprint) = 19),
    name                        TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    user_id                     TEXT NOT NULL,
    user_name                   TEXT NOT NULL,
    requester_ip                TEXT,
    created_at                  TEXT NOT NULL,
    decided_at                  TEXT NOT NULL,
    last_used_at                TEXT NOT NULL,
    idle_expires_at             TEXT NOT NULL,
    abs_expires_at              TEXT NOT NULL,
    revoked_at                  TEXT,
    revoked_reason              TEXT CHECK (revoked_reason IS NULL OR revoked_reason IN ('user','device')),
    -- The idle expiry slides but never past the absolute one, so the sweep
    -- and the indexes only need idle_expires_at.
    CHECK (idle_expires_at <= abs_expires_at)
);
CREATE INDEX idx_devices_user_live ON devices(user_id) WHERE revoked_at IS NULL;
CREATE INDEX idx_devices_idle_expires ON devices(idle_expires_at);
CREATE INDEX idx_devices_revoked_at ON devices(revoked_at) WHERE revoked_at IS NOT NULL;
-- Dbmate schema migrations
INSERT INTO "schema_migrations" (version) VALUES
  ('20220807000000'),
  ('20261125000000');
