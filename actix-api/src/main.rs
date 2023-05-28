mod actors;
mod constants;
mod messages;
mod models;

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
    App, Error, HttpRequest, HttpResponse, HttpServer, Responder,
};
use actix_web_actors::ws::{handshake, WebsocketContext};
use log::info;

use crate::{
    actors::{chat_server::ChatServer, chat_session::WsChatSession},
    auth::{fetch_oauth_request, generate_and_store_oauth_request, request_token, upsert_user},
    models::AppState,
};
use crate::{
    auth::AuthRequest,
    db::{get_pool, PostgresPool},
};
use reqwest::header::LOCATION;

const OAUTH_CLIENT_ID: &str = std::env!("OAUTH_CLIENT_ID");
const OAUTH_AUTH_URL: &str = std::env!("OAUTH_AUTH_URL");
const OAUTH_TOKEN_URL: &str = std::env!("OAUTH_TOKEN_URL");
const OAUTH_SECRET: &str = std::env!("OAUTH_CLIENT_SECRET");
const OAUTH_REDIRECT_URL: &str = std::env!("OAUTH_REDIRECT_URL");
const SCOPE: &str = "email%20profile%20openid";
const ACTIX_PORT: &str = std::env!("ACTIX_PORT");
const AFTER_LOGIN_URL: &str = concat!("http://localhost:", std::env!("TRUNK_SERVE_PORT"));

pub mod auth;
pub mod db;

/**
 * Function used by the Web Application to initiate OAuth.
 *
 * The server responds with the OAuth login URL.
 *
 * The server implements PKCE (Proof Key for Code Exchange) to protect itself and the users.
 */
#[get("/login")]
async fn login(pool: web::Data<PostgresPool>) -> Result<HttpResponse, Error> {
    // TODO: verify if user exists in the db by looking at the session cookie, (if the client provides one.)
    let pool2 = pool.clone();

    // 2. Generate and Store OAuth Request.
    let (csrf_token, pkce_challenge) = {
        let pool = pool2.clone();
        web::block(move || generate_and_store_oauth_request(pool)).await?
    }
    .map_err(|e| {
        log::error!("{:?}", e);
        error::ErrorInternalServerError(e)
    })?;

    // 3. Craft OAuth Login URL
    let oauth_login_url = format!("{oauth_url}?client_id={client_id}&redirect_uri={redirect_url}&response_type=code&scope={scope}&prompt=select_account&pkce_challenge={pkce_challenge}&state={state}&access_type=offline",
                                    oauth_url=OAUTH_AUTH_URL,
                                    redirect_url=OAUTH_REDIRECT_URL,
                                    client_id=OAUTH_CLIENT_ID,
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
) -> Result<HttpResponse, Error> {
    let state = info.state.clone();

    // 1. Fetch OAuth request, if this fails, probably a hacker is trying to p*wn us.
    let oauth_request = {
        let pool = pool.clone();
        web::block(move || fetch_oauth_request(pool, state)).await?
    }
    .map_err(|e| {
        log::error!("{:?}", e);
        error::ErrorBadRequest("couldn't find a request, are you a hacker?")
    })?;

    // 2. Request token from OAuth provider.
    let (oauth_response, claims) = request_token(
        OAUTH_REDIRECT_URL,
        OAUTH_CLIENT_ID,
        OAUTH_SECRET,
        &oauth_request.pkce_verifier,
        OAUTH_TOKEN_URL,
        &info.code,
    )
    .await
    .map_err(|err| {
        log::error!("{:?}", err);
        error::ErrorBadRequest("couldn't find a request, are you a hacker?")
    })?;

    // 3. Store tokens and create user.
    {
        let claims = claims.clone();
        web::block(move || upsert_user(pool, &claims, &oauth_response)).await?
    }
    .map_err(|err| {
        log::error!("{:?}", err);
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
    response.append_header((LOCATION, AFTER_LOGIN_URL));
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
) -> impl Responder {
    let (email, room) = session.into_inner();
    info!("socket connected");
    let chat = state.chat.clone();
    let actor = WsChatSession::new(chat, room, email);
    let codec = Codec::new().max_size(1_000_000);

    start_with_codec(actor, &req, stream, codec)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    info!("start");
    let chat = ChatServer::new().start();

    HttpServer::new(move || {
        let cors = Cors::permissive();

        let pool = get_pool();

        App::new()
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(AppState { chat: chat.clone() }))
            .wrap(cors)
            .service(handle_google_oauth_callback)
            .service(login)
            .service(ws_connect)
    })
    .bind(("0.0.0.0", ACTIX_PORT.parse::<u16>().unwrap()))?
    .run()
    .await
}
