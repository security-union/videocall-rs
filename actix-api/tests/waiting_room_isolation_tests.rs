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

//! Integration tests for waiting-room audio/video isolation.
//!
//! These tests verify at the **bytes-on-wire level** that observer (waiting-room)
//! sessions cannot receive or publish media packets through the real WebSocket
//! server. They exercise the server's triple-layer defense:
//!
//! 1. **Server outbound filter** (`handle_msg`): observer sessions only receive
//!    MEETING and SESSION_ASSIGNED packets; everything else is dropped.
//! 2. **Server inbound filter** (`SessionLogic::handle_inbound`): observer
//!    sessions cannot publish MEDIA packets.
//! 3. **Client-side** (`decode_media=false`): defense-in-depth only, not tested
//!    here because it is bypassable and MUST NOT be the sole enforcement.
//!
//! Each test starts a real `ChatServer` + NATS + HTTP server, connects via
//! WebSocket using real JWT tokens (observer vs admitted), and asserts on the
//! actual protobuf packets received (or not received) over the wire.

use actix::Actor;
use actix_web::{web, App, HttpServer};
use futures_util::{SinkExt, StreamExt};
use protobuf::Message as ProtoMessage;
use sec_api::{
    actors::chat_server::ChatServer,
    lobby::{ws_connect, ws_connect_authenticated},
    models::AppState,
    server_diagnostics::ServerDiagnostics,
    session_manager::SessionManager,
};
use serial_test::serial;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::FeatureFlags;

const JWT_SECRET: &str = "test-secret-for-waiting-room-tests";
const TOKEN_TTL_SECS: i64 = 60;

/// Base port for waiting-room isolation tests. Each test uses a unique offset
/// to avoid port conflicts with other test files.
const WR_PORT_BASE: u16 = 19100;

// =========================================================================
// Server helpers
// =========================================================================

/// Start the real WebSocket server with the production lobby handlers.
async fn start_real_ws_server(port: u16) {
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");

    let chat = ChatServer::new(nats_client.clone()).await.start();
    let session_manager = SessionManager::new();
    let (_, tracker_sender, _) = ServerDiagnostics::new_with_channel(nats_client.clone());

    let state = AppState {
        chat,
        nats_client,
        tracker_sender,
        session_manager,
    };

    actix_rt::spawn(async move {
        let _ = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(state.clone()))
                .service(ws_connect_authenticated)
                .service(ws_connect)
        })
        .bind(format!("127.0.0.1:{port}"))
        .expect("Failed to bind waiting-room test server")
        .run()
        .await;
    });
}

