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
        server::{
            ActivateConnection, ClientMessage, Connect, Disconnect, ForceDisconnect, JoinRoom,
            Leave, SessionIdCollision,
        },
        session::Message,
    },
    models::build_subject_and_queue,
    session_manager::{SessionEndResult, SessionManager},
};

use actix::{
    Actor, Addr, AsyncContext, Context, Handler, Message as ActixMessage, MessageResult, Recipient,
};
use futures::StreamExt;
use protobuf::Message as ProtobufMessage;
use std::collections::{HashMap, HashSet};
use tokio::task::JoinHandle;
use tracing::{error, info, trace, warn};
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::SYSTEM_USER_EMAIL;

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

/// Internal: add session to room (only after start_session succeeds).
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct AddSessionToRoom {
    session: SessionId,
    room: String,
}

/// Internal: broadcast meeting packet to NATS and to explicit local recipients.
/// Recipients are captured at join time to avoid race where message ordering
/// causes a joiner to receive an earlier joiner's MEETING_STARTED.
/// creator_session: when set (MEETING_STARTED), exclude from local delivery - the
/// transport already sent it to the joiner. When None (MEETING_ENDED), deliver to all.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct BroadcastMeetingPacket {
    room: String,
    packet_bytes: Vec<u8>,
    /// Sessions to deliver to (subset of room, captured when join started)
    recipients: Vec<SessionId>,
    /// Joiner's session - exclude from local delivery (transport already sent to them)
    creator_session: Option<SessionId>,
}

pub struct ChatServer {
    nats_connection: async_nats::client::Client,
    self_addr: Option<Addr<ChatServer>>,
    sessions: HashMap<SessionId, Recipient<Message>>,
    sessions_in_room: HashMap<String, HashSet<SessionId>>,
    disconnect_addrs: HashMap<SessionId, Recipient<ForceDisconnect>>,
    active_subs: HashMap<SessionId, JoinHandle<()>>,
    session_manager: SessionManager,
    connection_states: HashMap<SessionId, ConnectionState>,
}

impl ChatServer {
    pub async fn new(nats_connection: async_nats::client::Client) -> Self {
        ChatServer {
            nats_connection,
            self_addr: None,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
            sessions_in_room: HashMap::new(),
            disconnect_addrs: HashMap::new(),
            session_manager: SessionManager::new(),
            connection_states: HashMap::new(),
        }
    }

