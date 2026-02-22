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
use crate::messages::server::{Connect, Disconnect, ForceDisconnect, JoinRoom};
use crate::messages::session::Message;
use crate::server_diagnostics::{
    send_connection_ended, send_connection_started, DataTracker, TrackerSender,
};
use crate::session_manager::SessionManager;
use actix::Addr;
use std::sync::Arc;
use tracing::{info, trace};
use uuid::Uuid;

pub type SessionId = u64;
pub type RoomId = String;
pub type Email = String;

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
    pub id: SessionId,
    pub room: RoomId,
    pub email: Email,
    pub addr: Addr<ChatServer>,
    pub nats_client: async_nats::client::Client,
    pub tracker_sender: TrackerSender,
    pub session_manager: SessionManager,
}

impl SessionLogic {
    /// Create a new session logic instance
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        email: String,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
    ) -> Self {
        let id = (Uuid::new_v4().as_u128() & 0xFFFF_FFFF_FFFF_FFFF) as u64;
        info!(
            "new session: room={} email={} session_id={}",
            room, email, id
        );

        SessionLogic {
            id,
            room,
            email,
            addr,
            nats_client,
            tracker_sender,
            session_manager,
        }
    }

    // =========================================================================
    // Lifecycle
    // =========================================================================

    /// Track connection start for metrics
    pub fn track_connection_start(&self, transport: &str) {
        send_connection_started(
            &self.tracker_sender,
            self.id.to_string(),
            self.email.clone(),
            self.room.clone(),
            transport.to_string(),
        );
    }

    /// Build MEETING_STARTED packet
    pub fn build_meeting_started(&self, start_time_ms: u64, creator_id: &str) -> Vec<u8> {
        SessionManager::build_meeting_started_packet(&self.room, start_time_ms, creator_id, self.id)
    }

    /// Build MEETING_ENDED packet (for errors)
    pub fn build_meeting_ended(&self, reason: &str) -> Vec<u8> {
        SessionManager::build_meeting_ended_packet(&self.room, reason)
    }

    /// Create Connect message for ChatServer registration
    pub fn create_connect_message<M, D>(&self, msg_recipient: M, disconnect_recipient: D) -> Connect
    where
        M: Into<actix::Recipient<Message>>,
        D: Into<actix::Recipient<ForceDisconnect>>,
    {
        Connect {
            id: self.id,
            addr: msg_recipient.into(),
            disconnect_addr: disconnect_recipient.into(),
        }
    }

    /// Create JoinRoom message for ChatServer
    pub fn create_join_room_message(&self) -> JoinRoom {
        JoinRoom {
            room: self.room.clone(),
            session: self.id,
            user_id: self.email.clone(),
        }
    }

    /// Handle actor stopping - cleanup
    pub fn on_stopping(&self) {
        info!("Session stopping: {} in room {}", self.id, self.room);
        send_connection_ended(&self.tracker_sender, self.id.to_string());
        self.addr.do_send(Disconnect {
            session: self.id,
            room: self.room.clone(),
            user_id: self.email.clone(),
        });
    }

    // =========================================================================
    // Packet Handling
    // =========================================================================

    /// Handle an inbound packet from the client.
    ///
    /// Returns the action the transport should take.
    pub fn handle_inbound(&self, data: &[u8]) -> InboundAction {
        // Track received data
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_received(&self.id.to_string(), data.len() as u64);

        // Classify and handle
        match classify_packet(data) {
            PacketKind::Rtt => {
                trace!("RTT packet from {}, echoing back", self.email);
                let data_tracker = DataTracker::new(self.tracker_sender.clone());
                data_tracker.track_sent(&self.id.to_string(), data.len() as u64);
                InboundAction::Echo(Arc::new(data.to_vec()))
            }
            PacketKind::Health => {
                trace!("Health packet from {}", self.email);
                health_processor::process_health_packet_bytes(data, self.nats_client.clone());
                InboundAction::Processed
            }
            PacketKind::Data => InboundAction::Forward(Arc::new(data.to_vec())),
        }
    }

    /// Handle an outbound message from ChatServer (to be sent to client).
    ///
    /// Returns the bytes to send and tracks metrics.
    pub fn handle_outbound(&self, msg: &Message) -> Vec<u8> {
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_sent(&self.id.to_string(), msg.msg.len() as u64);
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
