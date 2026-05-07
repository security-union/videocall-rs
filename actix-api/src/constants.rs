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
pub const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs(3);

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

/// Bounded channel capacity for WebTransport outbound relay queue.
///
/// Sized for MTU-limited datagrams (~1200 bytes) and small stream
/// messages. 256 slots provides headroom for bursty traffic.
pub const WT_OUTBOUND_CHANNEL_CAPACITY: usize = 256;

/// Bounded channel capacity for WebSocket outbound relay queue.
///
/// Half the WebTransport capacity because WS frames are larger
/// (full frames vs MTU-limited datagrams). 128 slots at ~50KB avg
/// provides ~6.4MB max queue depth.
pub const WS_OUTBOUND_CHANNEL_CAPACITY: usize = 128;

// ---------------------------------------------------------------------------
// KEYFRAME_REQUEST Rate Limiting
// ---------------------------------------------------------------------------

/// Maximum number of KEYFRAME_REQUEST packets allowed per receiver session
/// within [`KEYFRAME_REQUEST_WINDOW_MS`], **across all target senders**.
///
/// This is a coarse, defense-in-depth ceiling that prevents a malicious or
/// malfunctioning client from issuing an unbounded fan-out of requests in a
/// short window. Per-target throttling is enforced separately by
/// [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`]. This global cap is sized to
/// allow a fresh joiner to request keyframes from many existing senders
/// simultaneously without being clipped (legitimate behaviour during the
/// first second after joining a populated room) while still bounding abuse.
pub const KEYFRAME_REQUEST_MAX_PER_SEC: u32 = 32;

/// Maximum number of KEYFRAME_REQUEST packets allowed per
/// `(receiver, target_sender)` pair within [`KEYFRAME_REQUEST_WINDOW_MS`].
///
/// Sized to 1/sec because a healthy decoder should at most need a single
/// keyframe per second per remote stream. The global per-receiver cap above
/// still applies as a safety net. Per-pair limiting is what fixes the
/// frozen-video-on-join bug observed in cc7tp on 2026-05-06: with the prior
/// global-only limiter at 2/sec, a fresh joiner into a 17-peer meeting could
/// only get keyframes for the first 2 of the 16 existing senders.
pub const KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER: u32 = 1;

/// Time window (in milliseconds) for KEYFRAME_REQUEST rate limiting.
pub const KEYFRAME_REQUEST_WINDOW_MS: u64 = 1000;

/// Stale-entry cleanup interval for the per-pair KEYFRAME_REQUEST limiter.
///
/// Cleanup runs every N requests (where N = this value) to amortize the
/// O(n) `retain()` cost. Mirrors the strategy used by `CongestionTracker`.
pub const KEYFRAME_LIMITER_CLEANUP_INTERVAL: u32 = 64;
