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

mod test_helpers;

use axum::body::Body;
use jsonwebtoken::{decode, DecodingKey, Validation};
use meeting_api::db::meetings as db_meetings;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, ParticipantStatusResponse},
    RoomAccessTokenClaims,
};

const HOST: &str = "host-3237@example.com";
const ATTENDEE: &str = "attendee-3237@example.com";

async fn create(pool: &sqlx::PgPool, room_id: &str, extra: serde_json::Value) {
    let mut body = serde_json::json!({ "meeting_id": room_id, "attendees": [] });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    let req = request_with_cookie("POST", "/api/v1/meetings", HOST)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    assert!(resp.status().is_success(), "create meeting {room_id}");
}

async fn send(pool: &sqlx::PgPool, req: axum::http::Request<Body>) -> ParticipantStatusResponse {
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    body.result
}

async fn join(pool: &sqlx::PgPool, room_id: &str, user: &str) -> ParticipantStatusResponse {
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), user)
        .body(Body::empty())
        .unwrap();
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    body.result
}

fn claim(token: &str) -> Option<i64> {
    let mut validation = Validation::default();
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
    decode::<RoomAccessTokenClaims>(
        token,
        &DecodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
        &validation,
    )
    .expect("token should be a valid JWT signed with test secret")
    .claims
    .meeting_started_at_ms
}

async fn started_at_ms(pool: &sqlx::PgPool, room_id: &str) -> i64 {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id")
        .expect("meeting exists")
        .started_at
        .timestamp_millis()
}

