-- migrate:up
-- Add a user-configurable display name column to the users table.
--
-- `preferred_display_name` stores the display name the user explicitly chose
-- inside a meeting (distinct from `name` which comes from the OAuth provider).
-- It is initialised to the provider name on first login and can later be
-- updated when the user joins a meeting with a custom display name.
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS preferred_display_name TEXT;

-- Backfill: copy the provider name for existing rows.
UPDATE users
SET preferred_display_name = name
WHERE preferred_display_name IS NULL AND name IS NOT NULL;

-- migrate:down
ALTER TABLE users
    DROP COLUMN IF EXISTS preferred_display_name;
