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

use actix::{prelude::Stream, Actor, StreamHandler};
use actix_cors::Cors;
use actix_http::{
    error::PayloadError,
    ws::{Codec, Message, ProtocolError},
};
use actix_web::{
    cookie::{
        time::{Duration, OffsetDateTime},
        Cookie, SameSite,
    },
    error, get,
    web::{self, Bytes},
    App, Error, HttpRequest, HttpResponse, HttpServer,
};
use actix_web_actors::ws::{handshake, WebsocketContext};
use reqwest::header::LOCATION;
use sec_api::{
    actors::{chat_server::ChatServer, chat_session::WsChatSession},
    auth::{
        fetch_oauth_request, generate_and_store_oauth_request, request_token, upsert_user,
        AuthRequest,
    },
    constants::VALID_ID_PATTERN,
    db::{get_pool, PostgresPool},
    meeting::MeetingManager,
    models::{AppConfig, AppState},
    server_diagnostics::ServerDiagnostics,
};
use tracing::{debug, error, info};
use videocall_types::truthy;

const SCOPE: &str = "email%20profile%20openid";
/**
 * Function used by the Web Application to initiate OAuth.
 *
 * The server responds with the OAuth login URL.
 *
 * The server implements PKCE (Proof Key for Code Exchange) to protect itself and the users.
 */
#[get("/login")]
async fn login(
    pool: web::Data<PostgresPool>,
    cfg: web::Data<AppConfig>,
) -> Result<HttpResponse, Error> {
    // TODO: verify if user exists in the db by looking at the session cookie, (if the client provides one.)
    let pool2 = pool.clone();

    // 2. Generate and Store OAuth Request.
    let (csrf_token, pkce_challenge) = {
        let pool = pool2.clone();
        web::block(move || generate_and_store_oauth_request(pool)).await?
    }
    .map_err(|e| {
        error!("{:?}", e);
        error::ErrorInternalServerError(e)
    })?;

    // 3. Craft OAuth Login URL
    let oauth_login_url = format!("{oauth_url}?client_id={client_id}&redirect_uri={redirect_url}&response_type=code&scope={scope}&prompt=select_account&pkce_challenge={pkce_challenge}&state={state}&access_type=offline",
                                    oauth_url=cfg.oauth_auth_url,
                                    redirect_url=cfg.oauth_redirect_url,
                                    client_id=cfg.oauth_client_id,
                                    scope=SCOPE,
                                    pkce_challenge=pkce_challenge.as_str(),
                                    state=&csrf_token.secret()
    );

    // 4. Redirect the browser to the OAuth Login URL.
    let mut response = HttpResponse::Found();
    response.append_header((LOCATION, oauth_login_url));
    Ok(response.finish())
}

/**
 * Handle OAuth callback from Web App.
 *
 * This service is responsible for using the provided authentication code to fetch
 * the OAuth access_token and refresh token.
 *
 * It upserts the user using their email and stores the access_token & refresh_code.
 */
#[get("/login/callback")]
async fn handle_google_oauth_callback(
    pool: web::Data<PostgresPool>,
    info: web::Query<AuthRequest>,
    cfg: web::Data<AppConfig>,
) -> Result<HttpResponse, Error> {
    let state = info.state.clone();

    // 1. Fetch OAuth request, if this fails, probably a hacker is trying to p*wn us.
    let oauth_request = {
        let pool = pool.clone();
        web::block(move || fetch_oauth_request(pool, state)).await?
    }
    .map_err(|e| {
        error!("{:?}", e);
        error::ErrorBadRequest("couldn't find a request, are you a hacker?")
    })?;

    // 2. Request token from OAuth provider.
    let (oauth_response, claims) = request_token(
        &cfg.oauth_auth_url,
        &cfg.oauth_client_id,
        &cfg.oauth_secret,
        &oauth_request.pkce_verifier,
        &cfg.oauth_token_url,
        &info.code,
    )
    .await
    .map_err(|err| {
        error!("{:?}", err);
        error::ErrorBadRequest("couldn't find a request, are you a hacker?")
    })?;

    // 3. Store tokens and create user.
    {
        let claims = claims.clone();
        web::block(move || upsert_user(pool, &claims, &oauth_response)).await?
    }
    .map_err(|err| {
        error!("{:?}", err);
        error::ErrorInternalServerError(err)
    })?;

    // 4. Create session cookie with email.
    let cookie = Cookie::build("email", claims.email)
        .path("/")
        .same_site(SameSite::Lax)
        // Session lasts only 360 secs to test cookie expiration.
        .expires(OffsetDateTime::now_utc().checked_add(Duration::seconds(360)))
        .finish();

    // 5. Send cookie and redirect browser to AFTER_LOGIN_URL
    let mut response = HttpResponse::Found();
    response.append_header((LOCATION, cfg.after_login_url.clone()));
    response.cookie(cookie);
    Ok(response.finish())
}

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

