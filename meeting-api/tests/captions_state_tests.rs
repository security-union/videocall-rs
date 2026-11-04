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

//! Server-side captions state: generation bookkeeping and the revokes the
//! server publishes on its own. Requires `DATABASE_URL` and `NATS_URL`.

mod captions_common;
mod test_helpers;

use std::time::{Duration, Instant};

use captions_common::*;
use meeting_api::captions::{self, RevokeReason};
use meeting_api::db::meetings as db_meetings;
use meeting_api::db::participants as db_participants;
use meeting_api::nats_consumers;
use meeting_api::nats_events::{
    MeetingBecameEmptyPayload, MeetingEndedByHostPayload, MEETING_BECAME_EMPTY_SUBJECT,
    MEETING_ENDED_BY_HOST_SUBJECT,
};
use meeting_api::state::AppState;
use serial_test::serial;
use test_helpers::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ATTENDEE: &str = "captions-attendee@example.com";
const GUEST: &str = "guest:captions-guest";

#[tokio::test]
#[serial]
async fn every_captions_change_bumps_the_generation() {
    let room = "captions-generation-db";
    let state = build_state(get_test_pool().await, None, None);
    create_meeting(&state, room, serde_json::json!({})).await;
    let (db, pk) = (&state.db, meeting_pk(&state, room).await);

    assert_eq!(
        db_meetings::captions_generation_if_on(db, pk)
            .await
            .unwrap(),
        None
    );
    assert_eq!(db_meetings::enable_captions(db, pk).await.unwrap(), Some(1));
    assert_eq!(db_meetings::enable_captions(db, pk).await.unwrap(), Some(1));
    assert_eq!(
        db_meetings::captions_generation_if_on(db, pk)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        db_meetings::revoke_captions_if_inactive(db, pk)
            .await
            .unwrap(),
        None
    );
    assert_eq!(db_meetings::revoke_captions(db, pk).await.unwrap(), Some(2));
    assert_eq!(
        db_meetings::captions_generation_if_on(db, pk)
            .await
            .unwrap(),
        None
    );
    assert_eq!(db_meetings::enable_captions(db, pk).await.unwrap(), Some(3));

    db_meetings::end_meeting(db, pk).await.unwrap();
    assert_eq!(
        db_meetings::captions_generation_if_on(db, pk)
            .await
            .unwrap(),
        None
    );
    assert_eq!(db_meetings::enable_captions(db, pk).await.unwrap(), None);
    assert_eq!(
        db_meetings::revoke_captions_if_inactive(db, pk)
            .await
            .unwrap(),
        Some((room.to_string(), 4))
    );
    assert_eq!(
        db_meetings::revoke_captions_if_inactive(db, pk)
            .await
            .unwrap(),
        None
    );
    assert_eq!(captions_state(&state, room).await, (false, 4));

    assert_eq!(db_meetings::revoke_captions(db, pk).await.unwrap(), Some(5));
    sqlx::query("UPDATE meetings SET deleted_at = NOW() WHERE id = $1")
        .bind(pk)
        .execute(db)
        .await
        .unwrap();
    assert_eq!(db_meetings::revoke_captions(db, pk).await.unwrap(), None);
    let generation: i64 =
        sqlx::query_scalar("SELECT captions_generation FROM meetings WHERE id = $1")
            .bind(pk)
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(generation, 5);
}

#[tokio::test]
#[serial]
async fn a_deleted_meeting_cannot_be_locked_for_captions() {
    let room = "captions-lock-deleted";
    let state = build_state(get_test_pool().await, None, None);
    create_meeting(&state, room, serde_json::json!({})).await;
    let pk = meeting_pk(&state, room).await;

    let mut tx = state.db.begin().await.unwrap();
    assert_eq!(
        db_meetings::lock_for_captions(&mut tx, pk).await.unwrap(),
        Some(())
    );
    tx.rollback().await.unwrap();

    db_meetings::soft_delete(&state.db, room, HOST)
        .await
        .unwrap()
        .expect("the creator soft-deletes the meeting");
    let mut tx = state.db.begin().await.unwrap();
    assert_eq!(
        db_meetings::lock_for_captions(&mut tx, pk).await.unwrap(),
        None
    );
    tx.rollback().await.unwrap();
}

