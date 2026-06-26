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
    /// Transport this session connected over (`"websocket"` | `"webtransport"`).
    ///
    /// Threaded through so the per-session NATS subscription loop's `handle_msg`
    /// closure can attribute an inbound actor-mailbox overflow drop to the
    /// receiver's transport — the `Recipient<Message>` stored in
    /// `ChatServer::sessions` is transport-erased, so the transport is otherwise
    /// unknown at the mailbox-drop site (dashboard audit Tier B #2 / #1057).
    pub transport: String,
    /// Shared receiver-downlink-congestion signal for #1219 Half 2.
    ///
    /// The transport actor owns this `Arc<AtomicU64>` and writes the monotonic
    /// epoch of the most recent REAL downlink overflow into it from
    /// `SessionLogic::on_outbound_drop`. It is handed to the per-receiver NATS
    /// fan-out closure (`handle_msg`) here — the same reversed-direction handoff
    /// pattern as the `transport` field above — so the closure can read the
    /// windowed signal to drive emergency layer shedding + the one-shot
    /// DOWNLINK_CONGESTION emit. `0` ([`DOWNLINK_EPOCH_NEVER`]) means "never
    /// congested".
    ///
    /// [`DOWNLINK_EPOCH_NEVER`]: crate::actors::session_logic::DOWNLINK_EPOCH_NEVER
    pub downlink_congested_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
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

/// Sent from a session's NATS loop when it receives a PARTICIPANT_LIST_REQUEST
/// event, so the ChatServer re-announces this session's PARTICIPANT_JOINED and
/// the requesting joiner learns about this peer.
///
/// The handler does NOT publish immediately. It records this responder once
/// (tracking distinct requesters) and arms one trailing
/// [`PARTICIPANT_REBROADCAST_COALESCE_MS`] timer; when it fires, the flush
/// re-announces once — broadcast for a wave (≥2 distinct requesters), unicast to
/// the lone requester otherwise — so a reconnection wave's M per-requester
/// publishes collapse to one. `requester_session` is both the arm-gate and the
/// unicast target: a requester on THIS instance was already served by the
/// in-memory replay in JoinRoom, so it does not arm (single-server stays
/// zero-cost).
///
/// [`PARTICIPANT_REBROADCAST_COALESCE_MS`]: crate::constants::PARTICIPANT_REBROADCAST_COALESCE_MS
#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct RebroadcastPresence {
    /// The responding peer's own session (the peer being announced).
    pub session: SessionId,
    /// The joiner that asked for the participant list. Gates arming (a local
    /// requester is already served by the in-memory replay) and is the unicast
    /// target for a single-join re-announce.
    pub requester_session: SessionId,
}
