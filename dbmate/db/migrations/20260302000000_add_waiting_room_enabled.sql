-- migrate:up
ALTER TABLE meetings ADD COLUMN waiting_room_enabled BOOLEAN NOT NULL DEFAULT TRUE;

-- migrate:down
ALTER TABLE meetings DROP COLUMN waiting_room_enabled;