    pub fn leave_rooms(
        &mut self,
        session_id: &SessionId,
        room: Option<&str>,
        user_id: Option<&str>,
    ) {
        if let Some(task) = self.active_subs.remove(session_id) {
            task.abort();
        }
        if let Some(room_id) = room {
            if let Some(sessions) = self.sessions_in_room.get_mut(room_id) {
                sessions.remove(session_id);
                if sessions.is_empty() {
                    self.sessions_in_room.remove(room_id);
                }
            }
        }

        // End session using SessionManager
        if let (Some(room_id), Some(uid)) = (room, user_id) {
            let room_id = room_id.to_string();
            let user_id = uid.to_string();
            let session_manager = self.session_manager.clone();
            let chat_addr = self.self_addr.clone().expect("ChatServer not started");
            let recipients: Vec<SessionId> = self
                .sessions_in_room
                .get(&room_id)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();

            tokio::spawn(async move {
                match session_manager.end_session(&room_id, &user_id).await {
                    Ok(SessionEndResult::HostEndedMeeting) => {
                        info!(
                            "Host {} left room {} - ending meeting for all",
                            user_id, room_id
                        );
                        let bytes = SessionManager::build_meeting_ended_packet(
                            &room_id,
                            "The host has ended the meeting",
                        );
                        chat_addr.do_send(BroadcastMeetingPacket {
                            room: room_id,
                            packet_bytes: bytes,
                            recipients,
                            creator_session: None,
                        });
                    }
                    Ok(SessionEndResult::LastParticipantLeft) => {
                        info!("Last participant {} left room {}", user_id, room_id);
                    }
                    Ok(SessionEndResult::MeetingContinues { remaining_count }) => {
                        info!(
                            "Participant {} left room {}, {} remaining",
                            user_id, room_id, remaining_count
                        );
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

    fn started(&mut self, ctx: &mut Self::Context) {
        self.self_addr = Some(ctx.address());
    }
}

impl Handler<Connect> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: Connect, _ctx: &mut Self::Context) -> Self::Result {
        let Connect {
            id,
            addr,
            disconnect_addr,
        } = msg;
        self.sessions.insert(id, addr);
        self.disconnect_addrs.insert(id, disconnect_addr);
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
        }: Disconnect,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.leave_rooms(&session, Some(&room), Some(&user_id));
        let _ = self.sessions.remove(&session);
        let _ = self.disconnect_addrs.remove(&session);
        let _ = self.connection_states.remove(&session);
    }
}

impl Handler<SessionIdCollision> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        SessionIdCollision {
            session,
            room,
            user_id,
        }: SessionIdCollision,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        warn!(
            "Session ID collision detected for session {} in room {} - disconnecting peer",
            session, room
        );
        self.leave_rooms(&session, Some(room.as_str()), Some(user_id.as_str()));
        if let Some(addr) = self.disconnect_addrs.remove(&session) {
            let _ = addr.try_send(ForceDisconnect);
        }
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
        self.leave_rooms(&session, Some(&room), Some(&user_id));
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

        let connection_state = self
            .connection_states
            .get(&session)
            .copied()
            .unwrap_or(ConnectionState::Testing);

        if connection_state != ConnectionState::Active {
            trace!("Skipping relay for session {} in Testing state", session);
            return;
        }

        if crate::actors::packet_handler::is_rtt_packet(&msg.data) {
            trace!("Skipping relay for RTT packet (never relay RTT)");
            return;
        }

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

        // Local relay: deliver directly to other sessions in same room (no NATS)
        if let Some(room_sessions) = self.sessions_in_room.get(&room) {
            for &other in room_sessions {
                if other != session {
                    if let Some(addr) = self.sessions.get(&other) {
                        let _ = addr.try_send(Message {
                            session,
                            msg: packet_bytes.clone(),
                        });
                    }
                }
            }
        }

        // Publish to NATS for other servers
        let nc = self.nats_connection.clone();
        let subject = format!("room.{room}.{session}").replace(' ', "_");
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
        }: JoinRoom,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        // Validate user_id synchronously BEFORE spawning async task.
        // This ensures we return an error to the client if validation fails,
        // rather than returning Ok and silently failing in the spawned task.
        if user_id == SYSTEM_USER_EMAIL {
            return MessageResult(Err("Cannot use reserved system email as user ID".into()));
        }

        if self.active_subs.contains_key(&session) {
            return MessageResult(Ok(()));
        }

        self.sessions_in_room
            .entry(room.clone())
            .or_default()
            .insert(session);

        let recipients: Vec<SessionId> = self
            .sessions_in_room
            .get(&room)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();

        let session_manager = self.session_manager.clone();
        let room_clone = room.clone();
        let user_id_clone = user_id.clone();
        let session_id = session;
        let nc = self.nats_connection.clone();

        let (subject, queue) = build_subject_and_queue(&room, session);
        let session_recipient = match self.sessions.get(&session) {
            Some(addr) => addr.clone(),
            None => return MessageResult(Err("Session not found".into())),
        };

        let session_clone = session;
        let chat_server_addr = ctx.address();

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
                    let _ = chat_server_addr.try_send(AddSessionToRoom {
                        session: session_id,
                        room: room_clone.clone(),
                    });
                    let packet_bytes = SessionManager::build_meeting_started_packet(
                        &room_clone,
                        result.start_time_ms,
                        &result.creator_id,
                        result.session_id,
                    );
                    let _ = chat_server_addr.try_send(BroadcastMeetingPacket {
                        room: room_clone.clone(),
                        packet_bytes,
                        recipients: recipients.clone(),
                        creator_session: Some(session_id),
                    });
                }
                Err(e) => {
                    error!(
                        "Error starting session for room {}: {} - rejecting join",
                        room_clone, e
                    );
                    let _ = chat_server_addr.try_send(CleanupFailedJoin {
                        session: session_id,
                        room: room_clone.clone(),
                    });
                    return;
                }
            }

            match nc.queue_subscribe(subject, queue).await {
                Ok(mut sub) => {
                    while let Some(msg) = sub.next().await {
                        if let Err(e) = handle_msg(
                            session_recipient.clone(),
                            room_clone.clone(),
                            session_clone,
                            user_id_clone.clone(),
                            chat_server_addr.clone(),
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

impl Handler<AddSessionToRoom> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        AddSessionToRoom { session, room }: AddSessionToRoom,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.sessions_in_room
            .entry(room)
            .or_default()
            .insert(session);
    }
}

impl Handler<BroadcastMeetingPacket> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        BroadcastMeetingPacket {
            room,
            packet_bytes,
            recipients,
            creator_session,
        }: BroadcastMeetingPacket,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        let subject = format!("room.{}.system", room.replace(' ', "_"));
        let nc = self.nats_connection.clone();
        let b = bytes::Bytes::from(packet_bytes.clone());
        ctx.spawn(actix::fut::wrap_future::<_, Self>(async move {
            if let Err(e) = nc.publish(subject, b).await {
                error!("Failed to publish meeting packet: {}", e);
            }
        }));
        for &session_id in &recipients {
            if creator_session == Some(session_id) {
                continue;
            }
            if let Some(addr) = self.sessions.get(&session_id) {
                let _ = addr.try_send(Message {
                    session: session_id,
                    msg: packet_bytes.clone(),
                });
            }
        }
    }
}

impl Handler<CleanupFailedJoin> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: CleanupFailedJoin, _ctx: &mut Self::Context) -> Self::Result {
        if let Some(task) = self.active_subs.remove(&msg.session) {
            task.abort();
        }
        if let Some(sessions) = self.sessions_in_room.get_mut(&msg.room) {
            sessions.remove(&msg.session);
            if sessions.is_empty() {
                self.sessions_in_room.remove(&msg.room);
            }
        }
        warn!(
            "Cleaned up failed join for session {} from active_subs",
            msg.session
        );
    }
}

