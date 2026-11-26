-- migrate:up

-- Device-key auth (SQLite mirror of dbmate/db/migrations/20261125000000_device_auth.sql).
-- Timestamps are RFC3339 TEXT, always bound from the application, so they sort
-- consistently with sqlx-encoded DateTime<Utc> values.

-- One-time codes of approved CLI logins (videocall-cli `auth login`,
-- authorization code + PKCE). Inserted when the signed-in user approves
-- (POST /api/v1/devices/authorizations), deleted when exchanged at
-- POST /api/v1/devices. Only the SHA-256 of the code is stored. A user has at
-- most one row: approving deletes the user's previous row and every expired
-- row, so no sweeper is needed.
CREATE TABLE IF NOT EXISTS device_auth_codes (
    code_hash                   TEXT PRIMARY KEY CHECK (length(code_hash) = 64),
    user_id                     TEXT NOT NULL,
    user_name                   TEXT NOT NULL,
    public_key                  TEXT NOT NULL CHECK (length(public_key) = 43),
    device_name                 TEXT NOT NULL CHECK (length(device_name) BETWEEN 1 AND 64),
    code_challenge              TEXT NOT NULL CHECK (length(code_challenge) BETWEEN 43 AND 128),
    -- The canonical loopback URI or the remote constant.
    redirect_uri                TEXT NOT NULL,
    expires_at                  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_device_auth_codes_user_id ON device_auth_codes(user_id);
CREATE INDEX IF NOT EXISTS idx_device_auth_codes_expires_at ON device_auth_codes(expires_at);

-- Approved CLI devices. One row per Ed25519 public key; it exists only once
-- a human approved the login and the CLI proved possession of the key. The
-- device signs a short-lived EdDSA JWT whose `kid` is `id`. No secret is
-- stored. Revoked and expired rows are kept; the device list shows only
-- live devices.
CREATE TABLE IF NOT EXISTS devices (
    id                          TEXT PRIMARY KEY CHECK (length(id) = 22),
    public_key                  TEXT NOT NULL UNIQUE CHECK (length(public_key) = 43),
    fingerprint                 TEXT NOT NULL CHECK (length(fingerprint) = 19),
    name                        TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    user_id                     TEXT NOT NULL,
    user_name                   TEXT NOT NULL,
    created_at                  TEXT NOT NULL,
    last_used_at                TEXT NOT NULL,
    idle_expires_at             TEXT NOT NULL,
    abs_expires_at              TEXT NOT NULL,
    revoked_at                  TEXT,
    revoked_reason              TEXT CHECK (revoked_reason IS NULL OR revoked_reason IN ('user','device')),
    -- The idle expiry slides but never past the absolute one.
    CHECK (idle_expires_at <= abs_expires_at)
);
CREATE INDEX IF NOT EXISTS idx_devices_user_live ON devices(user_id) WHERE revoked_at IS NULL;

-- migrate:down
DROP TABLE IF EXISTS devices;
DROP TABLE IF EXISTS device_auth_codes;
