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

//! Shared session logic for chat sessions.
//!
//! This module contains transport-agnostic session logic used by both
//! `WsChatSession` and `WtChatSession`. The actors become thin transport
//! adapters while all business logic lives here.

use crate::actors::chat_server::ChatServer;
use crate::actors::packet_handler::{classify_packet, KeyframeRequestLimiter, PacketKind};
use crate::client_diagnostics::health_processor;
use crate::constants::{
    CONGESTION_DROP_THRESHOLD, CONGESTION_NOTIFY_MIN_INTERVAL, CONGESTION_WINDOW,
};
use crate::messages::server::{ClientMessage, Connect, Disconnect, JoinRoom, Packet};
use crate::messages::session::Message;
use crate::metrics::{RELAY_ACTIVE_SESSIONS_PER_ROOM, RELAY_ROOM_BYTES_TOTAL};
use crate::server_diagnostics::{
    send_connection_ended, send_connection_started, DataTracker, TrackerSender,
};
use crate::session_manager::SessionManager;
use actix::Addr;
use protobuf::Message as ProtobufMessage;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub type SessionId = u64;
pub type RoomId = String;
pub type UserId = String;

/// Connection state for session management during election
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connection is in testing phase (during election)
    Testing,
    /// Connection is active and should broadcast to NATS
    Active,
}

/// Result of handling an inbound packet
#[derive(Debug)]
pub enum InboundAction {
    /// Echo the packet back to sender (RTT measurement)
    Echo(Arc<Vec<u8>>),
    /// Forward to ChatServer for room routing
    Forward(Arc<Vec<u8>>),
    /// Already processed (health packet), no further action
    Processed,
    /// Keep-alive ping, no action needed
    KeepAlive,
}

// =========================================================================
// Congestion Tracking
// =========================================================================

/// Per-sender drop tracking state for congestion feedback.
struct SenderDropState {
    /// Number of drops in the current window.
    drop_count: u32,
    /// Start of the current counting window.
    window_start: Instant,
    /// Last time a CONGESTION notification was sent for this sender.
    last_notify: Option<Instant>,
}

/// Tracks outbound packet drops per sender and generates CONGESTION feedback
/// when the drop rate exceeds the configured threshold.
///
/// Each receiver session has its own `CongestionTracker`. When the receiver's
/// outbound channel is full, the transport layer calls
/// [`CongestionTracker::record_drop`] with the sender's session ID. If enough
/// drops accumulate within the configured window, a CONGESTION `PacketWrapper`
/// is generated for publication to NATS so the sender can step down its
/// quality tier.
pub struct CongestionTracker {
    /// Drop state keyed by sender session ID.
    senders: HashMap<u64, SenderDropState>,
    /// Total drops since the last stale-entry cleanup. Cleanup runs every
    /// [`CLEANUP_INTERVAL`] drops to amortize the cost of `retain()`.
    total_drops: u32,
}

impl Default for CongestionTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of drops between stale-entry cleanup passes. Amortizes the
/// O(n) `retain()` cost so it does not run on every single drop.
const CLEANUP_INTERVAL: u32 = 100;

impl CongestionTracker {
    pub fn new() -> Self {
        Self {
            senders: HashMap::new(),
            total_drops: 0,
        }
    }

    /// Record a dropped outbound packet from the given sender.
    ///
    /// Returns `Some(sender_session_id)` when the drop threshold has been
    /// exceeded and a CONGESTION notification should be sent. Returns `None`
    /// if the threshold has not been met or the notification is rate-limited.
    ///
    /// Performs amortized cleanup of stale entries every [`CLEANUP_INTERVAL`]
    /// drops: any sender whose `window_start` is older than
    /// `CONGESTION_WINDOW * 10` (10 seconds of inactivity) is removed. This
    /// prevents unbounded growth when transient participants leave while
    /// avoiding an O(n) `retain()` on every single drop.
    pub fn record_drop(&mut self, sender_session_id: u64) -> Option<u64> {
        let now = Instant::now();

        // Amortized cleanup of stale sender entries.
        self.total_drops = self.total_drops.wrapping_add(1);
        if self.total_drops.is_multiple_of(CLEANUP_INTERVAL) {
            let stale_threshold = CONGESTION_WINDOW * 10;
            self.senders
                .retain(|_, state| now.duration_since(state.window_start) <= stale_threshold);
        }

        let state = self
            .senders
            .entry(sender_session_id)
            .or_insert_with(|| SenderDropState {
                drop_count: 0,
                window_start: now,
                last_notify: None,
            });

        // Reset window if it has elapsed.
        if now.duration_since(state.window_start) > CONGESTION_WINDOW {
            state.drop_count = 0;
            state.window_start = now;
        }

        state.drop_count += 1;

        if state.drop_count >= CONGESTION_DROP_THRESHOLD {
            // Rate-limit notifications.
            if let Some(last) = state.last_notify {
                if now.duration_since(last) < CONGESTION_NOTIFY_MIN_INTERVAL {
                    return None;
                }
            }
            state.last_notify = Some(now);
            state.drop_count = 0;
            state.window_start = now;
            Some(sender_session_id)
        } else {
            None
        }
    }
}

