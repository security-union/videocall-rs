-- migrate:up
-- The primary key (meeting_id, user_id) cannot serve a user_id-only lookup
-- (it is not the leading column). Issue #2702 round 11 adds a co-host
-- membership branch to db_meetings::list_by_owner / search_by_owner (the
-- "My Meetings" list and its search modal), needing exactly that reverse
-- lookup: "which meetings is this user a live co-host of". Verified via
-- EXPLAIN ANALYZE: those queries' plain WHERE ... OR EXISTS lets Postgres
-- hoist the EXISTS into one hashed sub-plan per request, built by reading
-- every live row for that user from meeting_co_hosts — without this index
-- that read is a sequential scan of the whole table. (The home-page feed's
-- list_feed_for_user does NOT benefit: its LATERAL join forces a per-row
-- nested loop that the existing primary key already serves.) A partial
-- index on the live (unsuspended) rows keeps the hash-build bounded by the
-- caller's own (typically few) entries as co-host designations accumulate
-- platform-wide.
CREATE INDEX IF NOT EXISTS meeting_co_hosts_user_id_idx
    ON meeting_co_hosts (user_id)
    WHERE NOT suspended;

-- migrate:down
DROP INDEX IF EXISTS meeting_co_hosts_user_id_idx;
