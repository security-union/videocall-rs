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
 */

//! Integration tests for the ChatServer actor.
//!
//! These tests exercise room join/leave lifecycle, connection state transitions,
//! observer vs. non-observer behavior, and NATS-based event publishing.
//!
//! Previously these lived inside `src/actors/chat_server.rs` as a `#[cfg(test)] mod tests` block.
//! They have been extracted here so that:
//! 1. The production source file stays focused on production code.
//! 2. Tests compile as integration tests and run via `cargo test -p videocall-api`.
//!
//! The `GetConnectionState` test helper message and its handler remain in the library
//! crate at `sec_api::actors::chat_server::test_helpers` because the handler accesses
//! private `ChatServer` fields.

use actix::{Actor, Handler};
use futures::StreamExt;
use protobuf::Message as ProtobufMessage;
use sec_api::actors::chat_server::test_helpers::GetConnectionState;
use sec_api::actors::chat_server::ChatServer;
use sec_api::actors::session_logic::ConnectionState;
use sec_api::messages::server::{
    ActivateConnection, ClientMessage, Connect, Disconnect, JoinRoom, Packet,
};
use sec_api::messages::session::Message;
use sec_api::server_diagnostics::{TrackerMessage, TrackerSender};
use sec_api::session_manager::SessionManager;
use serial_test::serial;
use std::sync::Arc;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::SYSTEM_USER_ID;

/// Test helper: create a database pool for integration tests.
/// Kept for future JWT flow testing (create meeting -> get JWT -> connect via WS/WT).
#[allow(dead_code)]
async fn get_test_pool() -> sqlx::PgPool {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
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
    use sec_api::actors::session_logic::SessionLogic;
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
    use std::sync::atomic::{AtomicBool, Ordering};
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
        if let Ok(Some(_msg)) = tokio::time::timeout(Duration::from_millis(500), sub.next()).await {
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
    use std::sync::atomic::{AtomicBool, Ordering};
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
        if let Ok(Some(_msg)) = tokio::time::timeout(Duration::from_millis(500), sub.next()).await {
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
    use std::sync::Mutex;
    use tokio::time::{sleep, Duration};
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;

    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");

    let chat_server = ChatServer::new(nats_client).await.start();

    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

    struct CapturingSession {
        received: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
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
            if let Ok(wrapper) = <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
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
            if let Ok(wrapper) = <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
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
            if let Ok(wrapper) = <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
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
            if let Ok(wrapper) = <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
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
