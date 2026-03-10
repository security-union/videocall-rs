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
use crate::actors::packet_handler::{classify_packet, PacketKind};
use crate::client_diagnostics::health_processor;
use crate::messages::server::{ClientMessage, Connect, Disconnect, JoinRoom, Packet};
use crate::messages::session::Message;
use crate::server_diagnostics::{
    send_connection_ended, send_connection_started, DataTracker, TrackerSender,
};
use crate::session_manager::SessionManager;
use actix::Addr;
use std::sync::Arc;
use tracing::{error, info, trace};
use uuid::Uuid;

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
    pub addr: Addr<ChatServer>,
    pub nats_client: async_nats::client::Client,
    pub tracker_sender: TrackerSender,
    pub session_manager: SessionManager,
    /// When true, this session is observer-only: it can receive messages
    /// but cannot publish media to the room.
    pub observer: bool,
}

impl SessionLogic {
    /// Create a new session logic instance
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        user_id: String,
        display_name: String,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
        observer: bool,
    ) -> Self {
        let id = (Uuid::new_v4().as_u128() & 0xffffffffffffffff) as u64;
        info!(
            "new session: room={} user_id={} display_name={} session_id={} observer={}",
            room, user_id, display_name, id, observer
        );

        SessionLogic {
            id,
            room,
            user_id,
            display_name,
            addr,
            nats_client,
            tracker_sender,
            session_manager,
            observer,
        }
    }

    // =========================================================================
    // Lifecycle
    // =========================================================================

    /// Track connection start for metrics
    pub fn track_connection_start(&self, transport: &str) {
        send_connection_started(
            &self.tracker_sender,
            self.id,
            self.user_id.clone(),
            self.room.clone(),
            transport.to_string(),
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
            observer: self.observer,
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
        send_connection_ended(&self.tracker_sender, self.id);
        self.addr.do_send(Disconnect {
            session: self.id,
            room: self.room.clone(),
            user_id: self.user_id.clone(),
            display_name: self.display_name.clone(),
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
    pub fn handle_inbound(&self, data: &[u8]) -> InboundAction {
        // Track received data
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_received(self.id, data.len() as u64);

        // Classify and handle
        match classify_packet(data) {
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
            PacketKind::Data => {
                if self.observer {
                    trace!(
                        "Observer session {} dropping media packet from {}",
                        self.id,
                        self.user_id
                    );
                    InboundAction::Processed
                } else {
                    InboundAction::Forward(Arc::new(data.to_vec()))
                }
            }
        }
    }

    /// Handle an outbound message from ChatServer (to be sent to client).
    ///
    /// Returns the bytes to send and tracks metrics.
    pub fn handle_outbound(&self, msg: &Message) -> Vec<u8> {
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_sent(self.id, msg.msg.len() as u64);
        msg.msg.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbound_action_debug() {
        let action = InboundAction::KeepAlive;
        assert_eq!(format!("{action:?}"), "KeepAlive");
    }
}
