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

use crate::{
    messages::{
        server::{ActivateConnection, ClientMessage, Connect, Disconnect, JoinRoom, Leave},
        session::Message,
    },
    models::build_subject_and_queue,
    session_manager::{SessionEndResult, SessionManager},
};

use actix::{
    Actor, AsyncContext, Context, Handler, Message as ActixMessage, MessageResult, Recipient,
};
use futures::StreamExt;
use protobuf::Message as ProtobufMessage;
use std::collections::HashMap;
use tokio::task::JoinHandle;
use tracing::{error, info, trace, warn};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::SYSTEM_USER_ID;

use super::session_logic::{ConnectionState, SessionId};

/// Internal message to clean up active_subs when a spawned join task fails.
/// This fixes a race condition where start_session could fail inside the spawned task,
/// leaving a stale entry in active_subs that blocks future join attempts.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct CleanupFailedJoin {
    session: SessionId,
    room: String,
}

pub struct ChatServer {
    nats_connection: async_nats::client::Client,
    sessions: HashMap<SessionId, Recipient<Message>>,
    active_subs: HashMap<SessionId, JoinHandle<()>>,
    session_manager: SessionManager,
    connection_states: HashMap<SessionId, ConnectionState>,
    /// Track which sessions are in which room, with their user_id and display_name.
    /// Used to send PARTICIPANT_JOINED for existing peers to new joiners.
    room_members: HashMap<String, Vec<(SessionId, String, String)>>,
}

impl ChatServer {
    pub async fn new(nats_connection: async_nats::client::Client) -> Self {
        ChatServer {
            nats_connection,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
            session_manager: SessionManager::new(),
            connection_states: HashMap::new(),
            room_members: HashMap::new(),
        }
    }

    pub fn leave_rooms(
        &mut self,
        session_id: &SessionId,
        room: Option<&str>,
        user_id: Option<&str>,
        display_name: Option<&str>,
        observer: bool,
    ) {
        // Remove the subscription task if it exists
        if let Some(task) = self.active_subs.remove(session_id) {
            task.abort();
        }

        // Remove from room_members tracking
        if let Some(room_id) = room {
            if let Some(members) = self.room_members.get_mut(room_id) {
                members.retain(|(sid, _, _)| sid != session_id);
                if members.is_empty() {
                    self.room_members.remove(room_id);
                }
            }
        }

        // End session using SessionManager
        if let (Some(room_id), Some(uid)) = (room, user_id) {
            let room_id = room_id.to_string();
            let user_id = uid.to_string();
            let display_name = display_name.unwrap_or(uid).to_string();
            let session_manager = self.session_manager.clone();
            let nc = self.nats_connection.clone();
            let session_id_val = *session_id;

            // Observer sessions (waiting room) should not publish PARTICIPANT_LEFT
            // since they were never real participants in the meeting.
            if observer {
                info!(
                    "Observer session {} for {} leaving room {} - skipping PARTICIPANT_LEFT",
                    session_id_val, user_id, room_id
                );
                tokio::spawn(async move {
                    if let Err(e) = session_manager.end_session(&room_id, &user_id).await {
                        error!("Error ending observer session for room {}: {}", room_id, e);
                    }
                });
                return;
            }

            tokio::spawn(async move {
                match session_manager.end_session(&room_id, &user_id).await {
                    Ok(SessionEndResult::HostEndedMeeting) => {
                        info!(
                            "Host {} left room {} - ending meeting for all",
                            user_id, room_id
                        );
                        // Notify all participants using MEETING packet (protobuf)
                        let bytes = SessionManager::build_meeting_ended_packet(
                            &room_id,
                            "The host has ended the meeting",
                        );
                        let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                        if let Err(e) = nc.publish(subject, bytes.into()).await {
                            error!("Error publishing MEETING_ENDED: {}", e);
                        }
                    }
                    Ok(SessionEndResult::LastParticipantLeft) => {
                        info!("Last participant {} left room {}", user_id, room_id);
                    }
                    Ok(SessionEndResult::MeetingContinues { remaining_count }) => {
                        info!(
                            "Participant {} left room {}, {} remaining",
                            user_id, room_id, remaining_count
                        );
                        // Notify remaining peers about the departed session
                        let bytes = SessionManager::build_peer_left_packet(
                            &room_id,
                            &user_id,
                            session_id_val,
                            &display_name,
                        );
                        let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                        if let Err(e) = nc.publish(subject, bytes.into()).await {
                            error!("Error publishing PARTICIPANT_LEFT: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Error ending session for room {}: {}", room_id, e);
                    }
                }
            });
        }
    }

    /// Get the session manager (for use by chat_session)
    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }
}

