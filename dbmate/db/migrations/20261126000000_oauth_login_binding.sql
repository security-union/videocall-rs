-- migrate:up

-- Fail fast instead of queueing every /login behind an ACCESS EXCLUSIVE lock
-- on oauth_requests if a long transaction holds it.
SET LOCAL lock_timeout = '5s';

-- A row with no csrf_state can never be matched by a callback.
DELETE FROM oauth_requests WHERE csrf_state IS NULL;
-- csrf_state is random, so duplicates can only be abandoned rows; keep one
-- so the UNIQUE index cannot fail the deploy. SQLite has had UNIQUE from
-- the start.
DELETE FROM oauth_requests a
    USING oauth_requests b
    WHERE a.csrf_state = b.csrf_state AND a.ctid < b.ctid;

-- Age, so abandoned IdP legs expire and are deleted.
ALTER TABLE oauth_requests ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
CREATE INDEX idx_oauth_requests_created_at ON oauth_requests(created_at);
CREATE UNIQUE INDEX idx_oauth_requests_csrf_state ON oauth_requests(csrf_state);
-- SHA-256 of the pre-login cookie set by /login, so /login/callback only
-- completes in the browser that started the login (login CSRF). NULL on
-- rows stored before this migration, which are accepted for their TTL.
ALTER TABLE oauth_requests ADD COLUMN browser_binding_hash TEXT;

-- migrate:down
DROP INDEX IF EXISTS idx_oauth_requests_csrf_state;
DROP INDEX IF EXISTS idx_oauth_requests_created_at;
ALTER TABLE oauth_requests DROP COLUMN IF EXISTS browser_binding_hash;
ALTER TABLE oauth_requests DROP COLUMN IF EXISTS created_at;