#[tokio::test]
#[serial]
async fn owner_join_of_an_ended_meeting_signs_the_new_instance_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3237-owner-restart";
    cleanup_test_data(&pool, room_id).await;

    let first = join(&pool, room_id, HOST).await;
    let first_start = started_at_ms(&pool, room_id).await;
    assert_eq!(
        claim(first.room_token.as_deref().expect("host room token")),
        Some(first_start)
    );

    let meeting = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    db_meetings::end_meeting(&pool, meeting.id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let restarted = join(&pool, room_id, HOST).await;
    let new_start = started_at_ms(&pool, room_id).await;
    assert!(new_start > first_start, "premise: a new instance started");
    assert_eq!(
        claim(restarted.room_token.as_deref().expect("host room token")),
        Some(new_start)
    );

    let attendee = join(&pool, room_id, ATTENDEE).await;
    let token = attendee
        .room_token
        .or(attendee.observer_token)
        .expect("attendee token");
    assert_eq!(claim(&token), Some(new_start));

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn attendee_who_activates_an_idle_meeting_gets_its_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3237-auto-activate";
    cleanup_test_data(&pool, room_id).await;
    create(
        &pool,
        room_id,
        serde_json::json!({ "waiting_room_enabled": false }),
    )
    .await;

    let attendee = join(&pool, room_id, ATTENDEE).await;
    let start = started_at_ms(&pool, room_id).await;
    assert_eq!(
        claim(attendee.room_token.as_deref().expect("auto-admitted token")),
        Some(start)
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn status_poll_token_carries_the_meeting_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3237-status";
    cleanup_test_data(&pool, room_id).await;
    join(&pool, room_id, HOST).await;

    let req = request_with_cookie("GET", &format!("/api/v1/meetings/{room_id}/status"), HOST)
        .body(Body::empty())
        .unwrap();
    let status = send(&pool, req).await;
    let start = started_at_ms(&pool, room_id).await;
    assert_eq!(
        claim(status.room_token.as_deref().expect("admitted token")),
        Some(start)
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn guest_status_poll_token_carries_the_meeting_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3237-guest-status";
    cleanup_test_data(&pool, room_id).await;
    create(
        &pool,
        room_id,
        serde_json::json!({ "allow_guests": true, "waiting_room_enabled": false }),
    )
    .await;
    join(&pool, room_id, HOST).await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room_id}/join-guest"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({ "display_name": "Guest" }).to_string(),
        ))
        .unwrap();
    let guest = send(&pool, req).await;
    let bearer = guest.observer_token.expect("guest observer token");

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v1/meetings/{room_id}/guest-status"))
        .header("Authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let status = send(&pool, req).await;
    let start = started_at_ms(&pool, room_id).await;
    assert_eq!(
        claim(status.room_token.as_deref().expect("admitted guest token")),
        Some(start)
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn co_host_join_token_carries_the_meeting_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3237-co-host";
    cleanup_test_data(&pool, room_id).await;
    create(
        &pool,
        room_id,
        serde_json::json!({ "co_hosts": [ATTENDEE] }),
    )
    .await;
    join(&pool, room_id, HOST).await;

    let co_host = join(&pool, room_id, ATTENDEE).await;
    assert!(co_host.is_host, "premise: joined through the co-host path");
    let start = started_at_ms(&pool, room_id).await;
    assert_eq!(
        claim(co_host.room_token.as_deref().expect("co-host token")),
        Some(start)
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn waiting_guest_status_observer_token_carries_the_meeting_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3237-guest-status-waiting";
    cleanup_test_data(&pool, room_id).await;
    create(
        &pool,
        room_id,
        serde_json::json!({ "allow_guests": true, "waiting_room_enabled": true }),
    )
    .await;
    join(&pool, room_id, HOST).await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room_id}/join-guest"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({ "display_name": "Guest" }).to_string(),
        ))
        .unwrap();
    let guest = send(&pool, req).await;
    assert_eq!(guest.status, "waiting");
    let bearer = guest.observer_token.expect("guest observer token");

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v1/meetings/{room_id}/guest-status"))
        .header("Authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let status = send(&pool, req).await;
    let start = started_at_ms(&pool, room_id).await;
    assert_eq!(
        claim(
            status
                .observer_token
                .as_deref()
                .expect("waiting guest observer token")
        ),
        Some(start)
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn attendee_auto_admitted_into_an_active_meeting_gets_its_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3237-wr-off-active-join";
    cleanup_test_data(&pool, room_id).await;
    create(
        &pool,
        room_id,
        serde_json::json!({ "waiting_room_enabled": false }),
    )
    .await;
    join(&pool, room_id, HOST).await;
    let start = started_at_ms(&pool, room_id).await;

    let attendee = join(&pool, room_id, ATTENDEE).await;
    assert_eq!(attendee.status, "admitted");
    assert!(
        attendee.observer_token.is_none(),
        "premise: auto-admitted, not waiting"
    );
    assert_eq!(
        claim(attendee.room_token.as_deref().expect("room token")),
        Some(start)
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn co_host_who_activates_an_idle_meeting_gets_its_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3237-co-host-activates";
    cleanup_test_data(&pool, room_id).await;
    create(
        &pool,
        room_id,
        serde_json::json!({ "co_hosts": [ATTENDEE] }),
    )
    .await;

    let co_host = join(&pool, room_id, ATTENDEE).await;
    assert!(co_host.is_host, "premise: joined through the co-host path");
    let start = started_at_ms(&pool, room_id).await;
    assert_eq!(
        claim(co_host.room_token.as_deref().expect("co-host token")),
        Some(start)
    );

    cleanup_test_data(&pool, room_id).await;
}

/// Force `room_id` to `idle` with its participants still admitted: the window in
/// which a status poll mints a token for a meeting the relay has already seen
/// go quiet.
async fn force_idle(pool: &sqlx::PgPool, room_id: &str) {
    sqlx::query("UPDATE meetings SET state = 'idle' WHERE room_id = $1")
        .bind(room_id)
        .execute(pool)
        .await
        .expect("force idle");
}

/// Start a new instance of `room_id`: end it, then re-enter as the owner.
async fn restart_as_owner(pool: &sqlx::PgPool, room_id: &str) -> i64 {
    let meeting = db_meetings::get_by_room_id(pool, room_id)
        .await
        .unwrap()
        .unwrap();
    db_meetings::end_meeting(pool, meeting.id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    join(pool, room_id, HOST).await;
    started_at_ms(pool, room_id).await
}

#[tokio::test]
#[serial]
async fn status_poll_token_minted_while_idle_carries_the_last_instance_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3287-status-idle";
    cleanup_test_data(&pool, room_id).await;
    join(&pool, room_id, HOST).await;
    let first_start = started_at_ms(&pool, room_id).await;
    force_idle(&pool, room_id).await;

    let req = request_with_cookie("GET", &format!("/api/v1/meetings/{room_id}/status"), HOST)
        .body(Body::empty())
        .unwrap();
    let status = send(&pool, req).await;
    let token_start = claim(status.room_token.as_deref().expect("admitted token"));
    assert_eq!(
        token_start,
        Some(first_start),
        "an idle meeting's token must carry the previous instance's start, not omit the claim"
    );

    let new_start = restart_as_owner(&pool, room_id).await;
    assert!(
        token_start.unwrap() < new_start,
        "premise: the new instance started after the idle-minted token, so the relay refuses it"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn guest_status_poll_token_minted_while_idle_carries_the_last_instance_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3287-guest-status-idle";
    cleanup_test_data(&pool, room_id).await;
    create(
        &pool,
        room_id,
        serde_json::json!({ "allow_guests": true, "waiting_room_enabled": false }),
    )
    .await;
    join(&pool, room_id, HOST).await;
    let first_start = started_at_ms(&pool, room_id).await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room_id}/join-guest"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({ "display_name": "Guest" }).to_string(),
        ))
        .unwrap();
    let guest = send(&pool, req).await;
    let bearer = guest.observer_token.expect("guest observer token");
    force_idle(&pool, room_id).await;

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v1/meetings/{room_id}/guest-status"))
        .header("Authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let status = send(&pool, req).await;
    let token_start = claim(status.room_token.as_deref().expect("admitted guest token"));
    assert_eq!(token_start, Some(first_start));

    let new_start = restart_as_owner(&pool, room_id).await;
    assert!(token_start.unwrap() < new_start);

    cleanup_test_data(&pool, room_id).await;
}

// ── #3287: a new instance is announced to the relays ─────────────────────

mod instance_announcement {
    use super::*;
    use futures::StreamExt;
    use meeting_api::db::meetings::Activation;
    use meeting_api::nats_events::publish_instance_started;
    use std::time::Duration;
    use videocall_meeting_types::instance::{
        MeetingInstanceStartedPayload, MEETING_INSTANCE_STARTED_SUBJECT,
    };

    async fn nats() -> async_nats::Client {
        maybe_nats().await.expect("NATS_URL must be set")
    }

    async fn watch(nc: &async_nats::Client) -> async_nats::Subscriber {
        let sub = nc
            .subscribe(MEETING_INSTANCE_STARTED_SUBJECT)
            .await
            .expect("subscribe");
        nc.flush().await.expect("flush");
        sub
    }

    /// Announcements for `room` seen within `wait`.
    async fn announced(
        sub: &mut async_nats::Subscriber,
        room: &str,
        wait: Duration,
    ) -> Vec<MeetingInstanceStartedPayload> {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, sub.next()).await {
            let payload: MeetingInstanceStartedPayload =
                serde_json::from_slice(&msg.payload).expect("payload JSON");
            if payload.room_id == room {
                seen.push(payload);
            }
        }
        seen
    }

    async fn join_on(
        pool: &sqlx::PgPool,
        nc: &async_nats::Client,
        room_id: &str,
        user: &str,
    ) -> ParticipantStatusResponse {
        let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), user)
            .body(Body::empty())
            .unwrap();
        let resp = build_app_on_nats(pool.clone(), nc.clone())
            .oneshot(req)
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
        response_json::<APIResponse<ParticipantStatusResponse>>(resp)
            .await
            .result
    }

    async fn end(pool: &sqlx::PgPool, room_id: &str) {
        let meeting = db_meetings::get_by_room_id(pool, room_id)
            .await
            .unwrap()
            .unwrap();
        db_meetings::end_meeting(pool, meeting.id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    async fn post(
        pool: &sqlx::PgPool,
        nc: &async_nats::Client,
        uri: &str,
        body: Option<serde_json::Value>,
    ) {
        let builder = request_with_cookie("POST", uri, HOST);
        let req = match body {
            Some(b) => builder
                .header("Content-Type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = build_app_on_nats(pool.clone(), nc.clone())
            .oneshot(req)
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{uri}: {}", resp.status());
    }

    /// A meeting whose host is in, with `ATTENDEE` waiting, then ended.
    async fn ended_with_a_waiting_attendee(
        pool: &sqlx::PgPool,
        nc: &async_nats::Client,
        room_id: &str,
    ) {
        cleanup_test_data(pool, room_id).await;
        create(pool, room_id, serde_json::json!({})).await;
        join_on(pool, nc, room_id, HOST).await;
        let attendee = join_on(pool, nc, room_id, ATTENDEE).await;
        assert_eq!(attendee.status, "waiting", "premise: attendee is waiting");
        end(pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn new_instance_activation_carries_the_db_started_at_the_tokens_sign() {
        let pool = get_test_pool().await;
        let room_id = "test-3287-activation-ms";
        cleanup_test_data(&pool, room_id).await;
        join(&pool, room_id, HOST).await;
        end(&pool, room_id).await;
        let meeting = db_meetings::get_by_room_id(&pool, room_id)
            .await
            .unwrap()
            .unwrap();

        let (_, activation) = db_meetings::owner_join(&pool, meeting.id, HOST, None, true)
            .await
            .expect("owner_join");

        let db_ms = started_at_ms(&pool, room_id).await;
        assert!(matches!(activation, Activation::NewInstance { .. }));
        assert_eq!(activation.new_instance_started_at_ms(), Some(db_ms));
        let attendee = join(&pool, room_id, ATTENDEE).await;
        let token = attendee
            .room_token
            .or(attendee.observer_token)
            .expect("attendee token");
        assert_eq!(claim(&token), activation.new_instance_started_at_ms());
        assert_eq!(Activation::Unchanged.new_instance_started_at_ms(), None);
        assert_eq!(Activation::Resumed.new_instance_started_at_ms(), None);

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn publish_instance_started_announces_only_a_new_instance() {
        let nc = nats().await;
        let mut sub = watch(&nc).await;
        let room = "inst-3287-publish";

        publish_instance_started(Some(&nc), room, &Activation::Unchanged).await;
        publish_instance_started(Some(&nc), room, &Activation::Resumed).await;
        publish_instance_started(None, room, &Activation::Resumed).await;
        nc.flush().await.unwrap();
        assert!(announced(&mut sub, room, Duration::from_millis(500))
            .await
            .is_empty());

        publish_instance_started(
            Some(&nc),
            room,
            &Activation::NewInstance {
                demoted: vec!["x@example.com".to_string()],
                captions_generation: 3,
                started_at_ms: 1_800_000_000_123,
            },
        )
        .await;
        nc.flush().await.unwrap();
        assert_eq!(
            announced(&mut sub, room, Duration::from_secs(2)).await,
            vec![MeetingInstanceStartedPayload {
                room_id: room.to_string(),
                started_at_ms: 1_800_000_000_123,
            }]
        );
    }

    /// The owner-join and attendee-activation sites each announce the DB start
    /// that the minted tokens carry; joining a live meeting announces nothing.
    #[tokio::test]
    #[serial]
    async fn joins_that_start_an_instance_announce_its_start() {
        let pool = get_test_pool().await;
        let nc = nats().await;
        let mut sub = watch(&nc).await;

        let owner_room = "test-3287-pub-owner";
        cleanup_test_data(&pool, owner_room).await;
        let first = join_on(&pool, &nc, owner_room, HOST).await;
        let got = announced(&mut sub, owner_room, Duration::from_secs(2)).await;
        let start = started_at_ms(&pool, owner_room).await;
        assert_eq!(got.len(), 1, "the first join starts an instance");
        assert_eq!(got[0].started_at_ms, start);
        assert_eq!(
            claim(first.room_token.as_deref().expect("host token")),
            Some(got[0].started_at_ms)
        );

        join_on(&pool, &nc, owner_room, ATTENDEE).await;
        assert!(
            announced(&mut sub, owner_room, Duration::from_millis(500))
                .await
                .is_empty(),
            "joining a live meeting announces nothing"
        );

        end(&pool, owner_room).await;
        join_on(&pool, &nc, owner_room, HOST).await;
        let got = announced(&mut sub, owner_room, Duration::from_secs(2)).await;
        assert_eq!(got.len(), 1, "restarting an ended meeting announces");
        assert_eq!(got[0].started_at_ms, started_at_ms(&pool, owner_room).await);
        assert!(got[0].started_at_ms > start);
        cleanup_test_data(&pool, owner_room).await;

        let attendee_room = "test-3287-pub-attendee";
        cleanup_test_data(&pool, attendee_room).await;
        create(
            &pool,
            attendee_room,
            serde_json::json!({ "waiting_room_enabled": false }),
        )
        .await;
        let attendee = join_on(&pool, &nc, attendee_room, ATTENDEE).await;
        let got = announced(&mut sub, attendee_room, Duration::from_secs(2)).await;
        assert_eq!(
            got.len(),
            1,
            "an attendee activating an idle meeting announces"
        );
        assert_eq!(
            got[0].started_at_ms,
            started_at_ms(&pool, attendee_room).await
        );
        assert_eq!(
            claim(attendee.room_token.as_deref().expect("attendee token")),
            Some(got[0].started_at_ms)
        );
        cleanup_test_data(&pool, attendee_room).await;
    }

    #[tokio::test]
    #[serial]
    async fn admitting_into_an_ended_meeting_announces_the_new_instance() {
        let pool = get_test_pool().await;
        let nc = nats().await;
        let mut sub = watch(&nc).await;
        let room_id = "test-3287-pub-admit";
        ended_with_a_waiting_attendee(&pool, &nc, room_id).await;
        announced(&mut sub, room_id, Duration::from_millis(300)).await;

        post(
            &pool,
            &nc,
            &format!("/api/v1/meetings/{room_id}/admit"),
            Some(serde_json::json!({ "user_id": ATTENDEE })),
        )
        .await;

        let got = announced(&mut sub, room_id, Duration::from_secs(2)).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].started_at_ms, started_at_ms(&pool, room_id).await);
        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn admit_all_into_an_ended_meeting_announces_the_new_instance() {
        let pool = get_test_pool().await;
        let nc = nats().await;
        let mut sub = watch(&nc).await;
        let room_id = "test-3287-pub-admit-all";
        ended_with_a_waiting_attendee(&pool, &nc, room_id).await;
        announced(&mut sub, room_id, Duration::from_millis(300)).await;

        post(
            &pool,
            &nc,
            &format!("/api/v1/meetings/{room_id}/admit-all"),
            None,
        )
        .await;

        let got = announced(&mut sub, room_id, Duration::from_secs(2)).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].started_at_ms, started_at_ms(&pool, room_id).await);
        cleanup_test_data(&pool, room_id).await;
    }

    /// Two joiners racing into an idle meeting (waiting room off) must end in
    /// ONE instance: if activation and admission were separate transactions,
    /// the second would find nobody present and start another.
    #[tokio::test]
    #[serial]
    async fn concurrent_joiners_of_an_idle_meeting_share_one_instance() {
        let pool = get_test_pool().await;
        let nc = nats().await;
        let mut sub = watch(&nc).await;
        let room_id = "test-3287-race-attendees";
        const JOINERS: usize = 6;
        const ROUNDS: usize = 5;

        for round in 0..ROUNDS {
            cleanup_test_data(&pool, room_id).await;
            create(
                &pool,
                room_id,
                serde_json::json!({ "waiting_room_enabled": false }),
            )
            .await;
            announced(&mut sub, room_id, Duration::from_millis(100)).await;

            let tasks: Vec<_> = (0..JOINERS)
                .map(|i| {
                    let (pool, nc) = (pool.clone(), nc.clone());
                    tokio::spawn(async move {
                        let user = format!("racer-{i}-3287@example.com");
                        join_on(&pool, &nc, room_id, &user).await
                    })
                })
                .collect();
            let mut responses = Vec::new();
            for t in tasks {
                responses.push(t.await.expect("join task"));
            }

            let start = started_at_ms(&pool, room_id).await;
            let got = announced(&mut sub, room_id, Duration::from_secs(1)).await;
            assert_eq!(got.len(), 1, "round {round}: exactly one instance starts");
            assert_eq!(got[0].started_at_ms, start);
            for r in &responses {
                assert_eq!(
                    claim(r.room_token.as_deref().expect("auto-admitted token")),
                    Some(start),
                    "round {round}: every joiner holds the one instance's start"
                );
            }
        }
        cleanup_test_data(&pool, room_id).await;
    }

    /// The co-host entry path of the same transaction: a live co-host and an
    /// attendee racing into an idle meeting share one instance, and the co-host
    /// is still admitted as host.
    #[tokio::test]
    #[serial]
    async fn a_co_host_and_an_attendee_racing_into_an_idle_meeting_share_one_instance() {
        let pool = get_test_pool().await;
        let nc = nats().await;
        let mut sub = watch(&nc).await;
        let room_id = "test-3287-race-co-host";
        const ROUNDS: usize = 5;

        for round in 0..ROUNDS {
            cleanup_test_data(&pool, room_id).await;
            create(
                &pool,
                room_id,
                serde_json::json!({ "waiting_room_enabled": false, "co_hosts": [ATTENDEE] }),
            )
            .await;
            announced(&mut sub, room_id, Duration::from_millis(100)).await;

            let (p1, n1, p2, n2) = (pool.clone(), nc.clone(), pool.clone(), nc.clone());
            let co = tokio::spawn(async move { join_on(&p1, &n1, room_id, ATTENDEE).await });
            let other =
                tokio::spawn(
                    async move { join_on(&p2, &n2, room_id, "plain-3287@example.com").await },
                );
            let (co, other) = (co.await.unwrap(), other.await.unwrap());

            let start = started_at_ms(&pool, room_id).await;
            assert!(
                co.is_host,
                "round {round}: the co-host entry path was taken"
            );
            assert_eq!(
                announced(&mut sub, room_id, Duration::from_secs(1))
                    .await
                    .len(),
                1,
                "round {round}: exactly one instance starts"
            );
            for r in [&co, &other] {
                assert_eq!(
                    claim(r.room_token.as_deref().expect("admitted token")),
                    Some(start)
                );
            }
        }
        cleanup_test_data(&pool, room_id).await;
    }

    /// Sequentially, the second joiner of an idle meeting finds the first
    /// present and gets the same start without announcing again.
    #[tokio::test]
    #[serial]
    async fn the_second_attendee_of_an_idle_meeting_is_unchanged_and_shares_the_start() {
        let pool = get_test_pool().await;
        let room_id = "test-3287-second-attendee";
        cleanup_test_data(&pool, room_id).await;
        create(
            &pool,
            room_id,
            serde_json::json!({ "waiting_room_enabled": false }),
        )
        .await;
        let meeting = db_meetings::get_by_room_id(&pool, room_id)
            .await
            .unwrap()
            .unwrap();

        let first = db_meetings::start_instance_and_join(
            &pool, meeting.id, ATTENDEE, None, false, false, true,
        )
        .await
        .unwrap()
        .expect("first joiner");
        let second = db_meetings::start_instance_and_join(
            &pool,
            meeting.id,
            "second-3287@example.com",
            None,
            false,
            false,
            true,
        )
        .await
        .unwrap()
        .expect("second joiner");

        assert!(matches!(first.activation, Activation::NewInstance { .. }));
        assert!(matches!(second.activation, Activation::Unchanged));
        assert_eq!(first.started_at_ms, second.started_at_ms);
        assert_eq!(first.started_at_ms, started_at_ms(&pool, room_id).await);
        assert!(matches!(
            second.entry,
            db_meetings::InstanceEntry::Attendee {
                auto_admitted: true,
                ..
            }
        ));
        cleanup_test_data(&pool, room_id).await;
    }
}

/// Join `joiner` into the active `room_id` while a new instance is uncommitted,
/// committing it only once the join waits on the meeting row lock — after the
/// route's entry read saw the old instance. Returns the join and the new start.
async fn join_across_a_new_instance(
    pool: &sqlx::PgPool,
    room_id: &'static str,
    extra: serde_json::Value,
    joiner: &'static str,
) -> (ParticipantStatusResponse, i64) {
    cleanup_test_data(pool, room_id).await;
    create(pool, room_id, extra).await;
    join(pool, room_id, HOST).await;
    let old_start = started_at_ms(pool, room_id).await;
    let meeting_pk = db_meetings::get_by_room_id(pool, room_id)
        .await
        .unwrap()
        .unwrap()
        .id;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut restart = pool.begin().await.unwrap();
    db_meetings::end_meeting_in(&mut restart, meeting_pk)
        .await
        .unwrap();
    db_meetings::activate(&mut *restart, meeting_pk)
        .await
        .unwrap();

    let join_task = tokio::spawn({
        let pool = pool.clone();
        async move { join(&pool, room_id, joiner).await }
    });
    let mut blocked = false;
    for _ in 0..200 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(blocked, "premise: the join waits on the meeting row lock");
    restart.commit().await.unwrap();

    let joined = join_task.await.unwrap();
    let new_start = started_at_ms(pool, room_id).await;
    assert!(new_start > old_start, "premise: a new instance started");
    (joined, new_start)
}

#[tokio::test]
#[serial]
async fn attendee_join_racing_a_new_instance_signs_the_new_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3287-attendee-races-new-instance";
    let (attendee, new_start) = join_across_a_new_instance(
        &pool,
        room_id,
        serde_json::json!({ "waiting_room_enabled": false }),
        ATTENDEE,
    )
    .await;
    assert_eq!(
        claim(attendee.room_token.as_deref().expect("room token")),
        Some(new_start)
    );
    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn co_host_join_racing_a_new_instance_signs_the_new_start() {
    let pool = get_test_pool().await;
    let room_id = "test-3287-co-host-races-new-instance";
    let (co_host, new_start) = join_across_a_new_instance(
        &pool,
        room_id,
        serde_json::json!({ "co_hosts": [ATTENDEE] }),
        ATTENDEE,
    )
    .await;
    assert!(co_host.is_host, "premise: joined through the co-host path");
    assert_eq!(
        claim(co_host.room_token.as_deref().expect("co-host token")),
        Some(new_start)
    );
    cleanup_test_data(&pool, room_id).await;
}
