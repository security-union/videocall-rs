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

//! A KEYFRAME_REQUEST reaches only its target (#3206). No NATS broker: each
//! forwarder is fed the copy NATS delivers to every `room.{room}.*` subscription.

use super::*;
use crate::actors::priority_drop::{PriorityDropDecision, QueueByteMeter, SharedQueueByteMeter};
use crate::actors::session_logic::{InboundAction, SessionLogic, DOWNLINK_EPOCH_NEVER};
use crate::actors::shed_escalation::DownlinkShedEscalation;
use crate::actors::transports::ws_chat_session::ws_classify_and_decide;
use crate::actors::transports::wt_chat_session::{
    wt_route_and_admit, WtAdmission, WtOutboundFrame,
};
use crate::constants::{
    ws_mailbox_capacity, wt_outbound_channel_capacity, AudioDownlinkLane,
    WS_OUTBOUND_CHANNEL_CAPACITY, WT_DATAGRAM_CHANNEL_CAPACITY,
};
use actix::Actor;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Mutex;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind;

const REQUESTER: &str = "requester@example.com";

struct Receiver {
    delivered: Arc<Mutex<Vec<bytes::Bytes>>>,
}

impl Actor for Receiver {
    type Context = actix::Context<Self>;
    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.set_mailbox_capacity(ws_mailbox_capacity());
    }
}

impl Handler<Message> for Receiver {
    type Result = ();
    fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) {
        self.delivered.lock().expect("lock").push(msg.msg);
    }
}

struct Flush;

impl ActixMessage for Flush {
    type Result = ();
}

impl Handler<Flush> for Receiver {
    type Result = ();
    fn handle(&mut self, _msg: Flush, _ctx: &mut Self::Context) {}
}

type Forward =
    Box<dyn Fn(async_nats::Message, Option<&PacketWrapper>) -> Result<(), std::io::Error>>;

struct Peer {
    session: u64,
    user_id: String,
    addr: Addr<Receiver>,
    delivered: Arc<Mutex<Vec<bytes::Bytes>>>,
    forward: Forward,
}

struct TestRoom {
    name: String,
    requester: SessionLogic,
    peers: Vec<Peer>,
}

fn participant(i: u64) -> String {
    format!("participant-{i:03}@example.com")
}

async fn room(name: &str, n: usize, transport: &str) -> TestRoom {
    let nc = async_nats::ConnectOptions::new()
        .retry_on_initial_connect()
        .connect("nats://127.0.0.1:1")
        .await
        .expect("a lazily connecting client needs no broker");
    let chat = ChatServer::new(nc.clone()).await.start();
    let (tracker, _rx) = tokio::sync::mpsc::unbounded_channel();
    let requester = SessionLogic::new(
        chat,
        name.to_string(),
        REQUESTER.to_string(),
        REQUESTER.to_string(),
        false,
        nc,
        tracker,
        crate::session_manager::SessionManager::new(),
        false,
        None,
        transport,
        false,
    );
    let peers = (1..n as u64)
        .map(|session| {
            let delivered = Arc::new(Mutex::new(Vec::new()));
            let addr = Receiver {
                delivered: delivered.clone(),
            }
            .start();
            let (forward, _emit) = handle_msg(
                addr.clone().recipient(),
                name.to_string(),
                session,
                false,
                participant(session),
                DesiredStreams::default(),
                LayerPrefs::default(),
                transport.to_string(),
                Arc::new(AtomicU64::new(DOWNLINK_EPOCH_NEVER)),
                Arc::new(AtomicBool::new(true)),
            );
            Peer {
                session,
                user_id: participant(session),
                addr,
                delivered,
                forward: Box::new(forward),
            }
        })
        .collect();
    TestRoom {
        name: name.to_string(),
        requester,
        peers,
    }
}

fn keyframe_request(target_user: &str, target_session: u64) -> Vec<u8> {
    wrapper(
        MediaKind::MEDIA_KIND_UNSPECIFIED,
        MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            user_id: target_user.as_bytes().to_vec(),
            target_session_id: target_session,
            data: b"VIDEO".to_vec(),
            ..Default::default()
        },
    )
}

fn wrapper(media_kind: MediaKind, inner: MediaPacket) -> Vec<u8> {
    PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        user_id: REQUESTER.as_bytes().to_vec(),
        media_kind: media_kind.into(),
        data: inner.write_to_bytes().expect("inner"),
        ..Default::default()
    }
    .write_to_bytes()
    .expect("wrapper")
}

async fn send(room: &mut TestRoom, raw: &[u8]) -> Vec<u64> {
    let forwarded = match room.requester.handle_inbound(raw) {
        InboundAction::Forward(bytes) => bytes,
        _ => panic!("the relay must admit the packet at ingress"),
    };
    let stamped =
        stamp_wrapper_for_broadcast(&forwarded, room.requester.id, &room.requester.user_id)
            .expect("stamp");
    let subject = format!("room.{}.{}", room.name, room.requester.id);
    for peer in &room.peers {
        let msg = async_nats::Message {
            subject: subject.as_str().into(),
            payload: stamped.clone().into(),
            reply: None,
            headers: None,
            status: None,
            description: None,
            length: 0,
        };
        let parsed = PacketWrapper::parse_from_bytes(&msg.payload).ok();
        (peer.forward)(msg, parsed.as_ref()).expect("forward");
    }
    let mut reached = Vec::new();
    for peer in &room.peers {
        peer.addr.send(Flush).await.expect("flush");
        let frames: Vec<bytes::Bytes> = std::mem::take(&mut *peer.delivered.lock().expect("lock"));
        assert!(frames.len() <= 1, "one send delivers at most one copy");
        if let Some(frame) = frames.first() {
            assert!(
                admitted_on_both_transports(frame),
                "session {}",
                peer.session
            );
            reached.push(peer.session);
        }
    }
    reached
}

