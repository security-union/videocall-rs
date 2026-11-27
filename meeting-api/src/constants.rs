/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

use lazy_static::lazy_static;
use std::env;

lazy_static! {
    // Core, see `Config::from_env`.
    pub static ref DATABASE_URL: Option<String> = env::var("DATABASE_URL").ok();
    pub static ref JWT_SECRET: Option<String> = env::var("JWT_SECRET").ok();
    pub static ref LISTEN_ADDR: Option<String> = env::var("LISTEN_ADDR").ok();
    pub static ref TOKEN_TTL_SECS: Option<String> = env::var("TOKEN_TTL_SECS").ok();
    pub static ref SESSION_TTL_SECS: Option<String> = env::var("SESSION_TTL_SECS").ok();
    pub static ref COOKIE_DOMAIN: Option<String> = env::var("COOKIE_DOMAIN").ok();
    pub static ref COOKIE_NAME: Option<String> = env::var("COOKIE_NAME").ok();
    pub static ref COOKIE_SECURE: Option<String> = env::var("COOKIE_SECURE").ok();
    pub static ref CORS_ALLOWED_ORIGIN: Option<String> = env::var("CORS_ALLOWED_ORIGIN").ok();
    pub static ref NATS_URL: Option<String> = env::var("NATS_URL").ok();
    pub static ref SERVICE_VERSION_URLS: Option<String> = env::var("SERVICE_VERSION_URLS").ok();

    // OAuth, see `OAuthConfig`.
    pub static ref OAUTH_CLIENT_ID: Option<String> = env::var("OAUTH_CLIENT_ID").ok();
    pub static ref OAUTH_SECRET: Option<String> = env::var("OAUTH_SECRET").ok();
    pub static ref OAUTH_ISSUER: Option<String> = env::var("OAUTH_ISSUER").ok();
    pub static ref OAUTH_AUTH_URL: Option<String> = env::var("OAUTH_AUTH_URL").ok();
    pub static ref OAUTH_TOKEN_URL: Option<String> = env::var("OAUTH_TOKEN_URL").ok();
    pub static ref OAUTH_JWKS_URL: Option<String> = env::var("OAUTH_JWKS_URL").ok();
    pub static ref OAUTH_USERINFO_URL: Option<String> = env::var("OAUTH_USERINFO_URL").ok();
    pub static ref OAUTH_SCOPES: Option<String> = env::var("OAUTH_SCOPES").ok();
    pub static ref OAUTH_REDIRECT_URL: Option<String> = env::var("OAUTH_REDIRECT_URL").ok();
    pub static ref AFTER_LOGIN_URL: Option<String> = env::var("AFTER_LOGIN_URL").ok();
    pub static ref ALLOWED_REDIRECT_URLS: Option<String> = env::var("ALLOWED_REDIRECT_URLS").ok();

    // CLI device sign-in, see `DeviceAuthConfig`.
    pub static ref DEVICE_TOKEN_AUDIENCE: Option<String> = env::var("DEVICE_TOKEN_AUDIENCE").ok();
}
