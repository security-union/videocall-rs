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

use std::sync::Arc;

use crate::actors::session_logic::{RoomId, SessionId};

use super::session::Message;
use actix::{Message as ActixMessage, Recipient};

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct ClientMessage {
    pub session: SessionId,
    pub user: String,
    pub room: RoomId,
    pub msg: Packet,
}

#[derive(ActixMessage)]
#[rtype(result = "Result<(), String>")]
pub struct JoinRoom {
    pub session: SessionId,
    pub room: RoomId,
    pub user_id: String,
    /// Participant's chosen display name (from JWT claims).
    /// Falls back to `user_id` when no display name is available.
    pub display_name: String,
    /// Server-authoritative guest flag, sourced from the authenticated JWT
    /// `is_guest` claim.
    pub is_guest: bool,
    /// When true, this is an observer session (waiting room) and should NOT
    /// trigger PARTICIPANT_JOINED notifications.
    pub observer: bool,
    /// Stable client instance identifier (UUID). Generated once per meeting join,
    /// survives reconnects. When present, the server uses it to find and evict
    /// the stale session from a previous connection by the same client instance.
    pub instance_id: Option<String>,
    /// Whether this participant is the meeting host.
    pub is_host: bool,
    /// Whether the meeting should end when the host leaves.
    pub end_on_host_leave: bool,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Connect {
    pub id: SessionId,
    pub addr: Recipient<Message>,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Packet {
    pub data: Arc<Vec<u8>>,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Disconnect {
    pub session: SessionId,
    pub room: RoomId,
    pub user_id: String,
    /// Participant's chosen display name (from JWT claims).
    /// Falls back to `user_id` when no display name is available.
    pub display_name: String,
    /// Server-authoritative guest flag (JWT-sourced). Included so the
    /// PARTICIPANT_LEFT broadcast carries the same `is_guest` signal as the
    /// matching PARTICIPANT_JOINED.
    pub is_guest: bool,
    /// When true, the disconnecting session is an observer (waiting room)
    /// and should NOT trigger PARTICIPANT_LEFT notifications.
    pub observer: bool,
    /// Whether this participant is the meeting host.
    pub is_host: bool,
    /// Whether the meeting should end when the host leaves.
    pub end_on_host_leave: bool,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Leave {
    pub session: SessionId,
    pub room: RoomId,
    pub user_id: String,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct ActivateConnection {
    pub session: SessionId,
}
