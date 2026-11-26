-- migrate:up

-- Fail fast instead of queueing every /login behind an ACCESS EXCLUSIVE lock
-- on oauth_requests if a long transaction holds it.
SET LOCAL lock_timeout = '5s';

-- oauth_requests cleanup runs first, under ROW EXCLUSIVE only, so the
-- ACCESS EXCLUSIVE lock the ALTER below takes is held over fewer rows.
-- A row with no csrf_state can never be matched by a callback: drop it.
-- (Rows cannot be purged by age here: the table has no timestamp yet.)
DELETE FROM oauth_requests WHERE csrf_state IS NULL;
-- csrf_state is a 32-byte random value, so duplicates can only be abandoned
-- rows; drop any (keeping one) so the UNIQUE index cannot fail the deploy.
-- Parity with SQLite, where the column was UNIQUE from the start.
DELETE FROM oauth_requests a
    USING oauth_requests b
    WHERE a.csrf_state = b.csrf_state AND a.ctid < b.ctid;

-- Age tracking so abandoned IdP legs can be swept. Indexes are built once,
-- over the final rows.
ALTER TABLE oauth_requests ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
CREATE INDEX idx_oauth_requests_created_at ON oauth_requests(created_at);
CREATE UNIQUE INDEX idx_oauth_requests_csrf_state ON oauth_requests(csrf_state);
-- SHA-256 of the pre-login cookie set by /login, so /login/callback only
-- completes in the browser that started the login. NULL on rows created
-- before this migration (accepted for their remaining TTL).
ALTER TABLE oauth_requests ADD COLUMN browser_binding_hash TEXT;

-- One browser-approved CLI login in flight (videocall-cli `auth login`,
-- authorization code + PKCE). Created by GET /cli/authorize, decided on the
-- consent page, consumed once by POST /api/v1/devices. Only the SHA-256 of
-- the one-time code and of the approval nonce is stored.
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
CREATE INDEX idx_cli_auth_requests_cap_key ON cli_auth_requests(requester_cap_key, expires_at)
    WHERE decided_at IS NULL;

-- Approved CLI devices. One row per Ed25519 public key; it exists only once
-- a human approved the login and the CLI proved possession of the key. The
-- device signs a short-lived EdDSA JWT whose `kid` is `id`. No secret is
-- stored. Revoked and expired rows are kept 30 days for audit and support;
-- the device list shows only live devices.
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
    revoked_reason              TEXT CHECK (revoked_reason IS NULL OR revoked_reason IN ('user','device')),
    -- The idle expiry slides but never past the absolute one, so the sweep
    -- and the indexes only need idle_expires_at.
    CHECK (idle_expires_at <= abs_expires_at)
);
CREATE INDEX idx_devices_user_live ON devices(user_id) WHERE revoked_at IS NULL;
CREATE INDEX idx_devices_idle_expires ON devices(idle_expires_at);
CREATE INDEX idx_devices_revoked_at ON devices(revoked_at) WHERE revoked_at IS NOT NULL;

-- migrate:down
DROP TABLE IF EXISTS devices;
DROP TABLE IF EXISTS cli_auth_requests;
DROP INDEX IF EXISTS idx_oauth_requests_csrf_state;
DROP INDEX IF EXISTS idx_oauth_requests_created_at;
ALTER TABLE oauth_requests DROP COLUMN IF EXISTS browser_binding_hash;
ALTER TABLE oauth_requests DROP COLUMN IF EXISTS created_at;
