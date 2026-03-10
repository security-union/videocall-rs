-- migrate:up
-- Identity Cleanup Phase 1: rename the "email" column to "user_id" in
-- meeting_participants. The column already stores an opaque user identifier
-- (the JWT `sub` claim), not necessarily an email address.

ALTER TABLE meeting_participants RENAME COLUMN email TO user_id;

-- Rename the unique constraint and index to match the new column name.
ALTER INDEX idx_meeting_participants_email RENAME TO idx_meeting_participants_user_id;
ALTER TABLE meeting_participants RENAME CONSTRAINT uq_meeting_participant TO uq_meeting_participant_user;

-- migrate:down
ALTER TABLE meeting_participants RENAME CONSTRAINT uq_meeting_participant_user TO uq_meeting_participant;
ALTER INDEX idx_meeting_participants_user_id RENAME TO idx_meeting_participants_email;
ALTER TABLE meeting_participants RENAME COLUMN user_id TO email;
