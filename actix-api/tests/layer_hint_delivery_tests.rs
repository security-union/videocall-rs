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

//! Positive end-to-end regression lock for relay-authored LAYER_HINT delivery
//! (#1182, covering the #1108 / PR #1177 delivery gap).
//!
//! ## What this proves (and why it must run against the REAL relay + NATS)
//!
//! Stage 3 of simulcast (#1108) lets the relay tell a publisher it may stop
//! encoding upper simulcast layers when EVERY receiver has asked for less. The
//! relay computes, per source, the UNION (max) over all receivers of the layer
//! each receiver requested for that source, and — after a debounce window —
//! emits a `LAYER_HINT` PacketWrapper on the publisher's OWN per-session NATS
//! subject (`room.{room}.{publisher}`) with the publisher's `session_id`
//! stamped (see `emit_layer_hint`).
//!
//! That self-addressed delivery only works because of the `is_layer_hint`
//! carve-out in the relay's self-echo guard (`chat_server.rs` `handle_msg`:
//! `if drop_self_echo && !is_congestion && !is_layer_hint`). Before PR #1177
//! that carve-out did not exist, so the relay GENERATED the hint and then DROPPED
//! it at the self-echo guard before it left the relay — the hint never reached
//! the publisher and the whole publish-side suppression mechanism was inert.
//!
//! A `handle_msg`-level unit mock could not catch that regression: the bug lived
//! exactly at the transport/self-subject delivery boundary. This test therefore
//! stands up the real `ChatServer` + NATS + WebSocket server (the same harness
//! `waiting_room_isolation_tests` uses), drives a real multi-receiver
//! LAYER_PREFERENCE flow, and asserts the PUBLISHER's own socket receives the
//! `LAYER_HINT` addressed to its session with the expected suppressed-layer
//! value.
//!
//! ## Why this is a REAL regression lock (adversarial check)
//!
//! Reverting the `&& !is_layer_hint` carve-out at `handle_msg` makes the relay
//! drop the self-addressed hint, so `wait_for_layer_hint` below times out and
//! the test FAILS — exactly the pre-#1177 behaviour. (Verified by reverting the
//! carve-out locally and re-running; see the PR notes.)
//!
//! ## Non-trivial union (not the Suppress(0) churn case)
//!
//! A lone publisher with NO receivers produces a Suppress(0) hint (the
//! zero-receiver union). That is the trivial churn case the issue explicitly
//! warns against. This test connects a publisher PLUS two admitted receivers,
//! and has BOTH receivers record a LAYER_PREFERENCE of layer 1 for the
//! publisher's VIDEO source. The union is therefore a real layer cap (1), well
//! below the full-ladder sentinel — so the asserted hint value is `1`, proving
//! the relay aggregated real per-receiver demand rather than emitting the
//! degenerate zero.

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
use videocall_types::protos::layer_hint_packet::layer_hint_packet::MediaKind as HintMediaKind;
use videocall_types::protos::layer_hint_packet::LayerHintPacket;
use videocall_types::protos::layer_preference_packet::layer_preference_packet::{
    Entry as PrefEntry, EntryMediaKind,
};
use videocall_types::protos::layer_preference_packet::LayerPreferencePacket;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::FeatureFlags;

const JWT_SECRET: &str = "test-secret-for-layer-hint-tests";
const TOKEN_TTL_SECS: i64 = 60;

/// Base port for layer-hint delivery tests. Distinct from the waiting-room
/// isolation test bases (19100+) to avoid port collisions.
const LH_PORT_BASE: u16 = 19200;

/// The debounce window the relay applies before emitting a LOWER (suppress)
/// LAYER_HINT, mirrored from `sec_api::constants::LAYER_HINT_SUPPRESS_DEBOUNCE_MS`.
///
/// Imported from the crate so this test stays in lock-step with the production
/// constant: if the debounce is retuned, the collection budget below scales with
/// it automatically rather than silently under-waiting and flaking.
const SUPPRESS_DEBOUNCE: Duration =
    Duration::from_millis(sec_api::constants::LAYER_HINT_SUPPRESS_DEBOUNCE_MS);

// =========================================================================
// Server + token + connection helpers (mirrors waiting_room_isolation_tests)
// =========================================================================

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
        .expect("Failed to bind layer-hint test server")
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

/// Build a LAYER_PREFERENCE PacketWrapper: this receiver tells the relay it
/// wants `desired_layer` of `source_session`'s VIDEO ladder.
fn make_layer_preference(sender_user_id: &str, source_session: u64, desired_layer: u32) -> Vec<u8> {
    let mut entry = PrefEntry::new();
    entry.session_id = source_session;
    entry.media_kind = EntryMediaKind::ENTRY_VIDEO.into();
    entry.desired_layer = desired_layer;

    let mut pref = LayerPreferencePacket::new();
    pref.entries = vec![entry];

    let mut wrapper = PacketWrapper::new();
    wrapper.packet_type = PacketType::LAYER_PREFERENCE.into();
    wrapper.user_id = sender_user_id.as_bytes().to_vec();
    wrapper.data = pref
        .write_to_bytes()
        .expect("LayerPreferencePacket serialization should succeed");

    wrapper
        .write_to_bytes()
        .expect("PacketWrapper serialization should succeed")
}

