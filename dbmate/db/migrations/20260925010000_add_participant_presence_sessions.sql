-- migrate:up
ALTER TABLE meeting_participants
    ADD COLUMN IF NOT EXISTS live_session_id BIGINT,
    ADD COLUMN IF NOT EXISTS left_session_ids BIGINT[] NOT NULL DEFAULT '{}';

ALTER TABLE meeting_participants ALTER COLUMN live_session_id SET DEFAULT 0;

COMMENT ON COLUMN meeting_participants.live_session_id IS
    'Relay session last reported present for this participant (issue #2702). '
    '0: none since the last REST join, admit or leave, so no transport departure applies. '
    'NULL: row predates presence tracking, so any transport departure applies.';

COMMENT ON COLUMN meeting_participants.left_session_ids IS
    'Relay sessions most recently reported left, newest first, at most 8. '
    'A presence report for one of them arrived late and is ignored.';

-- migrate:down
ALTER TABLE meeting_participants
    DROP COLUMN IF EXISTS left_session_ids,
    DROP COLUMN IF EXISTS live_session_id;
