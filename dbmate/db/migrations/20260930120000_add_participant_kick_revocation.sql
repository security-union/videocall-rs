-- migrate:up
ALTER TABLE meeting_participants
    ADD COLUMN IF NOT EXISTS kicked_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS kick_deny_until TIMESTAMPTZ;

COMMENT ON COLUMN meeting_participants.kicked_at IS
    'When a host last kicked this participant, on the database clock (issue #2934). '
    'The relay refuses room tokens issued up to one second after it. Not cleared on rejoin.';

COMMENT ON COLUMN meeting_participants.kick_deny_until IS
    'Until when the relay must keep refusing the room tokens this kick revoked.';

-- migrate:down
ALTER TABLE meeting_participants
    DROP COLUMN IF EXISTS kick_deny_until,
    DROP COLUMN IF EXISTS kicked_at;
