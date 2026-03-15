-- migrate:up

-- 1. Add UUID primary key to users (currently email is PK)
ALTER TABLE users ADD COLUMN id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE users ADD CONSTRAINT users_uuid_unique UNIQUE (id);

-- 2. Change meetings.creator_id from VARCHAR (email) to UUID
ALTER TABLE meetings ADD COLUMN creator_uuid UUID;
UPDATE meetings m SET creator_uuid = u.id FROM users u WHERE m.creator_id = u.email;
-- Safety check: fail if any meetings reference users not in the users table
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM meetings WHERE creator_uuid IS NULL AND creator_id IS NOT NULL) THEN
    RAISE EXCEPTION 'meetings.creator_uuid has NULLs — orphaned rows reference emails not in users table';
  END IF;
END $$;
ALTER TABLE meetings DROP COLUMN creator_id;
ALTER TABLE meetings RENAME COLUMN creator_uuid TO creator_id;

-- 3. Change meeting_participants.user_id from VARCHAR (email) to UUID
ALTER TABLE meeting_participants DROP CONSTRAINT uq_meeting_participant_user;
ALTER TABLE meeting_participants ADD COLUMN user_uuid UUID;
UPDATE meeting_participants mp SET user_uuid = u.id FROM users u WHERE mp.user_id = u.email;
-- Safety check: fail if any participants reference users not in the users table
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM meeting_participants WHERE user_uuid IS NULL AND user_id IS NOT NULL) THEN
    RAISE EXCEPTION 'meeting_participants.user_uuid has NULLs — orphaned rows reference emails not in users table';
  END IF;
END $$;
ALTER TABLE meeting_participants DROP COLUMN user_id;
ALTER TABLE meeting_participants RENAME COLUMN user_uuid TO user_id;
ALTER TABLE meeting_participants ADD CONSTRAINT uq_meeting_participant_user UNIQUE (meeting_id, user_id);

-- 4. Switch PK on users from email to UUID
ALTER TABLE users DROP CONSTRAINT users_pkey;
ALTER TABLE users ADD CONSTRAINT users_pkey PRIMARY KEY (id);
ALTER TABLE users ADD CONSTRAINT users_email_unique UNIQUE (email);

-- 5. Rebuild indexes
DROP INDEX IF EXISTS idx_meeting_participants_user_id;
CREATE INDEX idx_meeting_participants_user_id ON meeting_participants(user_id);
DROP INDEX IF EXISTS idx_meetings_creator_id;
CREATE INDEX idx_meetings_creator_id ON meetings(creator_id);

-- migrate:down
-- (reverse migration omitted for brevity; this is a one-way migration)
