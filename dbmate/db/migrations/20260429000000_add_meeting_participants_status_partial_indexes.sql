-- migrate:up
-- Partial composite indexes for the home-page feed query and per-meeting
-- waiting-count helpers.
--
-- The feed endpoint (`db_meetings::list_feed_for_user`) folds participant
-- counts via two LEFT JOIN LATERAL subqueries:
--
--     SELECT COUNT(*) FROM meeting_participants
--     WHERE meeting_id = m.id AND status = 'admitted';
--     SELECT COUNT(*) FROM meeting_participants
--     WHERE meeting_id = m.id AND status = 'waiting';
--
-- The existing single-column indexes (`idx_meeting_participants_meeting_id`,
-- `idx_meeting_participants_status`) force the planner to either hit the
-- meeting-id index and filter rows in memory by status, or hit the status
-- index (low selectivity for `admitted`) and filter by meeting. Worst-case
-- is O(participants_for_meeting) heap touches per outer row — at 200 outer
-- rows × 100 participants that's 40k heap touches per /feed call (~30-80 ms
-- cold cache).
--
-- These partial composite indexes give the planner an exact-match path:
-- index lookup keyed on `meeting_id`, restricted to the relevant status,
-- so the count is a single index range scan with no heap fetch.
--
-- The "waiting" partial index also benefits per-meeting `count_waiting`
-- calls in `get_meeting`, `update_meeting`, and `end_meeting_handler`.
CREATE INDEX IF NOT EXISTS idx_meeting_participants_meeting_id_admitted
    ON meeting_participants (meeting_id) WHERE status = 'admitted';

CREATE INDEX IF NOT EXISTS idx_meeting_participants_meeting_id_waiting
    ON meeting_participants (meeting_id) WHERE status = 'waiting';

-- migrate:down
DROP INDEX IF EXISTS idx_meeting_participants_meeting_id_admitted;
DROP INDEX IF EXISTS idx_meeting_participants_meeting_id_waiting;
