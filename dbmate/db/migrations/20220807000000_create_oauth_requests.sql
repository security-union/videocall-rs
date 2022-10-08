-- migrate:up
CREATE TABLE oauth_requests (
    pkce_challenge TEXT,
    pkce_verifier TEXT,
    csrf_state TEXT
);

CREATE TABLE users (
    email VARCHAR(255) PRIMARY KEY,
    access_token TEXT,
    refresh_token TEXT
);

-- migrate:down
DROP TABLE oauth_requests;
DROP TABLE users;
