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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use lazy_static::lazy_static;
use std::env;
use std::time::Duration;

/// How often heartbeat pings are sent
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// How long before lack of client response causes a timeout.
/// Set to 30s to tolerate up to 5 missed heartbeat intervals (5s each),
/// reducing false disconnects on flaky networks.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Grace period before broadcasting PARTICIPANT_LEFT after a disconnect.
/// If the same user_id reconnects within this window, the departure is
/// cancelled silently — no PARTICIPANT_LEFT or PARTICIPANT_JOINED is
/// broadcast, avoiding false join/leave notification spam.
pub const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs(2);

/// Regex pattern for validating usernames and room IDs
/// Allows alphanumeric characters, underscores, and hyphens
pub const VALID_ID_PATTERN: &str = "^[a-zA-Z0-9_-]*$";

/// Maximum incoming frame/stream size in bytes for both WebSocket and WebTransport.
///
/// 4 MB accommodates worst-case 1080p VP9 keyframes (1-2 MB raw) plus protobuf
/// wrapping overhead. The previous 1 MB limit caused session termination when a
/// participant shared a high-quality 1080p screen, because VP9 keyframes exceeded
/// the cap and triggered a protocol error that closed the entire connection.
pub const MAX_FRAME_SIZE: usize = 4_000_000;

// ---------------------------------------------------------------------------
// Server Congestion Feedback
// ---------------------------------------------------------------------------

/// Number of dropped outbound packets within [`CONGESTION_WINDOW`] that triggers
/// a CONGESTION notification back to the sender whose packets are being dropped.
pub const CONGESTION_DROP_THRESHOLD: u32 = 5;

/// Time window over which drops are counted. Drop counters reset after this
/// window elapses without new drops.
pub const CONGESTION_WINDOW: Duration = Duration::from_millis(1000);

/// Minimum interval between CONGESTION notifications sent to the same sender
/// session. Prevents flooding the sender with congestion signals when many
/// packets are dropped in quick succession.
pub const CONGESTION_NOTIFY_MIN_INTERVAL: Duration = Duration::from_millis(1000);

// ---------------------------------------------------------------------------
// KEYFRAME_REQUEST Rate Limiting
// ---------------------------------------------------------------------------

/// Maximum number of KEYFRAME_REQUEST packets allowed per session within
/// [`KEYFRAME_REQUEST_WINDOW_MS`]. Excess requests are silently dropped to
/// prevent a malicious client from forcing senders to continuously generate
/// expensive keyframes (5-10x larger than delta frames).
pub const KEYFRAME_REQUEST_MAX_PER_SEC: u32 = 2;

/// Time window (in milliseconds) for KEYFRAME_REQUEST rate limiting.
pub const KEYFRAME_REQUEST_WINDOW_MS: u64 = 1000;

/// Default server stats reporting interval in seconds
pub const DEFAULT_REPORTING_INTERVAL_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

lazy_static! {
    // Startup: the server binaries read these in `main`.
    pub static ref NATS_URL: Option<String> = env::var("NATS_URL").ok();
    pub static ref ACTIX_PORT: String =
        env::var("ACTIX_PORT").unwrap_or_else(|_| String::from("8080"));
    pub static ref METRICS_PORT: String =
        env::var("METRICS_PORT").unwrap_or_else(|_| "9091".to_string());
    pub static ref HEALTH_LISTEN_URL: Option<String> = env::var("HEALTH_LISTEN_URL").ok();
    pub static ref LISTEN_URL: Option<String> = env::var("LISTEN_URL").ok();
    pub static ref KEY_PATH: Option<String> = env::var("KEY_PATH").ok();
    pub static ref CERT_PATH: Option<String> = env::var("CERT_PATH").ok();
    pub static ref DATABASE_ENABLED: String =
        env::var("DATABASE_ENABLED").unwrap_or_else(|_| String::from("false"));
    pub(crate) static ref DATABASE_URL: Option<String> = env::var("DATABASE_URL").ok();

    // Legacy OAuth and cookies on the websocket server.
    pub static ref OAUTH_CLIENT_ID: String =
        env::var("OAUTH_CLIENT_ID").unwrap_or_else(|_| String::from(""));
    pub static ref OAUTH_AUTH_URL: String =
        env::var("OAUTH_AUTH_URL").unwrap_or_else(|_| String::from(""));
    pub static ref OAUTH_TOKEN_URL: String =
        env::var("OAUTH_TOKEN_URL").unwrap_or_else(|_| String::from(""));
    pub static ref OAUTH_SECRET: String =
        env::var("OAUTH_SECRET").unwrap_or_else(|_| String::from(""));
    pub static ref OAUTH_REDIRECT_URL: String =
        env::var("OAUTH_REDIRECT_URL").unwrap_or_else(|_| String::from(""));
    pub static ref UI_ENDPOINT: String =
        env::var("UI_ENDPOINT").unwrap_or_else(|_| String::from("/"));
    pub static ref COOKIE_DOMAIN: Option<String> = env::var("COOKIE_DOMAIN").ok();

    // Room token verification key; empty means unset.
    pub(crate) static ref JWT_SECRET: String = env::var("JWT_SECRET").unwrap_or_default();

    // Diagnostics identity and reporting.
    pub(crate) static ref REGION: String = env::var("REGION").unwrap_or_else(|_| "us-east".to_string());
    pub(crate) static ref SERVICE_TYPE: String =
        env::var("SERVICE_TYPE").unwrap_or_else(|_| "websocket".to_string());
    pub(crate) static ref SERVER_ID: Option<String> = env::var("SERVER_ID").ok();
    pub(crate) static ref HOSTNAME: Option<String> = env::var("HOSTNAME").ok();
    pub(crate) static ref SERVER_STATS_INTERVAL_SECS: u64 = env::var("SERVER_STATS_INTERVAL_SECS")
        .unwrap_or_else(|_| DEFAULT_REPORTING_INTERVAL_SECS.to_string())
        .parse::<u64>()
        .unwrap_or(DEFAULT_REPORTING_INTERVAL_SECS);

    // QUIC transport tuning for WebTransport.
    pub(crate) static ref QUIC_MAX_IDLE_TIMEOUT_SECS: u64 = env::var("QUIC_MAX_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    pub(crate) static ref QUIC_KEEP_ALIVE_INTERVAL_SECS: u64 = env::var("QUIC_KEEP_ALIVE_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
}
