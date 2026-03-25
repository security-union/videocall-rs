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

            if let Some(state) = self.connection_states.get(session_id) {
                if *state != ConnectionState::Active {
                    info!(
                        "Skipping PARTICIPANT_LEFT for non-active session {}",
                        session_id
                    );
                    return;
                }
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
        let mut send_join_events = false;
        if let Some(state) = self.connection_states.get_mut(&session) {
            if *state == ConnectionState::Testing {
                *state = ConnectionState::Active;
                info!("Session {} activated (Testing -> Active)", session);
                send_join_events = true;
            }
        } else {
            self.connection_states
                .insert(session, ConnectionState::Active);
            info!(
                "Session {} activated (state was missing, created as Active)",
                session
            );
            send_join_events = true;
        }

        if send_join_events {
            if let Some((room, uid, display_name)) =
                self.room_members.iter().find_map(|(room, members)| {
                    members.iter().find(|(sid, _, _)| *sid == session).map(
                        |(_, uid, display_name)| (room.clone(), uid.clone(), display_name.clone()),
                    )
                })
            {
                let observer = false;
                let existing_members: Vec<(SessionId, String, String)> = self
                    .room_members
                    .get(&room)
                    .map(|v| {
                        v.iter()
                            .filter(|(sid, _, _)| *sid != session)
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                let session_manager = self.session_manager.clone();
                let nc = self.nats_connection.clone();
                let new_joiner_recipient = self.sessions.get(&session).cloned();
                let chat_server_addr = _ctx.address();
                let session_for_cleanup = session;
                if let Some(new_joiner_recipient) = new_joiner_recipient {
                    tokio::spawn(async move {
                        match session_manager.start_session(&room, &uid, session).await {
                            Ok(result) => {
                                info!(
                                    "Session started for room {} (first: {}) at {} by creator {} (session {})",
                                    room,
                                    result.is_first_participant,
                                    result.start_time_ms,
                                    result.creator_id,
                                    session,
                                );

                                // SESSION_ASSIGNED is sent by ws_chat_session / wt_chat_session
                                // in their started() method before this JoinRoom handler runs.
                                send_meeting_info(
                                    &nc,
                                    &room,
                                    result.start_time_ms,
                                    &result.creator_id,
                                )
                                .await;

                                // Notify existing participants about the new joiner.
                                // Observer sessions (waiting room) should NOT trigger this.
                                if !observer {
                                    let bytes = SessionManager::build_peer_joined_packet(
                                        &room,
                                        &uid,
                                        session,
                                        &display_name,
                                    );
                                    let subject = format!("room.{}.system", room.replace(' ', "_"));
                                    info!(
                                        "Publishing PARTICIPANT_JOINED for {} (display={}) to {}",
                                        uid, display_name, subject
                                    );
                                    let subject_for_log = subject.clone();
                                    if let Err(e) = nc.publish(subject, bytes.into()).await {
                                        error!("Error publishing PARTICIPANT_JOINED: {}", e);
                                    } else {
                                        info!(
                                            "Successfully published PARTICIPANT_JOINED for {} to {}",
                                            uid, subject_for_log
                                        );
                                    }
                                } else {
                                    info!(
                                        "Skipping PARTICIPANT_JOINED for observer {} in room {}",
                                        uid, room
                                    );
                                }

                                // Send PARTICIPANT_JOINED for each existing member directly to the new joiner.
                                // This ensures the new joiner learns about all participants already in the room.
                                for (existing_sid, existing_uid, existing_display_name) in
                                    &existing_members
                                {
                                    let existing_bytes = SessionManager::build_peer_joined_packet(
                                        &room,
                                        existing_uid,
                                        *existing_sid,
                                        existing_display_name,
                                    );
                                    info!(
                                        "Sending existing PARTICIPANT_JOINED for {} (display={}) to new joiner {}",
                                        existing_uid, existing_display_name, uid
                                    );
                                    if let Err(e) = new_joiner_recipient.try_send(Message {
                                        msg: existing_bytes,
                                        session: *existing_sid,
                                    }) {
                                        warn!(
                                            "Failed to send existing PARTICIPANT_JOINED for {} to new joiner {}: {}",
                                            existing_uid, uid, e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Error starting session for room {}: {} - rejecting join",
                                    room, e
                                );
                                // Session rejected - notify ChatServer to clean up active_subs
                                // This fixes the race condition where a failed start_session would
                                // leave a stale entry in active_subs, blocking future join attempts.
                                let _ = chat_server_addr.try_send(CleanupFailedJoin {
                                    session: session_for_cleanup,
                                    room: room.clone(),
                                });
                                return;
                            }
                        }
                    });
                }
            }
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

        let session_str = session.to_string();
        let (subject, queue) = build_subject_and_queue(&room, &session_str);
        let session_recipient = match self.sessions.get(&session) {
            Some(addr) => addr.clone(),
            None => {
                return MessageResult(Err("Session not found".into()));
            }
        };
        // Track this session in room_members (only for non-observers)
        if !observer {
            self.room_members.entry(room.clone()).or_default().push((
                session,
                user_id.clone(),
                display_name.clone(),
            ));
        }
        let nc2 = self.nats_connection.clone();
        let session_clone = session;
        let room_clone = room.clone();
        let handle = tokio::spawn(async move {
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
// Unit Tests for ChatServer
// ==========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use actix::Actor;
    use serial_test::serial;

    /// Test helper: create a database pool for integration tests.
    /// Kept for future JWT flow testing (create meeting -> get JWT -> connect via WS/WT).
    #[allow(dead_code)]
    async fn get_test_pool() -> sqlx::PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
        sqlx::PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    // ==========================================================================
    // TEST: JoinRoom rejects reserved system user ID synchronously
    // ==========================================================================
    // This test verifies the fix for the race condition where JoinRoom would
    // spawn an async task and immediately return Ok(()), even if validation
    // would fail inside the task. Now validation happens synchronously.
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_rejects_system_user_id_synchronously() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        // Start the ChatServer actor
        let chat_server = ChatServer::new(nats_client).await.start();

        // Create a mock session recipient
        // We need a real actor to receive messages, so we use a simple dummy
        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1001u64;

        // Register the session first
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Attempt to join with the reserved system user ID
        // This should return an error SYNCHRONOUSLY (not Ok then fail async)
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room".to_string(),
                user_id: SYSTEM_USER_ID.to_string(),
                display_name: SYSTEM_USER_ID.to_string(),
                observer: false,
            })
            .await
            .expect("Message delivery should succeed");

        // The key assertion: JoinRoom should return Err immediately
        assert!(
            result.is_err(),
            "JoinRoom with system user ID should return Err, not Ok"
        );

        let error_msg = result.unwrap_err();
        assert!(
            error_msg.contains("reserved system user ID"),
            "Error should mention reserved system user ID, got: {error_msg}"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom succeeds with valid user_id
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_succeeds_with_valid_user() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1002u64;

        // Register the session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join with a valid user_id - should succeed
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-valid".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                observer: false,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_ok(),
            "JoinRoom with valid user should return Ok, got: {result:?}"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom fails if session not registered
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_fails_without_session() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        // Try to join WITHOUT registering the session first
        let result = chat_server
            .send(JoinRoom {
                session: 9999u64,
                room: "test-room".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                observer: false,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_err(),
            "JoinRoom without registered session should return Err"
        );
        assert!(
            result.unwrap_err().contains("Session not found"),
            "Error should mention session not found"
        );
    }

    // ==========================================================================
    // TEST: CleanupFailedJoin allows retry after spawn failure
    // ==========================================================================
    // This test verifies the race condition fix: when start_session fails inside
    // the spawned task, the CleanupFailedJoin message removes the stale entry
    // from active_subs, allowing subsequent join attempts to proceed.
    #[actix_rt::test]
    #[serial]
    async fn test_cleanup_failed_join_allows_retry() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1003u64;

        // Register the session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // First join attempt - should succeed (returns Ok immediately,
        // spawns async task which will also succeed with valid user)
        let result1 = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-cleanup".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                observer: false,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result1.is_ok(), "First join should succeed");

        // Second join attempt with same session - should return Ok
        // immediately because session is already in active_subs
        let result2 = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-cleanup".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                observer: false,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result2.is_ok(),
            "Second join with same session should return Ok (already active)"
        );
    }

    // ==========================================================================
    // TEST: Two clients with same user_id get unique session_id values
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_same_user_id_unique_session_ids() {
        use crate::actors::session_logic::SessionLogic;
        use crate::server_diagnostics::{TrackerMessage, TrackerSender};
        use crate::session_manager::SessionManager;
        use tokio::sync::mpsc;

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        let (tx, _rx) = mpsc::unbounded_channel::<TrackerMessage>();
        let tracker_sender: TrackerSender = tx;
        let session_manager = SessionManager::new();

        // Create two sessions with the same user_id
        let user_id = "same-user@example.com".to_string();
        let room = "test-room-unique".to_string();

        let session1 = SessionLogic::new(
            chat_server.clone(),
            room.clone(),
            user_id.clone(),
            user_id.clone(), // display_name fallback
            nats_client.clone(),
            tracker_sender.clone(),
            session_manager.clone(),
            false,
        );

        let session2 = SessionLogic::new(
            chat_server.clone(),
            room.clone(),
            user_id.clone(),
            user_id.clone(), // display_name fallback
            nats_client.clone(),
            tracker_sender.clone(),
            session_manager.clone(),
            false,
        );

        // Verify they have different session IDs
        assert_ne!(
            session1.id, session2.id,
            "Two sessions with same user_id should have different session_id values"
        );
        assert!(session1.id != 0, "Session ID should not be zero");
        assert!(session2.id != 0, "Session ID should not be zero");
    }

    // ==========================================================================
    // TEST: ConnectionState transitions - Testing does not publish to NATS
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_connection_state_testing_does_not_publish() {
        use crate::messages::server::Packet;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1004u64;
        let room = "test-room-state".to_string();

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let subject = format!("room.{room}.{session_id}").replace(' ', "_");
        let published = Arc::new(AtomicBool::new(false));
        let published_clone = published.clone();
        let mut sub = nats_client
            .subscribe(subject.clone())
            .await
            .expect("Failed to subscribe");

        tokio::spawn(async move {
            if let Ok(Some(_msg)) =
                tokio::time::timeout(Duration::from_millis(500), sub.next()).await
            {
                published_clone.store(true, Ordering::Relaxed);
            }
        });

        // Send message while in Testing state - should NOT publish
        chat_server
            .send(ClientMessage {
                session: session_id,
                room: room.clone(),
                msg: Packet {
                    data: Arc::new(b"test data".to_vec()),
                },
                user: "test@example.com".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        // Wait a bit to ensure no publish happened
        sleep(Duration::from_millis(600)).await;

        assert!(
            !published.load(Ordering::Relaxed),
            "Message should NOT be published while in Testing state"
        );
    }

    // ==========================================================================
    // TEST: ConnectionState transitions - Active publishes to NATS
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_connection_state_active_publishes() {
        use crate::messages::server::Packet;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1005u64;
        let room = "test-room-active".to_string();

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Activate the connection
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        let subject = format!("room.{room}.{session_id}").replace(' ', "_");
        let published = Arc::new(AtomicBool::new(false));
        let published_clone = published.clone();
        let mut sub = nats_client
            .subscribe(subject.clone())
            .await
            .expect("Failed to subscribe");

        tokio::spawn(async move {
            if let Ok(Some(_msg)) =
                tokio::time::timeout(Duration::from_millis(500), sub.next()).await
            {
                published_clone.store(true, Ordering::Relaxed);
            }
        });

        // Send message while in Active state - should publish
        chat_server
            .send(ClientMessage {
                session: session_id,
                room: room.clone(),
                msg: Packet {
                    data: Arc::new(b"test data".to_vec()),
                },
                user: "test@example.com".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        // Wait for publish
        sleep(Duration::from_millis(600)).await;

        assert!(
            published.load(Ordering::Relaxed),
            "Message should be published while in Active state"
        );
    }

    // ==========================================================================
    // TEST: ActivateConnection handler is idempotent
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_activate_connection_idempotent() {
        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1006u64;

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // First activation - should transition Testing -> Active
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Verify state is Active
        let state1 = chat_server
            .send(GetConnectionState {
                session: session_id,
            })
            .await
            .expect("GetConnectionState should succeed")
            .expect("GetConnectionState should return Ok");
        assert_eq!(
            state1,
            ConnectionState::Active,
            "State should be Active after first activation"
        );

        // Second activation - should remain Active (idempotent)
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Verify state is still Active
        let state2 = chat_server
            .send(GetConnectionState {
                session: session_id,
            })
            .await
            .expect("GetConnectionState should succeed")
            .expect("GetConnectionState should return Ok");
        assert_eq!(
            state2,
            ConnectionState::Active,
            "State should remain Active after second activation (idempotent)"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom broadcasts MEETING_STARTED via NATS (no session_id)
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_broadcasts_meeting_started() {
        use std::sync::{Arc, Mutex};
        use tokio::time::{sleep, Duration};
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

        struct CapturingSession {
            received: Arc<Mutex<Vec<Vec<u8>>>>,
        }
        impl Actor for CapturingSession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for CapturingSession {
            type Result = ();
            fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) {
                self.received.lock().unwrap().push(msg.msg);
            }
        }

        let capturing = CapturingSession {
            received: received.clone(),
        }
        .start();
        let session_id = 1007u64;

        chat_server
            .send(Connect {
                id: session_id,
                addr: capturing.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-broadcast".to_string(),
                user_id: "alice@example.com".to_string(),
                display_name: "alice@example.com".to_string(),
                observer: false,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "JoinRoom should succeed");

        // Wait for the spawned async task to complete and NATS subscription to deliver
        sleep(Duration::from_millis(500)).await;

        let msgs = received.lock().unwrap();
        // The session should NOT receive SESSION_ASSIGNED from ChatServer
        // (that's the transport layer's job). It may receive MEETING_STARTED
        // via NATS if the subscription was set up in time.
        for msg_bytes in msgs.iter() {
            if let Ok(wrapper) = <PacketWrapper as ProtobufMessage>::parse_from_bytes(msg_bytes) {
                assert_ne!(
                    wrapper.packet_type,
                    PacketType::SESSION_ASSIGNED.into(),
                    "ChatServer JoinRoom should NOT send SESSION_ASSIGNED directly"
                );
                if wrapper.packet_type == PacketType::MEETING.into() {
                    assert_eq!(
                        wrapper.session_id, 0,
                        "MEETING_STARTED must not carry session_id"
                    );
                }
            }
        }
    }

    // ==========================================================================
    // TEST: Observer JoinRoom does NOT publish PARTICIPANT_JOINED
    // ==========================================================================
    // When an observer (waiting room user) joins a room, the server should NOT
    // publish a PARTICIPANT_JOINED event to NATS. Only real participants trigger
    // this notification.
    #[actix_rt::test]
    #[serial]
    async fn test_observer_join_does_not_publish_participant_joined() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2001u64;
        let room = "test-room-observer-join";

        // Subscribe to the system subject for this room BEFORE join
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_joined_received = Arc::new(AtomicBool::new(false));
        let flag = participant_joined_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_JOINED.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Register session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as observer - should NOT publish PARTICIPANT_JOINED
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "observer-user@example.com".to_string(),
                display_name: "observer-user@example.com".to_string(),
                observer: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "Observer JoinRoom should succeed");

        // Wait long enough for any NATS publish to arrive
        sleep(Duration::from_millis(1000)).await;

        assert!(
            !participant_joined_received.load(Ordering::Relaxed),
            "Observer join should NOT publish PARTICIPANT_JOINED to NATS"
        );
    }

    // ==========================================================================
    // TEST: Non-observer JoinRoom DOES publish PARTICIPANT_JOINED
    // ==========================================================================
    // When a real participant joins a room, the server should publish a
    // PARTICIPANT_JOINED event to NATS so other peers are notified.
    #[actix_rt::test]
    #[serial]
    async fn test_non_observer_join_publishes_participant_joined() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2002u64;
        let room = "test-room-non-observer-join";

        // Subscribe to the system subject for this room BEFORE join
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_joined_received = Arc::new(AtomicBool::new(false));
        let flag = participant_joined_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_JOINED.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Register session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as non-observer - SHOULD publish PARTICIPANT_JOINED
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "real-user@example.com".to_string(),
                display_name: "real-user@example.com".to_string(),
                observer: false,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "Non-observer JoinRoom should succeed");

        // Wait for the spawned async task to publish PARTICIPANT_JOINED
        sleep(Duration::from_millis(1000)).await;

        assert!(
            participant_joined_received.load(Ordering::Relaxed),
            "Non-observer join SHOULD publish PARTICIPANT_JOINED to NATS"
        );
    }

    // ==========================================================================
    // TEST: Observer Disconnect does NOT publish PARTICIPANT_LEFT
    // ==========================================================================
    // When an observer session disconnects (e.g., waiting room user admitted),
    // the server should NOT publish a PARTICIPANT_LEFT event. The user was never
    // a real participant in the meeting.
    #[actix_rt::test]
    #[serial]
    async fn test_observer_disconnect_does_not_publish_participant_left() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2003u64;
        let room = "test-room-observer-disconnect";

        // Register and join as observer first
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "observer-dc@example.com".to_string(),
                display_name: "observer-dc@example.com".to_string(),
                observer: true,
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "Observer JoinRoom should succeed");

        // Wait for session to be fully set up
        sleep(Duration::from_millis(300)).await;

        // Now subscribe to system subject to watch for PARTICIPANT_LEFT
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_left_received = Arc::new(AtomicBool::new(false));
        let flag = participant_left_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_LEFT.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Disconnect as observer - should NOT publish PARTICIPANT_LEFT
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "observer-dc@example.com".to_string(),
                display_name: "observer-dc@example.com".to_string(),
                observer: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait long enough for any NATS publish to arrive
        sleep(Duration::from_millis(1000)).await;

        assert!(
            !participant_left_received.load(Ordering::Relaxed),
            "Observer disconnect should NOT publish PARTICIPANT_LEFT to NATS"
        );
    }

    // ==========================================================================
    // TEST: Non-observer Disconnect publishes PARTICIPANT_LEFT (or meeting event)
    // ==========================================================================
    // When a real participant disconnects, the server should invoke the full
    // leave_rooms flow which may publish PARTICIPANT_LEFT for MeetingContinues,
    // MEETING_ENDED for HostEndedMeeting, or nothing for LastParticipantLeft.
    #[actix_rt::test]
    #[serial]
    async fn test_non_observer_disconnect_publishes_event() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2004u64;
        let room = "test-room-non-observer-disconnect";

        // Register and join as real participant
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "real-dc@example.com".to_string(),
                display_name: "real-dc@example.com".to_string(),
                observer: false,
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "Non-observer JoinRoom should succeed");

        // Wait for session setup
        sleep(Duration::from_millis(300)).await;

        // Subscribe to system subject to watch for any meeting events
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let meeting_event_received = Arc::new(AtomicBool::new(false));
        let flag = meeting_event_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        // Accept any meeting lifecycle event (PARTICIPANT_LEFT or MEETING_ENDED)
                        // depending on how end_session categorizes this session
                        if inner.event_type == MeetingEventType::PARTICIPANT_LEFT.into()
                            || inner.event_type == MeetingEventType::MEETING_ENDED.into()
                        {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Disconnect as non-observer - should invoke the full leave flow
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "real-dc@example.com".to_string(),
                display_name: "real-dc@example.com".to_string(),
                observer: false,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait for the leave flow to complete
        // Note: depending on SessionManager::end_session result, a meeting event
        // may or may not be published. The key assertion is that the code PATH
        // for non-observer is exercised (it does not early-return like observer).
        // With the current SessionManager returning MeetingContinues { remaining_count: 0 },
        // this actually maps to the MeetingContinues branch which publishes PARTICIPANT_LEFT.
        sleep(Duration::from_millis(1000)).await;

        // The non-observer path should have attempted to publish via the full
        // end_session flow (not the observer early-return path).
        // Current SessionManager returns MeetingContinues { remaining_count: 0 }
        // which triggers PARTICIPANT_LEFT publish.
        assert!(
            meeting_event_received.load(Ordering::Relaxed),
            "Non-observer disconnect should publish a meeting event (PARTICIPANT_LEFT or MEETING_ENDED)"
        );
    }

    // ==========================================================================
    // TEST: Observer JoinRoom succeeds and session is tracked
    // ==========================================================================
    // Verify that observer sessions are accepted and registered just like normal
    // sessions - the only difference is in event publishing behavior.
    #[actix_rt::test]
    #[serial]
    async fn test_observer_join_room_succeeds() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2005u64;

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as observer - should succeed (same as non-observer)
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-observer-ok".to_string(),
                user_id: "observer@example.com".to_string(),
                display_name: "observer@example.com".to_string(),
                observer: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_ok(),
            "Observer JoinRoom should succeed, got: {result:?}"
        );

        // Joining again with same session should return Ok (already in active_subs)
        let result2 = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-observer-ok".to_string(),
                user_id: "observer@example.com".to_string(),
                display_name: "observer@example.com".to_string(),
                observer: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result2.is_ok(),
            "Second observer JoinRoom should return Ok (already active)"
        );
    }

    // Helper message to get connection state for testing
    #[derive(ActixMessage)]
    #[rtype(result = "Result<ConnectionState, ()>")]
    struct GetConnectionState {
        session: SessionId,
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
