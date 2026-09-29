-- migrate:up
-- Co-hosts designated by a meeting's owner (issue #2702).
CREATE TABLE IF NOT EXISTS meeting_co_hosts (
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    user_id VARCHAR(255) NOT NULL,
    persistent BOOLEAN NOT NULL,
    suspended BOOLEAN NOT NULL DEFAULT FALSE,
    added_by VARCHAR(255) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (meeting_id, user_id)
);

CREATE TRIGGER update_meeting_co_hosts_updated_at
BEFORE UPDATE ON meeting_co_hosts
FOR EACH ROW
EXECUTE FUNCTION update_updated_at_column();

-- migrate:down
DROP TRIGGER IF EXISTS update_meeting_co_hosts_updated_at ON meeting_co_hosts;
DROP TABLE IF EXISTS meeting_co_hosts;
