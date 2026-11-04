-- migrate:up
-- Per-user avatars (issue #3125).
CREATE TABLE IF NOT EXISTS user_avatars (
    user_id       VARCHAR(255) PRIMARY KEY CHECK (user_id NOT LIKE 'guest:%'),
    source        TEXT NOT NULL CHECK (source IN ('upload', 'gravatar')),
    content_type  TEXT NOT NULL CHECK (content_type IN ('image/jpeg', 'image/png', 'image/webp')),
    image         BYTEA NOT NULL CHECK (octet_length(image) BETWEEN 1 AND 65536),
    gravatar_hash TEXT CHECK (gravatar_hash ~ '^([0-9a-f]{32}|[0-9a-f]{64})$'),
    etag          TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT chk_user_avatars_gravatar_hash CHECK ((source = 'gravatar') = (gravatar_hash IS NOT NULL))
);

ALTER TABLE user_avatars ALTER COLUMN image SET STORAGE EXTERNAL;

CREATE TRIGGER update_user_avatars_updated_at
BEFORE UPDATE ON user_avatars
FOR EACH ROW
EXECUTE FUNCTION update_updated_at_column();

-- migrate:down
DROP TRIGGER IF EXISTS update_user_avatars_updated_at ON user_avatars;
DROP TABLE IF EXISTS user_avatars;
