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

//! Shared logic for chat session transports (WebSocket, WebTransport, etc.)

use crate::actors::chat_server::ChatServer;
use crate::actors::session_logic::{RoomId, SessionId};
use crate::messages::server::{ActivateConnection, ClientMessage, Packet};
use actix::Addr;
use protobuf::Message as ProtobufMessage;
use tracing::{error, info, trace};
use videocall_types::protos::packet_wrapper::packet_wrapper::ConnectionPhase;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Parse inbound packet and activate on first ACTIVE or UNSPECIFIED phase.
/// Skips if already activated or during PROBING.
/// Shared by WebSocket and WebTransport transports.
pub fn try_activate_from_first_packet(
    addr: &Addr<ChatServer>,
    session_id: SessionId,
    activated: &mut bool,
    data: &[u8],
) {
    if *activated {
        return;
    }
    let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) else {
        return;
    };
    let Ok(phase) = packet_wrapper.connection_phase.enum_value() else {
        return;
    };
    let should_activate = matches!(
        phase,
        ConnectionPhase::ACTIVE | ConnectionPhase::CONNECTION_PHASE_UNSPECIFIED
    );
    if !should_activate {
        return;
    }
    addr.do_send(ActivateConnection {
        session: session_id,
    });
    *activated = true;
    info!(
        "Session {} activated on first {:?} packet",
        session_id, phase
    );
}

/// Log force disconnect event. Caller is responsible for stopping the actor.
pub fn log_force_disconnect(session_id: SessionId, room: impl std::fmt::Display) {
    info!(
        "Force disconnect for session {} in room {}",
        session_id, room
    );
}

/// Forward inbound packet to ChatServer. Shared by WebSocket and WebTransport transports.
pub fn forward_packet_to_chat_server(
    addr: &Addr<ChatServer>,
    session: SessionId,
    email: impl Into<String>,
    room: impl Into<RoomId>,
    msg: Packet,
) {
    let room: RoomId = room.into();
    trace!(
        "Forwarding packet to ChatServer: session {} room {}",
        session,
        room
    );
    addr.do_send(ClientMessage {
        session,
        user: email.into(),
        room,
        msg,
    });
}

/// Handle JoinRoom response. Returns `Ok(())` on success; `Err(())` means caller should stop the actor.
pub fn handle_join_room_response<SendErr>(
    response: Result<Result<(), String>, SendErr>,
    room: impl std::fmt::Display,
    session_id: SessionId,
) -> Result<(), ()>
where
    SendErr: std::fmt::Debug,
{
    match response {
        Ok(Ok(())) => {
            info!(
                "Successfully joined room {} for session {}",
                room, session_id
            );
            Ok(())
        }
        Ok(Err(e)) => {
            error!("Failed to join room: {}", e);
            Err(())
        }
        Err(err) => {
            error!("Error sending JoinRoom: {:?}", err);
            Err(())
        }
    }
}
