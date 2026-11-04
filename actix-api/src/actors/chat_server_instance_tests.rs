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

//! Relay refusal of room tokens minted for an earlier meeting instance
//! (#3287). Needs a live NATS at `NATS_URL`.

use super::*;
use actix::Actor;
use serial_test::serial;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    /// MEETING_INSTANCE_SUPERSEDED addressed to this user.
    Superseded(String),
    Close(ForceClose),
}

struct Probe {
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Actor for Probe {
    type Context = actix::Context<Self>;
}

impl Handler<Message> for Probe {
    type Result = ();
    fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) {
        let Ok(wrapper) = PacketWrapper::parse_from_bytes(&msg.msg) else {
            return;
        };
        if wrapper.packet_type.enum_value() != Ok(PacketType::MEETING) {
            return;
        }
        let Ok(meeting) = MeetingPacket::parse_from_bytes(&wrapper.data) else {
            return;
        };
        if meeting.event_type.enum_value() == Ok(MeetingEventType::MEETING_INSTANCE_SUPERSEDED) {
            self.seen.lock().unwrap().push(Seen::Superseded(
                String::from_utf8_lossy(&meeting.target_user_id).into_owned(),
            ));
        }
    }
}

impl Handler<ForceClose> for Probe {
    type Result = ();
    fn handle(&mut self, msg: ForceClose, _ctx: &mut Self::Context) {
        self.seen.lock().unwrap().push(Seen::Close(msg));
    }
}

#[derive(ActixMessage)]
#[rtype(result = "Option<ConnectionState>")]
struct StateOf(SessionId);

impl Handler<StateOf> for ChatServer {
    type Result = Option<ConnectionState>;
    fn handle(&mut self, msg: StateOf, _ctx: &mut Self::Context) -> Self::Result {
        self.connection_states.get(&msg.0).copied()
    }
}

#[derive(ActixMessage)]
#[rtype(result = "Vec<SessionId>")]
struct MembersOf(String);

impl Handler<MembersOf> for ChatServer {
    type Result = MessageResult<MembersOf>;
    fn handle(&mut self, msg: MembersOf, _ctx: &mut Self::Context) -> Self::Result {
        MessageResult(
            self.room_members
                .get(&msg.0)
                .map(|m| m.iter().map(|r| r.session).collect())
                .unwrap_or_default(),
        )
    }
}

async fn nats() -> async_nats::Client {
    let url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    async_nats::connect(&url)
        .await
        .expect("Failed to connect to NATS")
}

struct Joined {
    seen: Arc<Mutex<Vec<Seen>>>,
    result: Result<(), String>,
}

