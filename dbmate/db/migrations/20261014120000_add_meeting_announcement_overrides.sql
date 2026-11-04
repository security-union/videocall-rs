-- migrate:up
SET LOCAL lock_timeout = '5s';
ALTER TABLE meetings
    ADD COLUMN IF NOT EXISTS entry_message_override TEXT NOT NULL DEFAULT 'participant',
    ADD COLUMN IF NOT EXISTS entry_sound_override TEXT NOT NULL DEFAULT 'participant',
    ADD COLUMN IF NOT EXISTS exit_message_override TEXT NOT NULL DEFAULT 'participant',
    ADD COLUMN IF NOT EXISTS exit_sound_override TEXT NOT NULL DEFAULT 'participant';

DO $$
BEGIN
    IF (SELECT count(*) FROM pg_constraint
        WHERE conrelid = 'meetings'::regclass
          AND conname IN ('chk_meetings_entry_message_override',
                          'chk_meetings_entry_sound_override',
                          'chk_meetings_exit_message_override',
                          'chk_meetings_exit_sound_override')) < 4 THEN
        ALTER TABLE meetings
            ADD CONSTRAINT chk_meetings_entry_message_override
                CHECK (entry_message_override IN ('participant', 'on', 'off')),
            ADD CONSTRAINT chk_meetings_entry_sound_override
                CHECK (entry_sound_override IN ('participant', 'on', 'off')),
            ADD CONSTRAINT chk_meetings_exit_message_override
                CHECK (exit_message_override IN ('participant', 'on', 'off')),
            ADD CONSTRAINT chk_meetings_exit_sound_override
                CHECK (exit_sound_override IN ('participant', 'on', 'off'));
    END IF;
END
$$;

-- migrate:down
ALTER TABLE meetings
    DROP COLUMN IF EXISTS exit_sound_override,
    DROP COLUMN IF EXISTS exit_message_override,
    DROP COLUMN IF EXISTS entry_sound_override,
    DROP COLUMN IF EXISTS entry_message_override;