#[get("/lobby/{email}/{room}")]
pub async fn ws_connect(
    session: web::Path<(String, String)>,
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let (email, room) = session.into_inner();

    // Validate email and room using the same pattern as WebTransport
    let email_clean = email.replace(' ', "_");
    let room_clean = room.replace(' ', "_");
    let re = regex::Regex::new(VALID_ID_PATTERN).unwrap();
    if !re.is_match(&email_clean) || !re.is_match(&room_clean) {
        error!(
            "Invalid email or room format: email={}, room={}",
            email, room
        );
        return Ok(HttpResponse::BadRequest().body("Invalid email or room format"));
    }

    debug!(
        "socket connected for email={}, room={}",
        email_clean, room_clean
    );
    let chat = state.chat.clone();
    let nats_client = state.nats_client.clone();
    let tracker_sender = state.tracker_sender.clone();
    let meeting_manager = state.meeting_manager.clone();
    let actor = WsChatSession::new(chat, room_clean, email_clean, nats_client, tracker_sender, meeting_manager);
    let codec = Codec::new().max_size(1_000_000);
    start_with_codec(actor, &req, stream, codec)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();
    info!("start");

    let nats_url = std::env::var("NATS_URL").expect("NATS_URL env var must be defined");
    let nats_client = async_nats::ConnectOptions::new()
        .require_tls(false)
        .ping_interval(std::time::Duration::from_secs(10))
        .connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");
    // Get database pool if enabled
    // let db_pool = if db_enabled {
    //     Some(get_pool())
    // } else {
    //     None
    // };
    
    let db_pool = Some(get_pool());
    let chat = ChatServer::new(nats_client.clone(), db_pool).await.start();

    // Create connection tracker with message channel
    let (connection_tracker, tracker_sender, tracker_receiver) =
        ServerDiagnostics::new_with_channel(nats_client.clone());

    // Start the connection tracker message processing task
    let connection_tracker = std::sync::Arc::new(connection_tracker);
    let tracker_task = connection_tracker.clone();
    tokio::spawn(async move {
        tracker_task.run_message_loop(tracker_receiver).await;
    });
    let oauth_client_id: String =
        std::env::var("OAUTH_CLIENT_ID").unwrap_or_else(|_| String::from(""));
    let oauth_auth_url: String =
        std::env::var("OAUTH_AUTH_URL").unwrap_or_else(|_| String::from(""));
    let oauth_token_url: String =
        std::env::var("OAUTH_TOKEN_URL").unwrap_or_else(|_| String::from(""));
    let oauth_secret: String =
        std::env::var("OAUTH_CLIENT_SECRET").unwrap_or_else(|_| String::from(""));
    let oauth_redirect_url: String =
        std::env::var("OAUTH_REDIRECT_URL").unwrap_or_else(|_| String::from(""));
    let after_login_url: String = std::env::var("UI_ENDPOINT").unwrap_or_else(|_| String::from(""));
    let db_enabled: bool = truthy(Some(
        &std::env::var("DATABASE_ENABLED").unwrap_or_else(|_| String::from("false")),
    ));

    HttpServer::new(move || {
        let cors = Cors::permissive();

        if oauth_client_id.is_empty() {
            App::new()
                .wrap(cors)
                .app_data(web::Data::new(AppState {
                    chat: chat.clone(),
                    nats_client: nats_client.clone(),
                    tracker_sender: tracker_sender.clone(),
                    meeting_manager: MeetingManager::new(),
                }))
                .service(ws_connect)
        } else {
            let pool = if db_enabled { Some(get_pool()) } else { None };
            App::new()
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(AppState {
                    chat: chat.clone(),
                    nats_client: nats_client.clone(),
                    tracker_sender: tracker_sender.clone(),
                    meeting_manager: MeetingManager::new(),
                }))
                .app_data(web::Data::new(AppConfig {
                    oauth_client_id: oauth_client_id.clone(),
                    oauth_auth_url: oauth_auth_url.clone(),
                    oauth_token_url: oauth_token_url.clone(),
                    oauth_secret: oauth_secret.clone(),
                    oauth_redirect_url: oauth_redirect_url.clone(),
                    after_login_url: after_login_url.clone(),
                }))
                .wrap(cors)
                .service(handle_google_oauth_callback)
                .service(login)
                .service(ws_connect)
        }
    })
    .bind((
        "0.0.0.0",
        std::env::var("ACTIX_PORT")
            .unwrap_or_else(|_| String::from("8080"))
            .parse::<u16>()
            .unwrap(),
    ))?
    .run()
    .await
}