fn admitted_on_both_transports(frame: &[u8]) -> bool {
    let pw = PacketWrapper::parse_from_bytes(frame).expect("delivered wrapper");
    let is_media = pw.packet_type == PacketType::MEDIA.into();
    let media_type = MediaPacket::parse_from_bytes(&pw.data)
        .ok()
        .and_then(|mp| mp.media_type.enum_value().ok());
    let (_, ws) = ws_classify_and_decide(
        Some(&pw),
        media_type,
        WS_OUTBOUND_CHANNEL_CAPACITY,
        &QueueByteMeter::default(),
    );
    let (uni_tx, _uni_rx) =
        tokio::sync::mpsc::channel::<WtOutboundFrame>(wt_outbound_channel_capacity());
    let (dgram_tx, _dgram_rx) =
        tokio::sync::mpsc::channel::<WtOutboundFrame>(WT_DATAGRAM_CHANNEL_CAPACITY);
    let wt = wt_route_and_admit(
        &uni_tx,
        &dgram_tx,
        &SharedQueueByteMeter::default(),
        frame.to_vec(),
        is_media,
        media_type == Some(MediaType::AUDIO),
        Some(&pw),
        media_type,
        pw.session_id,
        AudioDownlinkLane::Reliable,
        &DownlinkShedEscalation::new(),
    );
    matches!(ws, PriorityDropDecision::Admit) && matches!(wt, WtAdmission::Enqueued)
}

fn filtered(room: &str) -> f64 {
    RELAY_KEYFRAME_REQUEST_FILTERED_TOTAL
        .with_label_values(&[room])
        .get()
}

#[actix_rt::test]
async fn a_keyframe_request_reaches_only_its_target() {
    for n in [50usize, 200] {
        for transport in ["websocket", "webtransport"] {
            let name = format!("kf-target-{n}-{transport}");
            let mut room = room(&name, n, transport).await;
            let reached = send(&mut room, &keyframe_request(&participant(7), 7)).await;
            println!(
                "[#3206] n={n} {transport}: one keyframe request delivered to {} of {} receivers",
                reached.len(),
                n - 1
            );
            assert_eq!(reached, vec![7], "n={n} {transport}");
        }
    }
}

#[actix_rt::test]
async fn a_reconnected_target_still_receives_a_request_naming_its_old_session() {
    let mut room = room("kf-target-reconnect", 10, "websocket").await;
    let stale_session = 9_999;
    let reached = send(&mut room, &keyframe_request(&participant(4), stale_session)).await;
    assert_eq!(reached, vec![4]);
}

#[actix_rt::test]
async fn a_request_without_a_readable_target_is_delivered_as_today() {
    let mut room = room("kf-target-unreadable", 10, "webtransport").await;
    let before = filtered("kf-target-unreadable");
    let mut explicit_empty = Vec::new();
    let mut os = protobuf::CodedOutputStream::vec(&mut explicit_empty);
    os.write_enum(1, MediaType::KEYFRAME_REQUEST as i32)
        .expect("media_type");
    os.write_bytes(2, b"").expect("user_id");
    os.write_uint64(10, 5).expect("target_session_id");
    os.flush().expect("flush");
    drop(os);
    let explicit_empty = PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        user_id: REQUESTER.as_bytes().to_vec(),
        data: explicit_empty,
        ..Default::default()
    }
    .write_to_bytes()
    .expect("wrapper");
    for raw in [keyframe_request("", 4), explicit_empty] {
        let reached = send(&mut room, &raw).await;
        assert_eq!(reached, (1..10).collect::<Vec<u64>>());
    }
    assert_eq!(filtered("kf-target-unreadable"), before);
}

#[actix_rt::test]
async fn other_packet_kinds_are_unaffected() {
    let mut room = room("kf-target-other-kinds", 10, "websocket").await;
    let everyone: Vec<u64> = (1..10).collect();
    let addressed = participant(4).into_bytes();
    let heartbeat = wrapper(
        MediaKind::MEDIA_KIND_UNSPECIFIED,
        MediaPacket {
            media_type: MediaType::HEARTBEAT.into(),
            user_id: addressed.clone(),
            ..Default::default()
        },
    );
    let video = wrapper(
        MediaKind::VIDEO,
        MediaPacket {
            media_type: MediaType::VIDEO.into(),
            user_id: addressed.clone(),
            frame_type: "key".to_string(),
            data: vec![0; 64],
            ..Default::default()
        },
    );
    let mut tagged_request =
        PacketWrapper::parse_from_bytes(&keyframe_request(&participant(4), 4)).expect("request");
    tagged_request.media_kind = MediaKind::VIDEO.into();
    let tagged_request = tagged_request.write_to_bytes().expect("tagged");
    for (label, raw) in [
        ("heartbeat", heartbeat),
        ("video", video),
        ("tagged keyframe request", tagged_request),
    ] {
        assert_eq!(send(&mut room, &raw).await, everyone, "{label}");
    }
    assert_eq!(
        room.peers
            .iter()
            .filter(|p| p.user_id.as_bytes() == addressed)
            .count(),
        1
    );
}

#[actix_rt::test]
async fn each_dropped_copy_is_counted() {
    let mut room = room("kf-target-counter", 10, "websocket").await;
    let before = filtered("kf-target-counter");
    send(&mut room, &keyframe_request(&participant(2), 2)).await;
    assert_eq!(
        filtered("kf-target-counter") - before,
        8.0,
        "10 participants, one target"
    );
}