impl Actor for ChatServer {
    type Context = Context<Self>;
}

impl Handler<Connect> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: Connect, _ctx: &mut Self::Context) -> Self::Result {
        let Connect { id, addr } = msg;
        self.sessions.insert(id, addr);
        self.connection_states.insert(id, ConnectionState::Testing);
    }
}

impl Handler<Disconnect> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        Disconnect {
            session,
            room,
            user_id,
            display_name,
            observer,
        }: Disconnect,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.leave_rooms(
            &session,
            Some(&room),
            Some(&user_id),
            Some(&display_name),
            observer,
        );
        let _ = self.sessions.remove(&session);
        let _ = self.connection_states.remove(&session);
    }
}

impl Handler<Leave> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        Leave {
            session,
            room,
            user_id,
        }: Leave,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        // Leave is always a real participant, never an observer.
        // No display_name available from Leave message; leave_rooms will
        // fall back to user_id.
        self.leave_rooms(&session, Some(&room), Some(&user_id), None, false);
    }
}

impl Handler<ActivateConnection> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: ActivateConnection, _ctx: &mut Self::Context) -> Self::Result {
        let ActivateConnection { session } = msg;
        if let Some(state) = self.connection_states.get_mut(&session) {
            if *state == ConnectionState::Testing {
                *state = ConnectionState::Active;
                info!("Session {} activated (Testing -> Active)", session);
            }
        } else {
            self.connection_states
                .insert(session, ConnectionState::Active);
            info!(
                "Session {} activated (state was missing, created as Active)",
                session
            );
        }
    }
}

impl Handler<ClientMessage> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: ClientMessage, ctx: &mut Self::Context) -> Self::Result {
        let ClientMessage {
            session,
            room,
            msg,
            user: _,
        } = msg;
        trace!("got message in server room {room} session {session}");

        // Check connection state - only publish to NATS if Active
        let connection_state = self
            .connection_states
            .get(&session)
            .copied()
            .unwrap_or(ConnectionState::Testing);

        if connection_state != ConnectionState::Active {
            trace!(
                "Skipping NATS publish for session {} in Testing state",
                session
            );
            return; // Don't publish during Testing state
        }

        let nc = self.nats_connection.clone();
        let subject = format!("room.{room}.{session}");
        let subject = subject.replace(' ', "_");

        let packet_bytes =
            if let Ok(mut packet_wrapper) = PacketWrapper::parse_from_bytes(&msg.data) {
                if packet_wrapper.session_id == 0 {
                    packet_wrapper.session_id = session;
                }
                match packet_wrapper.write_to_bytes() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        error!("Failed to serialize PacketWrapper with session_id: {}", e);
                        msg.data.to_vec()
                    }
                }
            } else {
                msg.data.to_vec()
            };

        let b = bytes::Bytes::from(packet_bytes);
        let fut = async move {
            match nc.publish(subject.clone(), b).await {
                Ok(_) => trace!("published message to {subject}"),
                Err(e) => error!("error publishing message to {subject}: {e}"),
            }
        };
        let fut = actix::fut::wrap_future::<_, Self>(fut);
        ctx.spawn(fut);
    }
}

impl Handler<JoinRoom> for ChatServer {
    type Result = MessageResult<JoinRoom>;

