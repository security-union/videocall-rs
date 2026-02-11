-- migrate:up
-- Add host display name to meetings table so we can identify the host by their chosen username

ALTER TABLE meetings ADD COLUMN host_display_name VARCHAR(255) NULL;

-- Add display_name to meeting_participants to track each user's chosen username
ALTER TABLE meeting_participants ADD COLUMN display_name VARCHAR(255) NULL;

-- migrate:down
ALTER TABLE meeting_participants DROP COLUMN IF EXISTS display_name;
ALTER TABLE meetings DROP COLUMN IF EXISTS host_display_name;