#[derive(Debug, Clone, Copy)]
enum Trigger {
    EndRoute,
    LastHostLeavesAndEnds,
    LastHostLeavesToIdle,
    DeleteRoute,
    DepartureToIdle,
    RelayHostEnd,
    RelayBecameEmpty,
    OwnerRejoinStartsNewInstance,
    JoinerStartsNewInstance,
    AdmitStartsNewInstance,
    AdmitAllStartsNewInstance,
    LastGuestLeavesAndEnds,
}

async fn everyone_gone(state: &AppState, room: &str) {
    sqlx::query(
        "UPDATE meeting_participants SET status = 'left', left_at = NOW() \
         WHERE meeting_id = (SELECT id FROM meetings WHERE room_id = $1)",
    )
    .bind(room)
    .execute(&state.db)
    .await
    .unwrap();
}

async fn via_consumer(
    state: &AppState,
    nats: &async_nats::Client,
    room: &str,
    empty: bool,
) -> tokio::task::JoinHandle<()> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (db, feed_tx) = (
        state.db.clone(),
        meeting_api::feed_events::new_feed_channel().0,
    );
    let handle = if empty {
        nats_consumers::spawn_meeting_became_empty_consumer_inner(
            Some(nats.clone()),
            db,
            feed_tx,
            Some(ready_tx),
        )
    } else {
        nats_consumers::spawn_meeting_ended_by_host_consumer_inner(
            Some(nats.clone()),
            db,
            feed_tx,
            Some(ready_tx),
        )
    }
    .unwrap();
    ready_rx.await.unwrap();
    let (subject, payload) = if empty {
        let p = MeetingBecameEmptyPayload {
            room_id: room.into(),
        };
        (
            MEETING_BECAME_EMPTY_SUBJECT,
            serde_json::to_vec(&p).unwrap(),
        )
    } else {
        let p = MeetingEndedByHostPayload {
            room_id: room.into(),
        };
        (
            MEETING_ENDED_BY_HOST_SUBJECT,
            serde_json::to_vec(&p).unwrap(),
        )
    };
    nats.publish(subject, payload.into()).await.unwrap();
    handle
}

