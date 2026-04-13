-- migrate:up
ALTER TABLE meetings ADD COLUMN allow_guests BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE meeting_participants ADD COLUMN is_guest BOOLEAN NOT NULL DEFAULT FALSE;

-- migrate:down
ALTER TABLE meetings DROP COLUMN allow_guests;
ALTER TABLE meeting_participants DROP COLUMN is_guest;
