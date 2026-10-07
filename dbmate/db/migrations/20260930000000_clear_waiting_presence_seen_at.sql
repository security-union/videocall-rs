-- migrate:up
UPDATE meeting_participants
SET presence_seen_at = NULL
WHERE status = 'waiting' AND presence_seen_at IS NOT NULL;

-- migrate:down