    fn handle(
        &mut self,
        JoinRoom {
            session,
            room,
            user_id,
            display_name,
            observer,
        }: JoinRoom,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        // Validate user_id synchronously BEFORE spawning async task.
        // This ensures we return an error to the client if validation fails,
        // rather than returning Ok and silently failing in the spawned task.
        if user_id == SYSTEM_USER_ID {
            return MessageResult(Err("Cannot use reserved system user ID".into()));
        }

        if self.active_subs.contains_key(&session) {
            return MessageResult(Ok(()));
        }

        let session_manager = self.session_manager.clone();
        let room_clone = room.clone();
        let user_id_clone = user_id.clone();
        let display_name_clone = display_name.clone();
        let session_id = session;
        let nc = self.nats_connection.clone();

        let session_str = session.to_string();
        let (subject, queue) = build_subject_and_queue(&room, &session_str);
        let session_recipient = match self.sessions.get(&session) {
            Some(addr) => addr.clone(),
            None => {
                return MessageResult(Err("Session not found".into()));
            }
        };

        // Collect existing non-observer room members for notifying the new joiner
        let existing_members: Vec<(SessionId, String, String)> = if !observer {
            self.room_members.get(&room).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };

        // Track this session in room_members (only for non-observers)
        if !observer {
            self.room_members.entry(room.clone()).or_default().push((
                session,
                user_id.clone(),
                display_name.clone(),
            ));
        }

        // Clone the recipient so we can send existing member info directly to the new joiner
        let new_joiner_recipient = session_recipient.clone();

        let nc2 = self.nats_connection.clone();
        let session_clone = session;

        // Get ChatServer address for cleanup on failure
        let chat_server_addr = ctx.address();
        let session_for_cleanup = session;

        let handle = tokio::spawn(async move {
            // Start session using SessionManager - await result before subscribing
            match session_manager
                .start_session(&room_clone, &user_id_clone, session_id)
                .await
            {
                Ok(result) => {
                    info!(
                        "Session started for room {} (first: {}) at {} by creator {} (session {})",
                        room_clone,
                        result.is_first_participant,
                        result.start_time_ms,
                        result.creator_id,
                        session_id,
                    );

                    // SESSION_ASSIGNED is sent by ws_chat_session / wt_chat_session
                    // in their started() method before this JoinRoom handler runs.

                    send_meeting_info(&nc, &room_clone, result.start_time_ms, &result.creator_id)
                        .await;

                    // Notify existing participants about the new joiner.
                    // Observer sessions (waiting room) should NOT trigger this.
                    if !observer {
                        let bytes = SessionManager::build_peer_joined_packet(
                            &room_clone,
                            &user_id_clone,
                            session_id,
                            &display_name_clone,
                        );
                        let subject = format!("room.{}.system", room_clone.replace(' ', "_"));
                        info!(
                            "Publishing PARTICIPANT_JOINED for {} (display={}) to {}",
                            user_id_clone, display_name_clone, subject
                        );
                        let subject_for_log = subject.clone();
                        if let Err(e) = nc.publish(subject, bytes.into()).await {
                            error!("Error publishing PARTICIPANT_JOINED: {}", e);
                        } else {
                            info!(
                                "Successfully published PARTICIPANT_JOINED for {} to {}",
                                user_id_clone, subject_for_log
                            );
                        }
                    } else {
                        info!(
                            "Skipping PARTICIPANT_JOINED for observer {} in room {}",
                            user_id_clone, room_clone
                        );
                    }

                    // Send PARTICIPANT_JOINED for each existing member directly to the new joiner.
                    // This ensures the new joiner learns about all participants already in the room.
                    for (existing_sid, existing_uid, existing_display_name) in &existing_members {
                        let existing_bytes = SessionManager::build_peer_joined_packet(
                            &room_clone,
                            existing_uid,
                            *existing_sid,
                            existing_display_name,
                        );
                        info!(
                            "Sending existing PARTICIPANT_JOINED for {} (display={}) to new joiner {}",
                            existing_uid, existing_display_name, user_id_clone
                        );
                        if let Err(e) = new_joiner_recipient.try_send(Message {
                            msg: existing_bytes,
                            session: *existing_sid,
                        }) {
                            warn!(
                                "Failed to send existing PARTICIPANT_JOINED for {} to new joiner {}: {}",
                                existing_uid, user_id_clone, e
                            );
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "Error starting session for room {}: {} - rejecting join",
                        room_clone, e
                    );
                    // Session rejected - notify ChatServer to clean up active_subs
                    // This fixes the race condition where a failed start_session would
                    // leave a stale entry in active_subs, blocking future join attempts.
                    let _ = chat_server_addr.try_send(CleanupFailedJoin {
                        session: session_for_cleanup,
                        room: room_clone.clone(),
                    });
                    return;
                }
            }

            match nc2.queue_subscribe(subject, queue).await {
                Ok(mut sub) => {
                    while let Some(msg) = sub.next().await {
                        if let Err(e) = handle_msg(
                            session_recipient.clone(),
                            room_clone.clone(),
                            session_clone,
                        )(msg)
                        {
                            error!("Error handling message: {}", e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("{}", e)
                }
            }
        });

        self.active_subs.insert(session, handle);

        MessageResult(Ok(()))
    }
}

/// Handler for cleaning up failed join attempts.
/// When start_session fails inside the spawned task, it sends this message
/// to remove the stale entry from active_subs.
impl Handler<CleanupFailedJoin> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: CleanupFailedJoin, _ctx: &mut Self::Context) -> Self::Result {
        if let Some(task) = self.active_subs.remove(&msg.session) {
            // Abort the task (though it should already be finished/returned)
            task.abort();
            warn!(
                "Cleaned up failed join for session {} from active_subs",
                msg.session
            );
        }
        // Also remove from room_members since the join failed
        if let Some(members) = self.room_members.get_mut(&msg.room) {
            members.retain(|(sid, _, _)| *sid != msg.session);
            if members.is_empty() {
                self.room_members.remove(&msg.room);
            }
        }
    }
}

async fn send_meeting_info(
    nc: &async_nats::client::Client,
    room: &str,
    start_time_ms: u64,
    creator_id: &str,
) {
    let packet_bytes =
        SessionManager::build_meeting_started_packet(room, start_time_ms, creator_id);

    let subject = format!("room.{}.system", room.replace(' ', "_"));
    match nc.publish(subject.clone(), packet_bytes.into()).await {
        Ok(_) => info!("Sent meeting start time {} to {}", start_time_ms, subject),
        Err(e) => error!("Failed to send meeting info to room {}: {}", room, e),
    }
}

fn handle_msg(
    session_recipient: Recipient<Message>,
    room: String,
    session: SessionId,
) -> impl Fn(async_nats::Message) -> Result<(), std::io::Error> {
    move |msg| {
        if msg.subject == format!("room.{room}.{session}").replace(' ', "_").into() {
            // Self-skip prevents echo of our own broadcasts. However,
            // CONGESTION signals published on our subject by a congested
            // receiver must still be delivered — they are not echo.
            let is_congestion = PacketWrapper::parse_from_bytes(&msg.payload)
                .map(|pw| pw.packet_type == PacketType::CONGESTION.into())
                .unwrap_or(false);
            if !is_congestion {
                return Ok(());
            }
        }
        let message = Message {
            msg: msg.payload.to_vec(),
            session,
        };

        session_recipient.try_send(message).map_err(|e| {
            error!("error sending message to session {}: {}", session, e);
            std::io::Error::other(e)
        })
    }
}

// ==========================================================================
// Test helpers for ChatServer
// ==========================================================================
// These helpers need access to private ChatServer fields, so they must remain
// in the library crate. Integration tests in `tests/chat_server_tests.rs`
// import them via `sec_api::actors::chat_server::test_helpers`.
#[cfg(any(test, feature = "testing"))]
pub mod test_helpers {
    use super::super::session_logic::{ConnectionState, SessionId};
    use super::*;

    /// Message to query connection state for testing.
    #[derive(ActixMessage)]
    #[rtype(result = "Result<ConnectionState, ()>")]
    pub struct GetConnectionState {
        pub session: SessionId,
    }

    impl Handler<GetConnectionState> for ChatServer {
        type Result = Result<ConnectionState, ()>;

        fn handle(&mut self, msg: GetConnectionState, _ctx: &mut Self::Context) -> Self::Result {
            Ok(self
                .connection_states
                .get(&msg.session)
                .copied()
                .unwrap_or(ConnectionState::Testing))
        }
    }
}
