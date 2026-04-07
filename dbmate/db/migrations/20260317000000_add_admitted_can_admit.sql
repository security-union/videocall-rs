-- migrate:up
ALTER TABLE meetings ADD COLUMN admitted_can_admit BOOLEAN NOT NULL DEFAULT FALSE;

-- migrate:down
ALTER TABLE meetings DROP COLUMN admitted_can_admit;
