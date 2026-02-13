-- migrate:up
-- Drop the existing unique constraint on room_id
ALTER TABLE meetings DROP CONSTRAINT IF EXISTS meetings_room_id_key;

-- Create a partial unique index that only applies to non-deleted meetings
-- This allows the same room_id to be reused after a meeting is deleted
CREATE UNIQUE INDEX idx_meetings_room_id_unique_active
ON meetings(room_id)
WHERE deleted_at IS NULL;

-- migrate:down
-- Remove the partial unique index
DROP INDEX IF EXISTS idx_meetings_room_id_unique_active;

-- Restore the original unique constraint
ALTER TABLE meetings ADD CONSTRAINT meetings_room_id_key UNIQUE (room_id);