/// Shared session logic, transport-agnostic.
///
/// This struct contains all the business logic for a chat session.
/// The transport-specific actors (`WsChatSession`, `WtChatSession`)
/// own an instance of this and delegate to it.
pub struct SessionLogic {
    pub id: u64,
    pub room: RoomId,
    pub user_id: UserId,
    /// Participant's chosen display name (from JWT claims).
    /// Falls back to `user_id` when no display name is available.
    pub display_name: String,
    /// Server-authoritative guest flag (JWT `is_guest` claim).
    pub is_guest: bool,
    pub addr: Addr<ChatServer>,
    pub nats_client: async_nats::client::Client,
    pub tracker_sender: TrackerSender,
    pub session_manager: SessionManager,
    /// When true, this session is observer-only: it can receive messages
    /// but cannot publish media to the room.
    pub observer: bool,
    /// Stable client instance identifier (UUID). Survives reconnects within
    /// the same tab/meeting join. Used by the server to correlate reconnections.
    pub instance_id: Option<String>,
    /// Transport type for this session ("websocket" or "webtransport")
    pub transport: String,
    /// Tracks outbound packet drops per sender to generate CONGESTION feedback.
    pub congestion_tracker: CongestionTracker,
    /// Per-session rate limiter for KEYFRAME_REQUEST packets.
    pub keyframe_limiter: KeyframeRequestLimiter,
}

