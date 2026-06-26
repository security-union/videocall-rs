-- migrate:up
-- Refine the per-meeting participant-count partial indexes to "present-only"
-- (issue #1551).
--
-- The participant / waiting counts that back the meeting-settings "Activity"
-- card and the home feed are now restricted to participants who are CURRENTLY
-- present — `status = 'admitted'/'waiting' AND left_at IS NULL` — so a
-- participant who left (explicit REST /leave, or a transport disconnect marked
-- by the `internal.participant_left` consumer) is excluded. See
-- `db_participants::count_admitted` / `count_waiting` and the LEFT JOIN LATERAL
-- subqueries in `db_meetings::list_feed_for_user` / `list_joined_by_user`.
--
-- The previous partial indexes keyed on `status` alone still answered these
-- queries, but the planner had to filter `left_at IS NULL` after the index
-- lookup. Folding `left_at IS NULL` into the partial-index predicate keeps the
-- count a single index range scan over only the present rows, and prevents
-- accumulated `left_at IS NOT NULL` ghost rows (long-lived rooms with churn)
-- from bloating the scanned set.
--
-- Idempotent and safe to re-run: drop the old indexes, create the refined ones.
--
-- These are plain (non-CONCURRENTLY) DROP/CREATE INDEX statements, so they take
-- a brief write-blocking lock on `meeting_participants` during the rebuild. That
-- is accepted: the table is small and transient (live/recent participants, not
-- historical data), so the rebuild is near-instant, and `CREATE INDEX
-- CONCURRENTLY` cannot run inside dbmate's default per-migration transaction
-- wrapping. This matches the repo's universal convention — the prior sibling
-- migration (20260429000000_add_meeting_participants_status_partial_indexes)
-- created these same indexes the same non-concurrent way.
DROP INDEX IF EXISTS idx_meeting_participants_meeting_id_admitted;
DROP INDEX IF EXISTS idx_meeting_participants_meeting_id_waiting;

CREATE INDEX IF NOT EXISTS idx_meeting_participants_meeting_id_admitted_present
    ON meeting_participants (meeting_id)
    WHERE status = 'admitted' AND left_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_meeting_participants_meeting_id_waiting_present
    ON meeting_participants (meeting_id)
    WHERE status = 'waiting' AND left_at IS NULL;

-- migrate:down
DROP INDEX IF EXISTS idx_meeting_participants_meeting_id_admitted_present;
DROP INDEX IF EXISTS idx_meeting_participants_meeting_id_waiting_present;

CREATE INDEX IF NOT EXISTS idx_meeting_participants_meeting_id_admitted
    ON meeting_participants (meeting_id) WHERE status = 'admitted';

CREATE INDEX IF NOT EXISTS idx_meeting_participants_meeting_id_waiting
    ON meeting_participants (meeting_id) WHERE status = 'waiting';