/// Fires `trigger`; a returned consumer must outlive the caller's wait for its revoke.
async fn fire(
    trigger: Trigger,
    state: &AppState,
    nats: &async_nats::Client,
    room: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    let uri = format!("/api/v1/meetings/{room}");
    let null = serde_json::Value::Null;
    match trigger {
        Trigger::EndRoute => as_user(state, "POST", &format!("{uri}/end"), HOST, null).await,
        Trigger::LastHostLeavesAndEnds | Trigger::LastHostLeavesToIdle => {
            as_user(state, "POST", &format!("{uri}/leave"), HOST, null).await
        }
        Trigger::DeleteRoute => as_user(state, "DELETE", &uri, HOST, null).await,
        Trigger::DepartureToIdle => {
            let pk = meeting_pk(state, room).await;
            let (_, end) = db_participants::depart(&state.db, pk, HOST, false, true)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(end, None);
            let feed_tx = meeting_api::feed_events::new_feed_channel().0;
            nats_consumers::announce_departure(&state.db, Some(nats), &feed_tx, pk, room, end)
                .await;
        }
        Trigger::RelayHostEnd => return Some(via_consumer(state, nats, room, false).await),
        Trigger::RelayBecameEmpty => {
            everyone_gone(state, room).await;
            return Some(via_consumer(state, nats, room, true).await);
        }
        Trigger::OwnerRejoinStartsNewInstance => {
            everyone_gone(state, room).await;
            join(state, room, HOST).await;
        }
        Trigger::JoinerStartsNewInstance => {
            everyone_gone(state, room).await;
            sqlx::query("UPDATE meetings SET state = 'idle' WHERE room_id = $1")
                .bind(room)
                .execute(&state.db)
                .await
                .unwrap();
            join(state, room, ATTENDEE).await;
        }
        Trigger::AdmitStartsNewInstance | Trigger::AdmitAllStartsNewInstance => {
            join(state, room, ATTENDEE).await;
            sqlx::query(
                "UPDATE meeting_participants SET admitted_at = NOW() - INTERVAL '1 hour', \
                 presence_seen_at = NULL, live_session_id = 0 \
                 WHERE meeting_id = (SELECT id FROM meetings WHERE room_id = $1) \
                   AND status = 'admitted'",
            )
            .bind(room)
            .execute(&state.db)
            .await
            .unwrap();
            sqlx::query("UPDATE meetings SET state = 'idle' WHERE room_id = $1")
                .bind(room)
                .execute(&state.db)
                .await
                .unwrap();
            db_participants::force_heartbeat_watermark_fresh_for_test(&state.db)
                .await
                .unwrap();
            let state = &build_state(state.db.clone(), None, Some(nats.clone()));
            if matches!(trigger, Trigger::AdmitStartsNewInstance) {
                let body = serde_json::json!({ "user_id": ATTENDEE });
                as_user(state, "POST", &format!("{uri}/admit"), HOST, body).await
            } else {
                as_user(state, "POST", &format!("{uri}/admit-all"), HOST, null).await
            }
        }
        Trigger::LastGuestLeavesAndEnds => {
            everyone_gone(state, room).await;
            sqlx::query(
                "INSERT INTO meeting_participants \
                 (meeting_id, user_id, status, is_host, is_guest, admitted_at) \
                 VALUES ((SELECT id FROM meetings WHERE room_id = $1 AND deleted_at IS NULL), \
                         $2, 'admitted', FALSE, TRUE, NOW())",
            )
            .bind(room)
            .bind(GUEST)
            .execute(&state.db)
            .await
            .unwrap();
            let token = meeting_api::token::generate_room_token(
                TEST_JWT_SECRET,
                600,
                GUEST,
                room,
                false,
                "Guest",
                true,
                true,
            )
            .unwrap();
            let req = axum::http::Request::builder()
                .method("POST")
                .uri(format!("{uri}/leave-guest"))
                .header("Authorization", format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .unwrap();
            let (status, json) = send(state, req).await;
            assert!(status.is_success(), "leave-guest: {status} {json}");
        }
    }
    None
}

impl Trigger {
    fn reason(self) -> RevokeReason {
        match self {
            Trigger::EndRoute
            | Trigger::LastHostLeavesAndEnds
            | Trigger::RelayHostEnd
            | Trigger::LastGuestLeavesAndEnds => RevokeReason::Ended,
            Trigger::LastHostLeavesToIdle | Trigger::DepartureToIdle => RevokeReason::Departure,
            Trigger::DeleteRoute => RevokeReason::Deleted,
            Trigger::RelayBecameEmpty => RevokeReason::Idle,
            Trigger::OwnerRejoinStartsNewInstance
            | Trigger::JoinerStartsNewInstance
            | Trigger::AdmitStartsNewInstance
            | Trigger::AdmitAllStartsNewInstance => RevokeReason::NewInstance,
        }
    }

    fn new_instance(self) -> bool {
        self.reason() == RevokeReason::NewInstance
    }
}

#[tokio::test]
#[serial]
async fn server_revokes_captions_left_on_by_an_inactive_or_restarted_meeting() {
    let nats = maybe_nats().await.expect("NATS_URL must be set");
    let state = build_state(get_test_pool().await, None, Some(nats.clone()));
    for trigger in [
        Trigger::EndRoute,
        Trigger::LastHostLeavesAndEnds,
        Trigger::LastHostLeavesToIdle,
        Trigger::DeleteRoute,
        Trigger::DepartureToIdle,
        Trigger::RelayHostEnd,
        Trigger::RelayBecameEmpty,
        Trigger::OwnerRejoinStartsNewInstance,
        Trigger::JoinerStartsNewInstance,
        Trigger::AdmitStartsNewInstance,
        Trigger::AdmitAllStartsNewInstance,
        Trigger::LastGuestLeavesAndEnds,
    ] {
        for on in [true, false] {
            let room = "captions-server-revoke";
            let opts = match trigger {
                Trigger::LastHostLeavesToIdle | Trigger::DepartureToIdle => {
                    serde_json::json!({ "end_on_host_leave": false })
                }
                Trigger::JoinerStartsNewInstance => {
                    serde_json::json!({ "waiting_room_enabled": false })
                }
                _ => serde_json::json!({}),
            };
            let mut sub = create_subscribed(&state, &nats, room, opts).await;
            if on {
                let pk = meeting_pk(&state, room).await;
                db_meetings::enable_captions(&state.db, pk).await.unwrap();
            }
            let consumer = fire(trigger, &state, &nats, room).await;
            if on || trigger.new_instance() {
                let generation = if on { 2 } else { 0 };
                assert_eq!(
                    next_payload(&mut sub).await,
                    server_revoke(room, generation, trigger.reason()),
                    "{trigger:?}"
                );
                assert_eq!(
                    captions_state(&state, room).await,
                    (false, generation),
                    "{trigger:?}"
                );
            } else {
                assert_nothing_published(&mut sub).await;
                assert_eq!(
                    captions_state(&state, room).await,
                    (false, 0),
                    "{trigger:?}"
                );
            }
            if let Some(consumer) = consumer {
                consumer.abort();
            }
        }
    }
}

#[tokio::test]
#[serial]
async fn captions_are_live_only_while_the_meeting_is_active() {
    let room = "captions-idle";
    let state = build_state(get_test_pool().await, None, None);
    create_meeting(&state, room, serde_json::json!({})).await;
    let (db, pk) = (&state.db, meeting_pk(&state, room).await);
    assert_eq!(db_meetings::enable_captions(db, pk).await.unwrap(), Some(1));

    sqlx::query("UPDATE meetings SET state = 'idle' WHERE id = $1")
        .bind(pk)
        .execute(db)
        .await
        .unwrap();
    assert_eq!(
        db_meetings::captions_generation_if_on(db, pk)
            .await
            .unwrap(),
        None
    );
    assert_eq!(db_meetings::enable_captions(db, pk).await.unwrap(), None);
    assert_eq!(captions_state(&state, room).await, (true, 1));

    // Active again but soft-deleted: neither query may treat it as live.
    sqlx::query("UPDATE meetings SET state = 'active', deleted_at = NOW() WHERE id = $1")
        .bind(pk)
        .execute(db)
        .await
        .unwrap();
    assert_eq!(
        db_meetings::captions_generation_if_on(db, pk)
            .await
            .unwrap(),
        None
    );
    assert_eq!(db_meetings::enable_captions(db, pk).await.unwrap(), None);
    assert_eq!(captions_state(&state, room).await, (true, 1));
}

#[tokio::test]
#[serial]
async fn a_reused_room_id_continues_the_captions_generation() {
    let room = "captions-reused-room";
    let state = build_state(get_test_pool().await, None, None);
    create_meeting(&state, room, serde_json::json!({})).await;
    let pk = meeting_pk(&state, room).await;
    assert_eq!(
        db_meetings::enable_captions(&state.db, pk).await.unwrap(),
        Some(1)
    );
    assert_eq!(
        db_meetings::revoke_captions(&state.db, pk).await.unwrap(),
        Some(2)
    );
    as_user(
        &state,
        "DELETE",
        &format!("/api/v1/meetings/{room}"),
        HOST,
        serde_json::Value::Null,
    )
    .await;

    // Not create_meeting: that deletes the old row, and the point is that
    // the soft-deleted one stays behind.
    as_user(
        &state,
        "POST",
        "/api/v1/meetings",
        HOST,
        serde_json::json!({ "meeting_id": room, "attendees": [] }),
    )
    .await;
    join(&state, room, HOST).await;
    let pk2 = meeting_pk(&state, room).await;
    assert_ne!(pk, pk2, "a new row");
    assert_eq!(
        db_meetings::enable_captions(&state.db, pk2).await.unwrap(),
        Some(3),
        "generations continue across the room id's lifetimes"
    );
}

#[tokio::test]
#[serial]
async fn a_reused_room_id_skips_the_revoke_still_owed_to_the_deleted_row() {
    let room = "captions-reused-room-on";
    let state = build_state(get_test_pool().await, None, None);
    create_meeting(&state, room, serde_json::json!({})).await;
    let pk = meeting_pk(&state, room).await;
    assert_eq!(
        db_meetings::enable_captions(&state.db, pk).await.unwrap(),
        Some(1)
    );
    sqlx::query("UPDATE meetings SET deleted_at = NOW() WHERE id = $1")
        .bind(pk)
        .execute(&state.db)
        .await
        .unwrap();

    as_user(
        &state,
        "POST",
        "/api/v1/meetings",
        HOST,
        serde_json::json!({ "meeting_id": room, "attendees": [] }),
    )
    .await;
    join(&state, room, HOST).await;
    let pk2 = meeting_pk(&state, room).await;
    assert_ne!(pk, pk2, "a new row");
    let lease = db_meetings::enable_captions(&state.db, pk2)
        .await
        .unwrap()
        .unwrap();

    captions::reconcile(&state.db, None).await;
    let old: (bool, i64) =
        sqlx::query_as("SELECT captions_on, captions_generation FROM meetings WHERE id = $1")
            .bind(pk)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert!(!old.0, "the deleted row was revoked");
    assert!(
        lease > old.1,
        "lease {lease} vs the old row's revoke {}",
        old.1
    );
    assert_eq!(captions_state(&state, room).await, (true, lease));
}

#[tokio::test]
#[serial]
async fn presence_sweep_revokes_captions_left_on_by_a_lost_revoke() {
    let nats = maybe_nats().await.expect("NATS_URL must be set");
    let state = build_state(get_test_pool().await, None, Some(nats.clone()));
    let stale = [
        ("captions-stale-idle", "state = 'idle'"),
        ("captions-stale-ended", "state = 'ended'"),
        ("captions-stale-deleted", "deleted_at = NOW()"),
        ("captions-live", "state = 'active'"),
    ];
    let mut subs = Vec::new();
    for (room, set) in stale {
        let sub = create_subscribed(&state, &nats, room, serde_json::json!({})).await;
        let pk = meeting_pk(&state, room).await;
        db_meetings::enable_captions(&state.db, pk).await.unwrap();
        everyone_gone(&state, room).await;
        sqlx::query(&format!("UPDATE meetings SET {set} WHERE id = $1"))
            .bind(pk)
            .execute(&state.db)
            .await
            .unwrap();
        subs.push(sub);
    }

    let feed_tx = meeting_api::feed_events::new_feed_channel().0;
    nats_consumers::sweep_presence(&state.db, Some(&nats), &feed_tx)
        .await
        .unwrap();

    for ((room, _), sub) in stale.iter().zip(subs.iter_mut()).take(3) {
        let expected = server_revoke(room, 2, RevokeReason::Reconcile);
        assert_eq!(next_payload(sub).await, expected, "{room}");
        assert_eq!(captions_state(&state, room).await, (false, 2), "{room}");
    }
    assert_nothing_published(&mut subs[3]).await;
    assert_eq!(captions_state(&state, "captions-live").await, (true, 1));
}

#[tokio::test]
async fn invalid_room_id_publishes_nothing() {
    let nats = maybe_nats().await.expect("NATS_URL must be set");
    let mut sub = nats.subscribe("captions.control.a.b").await.unwrap();
    nats.flush().await.unwrap();
    let payload = captions::control_payload("a.b".into(), HOST.into(), true, 1);
    captions::publish_captions_control(&nats, &payload)
        .await
        .unwrap();
    assert_nothing_published(&mut sub).await;
}

/// A NATS server that completes the handshake, then vanishes, so a flush never completes.
async fn vanishing_nats() -> async_nats::Client {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let info = b"INFO {\"server_id\":\"fake\",\"max_payload\":1048576}\r\n";
        sock.write_all(info).await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await;
        sock.write_all(b"PONG\r\n").await.unwrap();
    });
    async_nats::connect(addr.to_string()).await.unwrap()
}

