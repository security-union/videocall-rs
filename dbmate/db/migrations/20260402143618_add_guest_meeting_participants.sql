-- migrate:up
ALTER TABLE meetings ADD COLUMN allow_guests BOOLEAN NOT NULL DEFAULT FALSE;

-- migrate:down
ALTER TABLE meetings DROP COLUMN allow_guests;
