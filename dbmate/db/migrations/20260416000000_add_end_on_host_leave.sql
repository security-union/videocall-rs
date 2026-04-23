-- migrate:up
ALTER TABLE meetings ADD COLUMN end_on_host_leave BOOLEAN NOT NULL DEFAULT TRUE;

-- migrate:down
ALTER TABLE meetings DROP COLUMN end_on_host_leave;
