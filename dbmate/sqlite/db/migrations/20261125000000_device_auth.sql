-- migrate:up

-- Device-key auth (SQLite mirror of dbmate/db/migrations/20261125000000_device_auth.sql).
-- Timestamps are RFC3339 TEXT, always bound from the application, so they sort
-- consistently with sqlx-encoded DateTime<Utc> values.

-- SQLite cannot ADD COLUMN with a non-constant default, so backfill existing
-- rows explicitly; the application always binds created_at on insert.
-- (csrf_state is already UNIQUE here from the initial schema.)
ALTER TABLE oauth_requests ADD COLUMN created_at TEXT;
UPDATE oauth_requests SET created_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
    WHERE created_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_oauth_requests_created_at ON oauth_requests(created_at);

-- One browser-approved CLI login in flight (videocall-cli `auth login`,
-- authorization code + PKCE). Created by GET /cli/authorize, decided on the
-- consent page, consumed once by POST /api/v1/devices. Only the SHA-256 of
-- the one-time code and of the approval nonce is stored.
CREATE TABLE IF NOT EXISTS cli_auth_requests (
    id                          TEXT PRIMARY KEY CHECK (length(id) = 22),
    code_challenge              TEXT NOT NULL CHECK (length(code_challenge) BETWEEN 43 AND 128),
    redirect_kind               TEXT NOT NULL CHECK (redirect_kind IN ('loopback','remote')),
    redirect_host               TEXT CHECK (redirect_host IS NULL OR redirect_host IN ('127.0.0.1','[::1]')),
    redirect_port               INTEGER CHECK (redirect_port IS NULL OR redirect_port BETWEEN 1024 AND 65535),
    client_state                TEXT NOT NULL CHECK (length(client_state) BETWEEN 16 AND 128),
    public_key                  TEXT NOT NULL CHECK (length(public_key) = 43),
    device_name                 TEXT NOT NULL CHECK (length(device_name) BETWEEN 1 AND 64),
    requester_ip                TEXT,
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
CREATE INDEX IF NOT EXISTS idx_cli_auth_requests_expires_at ON cli_auth_requests(expires_at);
CREATE INDEX IF NOT EXISTS idx_cli_auth_requests_ip ON cli_auth_requests(requester_ip, expires_at);

-- Approved CLI devices. One row per Ed25519 public key; it exists only once
-- a human approved the login and the CLI proved possession of the key. The
-- device signs a short-lived EdDSA JWT whose `kid` is `id`. No secret is
-- stored. Revoked and expired rows are kept 30 days for the device list.
CREATE TABLE IF NOT EXISTS devices (
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
    revoked_reason              TEXT CHECK (revoked_reason IS NULL OR revoked_reason IN ('user','device'))
);
CREATE INDEX IF NOT EXISTS idx_devices_user_live ON devices(user_id) WHERE revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_devices_abs_expires ON devices(abs_expires_at);
CREATE INDEX IF NOT EXISTS idx_devices_idle_expires ON devices(idle_expires_at);
CREATE INDEX IF NOT EXISTS idx_devices_revoked_at ON devices(revoked_at) WHERE revoked_at IS NOT NULL;

-- migrate:down
DROP TABLE IF EXISTS devices;
DROP TABLE IF EXISTS cli_auth_requests;
DROP INDEX IF EXISTS idx_oauth_requests_created_at;
ALTER TABLE oauth_requests DROP COLUMN created_at;
