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

/// Default bounded channel capacity for WebTransport outbound relay queue.
///
/// Sized for MTU-limited datagrams (~1200 bytes) and small stream
/// messages. The previous 256-slot bound was exceeded by ~1.6x during
/// a 17-peer cc7tp meeting on 2026-05-06, producing ~38.5k
/// `Outbound channel full` drops over the meeting (~480 inbound
/// msg/sec/receiver into a 256-slot queue).
///
/// 1024 gives ~4x headroom for typical 17-peer meetings; the value
/// can be raised further at deploy-time via the
/// `WT_OUTBOUND_CHANNEL_CAPACITY` environment variable for larger
/// meetings.
pub const WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT: usize = 1024;

/// Resolve the WebTransport outbound channel capacity from the
/// `WT_OUTBOUND_CHANNEL_CAPACITY` environment variable, falling back
/// to [`WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT`] if unset, unparseable,
/// or zero.
///
/// The lookup is memoised: the env var is read exactly once on the
/// first call. A non-zero `usize` parse yields the env value;
/// unparseable values (e.g. `"abc"`) emit a `warn!` and fall back to
/// the default. A literal `0` is also rejected so the channel is
/// never constructed with zero capacity (which would panic inside
/// `tokio::sync::mpsc::channel`).
pub fn wt_outbound_channel_capacity() -> usize {
    use std::sync::OnceLock;
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        resolve_wt_outbound_channel_capacity(
            std::env::var("WT_OUTBOUND_CHANNEL_CAPACITY")
                .ok()
                .as_deref(),
        )
    })
}

/// Pure resolver for [`wt_outbound_channel_capacity`]: maps the raw
/// optional environment string to the concrete channel capacity,
/// applying the same parse, zero-rejection and warn-on-failure rules
/// without touching any process-global state.
///
/// Extracted as a free function so unit tests can exercise the
/// resolution logic without racing against the `OnceLock` cache or
/// mutating the real process environment.
pub(crate) fn resolve_wt_outbound_channel_capacity(raw: Option<&str>) -> usize {
    match raw {
        Some(value) => match value.parse::<usize>() {
            Ok(0) => {
                tracing::warn!(
                    "WT_OUTBOUND_CHANNEL_CAPACITY=0 is invalid; falling back to default {}",
                    WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
                );
                WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
            }
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse WT_OUTBOUND_CHANNEL_CAPACITY={:?} as usize ({}); falling back to default {}",
                    value, e, WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
                );
                WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
            }
        },
        None => WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT,
    }
}

/// Bounded channel capacity for WebSocket outbound relay queue.
///
/// Half the WebTransport capacity because WS frames are larger
/// (full frames vs MTU-limited datagrams). 128 slots at ~50KB avg
/// provides ~6.4MB max queue depth.
pub const WS_OUTBOUND_CHANNEL_CAPACITY: usize = 128;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_wt_outbound_channel_capacity_unset_uses_default() {
        assert_eq!(
            resolve_wt_outbound_channel_capacity(None),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_valid_value_used_verbatim() {
        assert_eq!(resolve_wt_outbound_channel_capacity(Some("512")), 512);
        assert_eq!(resolve_wt_outbound_channel_capacity(Some("4096")), 4096);
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_garbage_falls_back_to_default() {
        assert_eq!(
            resolve_wt_outbound_channel_capacity(Some("abc")),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_zero_falls_back_to_default() {
        // A literal "0" must be rejected; mpsc::channel(0) panics.
        assert_eq!(
            resolve_wt_outbound_channel_capacity(Some("0")),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_negative_falls_back_to_default() {
        assert_eq!(
            resolve_wt_outbound_channel_capacity(Some("-1")),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_empty_falls_back_to_default() {
        assert_eq!(
            resolve_wt_outbound_channel_capacity(Some("")),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }
}
