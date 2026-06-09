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

//! Integration tests for waiting-room observer isolation over WebTransport.
//!
//! These tests complement the WebSocket-only tests in
//! `waiting_room_isolation_tests.rs` by exercising the same triple-layer
//! isolation guarantees over the WebTransport protocol path. Critically,
//! they cover BOTH WT inbound entry points:
//!
//! * **UniStream** (bridge.rs `accept_uni()` → length-prefix framed): tested
//!   by `test_wt_observer_inbound_media_via_unistream_is_dropped`
//! * **Datagram** (bridge.rs `read_datagram()`): tested by
//!   `test_wt_observer_inbound_media_via_datagram_is_dropped`
//!
//! A positive-control test (`test_wt_admitted_receives_media_from_ws_admitted`)
//! first proves that cross-transport media relay works for admitted sessions,
//! so any absence of media in the observer tests is due to the isolation filter
//! — not a broken WT path.
//!
//! Each test starts a shared `ChatServer` actor with BOTH a WS server (for
//! admitted participants) and a WT server (for the observer/admitted WT peer),
//! proving end-to-end isolation across transport boundaries.
//!
//! ## Prerequisites
//!
//! * A running NATS server (set `NATS_URL` env var, defaults to
//!   `nats://nats:4222`)
//! * TLS certificates at `certs/localhost.key` and `certs/localhost.pem`
//!   (generate with `make e2e-cert`)

use actix::Actor;
use actix_web::{web, App, HttpServer};
use futures_util::{SinkExt, StreamExt};
use protobuf::Message as ProtoMessage;
use rustls::crypto::CryptoProvider;
use sec_api::{
    actors::chat_server::ChatServer,
    lobby::{ws_connect, ws_connect_authenticated},
    models::AppState,
    server_diagnostics::ServerDiagnostics,
    session_manager::SessionManager,
    webtransport::{self, Certs, WebTransportOpt},
};
use serial_test::serial;
use std::net::ToSocketAddrs;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::FeatureFlags;

const JWT_SECRET: &str = "test-secret-for-wt-isolation-tests";
const TOKEN_TTL_SECS: i64 = 60;

/// Base port for WS servers in WT isolation tests (admitted participants).
const WT_ISO_WS_PORT_BASE: u16 = 19300;
/// Base port for WT servers in WT isolation tests (observer).
const WT_ISO_WT_PORT_BASE: u16 = 19400;

// =========================================================================
// Server helpers
// =========================================================================

/// Start both a WS server and a WT server sharing a single `ChatServer` actor.
/// This ensures both transports are in the same room so cross-transport
/// isolation can be verified.
async fn setup_ws_and_wt_server(ws_port: u16, wt_port: u16) {
    FeatureFlags::clear_meeting_management_override();
    std::env::set_var("JWT_SECRET", JWT_SECRET);

    // Install crypto provider (idempotent if already installed).
    let _ = CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");

    // Single shared ChatServer actor.
    let chat = ChatServer::new(nats_client.clone()).await.start();
    let session_manager = SessionManager::new();
    let (_, tracker_sender, tracker_receiver) =
        ServerDiagnostics::new_with_channel(nats_client.clone());

    // Start tracker message loop.
    let nats_for_tracker = nats_client.clone();
    actix_rt::spawn(async move {
        let tracker = std::sync::Arc::new(ServerDiagnostics::new(nats_for_tracker));
        tracker.run_message_loop(tracker_receiver).await;
    });

    // --- WS server (shares the same ChatServer) ---
    let ws_state = AppState {
        chat: chat.clone(),
        nats_client: nats_client.clone(),
        tracker_sender: tracker_sender.clone(),
        session_manager: session_manager.clone(),
    };
    actix_rt::spawn(async move {
        let _ = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(ws_state.clone()))
                .service(ws_connect_authenticated)
                .service(ws_connect)
        })
        .bind(format!("127.0.0.1:{ws_port}"))
        .expect("Failed to bind WS server for WT isolation tests")
        .run()
        .await;
    });

    // --- WT server (shares the same ChatServer) ---
    let opt = WebTransportOpt {
        listen: format!("0.0.0.0:{wt_port}")
            .to_socket_addrs()
            .expect("valid WT listen address")
            .next()
            .expect("at least one socket address"),
        certs: Certs {
            key: std::env::var("KEY_PATH")
                .unwrap_or_else(|_| "certs/localhost.key".to_string())
                .into(),
            cert: std::env::var("CERT_PATH")
                .unwrap_or_else(|_| "certs/localhost.pem".to_string())
                .into(),
        },
    };
    actix_rt::spawn(async move {
        if let Err(e) =
            webtransport::start(opt, chat, nats_client, tracker_sender, session_manager).await
        {
            eprintln!("WT server error: {e}");
        }
    });

    // Wait for WS server readiness.
    wait_for_ws_server(ws_port).await;
    // Wait for WT server readiness.
    wait_for_wt_server(wt_port).await;
}

