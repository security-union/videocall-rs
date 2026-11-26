-- migrate:up

-- Fail fast instead of queueing every /login behind an ACCESS EXCLUSIVE lock
-- on oauth_requests if a long transaction holds it.
SET LOCAL lock_timeout = '5s';

-- oauth_requests: age tracking so abandoned IdP legs can be swept.
ALTER TABLE oauth_requests ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
CREATE INDEX idx_oauth_requests_created_at ON oauth_requests(created_at);
-- csrf_state is a 32-byte random value, so duplicates can only be abandoned
-- rows; drop any (keeping one) so the UNIQUE index cannot fail the deploy.
-- Parity with SQLite, where the column was UNIQUE from the start.
DELETE FROM oauth_requests a
    USING oauth_requests b
    WHERE a.csrf_state = b.csrf_state AND a.ctid < b.ctid;
CREATE UNIQUE INDEX idx_oauth_requests_csrf_state ON oauth_requests(csrf_state);

-- One browser-approved CLI login in flight (videocall-cli `auth login`,
-- authorization code + PKCE). Created by GET /cli/authorize, decided on the
-- consent page, consumed once by POST /api/v1/devices. Only the SHA-256 of
-- the one-time code and of the approval nonce is stored.
CREATE TABLE cli_auth_requests (
    id                          TEXT PRIMARY KEY CHECK (length(id) = 22),
    code_challenge              TEXT NOT NULL CHECK (length(code_challenge) BETWEEN 43 AND 128),
    redirect_kind               TEXT NOT NULL CHECK (redirect_kind IN ('loopback','remote')),
    redirect_host               TEXT CHECK (redirect_host IS NULL OR redirect_host IN ('127.0.0.1','[::1]')),
    redirect_port               INTEGER CHECK (redirect_port IS NULL OR redirect_port BETWEEN 1024 AND 65535),
    client_state                TEXT NOT NULL CHECK (length(client_state) BETWEEN 16 AND 128),
    public_key                  TEXT NOT NULL CHECK (length(public_key) = 43),
    device_name                 TEXT NOT NULL CHECK (length(device_name) BETWEEN 1 AND 64),
    requester_ip                TEXT,
    created_at                  TIMESTAMPTZ NOT NULL,
    expires_at                  TIMESTAMPTZ NOT NULL,
    approval_nonce_hash         TEXT,
    approval_nonce_user_id      TEXT,
    approval_nonce_expires_at   TIMESTAMPTZ,
    decided_at                  TIMESTAMPTZ,
    decision                    TEXT CHECK (decision IS NULL OR decision IN ('approved','denied')),
    user_id                     TEXT,
    user_name                   TEXT,
    code_hash                   TEXT UNIQUE,
    code_expires_at             TIMESTAMPTZ,
    consumed_at                 TIMESTAMPTZ,
    CHECK ((redirect_kind = 'loopback') = (redirect_host IS NOT NULL AND redirect_port IS NOT NULL)),
    CHECK (decision IS NULL OR (decided_at IS NOT NULL AND user_id IS NOT NULL)),
    CHECK (code_hash IS NULL OR (decision = 'approved' AND code_expires_at IS NOT NULL))
);
CREATE INDEX idx_cli_auth_requests_expires_at ON cli_auth_requests(expires_at);
CREATE INDEX idx_cli_auth_requests_ip ON cli_auth_requests(requester_ip, expires_at);

-- Approved CLI devices. One row per Ed25519 public key; it exists only once
-- a human approved the login and the CLI proved possession of the key. The
-- device signs a short-lived EdDSA JWT whose `kid` is `id`. No secret is
-- stored. Revoked and expired rows are kept 30 days for the device list.
CREATE TABLE devices (
    id                          TEXT PRIMARY KEY CHECK (length(id) = 22),
    public_key                  TEXT NOT NULL UNIQUE CHECK (length(public_key) = 43),
    fingerprint                 TEXT NOT NULL CHECK (length(fingerprint) = 19),
    name                        TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    user_id                     TEXT NOT NULL,
    user_name                   TEXT NOT NULL,
    requester_ip                TEXT,
    created_at                  TIMESTAMPTZ NOT NULL,
    decided_at                  TIMESTAMPTZ NOT NULL,
    last_used_at                TIMESTAMPTZ NOT NULL,
    idle_expires_at             TIMESTAMPTZ NOT NULL,
    abs_expires_at              TIMESTAMPTZ NOT NULL,
    revoked_at                  TIMESTAMPTZ,
    revoked_reason              TEXT CHECK (revoked_reason IS NULL OR revoked_reason IN ('user','device'))
);
CREATE INDEX idx_devices_user_live ON devices(user_id) WHERE revoked_at IS NULL;
CREATE INDEX idx_devices_abs_expires ON devices(abs_expires_at);
CREATE INDEX idx_devices_idle_expires ON devices(idle_expires_at);
CREATE INDEX idx_devices_revoked_at ON devices(revoked_at) WHERE revoked_at IS NOT NULL;

-- migrate:down
DROP TABLE IF EXISTS devices;
DROP TABLE IF EXISTS cli_auth_requests;
DROP INDEX IF EXISTS idx_oauth_requests_csrf_state;
DROP INDEX IF EXISTS idx_oauth_requests_created_at;
ALTER TABLE oauth_requests DROP COLUMN IF EXISTS created_at;
