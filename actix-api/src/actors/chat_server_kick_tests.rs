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

//! Relay enforcement of host kicks (#2934). Needs a live NATS at `NATS_URL`.

use super::*;
use crate::messages::server::Packet;
use actix::Actor;
use serial_test::serial;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    KickNotice,
    Close,
}

/// A transport stand-in recording the kick notice and the close it is sent.
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
        if meeting.event_type.enum_value() == Ok(MeetingEventType::PARTICIPANT_KICKED) {
            self.seen.lock().unwrap().push(Seen::KickNotice);
        }
    }
}

impl Handler<ForceClose> for Probe {
    type Result = ();
    fn handle(&mut self, _msg: ForceClose, _ctx: &mut Self::Context) {
        self.seen.lock().unwrap().push(Seen::Close);
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
    session: SessionId,
    seen: Arc<Mutex<Vec<Seen>>>,
    result: Result<(), String>,
}

impl Joined {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn join(
    chat: &Addr<ChatServer>,
    session: SessionId,
    room: &str,
    user: &str,
    observer: bool,
    token_iat: Option<i64>,
) -> Joined {
    join_as(chat, session, room, user, observer, token_iat, !observer).await
}

async fn join_as(
    chat: &Addr<ChatServer>,
    session: SessionId,
    room: &str,
    user: &str,
    observer: bool,
    token_iat: Option<i64>,
    activate: bool,
) -> Joined {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe { seen: seen.clone() }.start();
    chat.send(Connect {
        id: session,
        addr: probe.clone().recipient(),
    })
    .await
    .expect("Connect");
    let result = chat
        .send(JoinRoom {
            session,
            room: room.to_string(),
            user_id: user.to_string(),
            display_name: user.to_string(),
            is_guest: false,
            observer,
            instance_id: None,
            is_host: false,
            transport: "websocket".to_string(),
            downlink_congested_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            token_iat,
            closer: Some(probe.recipient()),
        })
        .await
        .expect("JoinRoom");
    if result.is_ok() && activate {
        chat.send(ActivateConnection { session })
            .await
            .expect("ActivateConnection");
    }
    Joined {
        session,
        seen,
        result,
    }
}

fn kick(room: &str, user: &str, through: i64) -> ParticipantKickedPayload {
    ParticipantKickedPayload {
        room_id: room.to_string(),
        user_id: user.to_string(),
        kicked_at: through,
        revoke_iat_through: through,
        deny_until: unix_now_secs() + 3600,
    }
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn media_packet(user: &str) -> Packet {
    let wrapper = PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        user_id: user.as_bytes().to_vec(),
        data: vec![1, 2, 3],
        ..Default::default()
    };
    Packet {
        data: Arc::new(wrapper.write_to_bytes().expect("wrapper")),
        requires_host: false,
    }
}

/// Whether `session`'s `ClientMessage` reaches NATS within `wait`.
async fn publishes(
    chat: &Addr<ChatServer>,
    nc: &async_nats::Client,
    room: &str,
    user: &str,
    session: SessionId,
    wait: Duration,
) -> bool {
    let mut sub = nc
        .subscribe(format!("room.{room}.{session}"))
        .await
        .expect("subscribe");
    nc.flush().await.expect("flush");
    chat.send(ClientMessage {
        session,
        user: user.to_string(),
        room: room.to_string(),
        msg: media_packet(user),
        requires_host: false,
    })
    .await
    .expect("ClientMessage");
    tokio::time::timeout(wait, sub.next()).await.is_ok()
}

/// MUTATION: drop the `PARTICIPANT_KICKED_SUBJECT` subscription in `started`,
/// or the `revoke_session` loop in `Handler<ParticipantKicked>`, and this fails.
#[actix_rt::test]
#[serial]
async fn a_kick_on_nats_closes_every_pre_kick_session_of_that_user_in_that_room_only() {
    let nc = nats().await;
    let chat = ChatServer::new(nc.clone()).await.start();
    let room = format!("kick2934-a-{}", unix_now_secs());
    let other_room = format!("{room}-other");
    let now = unix_now_secs();

    let tab_one = join(&chat, 29_341, &room, "kicked@x", false, Some(now - 10)).await;
    let tab_two = join(&chat, 29_342, &room, "kicked@x", false, Some(now - 10)).await;
    let waiting = join(&chat, 29_343, &room, "kicked@x", true, Some(now - 10)).await;
    let bystander = join(&chat, 29_344, &room, "bystander@x", false, Some(now - 10)).await;
    let elsewhere = join(
        &chat,
        29_345,
        &other_room,
        "kicked@x",
        false,
        Some(now - 10),
    )
    .await;
    for j in [&tab_one, &tab_two, &waiting, &bystander, &elsewhere] {
        assert_eq!(j.result, Ok(()));
    }
    assert!(
        publishes(
            &chat,
            &nc,
            &room,
            "kicked@x",
            tab_one.session,
            Duration::from_secs(2)
        )
        .await
    );

    let payload = serde_json::to_vec(&kick(&room, "kicked@x", now - 5)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while tab_one.seen().is_empty() && std::time::Instant::now() < deadline {
        nc.publish(PARTICIPANT_KICKED_SUBJECT, payload.clone().into())
            .await
            .expect("publish");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    settle().await;

    for revoked in [&tab_one, &tab_two, &waiting] {
        assert_eq!(
            revoked.seen(),
            vec![Seen::KickNotice, Seen::Close],
            "session {} must get the kick notice, then be closed",
            revoked.session
        );
        assert_eq!(
            chat.send(StateOf(revoked.session)).await.unwrap(),
            Some(ConnectionState::Revoked)
        );
    }
    assert!(bystander.seen().is_empty());
    assert!(elsewhere.seen().is_empty());
    assert_eq!(
        chat.send(MembersOf(room.clone())).await.unwrap(),
        vec![bystander.session]
    );

    assert!(
        !publishes(
            &chat,
            &nc,
            &room,
            "kicked@x",
            tab_one.session,
            Duration::from_millis(500)
        )
        .await,
        "a kicked session must not publish"
    );
    chat.send(ActivateConnection {
        session: tab_one.session,
    })
    .await
    .unwrap();
    assert_eq!(
        chat.send(StateOf(tab_one.session)).await.unwrap(),
        Some(ConnectionState::Revoked),
        "re-activation must not resurrect a kicked session"
    );
    assert!(
        publishes(
            &chat,
            &nc,
            &room,
            "bystander@x",
            bystander.session,
            Duration::from_secs(2)
        )
        .await
    );
}

/// MUTATION: drop the `kick_denylist.refuses` check in `JoinRoom`, or make
/// `issued_through` strict (`<`), and this fails.
#[actix_rt::test]
#[serial]
async fn a_join_with_a_token_from_before_the_kick_is_refused_at_the_boundary() {
    let chat = ChatServer::new(nats().await).await.start();
    let room = format!("kick2934-b-{}", unix_now_secs());
    let through = unix_now_secs() - 5;
    chat.send(ParticipantKicked(kick(&room, "kicked@x", through)))
        .await
        .unwrap();

    for (session, iat) in [
        (29_351, Some(through)),
        (29_352, Some(through - 60)),
        (29_353, None),
    ] {
        let refused = join(&chat, session, &room, "kicked@x", false, iat).await;
        assert_eq!(
            refused.result,
            Err(JOIN_REFUSED_KICKED.to_string()),
            "iat {iat:?}"
        );
        settle().await;
        assert_eq!(refused.seen(), vec![Seen::KickNotice, Seen::Close]);
        assert_eq!(
            chat.send(StateOf(session)).await.unwrap(),
            Some(ConnectionState::Revoked)
        );
    }

    let observer = join(&chat, 29_354, &room, "kicked@x", true, Some(through)).await;
    assert_eq!(observer.result, Err(JOIN_REFUSED_KICKED.to_string()));

    for (session, user, in_room, iat) in [
        (29_355, "kicked@x", room.as_str(), Some(through + 1)),
        (29_356, "someone-else@x", room.as_str(), Some(through)),
        (29_357, "kicked@x", "kick2934-b-another-room", Some(through)),
    ] {
        let accepted = join(&chat, session, in_room, user, false, iat).await;
        assert_eq!(
            accepted.result,
            Ok(()),
            "{user} in {in_room} with iat {iat:?}"
        );
        settle().await;
        assert!(accepted.seen().is_empty());
    }
}

/// A kicked attendee may re-join a meeting with no waiting room. A token whose
/// `iat` is past `revoke_iat_through` joins, and a repeated revocation (a
/// retried kick, a presence re-assertion) does not touch it.
#[actix_rt::test]
#[serial]
async fn a_post_kick_rejoin_is_accepted_and_survives_a_repeated_revocation() {
    let chat = ChatServer::new(nats().await).await.start();
    let room = format!("kick2934-c-{}", unix_now_secs());
    let through = unix_now_secs() - 5;
    let old = join(&chat, 29_361, &room, "kicked@x", false, Some(through - 1)).await;
    chat.send(ParticipantKicked(kick(&room, "kicked@x", through)))
        .await
        .unwrap();
    settle().await;
    assert_eq!(old.seen(), vec![Seen::KickNotice, Seen::Close]);

    let rejoined = join(&chat, 29_362, &room, "kicked@x", false, Some(through + 1)).await;
    assert_eq!(rejoined.result, Ok(()));
    chat.send(ParticipantKicked(kick(&room, "kicked@x", through)))
        .await
        .unwrap();
    settle().await;
    assert!(rejoined.seen().is_empty());
    assert_eq!(
        chat.send(StateOf(rejoined.session)).await.unwrap(),
        Some(ConnectionState::Active)
    );
    assert_eq!(
        chat.send(MembersOf(room.clone())).await.unwrap(),
        vec![rejoined.session]
    );
}

/// MUTATION: clear `kick_denylist` for the room in `forget_room_if_empty`
/// (like the other per-room caches) and this fails.
#[actix_rt::test]
#[serial]
async fn the_revocation_outlives_the_room_draining() {
    let chat = ChatServer::new(nats().await).await.start();
    let room = format!("kick2934-d-{}", unix_now_secs());
    let through = unix_now_secs() - 5;
    let kicked = join(&chat, 29_371, &room, "kicked@x", false, Some(through - 1)).await;
    chat.send(ParticipantKicked(kick(&room, "kicked@x", through)))
        .await
        .unwrap();
    chat.send(Disconnect {
        session: kicked.session,
        room: room.clone(),
        user_id: "kicked@x".to_string(),
        display_name: "kicked@x".to_string(),
        is_guest: false,
        observer: false,
    })
    .await
    .unwrap();
    assert!(chat.send(MembersOf(room.clone())).await.unwrap().is_empty());

    let replay = join(&chat, 29_372, &room, "kicked@x", false, Some(through - 1)).await;
    assert_eq!(replay.result, Err(JOIN_REFUSED_KICKED.to_string()));
}

/// The relay announces the kicked session's departure once; its transport's
/// later `Leave` and `Disconnect` add no second PARTICIPANT_LEFT, even after
/// the reconnect grace period.
#[actix_rt::test]
#[serial]
async fn a_kicked_session_is_announced_left_exactly_once() {
    let nc = nats().await;
    let chat = ChatServer::new(nc.clone()).await.start();
    let room = format!("kick2934-e-{}", unix_now_secs());
    let through = unix_now_secs() - 5;
    let mut system = nc.subscribe(format!("room.{room}.system")).await.unwrap();
    nc.flush().await.unwrap();
    let kicked = join(&chat, 29_381, &room, "kicked@x", false, Some(through - 1)).await;
    let _stays = join(&chat, 29_382, &room, "stays@x", false, Some(through - 1)).await;

    chat.send(ParticipantKicked(kick(&room, "kicked@x", through)))
        .await
        .unwrap();
    chat.send(Leave {
        session: kicked.session,
        room: room.clone(),
        user_id: "kicked@x".to_string(),
    })
    .await
    .unwrap();
    chat.send(Disconnect {
        session: kicked.session,
        room: room.clone(),
        user_id: "kicked@x".to_string(),
        display_name: "kicked@x".to_string(),
        is_guest: false,
        observer: false,
    })
    .await
    .unwrap();

    let mut lefts = 0;
    let deadline = tokio::time::Instant::now() + RECONNECT_GRACE_PERIOD + Duration::from_secs(1);
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, system.next()).await {
        let Ok(wrapper) = PacketWrapper::parse_from_bytes(&msg.payload) else {
            continue;
        };
        let Ok(meeting) = MeetingPacket::parse_from_bytes(&wrapper.data) else {
            continue;
        };
        if meeting.event_type.enum_value() == Ok(MeetingEventType::PARTICIPANT_LEFT)
            && meeting.session_id == kicked.session
        {
            lefts += 1;
        }
    }
    assert_eq!(lefts, 1);
    assert_eq!(chat.send(StateOf(kicked.session)).await.unwrap(), None);
}

/// Revoking a kicked user's Active session before their not-yet-activated one
/// would leave the room seemingly occupied: no PARTICIPANT_LEFT fan-out to
/// meeting-api and no empty->idle report.
///
/// MUTATION: sort the revocation targets by session id alone and this fails.
#[actix_rt::test]
#[serial]
async fn the_active_session_of_a_kicked_user_is_revoked_last() {
    let nc = nats().await;
    let chat = ChatServer::new(nc.clone()).await.start();
    let room = format!("kick2934-f-{}", unix_now_secs());
    let through = unix_now_secs() - 5;
    let mut empty = nc.subscribe(MEETING_BECAME_EMPTY_SUBJECT).await.unwrap();
    nc.flush().await.unwrap();
    let active = join(&chat, 29_391, &room, "kicked@x", false, Some(through - 1)).await;
    let testing = join_as(
        &chat,
        29_392,
        &room,
        "kicked@x",
        false,
        Some(through - 1),
        false,
    )
    .await;
    assert_eq!((&active.result, &testing.result), (&Ok(()), &Ok(())));

    chat.send(ParticipantKicked(kick(&room, "kicked@x", through)))
        .await
        .unwrap();
    let mut announced = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, empty.next()).await {
        let payload: MeetingBecameEmptyPayload = serde_json::from_slice(&msg.payload).unwrap();
        if payload.room_id == room {
            announced = true;
            break;
        }
    }
    assert!(announced, "the room must be reported empty");
    assert_eq!(active.seen(), vec![Seen::KickNotice, Seen::Close]);
    assert_eq!(testing.seen(), vec![Seen::KickNotice, Seen::Close]);
}

/// After a relay restart (or a lost core-NATS event) the relay knows nothing of
/// the kick, so a pre-kick token joins; the client sends only RTT probes and
/// never activates, so it is never reported present. The relay's heartbeat
/// still names it for meeting-api's kick check, whose re-published revocation
/// closes it. Needs `DATABASE_URL` (migrated) as well as NATS.
///
/// MUTATION: stop listing unreported users in the relay heartbeat and this
/// fails.
#[actix_rt::test]
#[serial]
async fn a_never_activated_pre_kick_session_is_closed_via_the_heartbeat_kick_check() {
    use meeting_api::db::{meetings as db_meetings, participants as db_participants};

    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = sqlx::PgPool::connect(&url).await.expect("database");
    let room = format!("kick2934-g-{}", unix_now_secs());
    let meeting = db_meetings::create(&pool, &room, "host@x", None, &serde_json::json!([]))
        .await
        .expect("meeting");
    db_participants::upsert_host(&pool, meeting.id, "host@x", None)
        .await
        .expect("host");
    sqlx::query(
        "INSERT INTO meeting_participants \
         (meeting_id, user_id, status, is_host, is_guest, display_name, admitted_at, live_session_id) \
         VALUES ($1, 'kicked@x', 'admitted', FALSE, FALSE, 'kicked@x', NOW(), 0)",
    )
    .bind(meeting.id)
    .execute(&pool)
    .await
    .expect("target");
    let before_kick = unix_now_secs() - 10;
    assert!(matches!(
        db_participants::kick(&pool, meeting.id, "host@x", "kicked@x")
            .await
            .expect("kick"),
        db_participants::KickOutcome::Kicked { .. }
    ));

    let nc = nats().await;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    meeting_api::nats_consumers::spawn_presence_heartbeat_consumer_inner(
        Some(nc.clone()),
        pool.clone(),
        Some(ready_tx),
    )
    .expect("heartbeat consumer");
    ready_rx.await.expect("consumer ready");
    let mut server = ChatServer::new(nc.clone()).await;
    server.presence_heartbeat_interval = Duration::from_millis(300);
    let chat = server.start();

    let listener = join_as(
        &chat,
        29_401,
        &room,
        "kicked@x",
        false,
        Some(before_kick),
        false,
    )
    .await;
    assert_eq!(listener.result, Ok(()), "the relay never saw the kick");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while listener.seen().len() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(listener.seen(), vec![Seen::KickNotice, Seen::Close]);

    sqlx::query("DELETE FROM meeting_participants WHERE meeting_id = $1")
        .bind(meeting.id)
        .execute(&pool)
        .await
        .ok();
    sqlx::query("DELETE FROM meetings WHERE id = $1")
        .bind(meeting.id)
        .execute(&pool)
        .await
        .ok();
}