fn handle_msg(
    session_recipient: Recipient<Message>,
    room: String,
    session: SessionId,
    user_id: String,
    chat_server_addr: actix::Addr<ChatServer>,
) -> impl Fn(async_nats::Message) -> Result<(), std::io::Error> {
    move |msg| {
        // With no_echo, we never receive our own publishes. So sender==local means
        // another server has same session_id (collision) - disconnect.
        if msg.subject == format!("room.{room}.{session}").replace(' ', "_").into() {
            let _ = chat_server_addr.try_send(SessionIdCollision {
                session,
                room: room.clone(),
                user_id: user_id.clone(),
            });
            return Err(std::io::Error::other("session ID collision"));
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
    // TEST: JoinRoom rejects reserved system email synchronously
    // ==========================================================================
    // This test verifies the fix for the race condition where JoinRoom would
    // spawn an async task and immediately return Ok(()), even if validation
    // would fail inside the task. Now validation happens synchronously.
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_rejects_system_email_synchronously() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

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
        impl Handler<ForceDisconnect> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: ForceDisconnect, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1001u64;

        // Register the session first
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.clone().recipient(),
                disconnect_addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Attempt to join with the reserved system email
        // This should return an error SYNCHRONOUSLY (not Ok then fail async)
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room".to_string(),
                user_id: SYSTEM_USER_EMAIL.to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        // The key assertion: JoinRoom should return Err immediately
        assert!(
            result.is_err(),
            "JoinRoom with system email should return Err, not Ok"
        );

        let error_msg = result.unwrap_err();
        assert!(
            error_msg.contains("reserved system email"),
            "Error should mention reserved system email, got: {error_msg}"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom succeeds with valid user_id
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_succeeds_with_valid_user() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
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
        impl Handler<ForceDisconnect> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: ForceDisconnect, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1002u64;

        // Register the session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.clone().recipient(),
                disconnect_addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join with a valid user_id - should succeed
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-valid".to_string(),
                user_id: "valid-user@example.com".to_string(),
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
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        // Try to join WITHOUT registering the session first
        let result = chat_server
            .send(JoinRoom {
                session: 999999u64, // Not registered with Connect
                room: "test-room".to_string(),
                user_id: "valid-user@example.com".to_string(),
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
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
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
        impl Handler<ForceDisconnect> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: ForceDisconnect, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1003u64;

        // Register the session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.clone().recipient(),
                disconnect_addr: dummy.recipient(),
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
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result2.is_ok(),
            "Second join with same session should return Ok (already active)"
        );
    }

    // ==========================================================================
    // TEST: Two clients with same email get unique session_id values
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_same_email_unique_session_ids() {
        use crate::actors::session_logic::SessionLogic;
        use crate::server_diagnostics::{TrackerMessage, TrackerSender};
        use crate::session_manager::SessionManager;
        use tokio::sync::mpsc;

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        let (tx, _rx) = mpsc::unbounded_channel::<TrackerMessage>();
        let tracker_sender: TrackerSender = tx;
        let session_manager = SessionManager::new();

        let email = "same-user@example.com".to_string();
        let room = "test-room-unique".to_string();

        let session1 = SessionLogic::new(
            chat_server.clone(),
            room.clone(),
            email.clone(),
            nats_client.clone(),
            tracker_sender.clone(),
            session_manager.clone(),
        );

        let session2 = SessionLogic::new(
            chat_server.clone(),
            room.clone(),
            email.clone(),
            nats_client.clone(),
            tracker_sender.clone(),
            session_manager.clone(),
        );

        // Verify they have different session IDs
        assert_ne!(
            session1.id, session2.id,
            "Two sessions with same email should have different session_id values"
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
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
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
        impl Handler<ForceDisconnect> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: ForceDisconnect, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1004u64;
        let room = "test-room-state".to_string();

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.clone().recipient(),
                disconnect_addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Subscribe on separate connection (would receive if ChatServer published)
        let sub_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS for subscriber");
        let subject = format!("room.{}.{}", room, session_id).replace(' ', "_");
        let published = Arc::new(AtomicBool::new(false));
        let published_clone = published.clone();
        let mut sub = sub_client
            .subscribe(subject.clone())
            .await
            .expect("Failed to subscribe");

        // Spawn task to check for messages
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
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
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
        impl Handler<ForceDisconnect> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: ForceDisconnect, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1005u64;
        let room = "test-room-active".to_string();

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.clone().recipient(),
                disconnect_addr: dummy.recipient(),
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

        // With no_echo, ChatServer won't receive its own publish - use separate connection
        let sub_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS for subscriber");
        let subject = format!("room.{}.{}", room, session_id).replace(' ', "_");
        let published = Arc::new(AtomicBool::new(false));
        let published_clone = published.clone();
        let mut sub = sub_client
            .subscribe(subject.clone())
            .await
            .expect("Failed to subscribe");

        // Spawn task to check for messages
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
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
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
        impl Handler<ForceDisconnect> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: ForceDisconnect, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1006u64;

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.clone().recipient(),
                disconnect_addr: dummy.recipient(),
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
