-- migrate:up

-- See the Postgres migration of the same name.
DELETE FROM oauth_requests WHERE csrf_state IS NULL;
ALTER TABLE oauth_requests ADD COLUMN created_at TEXT;
UPDATE oauth_requests SET created_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
    WHERE created_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_oauth_requests_created_at ON oauth_requests(created_at);
ALTER TABLE oauth_requests ADD COLUMN browser_binding_hash TEXT;

-- migrate:down
DROP INDEX IF EXISTS idx_oauth_requests_created_at;
ALTER TABLE oauth_requests DROP COLUMN browser_binding_hash;
ALTER TABLE oauth_requests DROP COLUMN created_at;
