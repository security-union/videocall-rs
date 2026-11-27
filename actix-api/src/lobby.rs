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

//! WebSocket lobby handler for the Media Server.
//!
//! The only endpoint is **`GET /lobby?token=<JWT>`**: identity and room are
//! extracted from the room access token's claims. The old unauthenticated
//! `GET /lobby/{user_id}/{room}` form is gone and gets a 404.

use actix::prelude::Stream;
use actix::Actor;
use actix::StreamHandler;
use actix_http::error::PayloadError;
use actix_http::ws::{Codec, Message, ProtocolError};
use actix_web::web::Bytes;
use actix_web::{get, web, Error, HttpRequest, HttpResponse};
use actix_web_actors::ws::{handshake, WebsocketContext};
use tracing::{debug, error};

use crate::actors::transports::ws_chat_session::WsChatSession;
use crate::constants::{JWT_SECRET, MAX_FRAME_SIZE};
use crate::models::AppState;
use crate::token_validator;

/// Query parameters for the token-based lobby endpoint.
#[derive(Debug, serde::Deserialize)]
pub struct LobbyTokenQuery {
    /// JWT room access token. Identity and room are extracted from the claims.
    pub token: String,
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

/// WebSocket connection endpoint (token-based).
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
    let jwt_secret: &str = &JWT_SECRET;
    if jwt_secret.is_empty() {
        error!("JWT_SECRET not set");
        return Ok(HttpResponse::InternalServerError().body("Server misconfigured"));
    }

    let claims = match token_validator::decode_room_token(jwt_secret, &query.token) {
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
