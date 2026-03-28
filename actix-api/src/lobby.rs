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

//! WebSocket lobby handlers for the Media Server.
//!
//! Two endpoints:
//!
//! - **`GET /lobby?token=<JWT>`** (primary): Identity and room are extracted
//!   from the JWT claims. This is the only endpoint available when meeting
//!   management is enabled.
//!
//! - **`GET /lobby/{user_id}/{room}`** (deprecated): Identity and room come from
//!   URL path parameters. Only available when `FEATURE_MEETING_MANAGEMENT=false`.
//!   Returns 410 Gone when meeting management is enabled.

use actix::prelude::Stream;
use actix::Actor;
use actix::StreamHandler;
use actix_http::error::PayloadError;
use actix_http::ws::{Codec, Message, ProtocolError};
use actix_web::web::Bytes;
use actix_web::{get, web, Error, HttpRequest, HttpResponse};
use actix_web_actors::ws::{handshake, WebsocketContext};
use tracing::{debug, error};
use videocall_types::FeatureFlags;

use crate::actors::transports::ws_chat_session::WsChatSession;
use crate::constants::{MAX_FRAME_SIZE, VALID_ID_PATTERN};
use crate::models::AppState;
use crate::token_validator;

lazy_static::lazy_static! {
    /// Compiled regex for validating user_id and room in the deprecated path-based endpoint.
    static ref VALID_ID_RE: regex::Regex = regex::Regex::new(VALID_ID_PATTERN).expect("VALID_ID_PATTERN is a valid regex");
}

/// Query parameters for the token-based lobby endpoint.
#[derive(Debug, serde::Deserialize)]
pub struct LobbyTokenQuery {
    /// JWT room access token. Identity and room are extracted from the claims.
    pub token: String,
}

/// Query parameters for the deprecated path-based lobby endpoint.
#[derive(Debug, serde::Deserialize)]
pub struct LobbyQuery {
    /// Ignored in the deprecated endpoint (kept for backward compatibility).
    pub token: Option<String>,
}

/// Start a WebSocket connection with a custom codec.
fn start_with_codec<A, S>(
    actor: A,
    req: &HttpRequest,
    stream: S,
    codec: Codec,
) -> Result<HttpResponse, Error>
where
    A: Actor<Context = WebsocketContext<A>> + StreamHandler<Result<Message, ProtocolError>>,
    S: Stream<Item = Result<Bytes, PayloadError>> + 'static,
{
    let mut res = handshake(req)?;
    Ok(res.streaming(WebsocketContext::with_codec(actor, stream, codec)))
}

/// Primary WebSocket connection endpoint (token-based).
///
/// Identity (user_id) and room are extracted from the JWT claims.
/// No user_id or room in the URL path.
#[get("/lobby")]
pub async fn ws_connect_authenticated(
    query: web::Query<LobbyTokenQuery>,
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let jwt_secret = std::env::var("JWT_SECRET").unwrap_or_default();
    if jwt_secret.is_empty() {
        error!("JWT_SECRET not set");
        return Ok(HttpResponse::InternalServerError().body("Server misconfigured"));
    }

    let claims = match token_validator::decode_room_token(&jwt_secret, &query.token) {
        Ok(c) => c,
        Err(e) => {
            e.log("WS");
            let body = e.client_message().to_string();
            return if e.is_retryable() {
                Ok(HttpResponse::Unauthorized().body(body))
            } else {
                Ok(HttpResponse::Forbidden().body(body))
            };
        }
    };

    let user_id = claims.sub;
    let room = claims.room;
    let observer = claims.observer;
    let display_name = claims.display_name;

    debug!(
        "socket connected (token-based) for user_id={user_id}, room={room}, display_name={display_name}, observer={observer}"
    );
    let chat = state.chat.clone();
    let nats_client = state.nats_client.clone();
    let tracker_sender = state.tracker_sender.clone();
    let session_manager = state.session_manager.clone();
    let actor = WsChatSession::new(
        chat,
        room,
        user_id,
        display_name,
        nats_client,
        tracker_sender,
        session_manager,
        observer,
    );
    let codec = Codec::new().max_size(MAX_FRAME_SIZE);
    start_with_codec(actor, &req, stream, codec)
}

/// **DEPRECATED**: Use `GET /lobby?token=<JWT>` instead.
///
/// Path-based WebSocket connection endpoint (unauthenticated).
/// Identity and room are taken from URL path parameters.
/// Only available when `FEATURE_MEETING_MANAGEMENT` is disabled (FF=off).
/// When FF=on, returns 410 Gone.
#[get("/lobby/{user_id}/{room}")]
pub async fn ws_connect(
    session: web::Path<(String, String)>,
    _query: web::Query<LobbyQuery>,
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    if FeatureFlags::meeting_management_enabled() {
        return Ok(HttpResponse::Gone()
            .body("This endpoint is deprecated. Use GET /lobby?token=<JWT> instead."));
    }

    let (user_id, room) = session.into_inner();

    let user_id_clean = user_id.replace(' ', "_");
    let room_clean = room.replace(' ', "_");
    if !VALID_ID_RE.is_match(&user_id_clean) || !VALID_ID_RE.is_match(&room_clean) {
        error!(
            "Invalid user_id or room format: user_id={}, room={}",
            user_id, room
        );
        return Ok(HttpResponse::BadRequest().body("Invalid user_id or room format"));
    }

    debug!(
        "socket connected (deprecated path-based) for user_id={}, room={}",
        user_id_clean, room_clean
    );
    let chat = state.chat.clone();
    let nats_client = state.nats_client.clone();
    let tracker_sender = state.tracker_sender.clone();
    let session_manager = state.session_manager.clone();
    let actor = WsChatSession::new(
        chat,
        room_clean,
        user_id_clean.clone(),
        user_id_clean, // display_name fallback: use user_id for deprecated path
        nats_client,
        tracker_sender,
        session_manager,
        false, // deprecated path-based endpoint: never observer
    );
    let codec = Codec::new().max_size(MAX_FRAME_SIZE);
    start_with_codec(actor, &req, stream, codec)
}