/// Poll until the WS server accepts connections.
async fn wait_for_ws_server(port: u16) {
    let url = format!("ws://127.0.0.1:{port}/lobby/probe/probe");
    for _ in 0..50 {
        if tokio_tungstenite::connect_async(&url).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("WS server not ready after 5 seconds on port {port}");
}

/// Poll until the WT server accepts connections.
async fn wait_for_wt_server(wt_port: u16) {
    // Use a throwaway token for the probe connection.
    let probe_token = make_admitted_token("probe@test.com", "probe-room", "Probe");
    for _ in 0..50 {
        if connect_wt_with_token(&probe_token, wt_port).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("WT server not ready after 10 seconds on port {wt_port}");
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
// WS connection helpers
// =========================================================================

/// Connect to the WS server via the token-based endpoint.
async fn ws_connect_with_token(
    port: u16,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!(
        "ws://127.0.0.1:{port}/lobby?token={token}",
        token = urlencoding::encode(token)
    );
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS connection should succeed");
    ws
}

/// Wait for SESSION_ASSIGNED from the WS server. Returns the assigned session_id.
async fn ws_wait_for_session_assigned(
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
    panic!("Timeout waiting for WS SESSION_ASSIGNED");
}

/// Wait for MEETING_STARTED after SESSION_ASSIGNED has already been received (WS).
async fn ws_wait_for_meeting_started(
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
    panic!("Timeout waiting for WS MEETING_STARTED");
}

/// Collect all packets received on a WS connection within a timeout window.
async fn ws_collect_packets_for(
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
// WT connection helpers
// =========================================================================

/// Connect to the WT server via the token-based endpoint.
async fn connect_wt_with_token(
    token: &str,
    wt_port: u16,
) -> Result<web_transport_quinn::Session, Box<dyn std::error::Error>> {
    let url = format!(
        "https://127.0.0.1:{wt_port}/lobby?token={}",
        urlencoding::encode(token)
    );
    let url = url::Url::parse(&url)?;
    let client = web_transport_quinn::ClientBuilder::new()
        .dangerous()
        .with_no_certificate_verification()?;
    Ok(client.connect(url).await?)
}

/// Send a packet over WT via a uni-stream (length-prefix framed).
async fn wt_send_via_unistream(session: &web_transport_quinn::Session, bytes: Vec<u8>) {
    let mut s = session.open_uni().await.expect("open uni");
    let len: u32 = bytes
        .len()
        .try_into()
        .expect("packet exceeds u32::MAX bytes");
    s.write_all(&len.to_be_bytes())
        .await
        .expect("write length header");
    s.write_all(&bytes).await.expect("write payload");
    let _ = s.finish();
}

/// Send a packet over WT via a datagram.
fn wt_send_via_datagram(session: &web_transport_quinn::Session, bytes: Vec<u8>) {
    session
        .send_datagram(bytes.into())
        .expect("send datagram");
}

/// Read one length-prefix framed packet from a WT `RecvStream`.
///
/// Returns `None` if the stream is finished or an error occurs.
async fn wt_read_length_prefixed_frame(
    stream: &mut web_transport_quinn::RecvStream,
) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    if stream.read_exact(&mut len_buf).await.is_err() {
        return None;
    }
    let payload_len = u32::from_be_bytes(len_buf) as usize;
    if payload_len == 0 || payload_len > 4 * 1024 * 1024 {
        return None;
    }
    let mut payload = vec![0u8; payload_len];
    if stream.read_exact(&mut payload).await.is_err() {
        return None;
    }
    Some(payload)
}

/// Read one packet from the WT session, trying the persistent stream first,
/// then falling back to accepting a new uni-stream or datagram.
///
/// Takes ownership of the persistent stream and returns it (possibly updated)
/// to avoid borrow issues in `tokio::select!`.
async fn wt_read_one(
    session: &web_transport_quinn::Session,
    persistent: Option<web_transport_quinn::RecvStream>,
    timeout: Duration,
) -> (Option<Vec<u8>>, Option<web_transport_quinn::RecvStream>) {
    if let Some(mut stream) = persistent {
        match tokio::time::timeout(timeout, wt_read_length_prefixed_frame(&mut stream)).await {
            Ok(Some(payload)) => return (Some(payload), Some(stream)),
            Ok(None) => return (None, None), // stream finished
            Err(_) => return (None, Some(stream)), // timeout, stream still alive
        }
    }

    // No persistent stream — accept a new one or read a datagram.
    match tokio::time::timeout(timeout, async {
        tokio::select! {
            Ok(mut stream) = session.accept_uni() => {
                let payload = wt_read_length_prefixed_frame(&mut stream).await;
                (payload, Some(stream))
            }
            Ok(datagram) = session.read_datagram() => {
                (Some(datagram.to_vec()), None)
            }
        }
    })
    .await
    {
        Ok((payload, stream)) => (payload, stream),
        Err(_) => (None, None), // timeout
    }
}

/// Wait for SESSION_ASSIGNED on the WT session. Returns the persistent
/// `RecvStream` (if any) for subsequent reads.
async fn wt_wait_for_session_assigned(
    session: &web_transport_quinn::Session,
) -> Option<web_transport_quinn::RecvStream> {
    let mut persistent: Option<web_transport_quinn::RecvStream> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let session_assigned_type: protobuf::EnumOrUnknown<PacketType> =
        PacketType::SESSION_ASSIGNED.into();

    while tokio::time::Instant::now() < deadline {
        let (data, stream) = wt_read_one(session, persistent, Duration::from_millis(200)).await;
        persistent = stream;

        if let Some(payload) = data {
            if let Ok(wrapper) = PacketWrapper::parse_from_bytes(&payload) {
                if wrapper.packet_type == session_assigned_type {
                    assert!(
                        wrapper.session_id != 0,
                        "SESSION_ASSIGNED must carry session_id"
                    );
                    return persistent;
                }
            }
        }
    }
    panic!("Timeout waiting for WT SESSION_ASSIGNED");
}

/// Collect all packets received on the WT session within a timeout window.
///
/// Takes ownership of the persistent stream and returns it alongside the
/// collected packets.
async fn wt_collect_packets_for(
    session: &web_transport_quinn::Session,
    persistent: Option<web_transport_quinn::RecvStream>,
    duration: Duration,
) -> (Vec<PacketWrapper>, Option<web_transport_quinn::RecvStream>) {
    let mut packets = Vec::new();
    let mut persistent = persistent;
    let deadline = tokio::time::Instant::now() + duration;

    while tokio::time::Instant::now() < deadline {
        let (data, stream) = wt_read_one(session, persistent, Duration::from_millis(200)).await;
        persistent = stream;

        if let Some(payload) = data {
            if let Ok(wrapper) = PacketWrapper::parse_from_bytes(&payload) {
                packets.push(wrapper);
            }
        }
    }

    (packets, persistent)
}

// =========================================================================
// Packet helpers
// =========================================================================

/// Build a MEDIA PacketWrapper with the given media type, ready to send.
fn make_media_packet_typed(sender_user_id: &str, media_type: MediaType) -> Vec<u8> {
    let mut media = MediaPacket::new();
    media.media_type = media_type.into();
    media.user_id = sender_user_id.as_bytes().to_vec();
    media.data = vec![0xDE, 0xAD, 0xBE, 0xEF];

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

/// Build a MEDIA PacketWrapper with AUDIO media type (convenience wrapper).
fn make_media_packet(sender_user_id: &str) -> Vec<u8> {
    make_media_packet_typed(sender_user_id, MediaType::AUDIO)
}

/// Build a non-MEDIA PacketWrapper of the given type, ready to send.
fn make_packet_of_type(sender_user_id: &str, packet_type: PacketType) -> Vec<u8> {
    let mut wrapper = PacketWrapper::new();
    wrapper.packet_type = packet_type.into();
    wrapper.user_id = sender_user_id.as_bytes().to_vec();
    wrapper.data = vec![0xCA, 0xFE];

    wrapper
        .write_to_bytes()
        .expect("PacketWrapper serialization should succeed")
}

// =========================================================================
// Tests
// =========================================================================

/// Positive control: an admitted participant on WS can exchange media with an
/// admitted participant on WT. This proves that the cross-transport relay works
/// so that any absence of media in the observer tests below is due to the
/// server's isolation filter — not a broken WT path.
#[actix_rt::test]
#[serial]
async fn test_wt_admitted_receives_media_from_ws_admitted() {
    let ws_port = WT_ISO_WS_PORT_BASE;
    let wt_port = WT_ISO_WT_PORT_BASE;
    setup_ws_and_wt_server(ws_port, wt_port).await;

    let room = "wt-iso-positive-ctrl";

    // Admitted participant connects via WS.
    let token_ws = make_admitted_token("alice@test.com", room, "Alice");
    let mut ws = ws_connect_with_token(ws_port, &token_ws).await;
    let _sid_ws = ws_wait_for_session_assigned(&mut ws).await;
    ws_wait_for_meeting_started(&mut ws).await;

    // Admitted participant connects via WT.
    let token_wt = make_admitted_token("bob@test.com", room, "Bob");
    let wt_session = connect_wt_with_token(&token_wt, wt_port)
        .await
        .expect("WT admitted connection should succeed");
    let persistent = wt_wait_for_session_assigned(&wt_session).await;

    // Give the server time to fully register both sessions.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Alice (WS) sends a MEDIA packet.
    let media_bytes = make_media_packet("alice@test.com");
    ws.send(Message::Binary(media_bytes))
        .await
        .expect("Alice should be able to send media");

    // Bob (WT) should receive the MEDIA packet.
    let (packets, _persistent) =
        wt_collect_packets_for(&wt_session, persistent, Duration::from_secs(3)).await;
    let media_packets: Vec<_> = packets
        .iter()
        .filter(|p| p.packet_type == PacketType::MEDIA.into())
        .collect();

    assert!(
        !media_packets.is_empty(),
        "Admitted WT session Bob MUST receive MEDIA packets from admitted WS session Alice. \
         Got {} total packets but none were MEDIA. Packet types: {:?}. \
         If this fails, the cross-transport relay is broken — observer isolation tests \
         below are unreliable.",
        packets.len(),
        packets
            .iter()
            .map(|p| p.packet_type.enum_value())
            .collect::<Vec<_>>()
    );

    drop(ws);
    drop(wt_session);
}

/// An observer connected via WebTransport MUST NOT receive MEDIA packets sent
/// by an admitted participant connected via WebSocket. This exercises the
/// outbound allowlist in `chat_server.rs::handle_msg` on the WT delivery path,
/// proving cross-transport isolation.
#[actix_rt::test]
#[serial]
async fn test_wt_observer_does_not_receive_media() {
    let ws_port = WT_ISO_WS_PORT_BASE + 1;
    let wt_port = WT_ISO_WT_PORT_BASE + 1;
    setup_ws_and_wt_server(ws_port, wt_port).await;

    let room = "wt-iso-outbound-media";

    // Admitted participant connects via WS.
    let token_admitted = make_admitted_token("alice@test.com", room, "Alice");
    let mut ws_admitted = ws_connect_with_token(ws_port, &token_admitted).await;
    let _sid_admitted = ws_wait_for_session_assigned(&mut ws_admitted).await;
    ws_wait_for_meeting_started(&mut ws_admitted).await;

    // Observer connects via WT.
    let token_observer = make_observer_token("observer@test.com", room, "Observer");
    let wt_session = connect_wt_with_token(&token_observer, wt_port)
        .await
        .expect("WT observer connection should succeed");
    let persistent = wt_wait_for_session_assigned(&wt_session).await;

    // Give the server time to fully register both sessions.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Admitted participant sends MEDIA via WS.
    let media_bytes = make_media_packet("alice@test.com");
    ws_admitted
        .send(Message::Binary(media_bytes))
        .await
        .expect("Alice should be able to send media");

    // Collect packets on the WT observer side.
    let (packets, _persistent) =
        wt_collect_packets_for(&wt_session, persistent, Duration::from_secs(3)).await;
    let media_packets: Vec<_> = packets
        .iter()
        .filter(|p| p.packet_type == PacketType::MEDIA.into())
        .collect();

    assert!(
        media_packets.is_empty(),
        "WT observer MUST NOT receive any MEDIA packets. \
         Server outbound filter should have dropped them. \
         Got {} MEDIA packets out of {} total.",
        media_packets.len(),
        packets.len()
    );

    drop(ws_admitted);
    drop(wt_session);
}

/// An observer on WT sending a MEDIA packet via uni-stream (length-prefix
/// framed) MUST have it dropped by the server's inbound filter. The admitted
/// participant on WS must NOT receive the observer's media.
///
/// This exercises `handle_inbound` via the UniStream entry point
/// (bridge.rs `accept_uni()` path).
#[actix_rt::test]
#[serial]
async fn test_wt_observer_inbound_media_via_unistream_is_dropped() {
    let ws_port = WT_ISO_WS_PORT_BASE + 2;
    let wt_port = WT_ISO_WT_PORT_BASE + 2;
    setup_ws_and_wt_server(ws_port, wt_port).await;

    let room = "wt-iso-inbound-uni";

    // Admitted participant connects via WS.
    let token_admitted = make_admitted_token("bob@test.com", room, "Bob");
    let mut ws_admitted = ws_connect_with_token(ws_port, &token_admitted).await;
    let _sid_admitted = ws_wait_for_session_assigned(&mut ws_admitted).await;
    ws_wait_for_meeting_started(&mut ws_admitted).await;

    // Observer connects via WT.
    let token_observer = make_observer_token("observer@test.com", room, "Observer");
    let wt_session = connect_wt_with_token(&token_observer, wt_port)
        .await
        .expect("WT observer connection should succeed");
    let _persistent = wt_wait_for_session_assigned(&wt_session).await;

    // Give the server time to fully register both sessions.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Observer sends MEDIA via WT uni-stream.
    let media_bytes = make_media_packet("observer@test.com");
    wt_send_via_unistream(&wt_session, media_bytes).await;

    // The admitted participant on WS should NOT receive the observer's media.
    let packets = ws_collect_packets_for(&mut ws_admitted, Duration::from_secs(2)).await;
    let observer_media: Vec<_> = packets
        .iter()
        .filter(|p| {
            p.packet_type == PacketType::MEDIA.into() && p.user_id == b"observer@test.com"
        })
        .collect();

    assert!(
        observer_media.is_empty(),
        "Admitted session MUST NOT receive MEDIA packets from an observer via WT uni-stream. \
         Server inbound filter should have dropped them. \
         Got {} observer MEDIA packets out of {} total packets.",
        observer_media.len(),
        packets.len()
    );

    drop(ws_admitted);
    drop(wt_session);
}

/// An observer on WT sending a MEDIA packet via datagram MUST have it dropped
/// by the server's inbound filter. The admitted participant on WS must NOT
/// receive the observer's media.
///
/// This exercises `handle_inbound` via the Datagram entry point
/// (bridge.rs `read_datagram()` path).
#[actix_rt::test]
#[serial]
async fn test_wt_observer_inbound_media_via_datagram_is_dropped() {
    let ws_port = WT_ISO_WS_PORT_BASE + 3;
    let wt_port = WT_ISO_WT_PORT_BASE + 3;
    setup_ws_and_wt_server(ws_port, wt_port).await;

    let room = "wt-iso-inbound-dgram";

    // Admitted participant connects via WS.
    let token_admitted = make_admitted_token("charlie@test.com", room, "Charlie");
    let mut ws_admitted = ws_connect_with_token(ws_port, &token_admitted).await;
    let _sid_admitted = ws_wait_for_session_assigned(&mut ws_admitted).await;
    ws_wait_for_meeting_started(&mut ws_admitted).await;

    // Observer connects via WT.
    let token_observer = make_observer_token("observer@test.com", room, "Observer");
    let wt_session = connect_wt_with_token(&token_observer, wt_port)
        .await
        .expect("WT observer connection should succeed");
    let _persistent = wt_wait_for_session_assigned(&wt_session).await;

    // Give the server time to fully register both sessions.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Observer sends MEDIA via WT datagram.
    let media_bytes = make_media_packet("observer@test.com");
    wt_send_via_datagram(&wt_session, media_bytes);

    // The admitted participant on WS should NOT receive the observer's media.
    let packets = ws_collect_packets_for(&mut ws_admitted, Duration::from_secs(2)).await;
    let observer_media: Vec<_> = packets
        .iter()
        .filter(|p| {
            p.packet_type == PacketType::MEDIA.into() && p.user_id == b"observer@test.com"
        })
        .collect();

    assert!(
        observer_media.is_empty(),
        "Admitted session MUST NOT receive MEDIA packets from an observer via WT datagram. \
         Server inbound filter should have dropped them. \
         Got {} observer MEDIA packets out of {} total packets.",
        observer_media.len(),
        packets.len()
    );

    drop(ws_admitted);
    drop(wt_session);
}

/// An observer connected via WT MUST only receive packets on the allowlist
/// (SESSION_ASSIGNED, MEETING). All other packet types — MEDIA, AES_KEY,
/// RSA_PUB_KEY, DIAGNOSTICS, etc. — must be dropped by the outbound filter.
///
/// This is the completeness check for the outbound allowlist across the
/// WT transport boundary.
#[actix_rt::test]
#[serial]
async fn test_wt_observer_receives_only_allowlisted_packets() {
    let ws_port = WT_ISO_WS_PORT_BASE + 4;
    let wt_port = WT_ISO_WT_PORT_BASE + 4;
    setup_ws_and_wt_server(ws_port, wt_port).await;

    let room = "wt-iso-allowlist";

    // Admitted participant connects via WS.
    let token_admitted = make_admitted_token("alice@test.com", room, "Alice");
    let mut ws_admitted = ws_connect_with_token(ws_port, &token_admitted).await;
    let _sid_admitted = ws_wait_for_session_assigned(&mut ws_admitted).await;
    ws_wait_for_meeting_started(&mut ws_admitted).await;

    // Observer connects via WT.
    let token_observer = make_observer_token("observer@test.com", room, "Observer");
    let wt_session = connect_wt_with_token(&token_observer, wt_port)
        .await
        .expect("WT observer connection should succeed");
    let persistent = wt_wait_for_session_assigned(&wt_session).await;

    // Give the server time to fully register both sessions.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Admitted participant sends packets of various non-allowlisted types via WS.
    let blocked_types = [
        PacketType::MEDIA,
        PacketType::AES_KEY,
        PacketType::RSA_PUB_KEY,
        PacketType::DIAGNOSTICS,
    ];

    for ptype in blocked_types {
        let bytes = if ptype == PacketType::MEDIA {
            make_media_packet("alice@test.com")
        } else {
            make_packet_of_type("alice@test.com", ptype)
        };
        ws_admitted
            .send(Message::Binary(bytes))
            .await
            .expect("Alice should be able to send packet");
    }

    // Collect all packets the observer receives within the window.
    let (packets, _persistent) =
        wt_collect_packets_for(&wt_session, persistent, Duration::from_secs(3)).await;

    // Every packet the observer received must be SESSION_ASSIGNED or MEETING.
    let leaked: Vec<_> = packets
        .iter()
        .filter(|p| {
            p.packet_type != PacketType::MEETING.into()
                && p.packet_type != PacketType::SESSION_ASSIGNED.into()
        })
        .collect();

    assert!(
        leaked.is_empty(),
        "WT observer MUST only receive SESSION_ASSIGNED and MEETING packets. \
         Got {} leaked packets with types: {:?}",
        leaked.len(),
        leaked
            .iter()
            .map(|p| p.packet_type.enum_value())
            .collect::<Vec<_>>()
    );

    drop(ws_admitted);
    drop(wt_session);
}
