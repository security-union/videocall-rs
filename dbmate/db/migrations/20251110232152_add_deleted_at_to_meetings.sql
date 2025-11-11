-- migrate:up
ALTER TABLE meetings ADD COLUMN deleted_at TIMESTAMPTZ DEFAULT NULL;

-- migrate:down
ALTER TABLE meetings DROP COLUMN deleted_at;