#[tokio::test]
#[serial]
async fn revoke_if_inactive_does_not_wait_for_the_publish() {
    let room = "captions-revoke-off-task";
    let state = build_state(get_test_pool().await, None, None);
    create_meeting(&state, room, serde_json::json!({})).await;
    let pk = meeting_pk(&state, room).await;
    db_meetings::enable_captions(&state.db, pk).await.unwrap();
    sqlx::query("UPDATE meetings SET state = 'idle' WHERE id = $1")
        .bind(pk)
        .execute(&state.db)
        .await
        .unwrap();

    let nats = vanishing_nats().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while nats.connection_state() == async_nats::connection::State::Connected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let revoke =
        captions::revoke_if_inactive(&state.db, Some(&nats), pk, RevokeReason::Departure, None);
    assert!(tokio::time::timeout(Duration::from_secs(2), revoke)
        .await
        .is_ok());
    assert_eq!(captions_state(&state, room).await, (false, 2));
}

#[tokio::test]
async fn publish_that_cannot_flush_times_out() {
    let nats = vanishing_nats().await;
    let payload = captions::control_payload("captions-timeout".into(), HOST.into(), true, 1);
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        captions::publish_captions_control(&nats, &payload),
    )
    .await
    .expect("publish gave up on its own");
    assert!(result.is_err());
    assert!(started.elapsed() >= captions::PUBLISH_TIMEOUT);
}
