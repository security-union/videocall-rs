-- migrate:up
ALTER TABLE oauth_requests ADD COLUMN nonce VARCHAR(255);

-- migrate:down
ALTER TABLE oauth_requests DROP COLUMN IF EXISTS nonce;
