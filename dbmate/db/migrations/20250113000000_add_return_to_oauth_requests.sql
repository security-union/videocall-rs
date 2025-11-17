-- migrate:up
ALTER TABLE oauth_requests ADD COLUMN return_to TEXT;

-- migrate:down
ALTER TABLE oauth_requests DROP COLUMN return_to;