/// Collect on a socket until a LAYER_HINT addressed to `expected_session`
/// arrives, or the deadline elapses. Returns the parsed `LayerHintPacket` on
/// success, `None` on timeout.
async fn wait_for_layer_hint(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    expected_session: u64,
    budget: Duration,
) -> Option<LayerHintPacket> {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if let Ok(wrapper) = PacketWrapper::parse_from_bytes(&data) {
                            if wrapper.packet_type == PacketType::LAYER_HINT.into() {
                                // The relay stamps the publisher's own session_id on the
                                // self-addressed hint (see `emit_layer_hint`). Assert the
                                // hint is addressed to US, not some other publisher.
                                assert_eq!(
                                    wrapper.session_id, expected_session,
                                    "LAYER_HINT must be addressed to the publisher's own session_id"
                                );
                                if let Ok(hint) = LayerHintPacket::parse_from_bytes(&wrapper.data) {
                                    return Some(hint);
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    None
}

// =========================================================================
// Test
// =========================================================================

/// POSITIVE end-to-end proof of the #1108 / PR #1177 delivery path: an admitted
/// publisher receives a relay-authored, self-addressed LAYER_HINT once every
/// receiver in the room has asked for a layer below the publisher's full ladder.
///
/// Flow:
/// 1. Publisher + two receivers join the same room (all admitted).
/// 2. Both receivers send a LAYER_PREFERENCE requesting VIDEO layer 1 of the
///    publisher's source. The per-source union is therefore 1 (a real cap below
///    the full-ladder sentinel) — the NON-trivial case.
/// 3. The relay observes the downgrade, starts the suppress debounce, and after
///    `LAYER_HINT_SUPPRESS_DEBOUNCE_MS` its scheduled `notify_later` recheck
///    fires and emits the Suppress hint on the publisher's self-subject.
/// 4. The publisher's socket receives `LAYER_HINT` addressed to its own session
///    with `max_requested_layer == 1` for the VIDEO ladder.
///
/// REGRESSION LOCK: on pre-#1177 code (no `&& !is_layer_hint` carve-out in the
/// relay self-echo guard) the relay drops this self-addressed hint, so step 4
/// times out and this test FAILS.
#[actix_rt::test]
#[serial]
async fn test_publisher_receives_relay_authored_layer_hint() {
    let port = LH_PORT_BASE;
    setup(port).await;

    let room = "lh-publisher-receives-hint";

    // --- Publisher joins first so receivers can reference its session_id. ---
    let token_pub = make_admitted_token("publisher@test.com", room, "Publisher");
    let mut ws_pub = connect_with_token(port, &token_pub).await;
    let sid_pub = wait_for_session_assigned(&mut ws_pub).await;
    wait_for_meeting_started(&mut ws_pub).await;

    // --- Two receivers join the same room. ---
    let token_r1 = make_admitted_token("r1@test.com", room, "Receiver1");
    let mut ws_r1 = connect_with_token(port, &token_r1).await;
    let _sid_r1 = wait_for_session_assigned(&mut ws_r1).await;
    wait_for_meeting_started(&mut ws_r1).await;

    let token_r2 = make_admitted_token("r2@test.com", room, "Receiver2");
    let mut ws_r2 = connect_with_token(port, &token_r2).await;
    let _sid_r2 = wait_for_session_assigned(&mut ws_r2).await;
    wait_for_meeting_started(&mut ws_r2).await;

    // Let the server register all three sessions in room_members before the
    // receivers express a preference (the union is computed over room_members).
    tokio::time::sleep(Duration::from_millis(300)).await;

    // --- Both receivers request VIDEO layer 1 of the publisher's source. ---
    // Union over all receivers = max(1, 1) = 1 (< full-ladder sentinel), so the
    // relay owes a Suppress(1) hint to the publisher. Each preference is sent on
    // that receiver's OWN connection, so it is recorded against that receiver's
    // self-subject (ownership is established by subject, not payload).
    let pref_r1 = make_layer_preference("r1@test.com", sid_pub, 1);
    ws_r1
        .send(Message::Binary(pref_r1))
        .await
        .expect("Receiver 1 should send LAYER_PREFERENCE");

    let pref_r2 = make_layer_preference("r2@test.com", sid_pub, 1);
    ws_r2
        .send(Message::Binary(pref_r2))
        .await
        .expect("Receiver 2 should send LAYER_PREFERENCE");

    // --- Collect on the PUBLISHER socket past the suppress debounce. ---
    // The relay schedules a deferred `notify_later` recheck at
    // `now + SUPPRESS_DEBOUNCE` when it first sees the downgrade; that recheck is
    // what emits the lower hint even with no further preference change. We wait
    // the full debounce plus a generous slack for actor scheduling, NATS
    // round-trip, and slow CI runners.
    let budget = SUPPRESS_DEBOUNCE + Duration::from_secs(3);
    let hint = wait_for_layer_hint(&mut ws_pub, sid_pub, budget)
        .await
        .expect(
            "Publisher MUST receive a relay-authored LAYER_HINT addressed to its own session. \
             If this times out on fixed code, the #1108 delivery path is broken; on pre-#1177 \
             code the self-echo guard drops the self-addressed hint (the regression this locks).",
        );

    // The hint must carry the VIDEO ladder cap the receivers' union produced.
    let video_entry = hint
        .entries
        .iter()
        .find(|e| e.media_kind == HintMediaKind::VIDEO.into())
        .unwrap_or_else(|| {
            panic!(
                "LAYER_HINT must carry a VIDEO entry; got entries: {:?}",
                hint.entries
                    .iter()
                    .map(|e| (e.media_kind.enum_value(), e.max_requested_layer))
                    .collect::<Vec<_>>()
            )
        });

    assert_eq!(
        video_entry.max_requested_layer, 1,
        "Suppressed VIDEO layer must equal the receivers' union (both asked for layer 1), \
         proving the relay aggregated real per-receiver demand — not the degenerate \
         zero-receiver Suppress(0) case."
    );

    drop(ws_pub);
    drop(ws_r1);
    drop(ws_r2);
}
