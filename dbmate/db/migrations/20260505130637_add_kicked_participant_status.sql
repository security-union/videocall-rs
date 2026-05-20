-- migrate:up
ALTER TABLE meeting_participants DROP CONSTRAINT chk_participant_status;
ALTER TABLE meeting_participants ADD CONSTRAINT chk_participant_status
    CHECK (status IN ('waiting','admitted','rejected','left','kicked'));

-- migrate:down
ALTER TABLE meeting_participants DROP CONSTRAINT chk_participant_status;
ALTER TABLE meeting_participants ADD CONSTRAINT chk_participant_status
    CHECK (status IN ('waiting','admitted','rejected','left'));