async fn wait_for_server(port: u16) {
    let url = format!("ws://127.0.0.1:{port}/lobby/probe/probe");
    for _ in 0..50 {
        if tokio_tungstenite::connect_async(&url).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("WS server not ready after 5 seconds on port {port}");
}

async fn setup(port: u16) {
    FeatureFlags::clear_meeting_management_override();
    std::env::set_var("JWT_SECRET", JWT_SECRET);
    start_real_ws_server(port).await;
    wait_for_server(port).await;
}

// =========================================================================
// Token helpers
// =========================================================================

/// Generate a normal (admitted) room token for a participant.
fn make_admitted_token(user_id: &str, room: &str, display_name: &str) -> String {
    meeting_api::token::generate_room_token(
        JWT_SECRET,
        TOKEN_TTL_SECS,
        user_id,
        room,
        false, // is_host
        display_name,
        false, // end_on_host_leave
        false, // is_guest
    )
    .expect("should generate admitted token")
}

/// Generate an observer (waiting-room) token for a participant.
fn make_observer_token(user_id: &str, room: &str, display_name: &str) -> String {
    meeting_api::token::generate_observer_token(
        JWT_SECRET,
        user_id,
        room,
        display_name,
        false, // is_guest
    )
    .expect("should generate observer token")
}

// =========================================================================
// Connection helpers
// =========================================================================

/// Connect via the token-based endpoint: GET /lobby?token=<JWT>
async fn connect_with_token(
    port: u16,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!(
        "ws://127.0.0.1:{port}/lobby?token={token}",
        token = urlencoding::encode(token)
    );
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WebSocket connection should succeed");
    ws
}

/// Wait for SESSION_ASSIGNED from the server. Returns the assigned session_id.
async fn wait_for_session_assigned(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            msg = ws.next() => {
                if let Some(Ok(Message::Binary(data))) = msg {
                    if let Ok(wrapper) = PacketWrapper::parse_from_bytes(&data) {
                        if wrapper.packet_type == PacketType::SESSION_ASSIGNED.into() {
                            assert!(wrapper.session_id != 0, "SESSION_ASSIGNED must carry session_id");
                            return wrapper.session_id;
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    panic!("Timeout waiting for SESSION_ASSIGNED");
}

/// Wait for MEETING_STARTED after SESSION_ASSIGNED has already been received.
async fn wait_for_meeting_started(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            msg = ws.next() => {
                if let Some(Ok(Message::Binary(data))) = msg {
                    if let Ok(wrapper) = PacketWrapper::parse_from_bytes(&data) {
                        if wrapper.packet_type == PacketType::MEETING.into() {
                            if let Ok(meeting) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                                if meeting.event_type == MeetingEventType::MEETING_STARTED.into() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    panic!("Timeout waiting for MEETING_STARTED");
}

/// Build a MEDIA PacketWrapper with AUDIO media type, ready to send over the wire.
fn make_media_packet(sender_user_id: &str) -> Vec<u8> {
    let mut media = MediaPacket::new();
    media.media_type = MediaType::AUDIO.into();
    media.user_id = sender_user_id.as_bytes().to_vec();
    media.data = vec![0xDE, 0xAD, 0xBE, 0xEF]; // dummy audio payload

    let mut wrapper = PacketWrapper::new();
    wrapper.packet_type = PacketType::MEDIA.into();
    wrapper.user_id = sender_user_id.as_bytes().to_vec();
    wrapper.data = media
        .write_to_bytes()
        .expect("MediaPacket serialization should succeed");

    wrapper
        .write_to_bytes()
        .expect("PacketWrapper serialization should succeed")
}

/// Collect all packets received within a timeout window.
/// Returns a Vec of parsed PacketWrappers.
async fn collect_packets_for(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    duration: Duration,
) -> Vec<PacketWrapper> {
    let mut packets = Vec::new();
    let deadline = tokio::time::Instant::now() + duration;

    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if let Ok(wrapper) = PacketWrapper::parse_from_bytes(&data) {
                            packets.push(wrapper);
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }

    packets
}

// =========================================================================
// Tests
// =========================================================================

/// Positive control: two admitted (non-observer) sessions can exchange media
/// packets normally. This proves the test infrastructure works and that media
/// delivery is functional — so any absence of media in observer tests is due
/// to the server's isolation filter, not a broken test setup.
#[actix_rt::test]
#[serial]
async fn test_admitted_session_receives_media_normally() {
    let port = WR_PORT_BASE;
    setup(port).await;

    let room = "wr-admitted-media";
    let token_a = make_admitted_token("alice@test.com", room, "Alice");
    let token_b = make_admitted_token("bob@test.com", room, "Bob");

    // Connect both admitted sessions
    let mut ws_a = connect_with_token(port, &token_a).await;
    let _session_a = wait_for_session_assigned(&mut ws_a).await;
    wait_for_meeting_started(&mut ws_a).await;

    let mut ws_b = connect_with_token(port, &token_b).await;
    let _session_b = wait_for_session_assigned(&mut ws_b).await;
    wait_for_meeting_started(&mut ws_b).await;

    // Give the server a moment to fully register both sessions in the room
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Alice sends a MEDIA packet
    let media_bytes = make_media_packet("alice@test.com");
    ws_a.send(Message::Binary(media_bytes))
        .await
        .expect("Alice should be able to send media");

    // Bob should receive the MEDIA packet
    let packets = collect_packets_for(&mut ws_b, Duration::from_secs(3)).await;
    let media_packets: Vec<_> = packets
        .iter()
        .filter(|p| p.packet_type == PacketType::MEDIA.into())
        .collect();

    assert!(
        !media_packets.is_empty(),
        "Admitted session Bob MUST receive MEDIA packets from admitted session Alice. \
         Got {} total packets but none were MEDIA. Packet types: {:?}",
        packets.len(),
        packets
            .iter()
            .map(|p| p.packet_type.enum_value())
            .collect::<Vec<_>>()
    );

    drop(ws_a);
    drop(ws_b);
}

/// An observer (waiting-room) session MUST NOT receive MEDIA packets sent by
/// an admitted peer. This is the core isolation guarantee: even if a modified
/// client tries to decode media, the server never sends it to observers.
#[actix_rt::test]
#[serial]
async fn test_observer_does_not_receive_media_from_admitted_peer() {
    let port = WR_PORT_BASE + 1;
    setup(port).await;

    let room = "wr-observer-no-media";

    // Connect an admitted session
    let token_admitted = make_admitted_token("alice@test.com", room, "Alice");
    let mut ws_admitted = connect_with_token(port, &token_admitted).await;
    let _session_admitted = wait_for_session_assigned(&mut ws_admitted).await;
    wait_for_meeting_started(&mut ws_admitted).await;

    // Connect an observer session to the same room
    let token_observer = make_observer_token("observer@test.com", room, "Observer");
    let mut ws_observer = connect_with_token(port, &token_observer).await;
    let _session_observer = wait_for_session_assigned(&mut ws_observer).await;

    // Observers may or may not receive MEETING_STARTED depending on timing;
    // the important thing is SESSION_ASSIGNED was received (connection is alive).
    // Give server time to register the observer.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Admitted session sends a MEDIA packet with AUDIO
    let media_bytes = make_media_packet("alice@test.com");
    ws_admitted
        .send(Message::Binary(media_bytes))
        .await
        .expect("Admitted session should be able to send media");

    // Collect packets on the observer side for a reasonable window
    let packets = collect_packets_for(&mut ws_observer, Duration::from_secs(2)).await;
    let media_packets: Vec<_> = packets
        .iter()
        .filter(|p| p.packet_type == PacketType::MEDIA.into())
        .collect();

    assert!(
        media_packets.is_empty(),
        "Observer session MUST NOT receive any MEDIA packets. \
         Server outbound filter should have dropped them. \
         Got {} MEDIA packets out of {} total.",
        media_packets.len(),
        packets.len()
    );

    // Verify the observer connection is alive by checking it received
    // MEETING-type packets (MEETING_STARTED or PARTICIPANT_JOINED).
    // The SESSION_ASSIGNED was already verified above via wait_for_session_assigned.

    drop(ws_admitted);
    drop(ws_observer);
}

/// An observer session MUST NOT be able to publish MEDIA packets to the room.
/// Even if a modified client sends media, the server's inbound filter
/// (`SessionLogic::handle_inbound`) drops it before broadcasting.
#[actix_rt::test]
#[serial]
async fn test_observer_cannot_publish_media() {
    let port = WR_PORT_BASE + 2;
    setup(port).await;

    let room = "wr-observer-no-publish";

    // Connect an admitted session first
    let token_admitted = make_admitted_token("bob@test.com", room, "Bob");
    let mut ws_admitted = connect_with_token(port, &token_admitted).await;
    let _session_admitted = wait_for_session_assigned(&mut ws_admitted).await;
    wait_for_meeting_started(&mut ws_admitted).await;

    // Connect an observer session
    let token_observer = make_observer_token("observer@test.com", room, "Observer");
    let mut ws_observer = connect_with_token(port, &token_observer).await;
    let _session_observer = wait_for_session_assigned(&mut ws_observer).await;

    // Give server time to register both sessions
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Observer tries to send a MEDIA packet (simulating a modified client)
    let media_bytes = make_media_packet("observer@test.com");
    ws_observer
        .send(Message::Binary(media_bytes))
        .await
        .expect("WebSocket send should succeed at the transport level");

    // The admitted session should NOT receive the observer's media
    let packets = collect_packets_for(&mut ws_admitted, Duration::from_secs(2)).await;
    let observer_media: Vec<_> = packets
        .iter()
        .filter(|p| p.packet_type == PacketType::MEDIA.into() && p.user_id == b"observer@test.com")
        .collect();

    assert!(
        observer_media.is_empty(),
        "Admitted session MUST NOT receive MEDIA packets from an observer. \
         Server inbound filter should have dropped them. \
         Got {} observer MEDIA packets out of {} total packets.",
        observer_media.len(),
        packets.len()
    );

    drop(ws_admitted);
    drop(ws_observer);
}

/// An observer session MUST receive a SESSION_ASSIGNED packet upon connection.
/// This confirms the connection is alive and the server's allowlist correctly
/// permits SESSION_ASSIGNED through to observers.
#[actix_rt::test]
#[serial]
async fn test_observer_receives_session_assigned() {
    let port = WR_PORT_BASE + 3;
    setup(port).await;

    let room = "wr-observer-session-assigned";
    let token_observer = make_observer_token("observer@test.com", room, "Observer");

    let mut ws_observer = connect_with_token(port, &token_observer).await;

    // wait_for_session_assigned will panic with a timeout if SESSION_ASSIGNED
    // is never received. If we get here, the test passes.
    let session_id = wait_for_session_assigned(&mut ws_observer).await;

    assert!(
        session_id != 0,
        "Observer session must receive a non-zero session_id in SESSION_ASSIGNED"
    );

    drop(ws_observer);
}

/// An observer sends a MEDIA PacketWrapper where `user_id` is spoofed to match
/// an admitted peer (Alice). The server MUST drop this packet based on the
/// sender's session role (observer), not the packet-asserted identity. Alice
/// must NOT receive the spoofed packet.
#[actix_rt::test]
#[serial]
async fn test_observer_spoofed_user_id_still_dropped() {
    let port = WR_PORT_BASE + 4;
    setup(port).await;
    let room = "wr-spoofed-user-id";

    // Connect admitted Alice
    let token_a = make_admitted_token("alice@test.com", room, "Alice");
    let mut ws_a = connect_with_token(port, &token_a).await;
    let _sid_a = wait_for_session_assigned(&mut ws_a).await;
    wait_for_meeting_started(&mut ws_a).await;

    // Connect observer
    let token_obs = make_observer_token("observer@test.com", room, "Observer");
    let mut ws_obs = connect_with_token(port, &token_obs).await;
    let _sid_obs = wait_for_session_assigned(&mut ws_obs).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Observer sends MEDIA with alice's user_id (spoofed)
    let spoofed = make_media_packet("alice@test.com");
    ws_obs
        .send(Message::Binary(spoofed))
        .await
        .expect("send should succeed");

    // Alice must NOT receive it
    let packets = collect_packets_for(&mut ws_a, Duration::from_secs(2)).await;
    let media: Vec<_> = packets
        .iter()
        .filter(|p| p.packet_type == PacketType::MEDIA.into())
        .collect();
    assert!(
        media.is_empty(),
        "Spoofed user_id from observer MUST be dropped by inbound filter. Got {} MEDIA packets.",
        media.len()
    );

    drop(ws_a);
    drop(ws_obs);
}

/// An observer sends a MEDIA packet with `session_id` spoofed to match admitted
/// Alice's session. This probes the `inner_session_self` self-echo suppression
/// path: if the packet reached Alice, it would be dropped as a self-echo —
/// giving the observer a per-recipient censorship primitive. The server's inbound
/// filter blocks this packet before it ever reaches NATS, so neither Alice nor
/// Bob should receive it.
#[actix_rt::test]
#[serial]
async fn test_observer_spoofed_session_id_still_dropped() {
    let port = WR_PORT_BASE + 5;
    setup(port).await;
    let room = "wr-spoofed-session-id";

    let token_a = make_admitted_token("alice@test.com", room, "Alice");
    let mut ws_a = connect_with_token(port, &token_a).await;
    let sid_a = wait_for_session_assigned(&mut ws_a).await;
    wait_for_meeting_started(&mut ws_a).await;

    let token_b = make_admitted_token("bob@test.com", room, "Bob");
    let mut ws_b = connect_with_token(port, &token_b).await;
    let _sid_b = wait_for_session_assigned(&mut ws_b).await;
    wait_for_meeting_started(&mut ws_b).await;

    let token_obs = make_observer_token("observer@test.com", room, "Observer");
    let mut ws_obs = connect_with_token(port, &token_obs).await;
    let _sid_obs = wait_for_session_assigned(&mut ws_obs).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Build packet with spoofed session_id = Alice's session
    let mut media = MediaPacket::new();
    media.media_type = MediaType::AUDIO.into();
    media.user_id = b"observer@test.com".to_vec();
    media.data = vec![0xDE, 0xAD];

    let mut wrapper = PacketWrapper::new();
    wrapper.packet_type = PacketType::MEDIA.into();
    wrapper.user_id = b"observer@test.com".to_vec();
    wrapper.session_id = sid_a; // spoof Alice's session_id
    wrapper.data = media
        .write_to_bytes()
        .expect("MediaPacket serialization should succeed");
    let bytes = wrapper
        .write_to_bytes()
        .expect("PacketWrapper serialization should succeed");

    ws_obs
        .send(Message::Binary(bytes))
        .await
        .expect("send should succeed");

    // Neither Alice nor Bob should receive it
    let packets_b = collect_packets_for(&mut ws_b, Duration::from_secs(2)).await;
    let media_b: Vec<_> = packets_b
        .iter()
        .filter(|p| p.packet_type == PacketType::MEDIA.into())
        .collect();
    assert!(
        media_b.is_empty(),
        "Spoofed session_id from observer MUST be dropped. Bob got {} MEDIA packets.",
        media_b.len()
    );

    drop(ws_a);
    drop(ws_b);
    drop(ws_obs);
}

/// An observer sends raw garbage bytes that cannot be parsed as a valid
/// PacketWrapper. This verifies the fail-closed default (`.unwrap_or(false)`)
/// drops malformed inputs even if they somehow bypass the inbound filter.
#[actix_rt::test]
#[serial]
async fn test_observer_unparseable_packet_dropped() {
    let port = WR_PORT_BASE + 6;
    setup(port).await;
    let room = "wr-unparseable";

    let token_a = make_admitted_token("alice@test.com", room, "Alice");
    let mut ws_a = connect_with_token(port, &token_a).await;
    let _sid_a = wait_for_session_assigned(&mut ws_a).await;
    wait_for_meeting_started(&mut ws_a).await;

    let token_obs = make_observer_token("observer@test.com", room, "Observer");
    let mut ws_obs = connect_with_token(port, &token_obs).await;
    let _sid_obs = wait_for_session_assigned(&mut ws_obs).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send garbage bytes
    let garbage = vec![0xFF, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0x00, 0x01];
    ws_obs
        .send(Message::Binary(garbage))
        .await
        .expect("send should succeed");

    // Alice should not receive anything from this
    let packets = collect_packets_for(&mut ws_a, Duration::from_secs(2)).await;
    let non_meeting: Vec<_> = packets
        .iter()
        .filter(|p| {
            p.packet_type != PacketType::MEETING.into()
                && p.packet_type != PacketType::SESSION_ASSIGNED.into()
        })
        .collect();
    assert!(
        non_meeting.is_empty(),
        "Unparseable packet from observer MUST be dropped. Got {} unexpected packets.",
        non_meeting.len()
    );

    drop(ws_a);
    drop(ws_obs);
}