impl Joined {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_join(
    chat: &Addr<ChatServer>,
    session: SessionId,
    room: &str,
    user: &str,
    observer: bool,
    msa: Option<u64>,
    transport: &str,
    probe: &Addr<Probe>,
) -> Result<(), String> {
    chat.send(JoinRoom {
        session,
        room: room.to_string(),
        user_id: user.to_string(),
        display_name: user.to_string(),
        is_guest: false,
        observer,
        instance_id: None,
        is_host: false,
        transport: transport.to_string(),
        downlink_congested_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        token_iat: Some(unix_now_secs()),
        meeting_started_at_ms: msa,
        conn_seq: None,
        closer: Some(probe.clone().recipient()),
    })
    .await
    .expect("JoinRoom")
}

/// `transport` doubles as a per-test metric label so the global counters can
/// be asserted on without interference from other tests.
async fn join(
    chat: &Addr<ChatServer>,
    session: SessionId,
    room: &str,
    user: &str,
    observer: bool,
    msa: Option<u64>,
    transport: &str,
) -> Joined {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe { seen: seen.clone() }.start();
    chat.send(Connect {
        id: session,
        addr: probe.clone().recipient(),
    })
    .await
    .expect("Connect");
    let result = send_join(chat, session, room, user, observer, msa, transport, &probe).await;
    Joined { seen, result }
}

fn started(room: &str, started_at_ms: i64) -> MeetingInstanceStarted {
    MeetingInstanceStarted(MeetingInstanceStartedPayload {
        room_id: room.to_string(),
        started_at_ms,
    })
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn refused_total(transport: &str) -> f64 {
    RELAY_JOIN_STALE_INSTANCE_REFUSED_TOTAL
        .with_label_values(&[transport])
        .get()
}

fn unchecked_total(transport: &str) -> f64 {
    RELAY_JOIN_INSTANCE_UNCHECKED_TOTAL
        .with_label_values(&[transport])
        .get()
}

#[actix_rt::test]
#[serial]
async fn a_join_with_a_token_from_an_earlier_instance_is_notified_then_closed() {
    let chat = ChatServer::new(nats().await).await.start();
    let now_ms = unix_now_ms();
    let room = format!("inst3287-a-{now_ms}");
    let transport = "t3287-refuse";
    chat.send(started(&room, now_ms)).await.unwrap();
    let before = refused_total(transport);

    let stale = join(
        &chat,
        32_871,
        &room,
        "alice@x",
        false,
        Some((now_ms - 60_000) as u64),
        transport,
    )
    .await;
    settle().await;

    assert_eq!(stale.result, Err(JOIN_REFUSED_STALE_INSTANCE.to_string()));
    assert_eq!(
        stale.seen(),
        vec![
            Seen::Superseded("alice@x".to_string()),
            Seen::Close(ForceClose::InstanceSuperseded)
        ],
        "the notice, addressed to the joining user, must precede the close"
    );
    assert_eq!(
        chat.send(StateOf(32_871)).await.unwrap(),
        Some(ConnectionState::Revoked)
    );
    assert!(chat.send(MembersOf(room.clone())).await.unwrap().is_empty());
    assert_eq!(refused_total(transport), before + 1.0);
}

#[actix_rt::test]
#[serial]
async fn observer_unclaimed_equal_newer_and_other_room_joins_are_accepted() {
    let chat = ChatServer::new(nats().await).await.start();
    let now_ms = unix_now_ms();
    let room = format!("inst3287-b-{now_ms}");
    let transport = "t3287-accept";
    let current = now_ms as u64;
    chat.send(started(&room, now_ms)).await.unwrap();
    let refused_before = refused_total(transport);
    let unchecked_before = unchecked_total(transport);

    let cases: [(SessionId, &str, bool, Option<u64>); 5] = [
        (32_881, room.as_str(), true, Some(current - 60_000)),
        (32_882, room.as_str(), false, None),
        (32_883, room.as_str(), false, Some(current)),
        (32_884, room.as_str(), false, Some(current + 1)),
        (
            32_885,
            "inst3287-b-elsewhere",
            false,
            Some(current - 60_000),
        ),
    ];
    for (session, in_room, observer, msa) in cases {
        let joined = join(&chat, session, in_room, "bob@x", observer, msa, transport).await;
        settle().await;
        assert_eq!(
            joined.result,
            Ok(()),
            "session {session} observer={observer} msa={msa:?}"
        );
        assert!(joined.seen().is_empty());
    }
    assert_eq!(refused_total(transport), refused_before);
    assert_eq!(
        unchecked_total(transport),
        unchecked_before + 1.0,
        "only the token without the claim is counted as unchecked"
    );
}

/// An empty registry (a fresh relay) accepts any claim.
#[actix_rt::test]
#[serial]
async fn an_empty_registry_accepts_any_token() {
    let chat = ChatServer::new(nats().await).await.start();
    let room = format!("inst3287-c-{}", unix_now_ms());
    let joined = join(
        &chat,
        32_891,
        &room,
        "alice@x",
        false,
        Some(1),
        "t3287-empty",
    )
    .await;
    assert_eq!(joined.result, Ok(()));
    assert!(joined.seen().is_empty());
}

/// A relay that missed the NATS notice still learns the start from the first
/// token it admits, so a later older token is refused.
#[actix_rt::test]
#[serial]
async fn an_admitted_token_teaches_the_registry_but_an_observer_does_not() {
    let chat = ChatServer::new(nats().await).await.start();
    let now_ms = unix_now_ms() as u64;
    let room = format!("inst3287-d-{now_ms}");
    let transport = "t3287-learn";

    let newer = join(&chat, 32_901, &room, "a@x", false, Some(now_ms), transport).await;
    assert_eq!(newer.result, Ok(()));
    let older = join(
        &chat,
        32_902,
        &room,
        "b@x",
        false,
        Some(now_ms - 1000),
        transport,
    )
    .await;
    assert_eq!(older.result, Err(JOIN_REFUSED_STALE_INSTANCE.to_string()));

    let other = format!("inst3287-d-obs-{now_ms}");
    let observer = join(
        &chat,
        32_903,
        &other,
        "o@x",
        true,
        Some(now_ms + 4000),
        transport,
    )
    .await;
    assert_eq!(observer.result, Ok(()));
    let member = join(&chat, 32_904, &other, "m@x", false, Some(now_ms), transport).await;
    assert_eq!(
        member.result,
        Ok(()),
        "an observer's claim must not raise the room's known start"
    );
}

/// Notices arriving out of order never lower the known start.
#[actix_rt::test]
#[serial]
async fn an_out_of_order_instance_notice_does_not_lower_the_known_start() {
    let chat = ChatServer::new(nats().await).await.start();
    let now_ms = unix_now_ms();
    let room = format!("inst3287-e-{now_ms}");
    chat.send(started(&room, now_ms)).await.unwrap();
    chat.send(started(&room, now_ms - 5_000)).await.unwrap();

    let middle = join(
        &chat,
        32_911,
        &room,
        "a@x",
        false,
        Some((now_ms - 2_000) as u64),
        "t3287-order",
    )
    .await;
    assert_eq!(middle.result, Err(JOIN_REFUSED_STALE_INSTANCE.to_string()));
}

/// A new instance does not close sessions already connected.
#[actix_rt::test]
#[serial]
async fn a_new_instance_notice_leaves_live_sessions_alone() {
    let chat = ChatServer::new(nats().await).await.start();
    let now_ms = unix_now_ms();
    let room = format!("inst3287-f-{now_ms}");
    let live = join(
        &chat,
        32_921,
        &room,
        "a@x",
        false,
        Some((now_ms - 60_000) as u64),
        "t3287-live",
    )
    .await;
    assert_eq!(live.result, Ok(()));
    chat.send(started(&room, now_ms)).await.unwrap();
    settle().await;
    assert!(live.seen().is_empty());
    assert_eq!(chat.send(MembersOf(room)).await.unwrap(), vec![32_921]);
}

/// A real NATS message on `internal.meeting_instance_started` reaches the
/// registry.
#[actix_rt::test]
#[serial]
async fn a_nats_instance_notice_feeds_the_registry() {
    let nc = nats().await;
    let chat = ChatServer::new(nc.clone()).await.start();
    let now_ms = unix_now_ms();
    let room = format!("inst3287-g-{now_ms}");
    let payload = serde_json::to_vec(&MeetingInstanceStartedPayload {
        room_id: room.clone(),
        started_at_ms: now_ms,
    })
    .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut n = 0;
    let stale = loop {
        nc.publish(MEETING_INSTANCE_STARTED_SUBJECT, payload.clone().into())
            .await
            .expect("publish");
        tokio::time::sleep(Duration::from_millis(200)).await;
        n += 1;
        let probe = join(
            &chat,
            32_930 + n,
            &room,
            "a@x",
            false,
            Some((now_ms - 60_000) as u64),
            "t3287-nats",
        )
        .await;
        if probe.result.is_err() || std::time::Instant::now() > deadline {
            break probe;
        }
    };
    assert_eq!(stale.result, Err(JOIN_REFUSED_STALE_INSTANCE.to_string()));
    assert!(matches!(stale.seen().first(), Some(Seen::Superseded(_))));
}

/// A stale-claim `JoinRoom` for a session that never sent `Connect` is a
/// plain "Session not found": no notice or close, no refusal metric, and the
/// claim must not teach the registry.
#[actix_rt::test]
#[serial]
async fn a_stale_join_for_an_unconnected_session_is_session_not_found_and_leaves_no_trace() {
    let chat = ChatServer::new(nats().await).await.start();
    let now_ms = unix_now_ms();
    let room = format!("inst3287-l-{now_ms}");
    let transport = "t3287-unconnected";
    chat.send(started(&room, now_ms)).await.unwrap();
    let before = refused_total(transport);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe { seen: seen.clone() }.start();
    let stale = send_join(
        &chat,
        32_981,
        &room,
        "ghost@x",
        false,
        Some((now_ms - 60_000) as u64),
        transport,
        &probe,
    )
    .await;
    // A claim ahead of the known start would raise it if this join were recorded.
    let ahead = send_join(
        &chat,
        32_982,
        &room,
        "ghost@x",
        false,
        Some(now_ms as u64 + 3_000),
        transport,
        &probe,
    )
    .await;
    settle().await;

    assert_eq!(stale, Err("Session not found".to_string()));
    assert_eq!(ahead, Err("Session not found".to_string()));
    assert!(seen.lock().unwrap().is_empty(), "nothing is sent");
    assert_eq!(refused_total(transport), before, "no refusal is counted");
    let current = join(
        &chat,
        32_983,
        &room,
        "a@x",
        false,
        Some(now_ms as u64),
        transport,
    )
    .await;
    assert_eq!(
        current.result,
        Ok(()),
        "an unconnected join's claim must not be recorded in the registry"
    );
}
