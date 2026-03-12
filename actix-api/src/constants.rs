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
/// How long before lack of client response causes a timeout
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Regex pattern for validating usernames and room IDs
/// Allows alphanumeric characters, underscores, and hyphens
pub const VALID_ID_PATTERN: &str = "^[a-zA-Z0-9_-]*$";

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
