-- migrate:up

-- One-time codes of approved CLI logins (videocall-cli `auth login`,
-- authorization code + PKCE). Inserted when the signed-in user approves
-- (POST /api/v1/devices/authorizations), deleted when exchanged at
-- POST /api/v1/devices. Only the SHA-256 of the code is stored. A user has at
-- most one row: approving deletes the user's previous row and every expired
-- row, so no sweeper is needed.
CREATE TABLE device_auth_codes (
    code_hash                   TEXT PRIMARY KEY CHECK (length(code_hash) = 64),
    user_id                     TEXT NOT NULL,
    user_name                   TEXT NOT NULL,
    public_key                  TEXT NOT NULL CHECK (length(public_key) = 43),
    device_name                 TEXT NOT NULL CHECK (length(device_name) BETWEEN 1 AND 64),
    code_challenge              TEXT NOT NULL CHECK (length(code_challenge) BETWEEN 43 AND 128),
    -- The canonical loopback URI or the remote constant.
    redirect_uri                TEXT NOT NULL,
    expires_at                  TIMESTAMPTZ NOT NULL
);
CREATE INDEX idx_device_auth_codes_user_id ON device_auth_codes(user_id);
CREATE INDEX idx_device_auth_codes_expires_at ON device_auth_codes(expires_at);

-- Approved CLI devices. One row per Ed25519 public key; it exists only once
-- a human approved the login and the CLI proved possession of the key. The
-- device signs a short-lived EdDSA JWT whose `kid` is `id`. No secret is
-- stored. Revoked and expired rows are kept; the device list shows only
-- live devices.
CREATE TABLE devices (
    id                          TEXT PRIMARY KEY CHECK (length(id) = 22),
    public_key                  TEXT NOT NULL UNIQUE CHECK (length(public_key) = 43),
    fingerprint                 TEXT NOT NULL CHECK (length(fingerprint) = 19),
    name                        TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    user_id                     TEXT NOT NULL,
    user_name                   TEXT NOT NULL,
    created_at                  TIMESTAMPTZ NOT NULL,
    last_used_at                TIMESTAMPTZ NOT NULL,
    idle_expires_at             TIMESTAMPTZ NOT NULL,
    abs_expires_at              TIMESTAMPTZ NOT NULL,
    revoked_at                  TIMESTAMPTZ,
    revoked_reason              TEXT CHECK (revoked_reason IS NULL OR revoked_reason IN ('user','device')),
    -- The idle expiry slides but never past the absolute one.
    CHECK (idle_expires_at <= abs_expires_at)
);
CREATE INDEX idx_devices_user_live ON devices(user_id) WHERE revoked_at IS NULL;

-- migrate:down
DROP TABLE IF EXISTS devices;
DROP TABLE IF EXISTS device_auth_codes;
