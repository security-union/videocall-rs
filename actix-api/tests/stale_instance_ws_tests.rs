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

use actix::Actor;
use actix_web::{web, App, HttpServer};
use futures_util::StreamExt;
use protobuf::Message as ProtoMessage;
use sec_api::{
    actors::chat_server::ChatServer, lobby::ws_connect_authenticated, models::AppState,
    server_diagnostics::ServerDiagnostics, session_manager::SessionManager,
};
use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use videocall_meeting_types::kick::{ParticipantKickedPayload, PARTICIPANT_KICKED_SUBJECT};
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::FeatureFlags;

const JWT_SECRET: &str = "test-secret-for-stale-instance-tests";
const PORT: u16 = 18291;
const LIVE_PORT: u16 = 18292;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn token(user: &str, room: &str, meeting_started_at_ms: Option<i64>) -> String {
    meeting_api::token::generate_room_token_at(
        JWT_SECRET,
        60,
        chrono::Utc::now().timestamp(),
        user,
        room,
        false,
        user,
        false,
        false,
        meeting_started_at_ms,
    )
    .expect("room token")
}

async fn connect(token: &str) -> Ws {
    connect_on(PORT, token).await
}

async fn connect_on(port: u16, token: &str) -> Ws {
    let url = format!(
        "ws://127.0.0.1:{port}/lobby?token={}",
        urlencoding::encode(token)
    );
    tokio_tungstenite::connect_async(&url)
        .await
        .expect("the upgrade succeeds before JoinRoom refuses the session")
        .0
}

fn target_of_superseded(msg: &Message) -> Option<Vec<u8>> {
    let Message::Binary(bytes) = msg else {
        return None;
    };
    let w = PacketWrapper::parse_from_bytes(bytes).ok()?;
    if w.packet_type.enum_value() != Ok(PacketType::MEETING) {
        return None;
    }
    let m = MeetingPacket::parse_from_bytes(&w.data).ok()?;
    (m.event_type.enum_value() == Ok(MeetingEventType::MEETING_INSTANCE_SUPERSEDED))
        .then_some(m.target_user_id)
}

/// What a socket received: the superseded notice's target, whether the stream
/// ended, and the close frame's code and reason if it carried one.
struct Drained {
    notice: Option<Vec<u8>>,
    ended: bool,
    close: Option<(CloseCode, String)>,
}

async fn drain_all(ws: &mut Ws, wait: Duration) -> Drained {
    let mut out = Drained {
        notice: None,
        ended: false,
        close: None,
    };
    out.ended = tokio::time::timeout(wait, async {
        while let Some(frame) = ws.next().await {
            match frame {
                Ok(msg) => {
                    if let Some(t) = target_of_superseded(&msg) {
                        out.notice = Some(t);
                    }
                    if let Message::Close(frame) = msg {
                        out.close = frame.map(|f| (f.code, f.reason.to_string()));
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    })
    .await
    .is_ok();
    out
}

async fn drain(ws: &mut Ws, wait: Duration) -> (Option<Vec<u8>>, bool) {
    let drained = drain_all(ws, wait).await;
    (drained.notice, drained.ended)
}

async fn start_server(port: u16) -> async_nats::Client {
    FeatureFlags::clear_meeting_management_override();
    std::env::set_var("JWT_SECRET", JWT_SECRET);
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    let nats_client = async_nats::connect(&nats_url).await.expect("NATS");
    let (_, tracker_sender, _) = ServerDiagnostics::new_with_channel(nats_client.clone());
    let state = AppState {
        chat: ChatServer::new(nats_client.clone()).await.start(),
        nats_client: nats_client.clone(),
        tracker_sender,
        session_manager: SessionManager::new(),
    };
    actix_rt::spawn(async move {
        let _ = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(state.clone()))
                .service(ws_connect_authenticated)
        })
        .bind(format!("127.0.0.1:{port}"))
        .expect("bind")
        .run()
        .await;
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    nats_client
}

#[actix_rt::test]
async fn a_ws_client_with_a_stale_instance_token_is_notified_then_closed() {
    start_server(PORT).await;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let room = format!("ws-inst-3287-{now_ms}");

    let mut current = connect(&token("new@test.com", &room, Some(now_ms))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut stale = connect(&token("old@test.com", &room, Some(now_ms - 60_000))).await;
    let mut unclaimed = connect(&token("legacy@test.com", &room, None)).await;

    let stale_end = drain_all(&mut stale, Duration::from_secs(5)).await;
    assert_eq!(
        stale_end.notice.as_deref(),
        Some(b"old@test.com".as_slice()),
        "the stale client must receive the notice addressed to it"
    );
    assert!(
        stale_end.ended,
        "the relay must then close the stale socket"
    );
    assert_eq!(
        stale_end.close,
        Some((CloseCode::Policy, "meeting instance superseded".to_string())),
        "a refusal closes with its own reason, not the kick's"
    );

    let (notice, ended) = drain(&mut current, Duration::from_millis(1500)).await;
    assert_eq!((notice, ended), (None, false), "current token stays up");
    let (notice, ended) = drain(&mut unclaimed, Duration::from_millis(1500)).await;
    assert_eq!((notice, ended), (None, false), "unclaimed token stays up");
}

/// A host kick still closes with "removed by host"; other users stay up.
#[actix_rt::test]
async fn a_kick_keeps_its_own_close_reason() {
    let nats = start_server(LIVE_PORT).await;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let room = format!("ws-kick-3287-{now_ms}");

    let mut kicked = connect_on(LIVE_PORT, &token("kicked@test.com", &room, Some(now_ms))).await;
    let mut bystander = connect_on(LIVE_PORT, &token("other@test.com", &room, Some(now_ms))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let now_secs = chrono::Utc::now().timestamp();
    nats.publish(
        PARTICIPANT_KICKED_SUBJECT,
        serde_json::to_vec(&ParticipantKickedPayload {
            room_id: room.clone(),
            user_id: "kicked@test.com".to_string(),
            kicked_at: now_secs,
            revoke_iat_through: now_secs,
            deny_until: now_secs + 3600,
        })
        .unwrap()
        .into(),
    )
    .await
    .unwrap();
    nats.flush().await.unwrap();

    let kicked_end = drain_all(&mut kicked, Duration::from_secs(5)).await;
    assert_eq!(
        kicked_end.close,
        Some((CloseCode::Policy, "removed by host".to_string())),
        "the kick keeps its own close reason"
    );
    assert_eq!(kicked_end.notice, None);
    let (notice, ended) = drain(&mut bystander, Duration::from_millis(1500)).await;
    assert_eq!((notice, ended), (None, false), "other users stay up");
}