impl SessionLogic {
    /// Create a new session logic instance
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        user_id: String,
        display_name: String,
        is_guest: bool,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
        observer: bool,
        instance_id: Option<String>,
        transport: &str,
    ) -> Self {
        let id = (Uuid::new_v4().as_u128() & 0xffffffffffffffff) as u64;
        info!(
            "new session: room={} user_id={} display_name={} is_guest={} session_id={} observer={} transport={}",
            room, user_id, display_name, is_guest, id, observer, transport
        );

        SessionLogic {
            id,
            room,
            user_id,
            display_name,
            is_guest,
            addr,
            nats_client,
            tracker_sender,
            session_manager,
            observer,
            instance_id,
            transport: transport.to_string(),
            congestion_tracker: CongestionTracker::new(),
            keyframe_limiter: KeyframeRequestLimiter::new(),
        }
    }

    // =========================================================================
    // Lifecycle
    // =========================================================================

    /// Track connection start for metrics
    pub fn track_connection_start(&self) {
        RELAY_ACTIVE_SESSIONS_PER_ROOM
            .with_label_values(&[&self.room, &self.transport])
            .inc();
        send_connection_started(
            &self.tracker_sender,
            self.id,
            self.user_id.clone(),
            self.room.clone(),
            self.transport.clone(),
        );
    }

    /// Build MEETING_STARTED packet
    pub fn build_meeting_started(&self, start_time_ms: u64, creator_id: &str) -> Vec<u8> {
        SessionManager::build_meeting_started_packet(&self.room, start_time_ms, creator_id)
    }

    /// Build SESSION_ASSIGNED packet for this session
    pub fn build_session_assigned(&self) -> Vec<u8> {
        SessionManager::build_session_assigned_packet(self.id)
    }

    /// Build MEETING_ENDED packet (for errors)
    pub fn build_meeting_ended(&self, reason: &str) -> Vec<u8> {
        SessionManager::build_meeting_ended_packet(&self.room, reason)
    }

    /// Create Connect message for ChatServer registration
    pub fn create_connect_message<R>(&self, recipient: R) -> Connect
    where
        R: Into<actix::Recipient<Message>>,
    {
        Connect {
            id: self.id,
            addr: recipient.into(),
        }
    }

    /// Create JoinRoom message for ChatServer
    pub fn create_join_room_message(&self) -> JoinRoom {
        JoinRoom {
            room: self.room.clone(),
            session: self.id,
            user_id: self.user_id.clone(),
            display_name: self.display_name.clone(),
            is_guest: self.is_guest,
            observer: self.observer,
            instance_id: self.instance_id.clone(),
        }
    }

    /// Create ClientMessage for forwarding a packet to ChatServer (NATS broadcast).
    pub fn create_client_message(&self, msg: Packet) -> ClientMessage {
        ClientMessage {
            session: self.id,
            user: self.user_id.clone(),
            room: self.room.clone(),
            msg,
        }
    }

    /// Handle JoinRoom response. Returns true if the session should stop (error case).
    pub fn handle_join_room_result(
        &self,
        result: Result<Result<(), String>, actix::MailboxError>,
    ) -> bool {
        match result {
            Ok(Ok(())) => {
                info!(
                    "Successfully joined room {} for session {}",
                    self.room, self.id
                );
                false
            }
            Ok(Err(e)) => {
                error!("Failed to join room: {}", e);
                true
            }
            Err(err) => {
                error!("Error sending JoinRoom: {:?}", err);
                true
            }
        }
    }

    /// Handle actor stopping - cleanup
    pub fn on_stopping(&self) {
        info!("Session stopping: {} in room {}", self.id, self.room);
        RELAY_ACTIVE_SESSIONS_PER_ROOM
            .with_label_values(&[&self.room, &self.transport])
            .dec();
        send_connection_ended(&self.tracker_sender, self.id);
        self.addr.do_send(Disconnect {
            session: self.id,
            room: self.room.clone(),
            user_id: self.user_id.clone(),
            display_name: self.display_name.clone(),
            is_guest: self.is_guest,
            observer: self.observer,
        });
    }

    // =========================================================================
    // Packet Handling
    // =========================================================================

    /// Returns true if this action should trigger connection activation.
    /// RTT probes (Echo) do not activate; any other packet does.
    pub fn should_activate_on_action(action: &InboundAction) -> bool {
        !matches!(action, InboundAction::Echo(_))
    }

    /// Handle an inbound packet from the client.
    ///
    /// Returns the action the transport should take.
    /// Observer sessions can still send RTT and health packets but all media
    /// data packets are silently dropped.
    pub fn handle_inbound(&mut self, data: &[u8]) -> InboundAction {
        // Track received data
        RELAY_ROOM_BYTES_TOTAL
            .with_label_values(&[&self.room, "inbound"])
            .inc_by(data.len() as f64);
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_received(self.id, data.len() as u64);

        // Classify and handle
        match classify_packet(data) {
            PacketKind::Dropped => {
                debug!(
                    "Dropping disallowed packet from session {} (user {})",
                    self.id, self.user_id
                );
                InboundAction::Processed
            }
            PacketKind::Rtt => {
                trace!("RTT packet from {}, echoing back", self.user_id);
                let data_tracker = DataTracker::new(self.tracker_sender.clone());
                data_tracker.track_sent(self.id, data.len() as u64);
                InboundAction::Echo(Arc::new(data.to_vec()))
            }
            PacketKind::Health => {
                trace!("Health packet from {}", self.user_id);
                health_processor::process_health_packet_bytes(data, self.nats_client.clone());
                InboundAction::Processed
            }
            PacketKind::KeyframeRequest => {
                if self.observer {
                    return InboundAction::Processed;
                }
                // Rate-limit KEYFRAME_REQUEST packets to prevent abuse.
                // A malicious client could flood these to force senders to
                // continuously generate expensive keyframes.
                if !self.keyframe_limiter.allow() {
                    warn!(
                        "Rate-limiting KEYFRAME_REQUEST from session {} (user {})",
                        self.id, self.user_id
                    );
                    return InboundAction::Processed;
                }
                InboundAction::Forward(Arc::new(data.to_vec()))
            }
            PacketKind::Data => {
                if self.observer {
                    trace!(
                        "Observer session {} dropping media packet from {}",
                        self.id,
                        self.user_id
                    );
                    return InboundAction::Processed;
                }

                InboundAction::Forward(Arc::new(data.to_vec()))
            }
        }
    }

    /// Handle an outbound message from ChatServer (to be sent to client).
    ///
    /// Returns the bytes to send and tracks metrics.
    pub fn handle_outbound(&self, msg: &Message) -> Vec<u8> {
        RELAY_ROOM_BYTES_TOTAL
            .with_label_values(&[&self.room, "outbound"])
            .inc_by(msg.msg.len() as f64);
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_sent(self.id, msg.msg.len() as u64);
        msg.msg.clone()
    }

    // =========================================================================
    // Congestion Feedback
    // =========================================================================

    /// Record that an outbound packet from `sender_session_id` was dropped
    /// because the outbound channel to this receiver was full.
    ///
    /// If the drop threshold is exceeded, a CONGESTION `PacketWrapper` is
    /// published to NATS so the sender's client can step down its quality
    /// tier. The notification is rate-limited per sender session.
    pub fn on_outbound_drop(&mut self, sender_session_id: u64, sender_user_id: &[u8]) {
        if let Some(sender_sid) = self.congestion_tracker.record_drop(sender_session_id) {
            warn!(
                "Congestion: session {} dropping packets from sender {} (user: {}), sending CONGESTION signal",
                self.id, sender_sid, String::from_utf8_lossy(sender_user_id),
            );

            // Build a CONGESTION PacketWrapper targeted at the sender.
            // `user_id`: receiver's identity (for sender-side logging).
            // `data`: sender's user_id — the stable identifier the client
            //         matches against (session_id is ephemeral and rotates
            //         on reconnect, causing missed signals).
            // `session_id`: sender's session_id (kept for NATS routing).
            let congestion_packet = PacketWrapper {
                packet_type: PacketType::CONGESTION.into(),
                user_id: self.user_id.as_bytes().to_vec(),
                data: sender_user_id.to_vec(),
                session_id: sender_sid,
                ..Default::default()
            };

            match congestion_packet.write_to_bytes() {
                Ok(bytes) => {
                    let subject = format!("room.{}.{}", self.room.replace(' ', "_"), sender_sid);
                    let nc = self.nats_client.clone();
                    let bytes = bytes::Bytes::from(bytes);
                    tokio::spawn(async move {
                        if let Err(e) = nc.publish(subject, bytes).await {
                            error!("Failed to publish CONGESTION signal: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to serialize CONGESTION packet: {}", e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_inbound_action_debug() {
        let action = InboundAction::KeepAlive;
        assert_eq!(format!("{action:?}"), "KeepAlive");
    }

    #[test]
    fn test_congestion_tracker_cleans_stale_entries() {
        let mut tracker = CongestionTracker::new();

        // Insert a stale entry by manually inserting with an old window_start.
        let stale_id = 1000;
        tracker.senders.insert(
            stale_id,
            SenderDropState {
                drop_count: 0,
                // 20 seconds ago — well past the 10 * CONGESTION_WINDOW threshold
                window_start: Instant::now() - (CONGESTION_WINDOW * 20),
                last_notify: None,
            },
        );

        // Insert a fresh entry.
        let fresh_id = 2000;
        tracker.senders.insert(
            fresh_id,
            SenderDropState {
                drop_count: 0,
                window_start: Instant::now(),
                last_notify: None,
            },
        );

        assert_eq!(tracker.senders.len(), 2);

        // Set total_drops so the next record_drop triggers cleanup.
        tracker.total_drops = CLEANUP_INTERVAL - 1;

        // Recording a drop for a new sender should trigger cleanup.
        let trigger_id = 3000;
        tracker.record_drop(trigger_id);

        // The stale entry should have been removed.
        assert!(
            !tracker.senders.contains_key(&stale_id),
            "stale sender entry should be cleaned up"
        );
        // Fresh and trigger entries should remain.
        assert!(tracker.senders.contains_key(&fresh_id));
        assert!(tracker.senders.contains_key(&trigger_id));
    }

    #[test]
    fn test_congestion_tracker_retains_active_entries() {
        let mut tracker = CongestionTracker::new();

        // Record drops for two senders.
        tracker.record_drop(100);
        tracker.record_drop(200);

        assert_eq!(tracker.senders.len(), 2);

        // Record another drop — both entries are fresh, nothing should be cleaned.
        tracker.record_drop(100);

        assert_eq!(tracker.senders.len(), 2);
        assert!(tracker.senders.contains_key(&100));
        assert!(tracker.senders.contains_key(&200));
    }

    // =====================================================================
    // Drop recording and counting
    // =====================================================================

    #[test]
    fn test_drop_recording_increments_count() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 42;

        // Record a single drop — should not yet trigger notification.
        let result = tracker.record_drop(sender_id);
        assert!(
            result.is_none(),
            "single drop should not trigger notification"
        );

        // The internal count should be 1.
        let state = tracker.senders.get(&sender_id).unwrap();
        assert_eq!(state.drop_count, 1);
    }

    #[test]
    fn test_drop_recording_multiple_senders_independent() {
        let mut tracker = CongestionTracker::new();

        // Record drops for two different senders.
        for _ in 0..3 {
            tracker.record_drop(100);
        }
        for _ in 0..2 {
            tracker.record_drop(200);
        }

        // Each sender should have independent counts.
        assert_eq!(tracker.senders.get(&100).unwrap().drop_count, 3);
        assert_eq!(tracker.senders.get(&200).unwrap().drop_count, 2);
    }

    #[test]
    fn test_drop_window_resets_after_expiry() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 50;

        // Manually insert a sender with a window that started in the past
        // (just beyond CONGESTION_WINDOW) so the next record_drop resets it.
        tracker.senders.insert(
            sender_id,
            SenderDropState {
                drop_count: 3,
                window_start: Instant::now() - (CONGESTION_WINDOW + Duration::from_millis(10)),
                last_notify: None,
            },
        );

        // record_drop should reset the window and set count to 1 (not 4).
        tracker.record_drop(sender_id);
        let state = tracker.senders.get(&sender_id).unwrap();
        assert_eq!(
            state.drop_count, 1,
            "drop count should reset to 1 after window expiry"
        );
    }

    // =====================================================================
    // Congestion notification triggering
    // =====================================================================

    #[test]
    fn test_notification_triggers_at_threshold() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 99;

        // Record drops up to one less than threshold — no notification.
        for _ in 0..(CONGESTION_DROP_THRESHOLD - 1) {
            let result = tracker.record_drop(sender_id);
            assert!(result.is_none());
        }

        // The threshold-th drop should trigger a notification.
        let result = tracker.record_drop(sender_id);
        assert_eq!(
            result,
            Some(sender_id),
            "should return sender_id when threshold is reached"
        );
    }

    #[test]
    fn test_notification_resets_count_after_trigger() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 77;

        // Reach threshold to trigger notification.
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            tracker.record_drop(sender_id);
        }

        // After triggering, count should be reset to 0.
        let state = tracker.senders.get(&sender_id).unwrap();
        assert_eq!(
            state.drop_count, 0,
            "drop count should reset after notification"
        );
    }

    #[test]
    fn test_rate_limiting_suppresses_rapid_notifications() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 55;

        // First burst: trigger notification.
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            tracker.record_drop(sender_id);
        }
        // The last call above returned Some(55). Now the last_notify is set.

        // Second burst immediately after: should be rate-limited because
        // CONGESTION_NOTIFY_MIN_INTERVAL has not elapsed.
        for i in 0..CONGESTION_DROP_THRESHOLD {
            let result = tracker.record_drop(sender_id);
            if i < CONGESTION_DROP_THRESHOLD - 1 {
                // Below threshold — always None.
                assert!(result.is_none());
            } else {
                // At threshold — rate-limited, so still None.
                assert!(
                    result.is_none(),
                    "notification should be suppressed by rate limiter"
                );
            }
        }
    }

    // =====================================================================
    // Stale entry cleanup
    // =====================================================================

    #[test]
    fn test_stale_cleanup_removes_multiple_stale_entries() {
        let mut tracker = CongestionTracker::new();

        // Insert several stale entries.
        for id in 1..=5 {
            tracker.senders.insert(
                id,
                SenderDropState {
                    drop_count: 0,
                    window_start: Instant::now() - (CONGESTION_WINDOW * 20),
                    last_notify: None,
                },
            );
        }

        // Insert one fresh entry.
        tracker.senders.insert(
            100,
            SenderDropState {
                drop_count: 0,
                window_start: Instant::now(),
                last_notify: None,
            },
        );

        assert_eq!(tracker.senders.len(), 6);

        // Set total_drops so the next record_drop triggers cleanup.
        tracker.total_drops = CLEANUP_INTERVAL - 1;

        // Trigger cleanup by recording a drop.
        tracker.record_drop(200);

        // All stale entries (1-5) should be gone; fresh (100) and new (200) remain.
        assert_eq!(tracker.senders.len(), 2);
        assert!(tracker.senders.contains_key(&100));
        assert!(tracker.senders.contains_key(&200));
    }

    #[test]
    fn test_entry_just_under_boundary_is_retained() {
        let mut tracker = CongestionTracker::new();

        // Insert an entry slightly under the stale boundary (10 * CONGESTION_WINDOW).
        // Use a 500ms margin to account for time elapsed between insertion and
        // the `retain` call inside `record_drop`.
        tracker.senders.insert(
            1,
            SenderDropState {
                drop_count: 2,
                window_start: Instant::now() - (CONGESTION_WINDOW * 10)
                    + Duration::from_millis(500),
                last_notify: None,
            },
        );

        // Set total_drops so the next record_drop triggers cleanup.
        tracker.total_drops = CLEANUP_INTERVAL - 1;

        tracker.record_drop(2);

        // Entry 1 is within the boundary — should be retained.
        assert!(
            tracker.senders.contains_key(&1),
            "entry just under stale boundary should be retained"
        );
    }

    // =====================================================================
    // should_notify_sender() — tested indirectly through record_drop
    // =====================================================================

    #[test]
    fn test_first_notification_for_sender_has_no_rate_limit() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 10;

        // First time reaching threshold — no prior last_notify, should fire.
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            tracker.record_drop(sender_id);
        }

        // Verify last_notify was set.
        let state = tracker.senders.get(&sender_id).unwrap();
        assert!(
            state.last_notify.is_some(),
            "last_notify should be set after first notification"
        );
    }

    #[test]
    fn test_notification_allowed_after_rate_limit_expires() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 30;

        // Simulate a previous notification that happened long enough ago
        // that the rate limit has expired.
        tracker.senders.insert(
            sender_id,
            SenderDropState {
                drop_count: 0,
                window_start: Instant::now(),
                last_notify: Some(
                    Instant::now() - CONGESTION_NOTIFY_MIN_INTERVAL - Duration::from_millis(10),
                ),
            },
        );

        // Record enough drops to hit threshold.
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            tracker.record_drop(sender_id);
        }

        // Should trigger because rate limit has expired.
        // The last record_drop was the threshold-th, which was the one that returned.
        // We need to check the return value of the last call.
        // Let's redo this more carefully.
        let mut tracker2 = CongestionTracker::new();
        tracker2.senders.insert(
            sender_id,
            SenderDropState {
                drop_count: 0,
                window_start: Instant::now(),
                last_notify: Some(
                    Instant::now() - CONGESTION_NOTIFY_MIN_INTERVAL - Duration::from_millis(10),
                ),
            },
        );

        let mut triggered = false;
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            if tracker2.record_drop(sender_id).is_some() {
                triggered = true;
            }
        }
        assert!(
            triggered,
            "notification should fire after rate-limit window expires"
        );
    }

    #[test]
    fn test_default_trait_impl() {
        // Verify Default trait works and produces an empty tracker.
        let tracker = CongestionTracker::default();
        assert!(tracker.senders.is_empty());
    }

    #[test]
    fn test_should_activate_on_action() {
        // Echo (RTT probe) should NOT activate.
        assert!(!SessionLogic::should_activate_on_action(
            &InboundAction::Echo(Arc::new(vec![]))
        ));
        // Forward, Processed, KeepAlive should activate.
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::Forward(Arc::new(vec![]))
        ));
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::Processed
        ));
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::KeepAlive
        ));
    }
}
