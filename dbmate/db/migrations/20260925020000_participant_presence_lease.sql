-- migrate:up
ALTER TABLE meeting_participants ADD COLUMN IF NOT EXISTS presence_seen_at TIMESTAMPTZ;

ALTER TABLE meeting_participants ALTER COLUMN live_session_id DROP DEFAULT;

COMMENT ON COLUMN meeting_participants.live_session_id IS
    'Relay session last reported present for this participant (issue #2702). '
    '0: none since the last REST join, admit or leave, so no transport departure applies. '
    'NULL: written by a meeting-api that predates presence tracking, so any transport departure applies.';

COMMENT ON COLUMN meeting_participants.presence_seen_at IS
    'When a relay last reported this participant''s session present. Presence is a lease on it.';

-- Backs the presence sweep's own scan predicate, which otherwise degrades to
-- a sequential scan as meeting history accumulates. Trade-off: this column is
-- also what every heartbeat renewal writes, so it costs the Postgres HOT
-- update fast path on that write.
CREATE INDEX IF NOT EXISTS meeting_participants_presence_sweep_idx
    ON meeting_participants (presence_seen_at)
    WHERE status = 'admitted' AND left_at IS NULL;

-- Singleton: the newest presence heartbeat batch any replica applied. Stale
-- or missing degrades every presence decision to latch semantics — see
-- db::participants::presence_healthy and docs/MEETING_API.md.
CREATE TABLE IF NOT EXISTS presence_heartbeat_watermark (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE,
    updated_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT presence_heartbeat_watermark_singleton CHECK (id)
);

-- Retire rows only for meetings that actually ENDED — an `idle` meeting under
-- the old per-binary set_idle can still hold a participant genuinely present
-- via a different relay replica.
UPDATE meeting_participants mp
SET status = 'left', left_at = NOW()
FROM meetings m
WHERE m.id = mp.meeting_id
  AND mp.status = 'admitted' AND mp.left_at IS NULL
  AND m.state = 'ended';

-- Every admitted, non-left row of an `active` OR `idle` meeting gets one
-- lease to be confirmed by the next heartbeat/sweep cycle, for the same
-- reason.
UPDATE meeting_participants mp
SET presence_seen_at = NOW()
FROM meetings m
WHERE m.id = mp.meeting_id
  AND mp.status = 'admitted' AND mp.left_at IS NULL
  AND mp.presence_seen_at IS NULL
  AND m.state IN ('active', 'idle');

-- migrate:down
DROP TABLE IF EXISTS presence_heartbeat_watermark;
DROP INDEX IF EXISTS meeting_participants_presence_sweep_idx;
ALTER TABLE meeting_participants DROP COLUMN IF EXISTS presence_seen_at;
ALTER TABLE meeting_participants ALTER COLUMN live_session_id SET DEFAULT 0;
