-- migrate:up

-- Consolidated SQLite schema, equivalent to the PostgreSQL migration set in
-- `dbmate/db/migrations/` through 20260307000001_rename_email_to_user_id.
--
-- dbmate has no dialect layer, so this file is the one place where duplication
-- between the two backends is unavoidable. It is deliberately named after the
-- last PostgreSQL migration it covers, so any new migration ported here sorts
-- after it. When you add a PostgreSQL migration, add the matching SQLite one.
--
-- Deviations from a literal transliteration, all deliberate:
--
--  * `session_participants` is omitted. It has no Rust references.
--  * Timestamps are TEXT holding RFC 3339. Where PostgreSQL writes `NOW()`,
--    the SQLite build binds `chrono::Utc::now()` (see `db::now_expr`) rather
--    than writing `datetime('now')`, whose `2026-07-21 04:39:04` rendering
--    would not sort lexicographically against RFC 3339 in the same column and
--    would break `ORDER BY created_at DESC`. The DEFAULTs below emit RFC 3339
--    for that same reason, so a DEFAULT and a bound value stay comparable.
--  * There are no `updated_at` triggers. SQLite evaluates `RETURNING` *before*
--    AFTER-triggers run, so an `UPDATE ... RETURNING updated_at` driven by a
--    trigger returns the stale value. Every UPDATE sets `updated_at`
--    explicitly instead. PostgreSQL keeps its BEFORE trigger and overwrites
--    that explicit write with the same `transaction_timestamp()`, so the two
--    backends agree without the SQL diverging.
--  * `VARCHAR(n)` becomes `TEXT` plus a `CHECK (length(col) <= n)`, because
--    SQLite ignores type-name length limits entirely.

-- OAuth request storage for in-flight PKCE/CSRF flows.
CREATE TABLE oauth_requests (
    pkce_challenge TEXT,
    pkce_verifier  TEXT,
    csrf_state     TEXT,
    return_to      TEXT,
    nonce          TEXT CHECK (nonce IS NULL OR length(nonce) <= 255)
);

-- User accounts with OAuth tokens.
-- `created_at` / `last_login` mirror PostgreSQL `TIMESTAMP` (no time zone) and
-- are bound as naive datetimes, hence the space-separated DEFAULT.
CREATE TABLE users (
    email         TEXT PRIMARY KEY CHECK (length(email) <= 255),
    access_token  TEXT,
    refresh_token TEXT,
    name          TEXT,
    created_at    TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now')),
    last_login    TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
);

CREATE TABLE meetings (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id              TEXT NOT NULL CHECK (length(room_id) <= 255),
    started_at           TEXT NOT NULL,
    ended_at             TEXT,
    created_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    deleted_at           TEXT,
    creator_id           TEXT CHECK (creator_id IS NULL OR length(creator_id) <= 255),
    password_hash        TEXT CHECK (password_hash IS NULL OR length(password_hash) <= 255),
    state                TEXT NOT NULL DEFAULT 'idle'
                         CHECK (state IN ('idle', 'active', 'ended')),
    -- `json_type(...) = 'array'` is not redundant: json_array_length returns 0
    -- for a JSON object, so without it `{"a":1}` would pass here while
    -- PostgreSQL's jsonb_array_length raises.
    attendees            TEXT NOT NULL DEFAULT '[]'
                         CHECK (json_type(attendees) = 'array'
                                AND json_array_length(attendees) <= 100),
    host_display_name    TEXT CHECK (host_display_name IS NULL OR length(host_display_name) <= 255),
    waiting_room_enabled BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE INDEX idx_meetings_room_id ON meetings(room_id);
CREATE INDEX idx_meetings_creator_id ON meetings(creator_id);
CREATE INDEX idx_meetings_state ON meetings(state);

-- Partial unique index: a room_id may be reused once the meeting is soft-deleted.
CREATE UNIQUE INDEX idx_meetings_room_id_unique_active
    ON meetings(room_id)
    WHERE deleted_at IS NULL;

-- Meeting participants: waiting room and admission tracking.
-- The ON DELETE CASCADE is inert unless `PRAGMA foreign_keys = ON`, which
-- `meeting_api::db::connect` sets via SqliteConnectOptions::foreign_keys on
-- every pooled connection.
CREATE TABLE meeting_participants (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    meeting_id   INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    user_id      TEXT NOT NULL CHECK (length(user_id) <= 255),
    status       TEXT NOT NULL DEFAULT 'waiting'
                 CHECK (status IN ('waiting', 'admitted', 'rejected', 'left')),
    is_host      BOOLEAN NOT NULL DEFAULT FALSE,
    is_required  BOOLEAN NOT NULL DEFAULT FALSE,
    joined_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    admitted_at  TEXT,
    left_at      TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    display_name TEXT CHECK (display_name IS NULL OR length(display_name) <= 255),

    CONSTRAINT uq_meeting_participant_user UNIQUE (meeting_id, user_id)
);

CREATE INDEX idx_meeting_participants_meeting_id ON meeting_participants(meeting_id);
CREATE INDEX idx_meeting_participants_user_id ON meeting_participants(user_id);
CREATE INDEX idx_meeting_participants_status ON meeting_participants(status);

-- migrate:down

DROP TABLE meeting_participants;
DROP TABLE meetings;
DROP TABLE users;
DROP TABLE oauth_requests;
