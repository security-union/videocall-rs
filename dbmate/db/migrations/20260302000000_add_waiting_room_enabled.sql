-- migrate:up
-- Column waiting_room_enabled already exists from 20251221011540_add_meeting_protection_fields
-- (added as BOOLEAN DEFAULT FALSE). Ensure it is NOT NULL and defaults to TRUE.
ALTER TABLE meetings ADD COLUMN IF NOT EXISTS waiting_room_enabled BOOLEAN DEFAULT FALSE;
ALTER TABLE meetings ALTER COLUMN waiting_room_enabled SET NOT NULL;
ALTER TABLE meetings ALTER COLUMN waiting_room_enabled SET DEFAULT TRUE;

-- migrate:down
ALTER TABLE meetings ALTER COLUMN waiting_room_enabled DROP NOT NULL;
ALTER TABLE meetings ALTER COLUMN waiting_room_enabled SET DEFAULT FALSE;
