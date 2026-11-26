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

mod bridge;

use crate::actors::chat_server::ChatServer;
use crate::actors::transports::wt_chat_session::{WtChatSession, WtOutbound};
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use crate::token_validator::{self, TokenError};
use actix::prelude::*;
use anyhow::{Context, Result};
use bridge::WebTransportBridge;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::Read;
use std::time::Duration;
use std::{fs, io};
use std::{net::SocketAddr, path::PathBuf};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace_span, warn};
use videocall_meeting_types::token::RoomAccessTokenClaims;
use web_transport_quinn::http::StatusCode;

lazy_static::lazy_static! {
    static ref QUIC_MAX_IDLE_TIMEOUT_SECS: u64 = std::env::var("QUIC_MAX_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    static ref QUIC_KEEP_ALIVE_INTERVAL_SECS: u64 = std::env::var("QUIC_KEEP_ALIVE_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
}

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

#[cfg(test)]
lazy_static::lazy_static! {
    static ref TEST_PACKET_COUNTERS: Arc<Mutex<HashMap<String, AtomicU64>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

#[cfg(test)]
fn increment_test_packet_counter_for_user(username: &str) {
    let counters = TEST_PACKET_COUNTERS.clone();
    let mut counters_map = counters.lock().unwrap();
    let counter = counters_map
        .entry(username.to_string())
        .or_insert_with(|| AtomicU64::new(0));
    counter.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
fn get_test_packet_counter_for_user(username: &str) -> u64 {
    let counters = TEST_PACKET_COUNTERS.clone();
    let counters_map = counters.lock().unwrap();
    counters_map
        .get(username)
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

#[cfg(test)]
fn reset_test_packet_counters() {
    let counters = TEST_PACKET_COUNTERS.clone();
    let mut counters_map = counters.lock().unwrap();
    counters_map.clear();
}

use web_transport_quinn::{quinn, Session};

/// How long a refused CONNECT keeps its QUIC connection open after the error
/// status is written.
///
/// Dropping the last connection handle closes the connection at once, and
/// quinn discards stream data that hasn't gone out yet, so the client would
/// see a bare connection close instead of the status. Holding the connection
/// lets the response be sent and retransmitted on a lossy, high-latency link.
/// We stop early as soon as the client closes, which a client does right
/// after reading a non-200 CONNECT response.
const REJECT_LINGER: Duration = Duration::from_secs(3);

/// Upper bound on the QUIC handshake plus the HTTP/3 SETTINGS and CONNECT
/// exchange, measured from the moment the endpoint hands us the connection.
///
/// Without it, a client that finishes QUIC and never opens its SETTINGS or
/// CONNECT stream holds a task and a connection until the 30 s idle timeout,
/// and our 1 s keep-alives stop that from ever firing.
///
/// Why 10 s: a clean handshake is about three round trips, roughly 1 s on a
/// 300 ms mobile link. Each lost flight costs a probe timeout, which starts
/// near 1 s before quinn has an RTT sample and doubles on every repeat, so
/// three losses in a row add about 1 + 2 + 4 = 7 s. 10 s rides out that much
/// loss; a link worse than that can't carry a call anyway. It applies only
/// before the session is accepted, never to a running session.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Videocall WebTransport API
///
/// This module contains the implementation of the Videocall WebTransport API.
/// It is responsible for accepting incoming WebTransport connections and handling them.
/// It also contains the logic for handling the WebTransport handshake and the WebTransport session.
///
///
pub const WEB_TRANSPORT_ALPN: &[&[u8]] = &[b"h3", b"h3-32", b"h3-31", b"h3-30", b"h3-29"];

pub const QUIC_ALPN: &[u8] = b"hq-29";

// Note: is_rtt_packet and KEEP_ALIVE_PING are now handled by WtChatSession actor

#[derive(Debug)]
pub struct WebTransportOpt {
    pub listen: SocketAddr,
    pub certs: Certs,
}

#[derive(Debug, Clone)]
pub struct Certs {
    pub cert: PathBuf,
    pub key: PathBuf,
}

fn get_key_and_cert_chain<'a>(
    certs: Certs,
) -> anyhow::Result<(PrivateKeyDer<'a>, Vec<CertificateDer<'a>>)> {
    let key_path = certs.key;
    let cert_path = certs.cert;
    let mut keys = fs::File::open(key_path).context("failed to open key file")?;

    // Read the keys into a Vec so we can parse it twice.
    let mut buf = Vec::new();
    keys.read_to_end(&mut buf)?;

    // Try to parse a PKCS#8 key
    // -----BEGIN PRIVATE KEY-----
    let key = rustls_pemfile::private_key(&mut io::Cursor::new(&buf))
        .context("failed to load private key")?
        .context("missing private key")?;

    // Read the PEM certificate chain
    let chain = fs::File::open(cert_path).context("failed to open cert file")?;
    let mut chain = io::BufReader::new(chain);

    let chain: Vec<CertificateDer> = rustls_pemfile::certs(&mut chain)
        .collect::<Result<_, _>>()
        .context("failed to load certs")?;

    anyhow::ensure!(!chain.is_empty(), "could not find certificate");
    Ok((key, chain))
}

/// Start the WebTransport server
///
/// # Arguments
/// * `opt` - WebTransport server options (listen address, certs)
/// * `chat_server` - Address of the ChatServer actor
/// * `nats_client` - NATS client for health packet processing
/// * `pool` - Optional database pool
/// * `tracker_sender` - Server diagnostics tracker
/// * `session_manager` - Session lifecycle manager
pub async fn start(
    opt: WebTransportOpt,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("WebTransportOpt: {opt:#?}");

    let (key, certs) = get_key_and_cert_chain(opt.certs)?;

    // Manually configure Quinn with custom transport settings for fast disconnect detection
    let provider = rustls::crypto::ring::default_provider();

    let mut server_crypto_config = rustls::ServerConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    server_crypto_config.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];

    let mut server_config = quinn::ServerConfig::with_crypto(std::sync::Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto_config)?,
    ));

    // Configure transport with aggressive timeouts for fast disconnect detection
    let mut transport_config = quinn::TransportConfig::default();

    // Detect disconnection after inactivity (configurable via QUIC_MAX_IDLE_TIMEOUT_SECS)
    transport_config.max_idle_timeout(Some(
        std::time::Duration::from_secs(*QUIC_MAX_IDLE_TIMEOUT_SECS).try_into()?,
    ));

    // Send keep-alive pings to maintain connection (configurable via QUIC_KEEP_ALIVE_INTERVAL_SECS)
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(
        *QUIC_KEEP_ALIVE_INTERVAL_SECS,
    )));

    server_config.transport_config(std::sync::Arc::new(transport_config));

    // Create Quinn endpoint with our custom config
    let endpoint = quinn::Endpoint::server(server_config, opt.listen)?;

    info!(
        "listening on {} with {}s idle timeout and {}s keep-alive",
        opt.listen, *QUIC_MAX_IDLE_TIMEOUT_SECS, *QUIC_KEEP_ALIVE_INTERVAL_SECS
    );

    // Accept new WebTransport connections.
    //
    // This is what `web_transport_quinn::Server::accept` does, with one change:
    // we keep our own handle to the QUIC connection so a refused CONNECT can
    // linger long enough for its error status to reach the client (see
    // `REJECT_LINGER`).
    //
    // NOTE: We use actix_rt::spawn instead of tokio::spawn because the WtChatSession
    // actor requires the actix LocalSet context for spawn_local.
    while let Some(incoming) = endpoint.accept().await {
        trace_span!("New connection being attempted");
        let chat_server = chat_server.clone();
        let nats_client = nats_client.clone();
        let tracker_sender = tracker_sender.clone();
        let session_manager = session_manager.clone();
        actix_rt::spawn(async move {
            let handshake = async {
                let conn = incoming
                    .await
                    .map_err(|err| format!("QUIC handshake failed: {err}"))?;
                let request = web_transport_quinn::Request::accept(conn.clone())
                    .await
                    .map_err(|err| format!("WebTransport handshake failed: {err}"))?;
                Ok::<_, String>((request, conn))
            };
            // On timeout the future is dropped, which drops the connection
            // and closes it.
            let (request, conn) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
                Ok(Ok(accepted)) => accepted,
                Ok(Err(err)) => {
                    debug!("{err}");
                    return;
                }
                Err(_) => {
                    debug!("WebTransport handshake timed out after {HANDSHAKE_TIMEOUT:?}");
                    return;
                }
            };
            if let Err(err) = run_webtransport_connection_from_request(
                request,
                conn,
                chat_server,
                nats_client,
                tracker_sender,
                session_manager,
            )
            .await
            {
                error!("Failed to handle WebTransport connection: {err:?}");
            }
        });
    }

    Ok(())
}

/// Why a WebTransport CONNECT was refused, and the HTTP status to answer with.
#[derive(Debug)]
enum ConnectRejection {
    /// The path is not `/lobby`. Includes the removed `/lobby/{user_id}/{room}` form.
    NotFound,
    /// `JWT_SECRET` is not configured on this server.
    Misconfigured,
    /// The room access token is missing or failed validation.
    Token(TokenError),
}

impl ConnectRejection {
    fn status(&self) -> StatusCode {
        match self {
            ConnectRejection::NotFound => StatusCode::NOT_FOUND,
            ConnectRejection::Misconfigured => StatusCode::INTERNAL_SERVER_ERROR,
            ConnectRejection::Token(e) => {
                StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::FORBIDDEN)
            }
        }
    }
}

/// Decide whether a WebTransport CONNECT may join, from its URL alone.
///
/// The only accepted form is `/lobby?token=<JWT>`; identity and room come from
/// the token claims. This mirrors the WebSocket `GET /lobby` handler.
fn authorize_connect(
    path: &str,
    token: Option<&str>,
    jwt_secret: &str,
) -> Result<RoomAccessTokenClaims, ConnectRejection> {
    if path.trim_end_matches('/') != "/lobby" {
        return Err(ConnectRejection::NotFound);
    }
    if jwt_secret.is_empty() {
        return Err(ConnectRejection::Misconfigured);
    }
    let token = token
        .filter(|t| !t.is_empty())
        .ok_or(ConnectRejection::Token(TokenError::Missing))?;
    token_validator::decode_room_token(jwt_secret, token).map_err(ConnectRejection::Token)
}

async fn run_webtransport_connection_from_request(
    request: web_transport_quinn::Request,
    conn: quinn::Connection,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
) -> anyhow::Result<()> {
    // Never log the full URL: the query string carries the room token.
    let url = request.url();
    debug!("received WebTransport CONNECT for path {}", url.path());

    let token = url
        .query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, val)| val.into_owned());
    let jwt_secret = std::env::var("JWT_SECRET").unwrap_or_default();

    let claims = match authorize_connect(url.path(), token.as_deref(), &jwt_secret) {
        Ok(claims) => claims,
        Err(rejection) => {
            match &rejection {
                ConnectRejection::NotFound => {
                    info!("WT connection rejected: unknown path {}", url.path())
                }
                ConnectRejection::Misconfigured => error!("JWT_SECRET not set"),
                ConnectRejection::Token(e) => e.log("WT"),
            }
            reject_connect(request, conn, rejection.status()).await;
            return Ok(());
        }
    };
    // Only the refusal path needs our extra handle; let the session own the
    // connection from here on, exactly as before.
    drop(conn);

    info!(
        "WT token-based connection: user_id={}, room={}, display_name={}, observer={}",
        claims.sub, claims.room, claims.display_name, claims.observer
    );
    let username = claims.sub;
    let lobby_id = claims.room;
    let observer = claims.observer;
    let display_name = claims.display_name;

    // Accept the session.
    let session = request.ok().await.context("failed to accept session")?;
    debug!("accepted session");

    // Run the session with actor
    if let Err(err) = handle_webtransport_session(
        session,
        &username,
        &lobby_id,
        &display_name,
        chat_server,
        nats_client,
        tracker_sender,
        session_manager,
        observer,
    )
    .await
    {
        info!("closing session: {}", err);
    }
    Ok(())
}

/// Answer a CONNECT with an error status, then hold the QUIC connection until
/// the client closes it or `REJECT_LINGER` passes, so the status is delivered.
async fn reject_connect(
    request: web_transport_quinn::Request,
    conn: quinn::Connection,
    status: StatusCode,
) {
    if let Err(err) = request.close(status).await {
        debug!("failed to send WebTransport rejection {status}: {err}");
        return;
    }
    let _ = tokio::time::timeout(REJECT_LINGER, conn.closed()).await;
}

/// Handle a WebTransport session using the WtChatSession actor
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    level = "trace",
    skip(session, chat_server, nats_client, tracker_sender, session_manager)
)]
async fn handle_webtransport_session(
    session: Session,
    username: &str,
    lobby_id: &str,
    display_name: &str,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
    observer: bool,
) -> anyhow::Result<()> {
    // Create channel for actor → WebTransport I/O
    let (outbound_tx, outbound_rx) = mpsc::channel::<WtOutbound>(256);

    // Start the WtChatSession actor
    let actor = WtChatSession::new(
        chat_server,
        lobby_id.to_string(),
        username.to_string(),
        display_name.to_string(),
        outbound_tx,
        nats_client,
        tracker_sender,
        session_manager,
        observer,
    );
    let actor_addr = actor.start();

    // Create bridge (with test callback if in test mode)
    #[cfg(test)]
    let on_packet_sent = {
        let username_for_callback = username.to_string();
        Some(
            Box::new(move || increment_test_packet_counter_for_user(&username_for_callback))
                as bridge::PacketSentCallback,
        )
    };
    #[cfg(not(test))]
    let on_packet_sent: Option<bridge::PacketSentCallback> = None;

    let mut bridge = WebTransportBridge::new_with_callback(
        session,
        actor_addr.clone(),
        outbound_rx,
        on_packet_sent,
    );
    bridge.wait_for_disconnect().await;
    bridge.shutdown().await;

    // Signal actor to stop
    actor_addr.do_send(crate::actors::transports::wt_chat_session::StopSession);

    warn!("Finished handling WebTransport session for {username} in {lobby_id}");
    Ok(())
}

// Note: handle_send_connection_started and handle_send_connection_ended are now
// handled by the WtChatSession actor internally via ChatServer coordination.

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message as ProtobufMessage;
    use rustls::crypto::CryptoProvider;
    use std::time::Duration;
    use videocall_types::protos::media_packet::media_packet::MediaType as VcMediaType;
    use videocall_types::protos::media_packet::MediaPacket as VcMediaPacket;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType as VcPacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper as VcPacketWrapper;

    const KEEP_ALIVE_PING: &[u8] = b"ping";

    async fn start_webtransport_server() -> tokio::task::JoinHandle<()> {
        if let Err(e) = CryptoProvider::install_default(rustls::crypto::ring::default_provider()) {
            error!("Error installing crypto provider: {e:?}");
        }
        use crate::actors::chat_server::ChatServer;
        use crate::server_diagnostics::ServerDiagnostics;
        use crate::session_manager::SessionManager;
        use crate::webtransport::{self, Certs};
        use actix::Actor;
        use std::net::ToSocketAddrs;

        // The server only accepts room tokens; every test signs with this secret.
        std::env::set_var("JWT_SECRET", JWT_SECRET);

        // Connect to NATS
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        // Start ChatServer actor
        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        // Create SessionManager
        let session_manager = SessionManager::new();

        // Create connection tracker
        let (_, tracker_sender, tracker_receiver) =
            ServerDiagnostics::new_with_channel(nats_client.clone());

        // Clone nats_client for use in both spawned tasks
        let nats_client_for_tracker = nats_client.clone();

        // Start tracker message loop
        actix_rt::spawn(async move {
            let connection_tracker =
                std::sync::Arc::new(ServerDiagnostics::new(nats_client_for_tracker));
            connection_tracker.run_message_loop(tracker_receiver).await;
        });

        // Start WebTransport server
        let opt = webtransport::WebTransportOpt {
            listen: std::env::var("LISTEN_URL")
                .unwrap_or_else(|_| "0.0.0.0:4433".to_string())
                .to_socket_addrs()
                .expect("expected LISTEN_URL to be a valid socket address")
                .next()
                .expect("expected LISTEN_URL to be a valid socket address"),
            certs: Certs {
                key: std::env::var("KEY_PATH")
                    .unwrap_or_else(|_| "certs/localhost.key".to_string())
                    .into(),
                cert: std::env::var("CERT_PATH")
                    .unwrap_or_else(|_| "certs/localhost.pem".to_string())
                    .into(),
            },
        };

        actix_rt::spawn(async move {
            if let Err(e) = webtransport::start(
                opt,
                chat_server,
                nats_client,
                tracker_sender,
                session_manager,
            )
            .await
            {
                eprintln!("WebTransport server error: {e}");
            }
        })
    }

    async fn wait_for_condition<F, Fut, T>(
        mut condition: F,
        timeout_duration: Duration,
        check_interval: Duration,
    ) -> Result<T, &'static str>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout_duration {
            if let Some(result) = condition().await {
                return Ok(result);
            }
            tokio::time::sleep(check_interval).await;
        }
        Err("Condition not met within timeout")
    }

    async fn wait_for_condition_bool<F, Fut>(
        condition: F,
        timeout_duration: Duration,
        check_interval: Duration,
    ) -> Result<(), &'static str>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout_duration {
            if condition().await {
                return Ok(());
            }
            tokio::time::sleep(check_interval).await;
        }
        Err("Condition not met within timeout")
    }

    async fn wait_for_server_ready() {
        let condition = || async {
            match connect_client_as("test", "test").await {
                Ok(_) => {
                    info!("Server is ready!");
                    true
                }
                Err(e) => {
                    error!("Error connecting to server: {e:?}");
                    info!("Retrying connection to server...");
                    false
                }
            }
        };

        wait_for_condition_bool(
            condition,
            Duration::from_secs(10),
            Duration::from_millis(200),
        )
        .await
        .expect("WebTransport server not ready after 10 seconds");
    }

    /// Mint a room token for `user` in `room`, signed with the test secret.
    fn token_for(user: &str, room: &str) -> String {
        meeting_api::token::generate_room_token(JWT_SECRET, TOKEN_TTL_SECS, user, room, false, user)
            .expect("generate room token")
    }

    /// Connect as `user` to `room` through the token endpoint.
    async fn connect_client_as(
        user: &str,
        room: &str,
    ) -> Result<web_transport_quinn::Session, Box<dyn std::error::Error>> {
        connect_client_with_token(&token_for(user, room)).await
    }

    /// Open a WebTransport session to an arbitrary path and query on the test
    /// server, returning the raw client error so tests can read the status.
    async fn connect_raw(
        path_and_query: &str,
    ) -> Result<web_transport_quinn::Session, web_transport_quinn::ClientError> {
        let base = std::env::var("WEBTRANSPORT_URL")
            .unwrap_or_else(|_| "https://127.0.0.1:4433".to_string());
        let url = url::Url::parse(&format!("{}{}", base.trim_end_matches('/'), path_and_query))
            .expect("valid test URL");
        let client = unsafe {
            web_transport_quinn::ClientBuilder::new()
                .with_no_certificate_verification()
                .expect("build client")
        };
        client.connect(url).await
    }

    /// The HTTP status of a refused CONNECT, if that is why `err` happened.
    ///
    /// web-transport-quinn keeps its connect error types private, so we read
    /// the status out of the `Debug` text. A non-200 response surfaces as
    /// `HttpError(ProtoError(WrongStatus(Some(401))))` in 0.8.1; the
    /// `ErrorStatus(401)` form is the crate's other path for the same thing.
    fn rejection_status(err: &web_transport_quinn::ClientError) -> Option<u16> {
        let web_transport_quinn::ClientError::HttpError(inner) = err else {
            return None;
        };
        let text = format!("{inner:?}");
        ["WrongStatus(Some(", "ErrorStatus("]
            .iter()
            .find_map(|marker| text.split(marker).nth(1))
            .and_then(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .ok()
            })
    }

    /// Connect via the token-based endpoint: GET /lobby?token=<JWT>
    async fn connect_client_with_token(
        token: &str,
    ) -> Result<web_transport_quinn::Session, Box<dyn std::error::Error>> {
        let base = std::env::var("WEBTRANSPORT_URL")
            .unwrap_or_else(|_| "https://127.0.0.1:4433".to_string());
        let url_str = format!(
            "{}/lobby?token={}",
            base.trim_end_matches('/'),
            urlencoding::encode(token)
        );
        let url = url::Url::parse(&url_str)?;

        let client = unsafe {
            web_transport_quinn::ClientBuilder::new().with_no_certificate_verification()?
        };
        Ok(client.connect(url).await?)
    }

    async fn send_packet(session: &web_transport_quinn::Session, bytes: Vec<u8>) {
        let mut s = session.open_uni().await.expect("open uni");
        s.write_all(&bytes).await.expect("write packet");
        // Don't call finish() to avoid closing the session prematurely
    }

    async fn keep_alive(session: &web_transport_quinn::Session) {
        // Send a small datagram to keep connection alive
        let ping_data = KEEP_ALIVE_PING;
        if let Err(e) = session.send_datagram(ping_data.to_vec().into()) {
            debug!("Keep-alive ping failed: {}", e);
        }
    }

    /// Wait for a session to receive its MEETING_STARTED packet.
    /// The server subscribes to NATS BEFORE sending MEETING_STARTED, so receiving
    /// this packet confirms the session is fully initialized and ready to relay packets.
    async fn wait_for_session_ready(
        session: &web_transport_quinn::Session,
        name: &str,
    ) -> Result<(), &'static str> {
        println!("Waiting for {name} to receive MEETING_STARTED...");
        let session_clone = session.clone();
        wait_for_condition(
            || {
                let session = session_clone.clone();
                async move {
                    // Race between uni stream and datagram since non-MEDIA
                    // packets (e.g., MEETING, AES_KEY) may arrive via datagrams
                    // (send_auto routes small control packets as datagrams).
                    let buf = tokio::select! {
                        Ok(mut stream) = session.accept_uni() => {
                            stream.read_to_end(usize::MAX).await.ok()
                        }
                        Ok(datagram) = session.read_datagram() => {
                            Some(datagram.to_vec())
                        }
                        else => None,
                    };
                    if let Some(buf) = buf {
                        if !buf.is_empty() {
                            if let Ok(wrapper) = VcPacketWrapper::parse_from_bytes(&buf) {
                                if wrapper.packet_type
                                    == videocall_types::protos::packet_wrapper::packet_wrapper::PacketType::MEETING.into()
                                {
                                    println!("✓ {name} received MEETING_STARTED - session ready");
                                    return Some(());
                                }
                            }
                        }
                    }
                    None
                }
            },
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .map(|_| ())
    }

    #[actix_rt::test]
    async fn test_relay_packet_webtransport_between_two_clients() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(std::io::stderr)
            .try_init();

        // Wrap entire test with 15 second timeout
        let test_result = tokio::time::timeout(Duration::from_secs(15), async {
            test_relay_packet_impl().await
        })
        .await;

        match test_result {
            Ok(Ok(())) => println!("Test completed successfully"),
            Ok(Err(e)) => panic!("Test failed: {e}"),
            Err(_) => panic!("Test timed out after 15 seconds"),
        }
    }

    async fn test_relay_packet_impl() -> anyhow::Result<()> {
        println!("=== STARTING INTEGRATION TEST ===");

        println!("Starting WebTransport server...");
        let _wt_handle = start_webtransport_server().await;

        println!("Waiting for server to be ready...");
        wait_for_server_ready().await;

        let meeting = "it-meeting-1";
        let user_a = "user-a";
        let user_b = "user-b";

        println!("Connecting client A: {user_a}");
        let session_a = connect_client_as(user_a, meeting)
            .await
            .expect("connect client A");
        println!("Client A connected!");

        println!("Connecting client B: {user_b}");
        let session_b = connect_client_as(user_b, meeting)
            .await
            .expect("connect client B");
        println!("Client B connected!");

        // Wait for both sessions to be fully ready (receive MEETING_STARTED)
        // This confirms NATS subscriptions are established
        wait_for_session_ready(&session_a, "client A")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        wait_for_session_ready(&session_b, "client B")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        // Start keep-alive tasks that will be cancelled when test ends
        let session_a_keep = session_a.clone();
        let session_b_keep = session_b.clone();

        let _keep_alive_a = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                keep_alive(&session_a_keep).await;
            }
        });

        let _keep_alive_b = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                keep_alive(&session_b_keep).await;
            }
        });

        // Craft a MEDIA packet that is not RTT and not health
        let media = VcMediaPacket {
            media_type: VcMediaType::AUDIO.into(),
            user_id: user_a.as_bytes().to_vec(),
            ..Default::default()
        };
        let packet = VcPacketWrapper {
            packet_type: VcPacketType::MEDIA.into(),
            user_id: user_a.as_bytes().to_vec(),
            data: media.write_to_bytes().expect("serialize media"),
            ..Default::default()
        };
        let bytes = packet.write_to_bytes().expect("serialize wrapper");

        println!("Sending packet from A to B (size: {} bytes)", bytes.len());
        send_packet(&session_a, bytes.clone()).await;
        println!("Packet sent from A!");

        // Wait for B to receive the packet
        println!("Waiting for packet on B...");
        let session_b_recv = session_b.clone();
        let received = wait_for_condition(
            || {
                let session = session_b_recv.clone();
                async move {
                    // Race between uni stream and datagram since non-MEDIA
                    // packets (e.g., MEETING, AES_KEY) may arrive via datagrams
                    // (send_auto routes small control packets as datagrams).
                    let buf = tokio::select! {
                        Ok(mut stream) = session.accept_uni() => {
                            stream.read_to_end(usize::MAX).await.ok()
                        }
                        Ok(datagram) = session.read_datagram() => {
                            Some(datagram.to_vec())
                        }
                        else => None,
                    };
                    if let Some(buf) = buf {
                        if !buf.is_empty() {
                            if let Ok(pkt) = VcPacketWrapper::parse_from_bytes(&buf) {
                                let media_type: ::protobuf::EnumOrUnknown<VcPacketType> =
                                    VcPacketType::MEDIA.into();
                                if pkt.packet_type == media_type {
                                    println!(
                                        "Received MEDIA packet on B (size: {} bytes)",
                                        buf.len()
                                    );
                                    return Some(buf);
                                }
                                println!(
                                    "Skipping non-MEDIA packet on B (type: {:?})",
                                    pkt.packet_type
                                );
                            } else {
                                println!("Received unparseable packet on B, returning it");
                                return Some(buf);
                            }
                        }
                    }
                    None
                }
            },
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .expect("Should receive packet from A on B");

        println!("Packet successfully relayed!");

        // Parse both packets to compare content (server may add session_id)
        let sent_packet = VcPacketWrapper::parse_from_bytes(&bytes).expect("parse sent packet");
        let received_packet =
            VcPacketWrapper::parse_from_bytes(&received).expect("parse received packet");

        // Compare meaningful fields (server adds session_id, so we ignore it)
        assert_eq!(
            sent_packet.packet_type, received_packet.packet_type,
            "packet_type must match"
        );
        assert_eq!(
            sent_packet.user_id, received_packet.user_id,
            "user_id must match"
        );
        assert_eq!(sent_packet.data, received_packet.data, "data must match");
        // Verify that server added session_id (it should not be empty)
        assert!(
            received_packet.session_id != 0,
            "server should add session_id"
        );

        println!("=== INTEGRATION TEST PASSED ===");
        Ok(())
    }

    #[actix_rt::test]
    async fn test_lobby_isolation() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(std::io::stderr)
            .try_init();

        // Wrap entire test with 15 second timeout
        let test_result = tokio::time::timeout(Duration::from_secs(15), async {
            test_lobby_isolation_impl().await
        })
        .await;

        match test_result {
            Ok(Ok(())) => println!("Test completed successfully"),
            Ok(Err(e)) => panic!("Test failed: {e}"),
            Err(_) => panic!("Test timed out after 15 seconds"),
        }
    }

    async fn test_lobby_isolation_impl() -> anyhow::Result<()> {
        println!("=== STARTING COMPREHENSIVE LOBBY ISOLATION TEST ===");

        // ========== SETUP ==========
        reset_test_packet_counters();
        println!("✓ Reset packet counters");

        println!("Starting WebTransport server...");
        let _wt_handle = start_webtransport_server().await;
        wait_for_server_ready().await;
        println!("✓ Server ready");

        // ========== CLIENT CONNECTIONS ==========
        let lobby_1 = "lobby-secure";
        let lobby_2 = "lobby-public";
        let user_a = "alice";
        let user_b = "bob";
        let user_c = "charlie";
        assert_eq!(get_test_packet_counter_for_user(user_a), 0);
        assert_eq!(get_test_packet_counter_for_user(user_b), 0);
        assert_eq!(get_test_packet_counter_for_user(user_c), 0);

        println!("\n--- Establishing Connections ---");

        let session_a = connect_client_as(user_a, lobby_1)
            .await
            .expect("connect alice");
        println!("✓ Alice connected to lobby-secure");

        let session_b = connect_client_as(user_b, lobby_2)
            .await
            .expect("connect bob");
        println!("✓ Bob connected to lobby-public");

        let session_c = connect_client_as(user_c, lobby_1)
            .await
            .expect("connect charlie");
        println!("✓ Charlie connected to lobby-secure");

        // Wait for all sessions to be fully ready (receive MEETING_STARTED)
        // This confirms NATS subscriptions are established before sending test packets
        wait_for_session_ready(&session_a, "Alice")
            .await
            .expect("Alice session ready");
        wait_for_session_ready(&session_b, "Bob")
            .await
            .expect("Bob session ready");
        wait_for_session_ready(&session_c, "Charlie")
            .await
            .expect("Charlie session ready");

        // Keep connections alive
        start_keep_alive_tasks(&session_a, &session_b, &session_c).await;

        // Give NATS subscriptions time to fully propagate after all sessions
        // are ready. MEETING_STARTED is only published for the first participant
        // now (the transport actors send it directly to every joiner), so we use
        // a short delay instead of waiting for a cross-session broadcast.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // ========== ACTIVATE ALL CONNECTIONS ==========
        // ActivateConnection (and its PARTICIPANT_JOINED broadcast) is deferred
        // until the first non-RTT data packet. Send an activation packet from
        // each client so these broadcasts happen BEFORE the counting phase.
        println!("\n--- Activating connections ---");
        let activation_a =
            create_test_packet(user_a, VcMediaType::AUDIO, "activation-a".to_string());
        let activation_b =
            create_test_packet(user_b, VcMediaType::AUDIO, "activation-b".to_string());
        let activation_c =
            create_test_packet(user_c, VcMediaType::AUDIO, "activation-c".to_string());
        send_packet(&session_a, activation_a).await;
        send_packet(&session_b, activation_b).await;
        send_packet(&session_c, activation_c).await;
        println!("✓ Sent activation packets from all clients");

        // Wait for PARTICIPANT_JOINED broadcasts to settle.
        // Alice and Charlie are in the same room, so each receives the other's
        // PARTICIPANT_JOINED. Allow time for the async broadcast pipeline
        // (ActivateConnection -> NATS publish -> subscription delivery).
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Reset counters so the counting phase starts from zero.
        reset_test_packet_counters();
        println!("✓ Reset packet counters after activation");

        // ========== PHASE 1: CROSS-LOBBY ISOLATION TEST ==========
        println!("\n--- Phase 1: Testing Cross-Lobby Isolation ---");

        let [count_a, count_b, count_c] = get_all_user_counts(&[user_a, user_b, user_c])[..] else {
            panic!("Expected exactly 3 user counts");
        };
        println!("Initial counts: A={count_a}, B={count_b}, C={count_c}");

        // Alice sends 3 packets to her lobby (should only reach Charlie, not Bob)
        for i in 1..=3 {
            let packet = create_test_packet(user_a, VcMediaType::AUDIO, format!("alice-msg-{i}"));
            send_packet(&session_a, packet).await;
            println!("✓ Alice sent packet #{i}");
        }

        // Wait for Charlie to receive Alice's 3 packets
        let expected_charlie_count = count_c + 3;
        wait_for_condition_bool(
            || async move { get_test_packet_counter_for_user(user_c) >= expected_charlie_count },
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .expect("Charlie should receive Alice's packets");

        let [alice_count_after, bob_count_after, charlie_count_after] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "After Alice's packets: A={alice_count_after}, B={bob_count_after}, C={charlie_count_after}"
        );

        // Bob should have received ZERO packets (different lobby)
        assert_eq!(
            bob_count_after, count_b,
            "❌ ISOLATION BREACH: Bob in lobby-public received packets from Alice in lobby-secure!"
        );
        println!("✅ Confirmed: Bob (lobby-public) isolated from Alice's packets");

        // Alice should NOT receive her own packets back (no self-echo)
        assert_eq!(
            alice_count_after, count_a,
            "❌ Alice received her own packets back! Self-echo should be prevented."
        );
        println!("✅ Confirmed: Alice does not receive her own packets back (no self-echo)");

        // Charlie should have received Alice's packets (same lobby)
        assert!(
            charlie_count_after >= count_c + 3,
            "❌ Charlie should have received Alice's 3 packets, but only got {} new packets",
            charlie_count_after - count_c
        );
        println!("✅ Confirmed: Charlie (lobby-secure) received Alice's packets");

        // ========== PHASE 2: BIDIRECTIONAL SAME-LOBBY TEST ==========
        println!("\n--- Phase 2: Testing Bidirectional Same-Lobby Communication ---");

        let [alice_before_bidi, bob_before_bidi, charlie_before_bidi] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };

        // Charlie responds to Alice with 2 packets
        for i in 1..=2 {
            let packet =
                create_test_packet(user_c, VcMediaType::VIDEO, format!("charlie-reply-{i}"));
            send_packet(&session_c, packet).await;
            println!("✓ Charlie sent reply #{i}");
        }

        // Wait for Alice to receive Charlie's 2 packets
        let expected_alice_count = alice_before_bidi + 2;
        wait_for_condition_bool(
            || async move { get_test_packet_counter_for_user(user_a) >= expected_alice_count },
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .expect("Alice should receive Charlie's replies");

        let [alice_after_bidi, bob_after_bidi, charlie_after_bidi] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "After Charlie's replies: A={alice_after_bidi}, B={bob_after_bidi}, C={charlie_after_bidi}"
        );

        // Alice should receive Charlie's replies
        assert!(
            alice_after_bidi >= alice_before_bidi + 2,
            "❌ Alice should have received Charlie's 2 replies"
        );
        println!("✅ Confirmed: Bidirectional communication within lobby-secure works");

        // Charlie should NOT receive his own packets back (no self-echo)
        assert_eq!(
            charlie_after_bidi, charlie_before_bidi,
            "❌ Charlie received his own packets back! Self-echo should be prevented."
        );
        println!("✅ Confirmed: Charlie does not receive his own packets back (no self-echo)");

        // Bob should still have zero new packets
        assert_eq!(
            bob_after_bidi, bob_before_bidi,
            "❌ ISOLATION BREACH: Bob received packets from Charlie!"
        );
        println!("✅ Confirmed: Bob remains isolated from lobby-secure traffic");

        // ========== PHASE 3: ISOLATED LOBBY COMMUNICATION ==========
        println!("\n--- Phase 3: Testing Bob's Isolated Communication ---");

        let [alice_before_bob, bob_before_bob, charlie_before_bob] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };

        // Bob sends packet in his own lobby (should not reach Alice, Charlie, or echo back to himself)
        let packet = create_test_packet(user_b, VcMediaType::AUDIO, "bob-isolated-msg".to_string());
        send_packet(&session_b, packet).await;
        println!("✓ Bob sent packet in lobby-public");

        // Wait to verify isolation - expect this to timeout since no one should receive Bob's packet
        // We use a short timeout since we're verifying nothing happens
        let _ = wait_for_condition_bool(
            || async move {
                // This should never become true - Bob is alone in his lobby
                get_test_packet_counter_for_user(user_a) > alice_before_bob
                    || get_test_packet_counter_for_user(user_b) > bob_before_bob
                    || get_test_packet_counter_for_user(user_c) > charlie_before_bob
            },
            Duration::from_millis(300), // Short timeout - we expect this to fail
            Duration::from_millis(50),
        )
        .await;
        // We don't care if this times out - that's expected behavior

        let [alice_after_bob, bob_after_bob, charlie_after_bob] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "After Bob's packet: A={alice_after_bob}, B={bob_after_bob}, C={charlie_after_bob}"
        );

        // Alice and Charlie should not receive Bob's packet
        assert_eq!(
            alice_after_bob, alice_before_bob,
            "❌ Alice received Bob's packet across lobbies!"
        );
        assert_eq!(
            charlie_after_bob, charlie_before_bob,
            "❌ Charlie received Bob's packet across lobbies!"
        );

        // Bob should NOT receive his own packet back
        assert_eq!(
            bob_after_bob, bob_before_bob,
            "❌ Bob received his own packet back! Self-echo should be prevented."
        );

        println!("✅ Confirmed: Bob's packets isolated to lobby-public");
        println!("✅ Confirmed: Bob does not receive his own packets back (no self-echo)");

        // ========== SUMMARY ==========
        println!("\n=== COMPREHENSIVE LOBBY ISOLATION TEST PASSED ===");
        let [alice_final, bob_final, charlie_final] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!("Final packet counts:");
        println!("  • Alice (lobby-secure): {alice_final}");
        println!("  • Bob   (lobby-public):  {bob_final}");
        println!("  • Charlie (lobby-secure): {charlie_final}");
        println!("✅ All lobby isolation requirements verified!");
        println!("✅ Self-echo prevention verified for all users!");

        Ok(())
    }

    // ========== HELPER FUNCTIONS ==========

    async fn start_keep_alive_tasks(
        session_a: &web_transport_quinn::Session,
        session_b: &web_transport_quinn::Session,
        session_c: &web_transport_quinn::Session,
    ) {
        let session_a_keep = session_a.clone();
        let session_b_keep = session_b.clone();
        let session_c_keep = session_c.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(8));
            loop {
                interval.tick().await;
                keep_alive(&session_a_keep).await;
            }
        });

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(8));
            loop {
                interval.tick().await;
                keep_alive(&session_b_keep).await;
            }
        });

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(8));
            loop {
                interval.tick().await;
                keep_alive(&session_c_keep).await;
            }
        });
    }

    fn create_test_packet(sender: &str, media_type: VcMediaType, _message: String) -> Vec<u8> {
        let media = VcMediaPacket {
            media_type: media_type.into(),
            user_id: sender.as_bytes().to_vec(),
            ..Default::default()
        };
        let packet = VcPacketWrapper {
            packet_type: VcPacketType::MEDIA.into(),
            user_id: sender.as_bytes().to_vec(),
            data: media.write_to_bytes().expect("serialize media"),
            ..Default::default()
        };
        packet.write_to_bytes().expect("serialize wrapper")
    }

    fn get_all_user_counts(users: &[&str]) -> Vec<u64> {
        users
            .iter()
            .map(|user| get_test_packet_counter_for_user(user))
            .collect()
    }

    // ==========================================================================
    // Meeting Lifecycle Integration Test (WebTransport)
    // Tests: meeting creation, participant join/leave, meeting end
    // ==========================================================================

    #[actix_rt::test]
    #[serial_test::serial]
    async fn test_meeting_lifecycle_webtransport() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(std::io::stderr)
            .try_init();

        let test_result = tokio::time::timeout(Duration::from_secs(30), async {
            test_meeting_lifecycle_impl().await
        })
        .await;

        match test_result {
            Ok(Ok(())) => println!("Test completed successfully"),
            Ok(Err(e)) => panic!("Test failed: {e}"),
            Err(_) => panic!("Test timed out after 30 seconds"),
        }
    }

    async fn test_meeting_lifecycle_impl() -> anyhow::Result<()> {
        println!("=== STARTING SESSION LIFECYCLE TEST (WebTransport) ===");

        let room_id = "wt-meeting-lifecycle-test";

        println!("Starting WebTransport server...");
        let _wt_handle = start_webtransport_server().await;
        wait_for_server_ready().await;
        println!("✓ Server ready");

        // ========== STEP 1: First user connects ==========
        println!("\n--- Step 1: Alice connects (first participant) ---");

        let session_alice = connect_client_as("alice", room_id)
            .await
            .expect("connect alice");
        wait_for_session_ready(&session_alice, "Alice")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Alice connected");

        // ========== STEP 2: Second user connects ==========
        println!("\n--- Step 2: Bob connects (second participant) ---");

        let session_bob = connect_client_as("bob", room_id)
            .await
            .expect("connect bob");
        wait_for_session_ready(&session_bob, "Bob")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Bob connected");

        // ========== STEP 3: Third user connects ==========
        println!("\n--- Step 3: Charlie connects (third participant) ---");

        let session_charlie = connect_client_as("charlie", room_id)
            .await
            .expect("connect charlie");
        wait_for_session_ready(&session_charlie, "Charlie")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Charlie connected");

        // ========== STEP 4: Charlie disconnects ==========
        println!("\n--- Step 4: Charlie disconnects ---");
        session_charlie.close(0u32, b"test disconnect");
        drop(session_charlie);
        tokio::time::sleep(Duration::from_millis(500)).await;
        println!("✓ Charlie disconnected");

        // ========== STEP 5: Bob disconnects ==========
        println!("\n--- Step 5: Bob disconnects ---");
        session_bob.close(0u32, b"test disconnect");
        drop(session_bob);
        tokio::time::sleep(Duration::from_millis(500)).await;
        println!("✓ Bob disconnected");

        // ========== STEP 6: Alice (last) disconnects ==========
        println!("\n--- Step 6: Alice disconnects - session ends ---");
        session_alice.close(0u32, b"test disconnect");
        drop(session_alice);
        tokio::time::sleep(Duration::from_millis(500)).await;
        println!("✓ Alice disconnected");

        println!("\n=== SESSION LIFECYCLE TEST PASSED (WebTransport) ===");
        Ok(())
    }

    /// Test helper: create a database pool for future JWT flow integration tests.
    #[allow(dead_code)]
    async fn get_test_pool() -> sqlx::PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this test");
        sqlx::PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    // =====================================================================
    // JWT token-based WebTransport tests
    // =====================================================================

    const JWT_SECRET: &str = "test-secret-for-integration-tests";
    const TOKEN_TTL_SECS: i64 = 600;

    /// Start the real WT server for JWT tests.
    async fn setup_jwt_wt() {
        let _handle = start_webtransport_server().await;
        wait_for_server_ready().await;
    }

    #[actix_rt::test]
    async fn test_wt_valid_token_connects() {
        setup_jwt_wt().await;

        let token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            TOKEN_TTL_SECS,
            "alice@test.com",
            "wt-jwt-room-1",
            true,
            "Alice",
        )
        .expect("generate token");

        let session = connect_client_with_token(&token)
            .await
            .expect("valid token should connect via WebTransport");
        wait_for_session_ready(&session, "alice")
            .await
            .expect("should receive MEETING_STARTED");
        session.close(0u32, b"done");
    }

    #[actix_rt::test]
    async fn test_wt_expired_token_rejected() {
        setup_jwt_wt().await;

        let token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            -120,
            "alice@test.com",
            "wt-jwt-room-2",
            false,
            "Alice",
        )
        .expect("generate token");

        let result = connect_client_with_token(&token).await;
        assert!(
            result.is_err(),
            "expired token should be rejected via WebTransport"
        );
    }

    #[actix_rt::test]
    async fn test_wt_wrong_secret_rejected() {
        setup_jwt_wt().await;

        let token = meeting_api::token::generate_room_token(
            "completely-different-secret",
            TOKEN_TTL_SECS,
            "alice@test.com",
            "wt-jwt-room-3",
            false,
            "Alice",
        )
        .expect("generate token");

        let result = connect_client_with_token(&token).await;
        assert!(
            result.is_err(),
            "wrong-secret token should be rejected via WebTransport"
        );
    }

    #[actix_rt::test]
    async fn test_wt_garbage_token_rejected() {
        setup_jwt_wt().await;

        let result = connect_client_with_token("not.a.real.jwt").await;
        assert!(
            result.is_err(),
            "garbage token should be rejected via WebTransport"
        );
    }

    #[actix_rt::test]
    async fn test_wt_token_identity_extracted() {
        setup_jwt_wt().await;

        let token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            TOKEN_TTL_SECS,
            "bob@example.com",
            "wt-special-room",
            false,
            "Bob",
        )
        .expect("generate token");

        let session = connect_client_with_token(&token)
            .await
            .expect("token with identity should connect");
        wait_for_session_ready(&session, "bob")
            .await
            .expect("should receive MEETING_STARTED");
        session.close(0u32, b"done");
    }

    // -- authorize_connect: pure URL/token decisions, no network --

    #[test]
    fn authorize_connect_refuses_path_form_with_404() {
        let rejection = authorize_connect("/lobby/alice/room-1", None, JWT_SECRET).unwrap_err();
        assert!(matches!(rejection, ConnectRejection::NotFound));
        assert_eq!(rejection.status(), StatusCode::NOT_FOUND);

        let token = token_for("alice", "room-1");
        let rejection =
            authorize_connect("/lobby/alice/room-1", Some(&token), JWT_SECRET).unwrap_err();
        assert_eq!(rejection.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn authorize_connect_refuses_other_paths_with_404() {
        for path in ["/", "/lobbyx", "/other", "/lobby/extra"] {
            let rejection = authorize_connect(path, None, JWT_SECRET).unwrap_err();
            assert_eq!(rejection.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[test]
    fn authorize_connect_missing_or_empty_token_is_401() {
        for token in [None, Some("")] {
            let rejection = authorize_connect("/lobby", token, JWT_SECRET).unwrap_err();
            assert!(matches!(
                rejection,
                ConnectRejection::Token(TokenError::Missing)
            ));
            assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[test]
    fn authorize_connect_garbage_token_is_403() {
        let rejection = authorize_connect("/lobby", Some("not.a.jwt"), JWT_SECRET).unwrap_err();
        assert_eq!(rejection.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn authorize_connect_wrong_secret_is_403() {
        let token = token_for("alice", "room-1");
        let rejection = authorize_connect("/lobby", Some(&token), "another-secret").unwrap_err();
        assert_eq!(rejection.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn authorize_connect_without_server_secret_is_500() {
        let token = token_for("alice", "room-1");
        let rejection = authorize_connect("/lobby", Some(&token), "").unwrap_err();
        assert!(matches!(rejection, ConnectRejection::Misconfigured));
        assert_eq!(rejection.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn authorize_connect_valid_token_yields_claims() {
        for path in ["/lobby", "/lobby/"] {
            let token = token_for("alice", "room-1");
            let claims = authorize_connect(path, Some(&token), JWT_SECRET).expect("valid token");
            assert_eq!(claims.sub, "alice");
            assert_eq!(claims.room, "room-1");
            assert!(!claims.observer);
        }
    }

    /// Accepts any server certificate. The test cert has no SAN, so a real
    /// verifier would refuse it; this is only for the raw-QUIC test below.
    #[derive(Debug)]
    struct AcceptAnyCert(Arc<CryptoProvider>);

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    /// A client that completes the QUIC handshake but never sends HTTP/3
    /// SETTINGS or a CONNECT is dropped once `HANDSHAKE_TIMEOUT` passes, and
    /// not before.
    #[actix_rt::test]
    async fn test_wt_silent_client_dropped_after_handshake_timeout() {
        setup_jwt_wt().await;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth();
        tls.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC client config"),
        ));

        let mut endpoint =
            quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client endpoint");
        endpoint.set_default_client_config(client_config);
        let conn = endpoint
            .connect("127.0.0.1:4433".parse().unwrap(), "localhost")
            .expect("start QUIC connect")
            .await
            .expect("QUIC handshake completes");

        // Say nothing on HTTP/3 and wait for the server to give up.
        let started = std::time::Instant::now();
        let closed =
            tokio::time::timeout(HANDSHAKE_TIMEOUT + Duration::from_secs(5), conn.closed())
                .await
                .expect("server should close a silent connection after HANDSHAKE_TIMEOUT");
        let elapsed = started.elapsed();

        assert!(
            elapsed + Duration::from_millis(500) >= HANDSHAKE_TIMEOUT,
            "closed too early ({elapsed:?}): {closed:?}"
        );
    }

    #[actix_rt::test]
    async fn test_wt_path_form_is_refused() {
        setup_jwt_wt().await;

        // The removed `/lobby/{user_id}/{room}` form gets a 404, like the
        // WebSocket relay, with or without a valid token attached.
        let err = connect_raw("/lobby/alice/wt-jwt-room-6")
            .await
            .expect_err("path form must be refused");
        assert_eq!(rejection_status(&err), Some(404), "got {err:?}");

        let token = token_for("alice", "wt-jwt-room-6");
        let err = connect_raw(&format!(
            "/lobby/alice/wt-jwt-room-6?token={}",
            urlencoding::encode(&token)
        ))
        .await
        .expect_err("path form must be refused even with a token");
        assert_eq!(rejection_status(&err), Some(404), "got {err:?}");
    }

    #[actix_rt::test]
    async fn test_wt_missing_token_is_401() {
        setup_jwt_wt().await;

        for path in ["/lobby", "/lobby?token="] {
            let err = connect_raw(path)
                .await
                .expect_err("missing token must be refused");
            assert_eq!(rejection_status(&err), Some(401), "{path}: got {err:?}");
        }
    }

    #[actix_rt::test]
    async fn test_wt_garbage_and_expired_token_statuses() {
        setup_jwt_wt().await;

        let err = connect_raw("/lobby?token=not.a.real.jwt")
            .await
            .expect_err("garbage token must be refused");
        assert_eq!(rejection_status(&err), Some(403), "got {err:?}");

        let expired = meeting_api::token::generate_room_token(
            JWT_SECRET,
            -120,
            "alice@test.com",
            "wt-jwt-room-8",
            false,
            "Alice",
        )
        .expect("generate token");
        let err = connect_raw(&format!("/lobby?token={}", urlencoding::encode(&expired)))
            .await
            .expect_err("expired token must be refused");
        assert_eq!(rejection_status(&err), Some(401), "got {err:?}");
    }

    #[actix_rt::test]
    async fn test_wt_host_and_attendee_tokens_both_connect() {
        setup_jwt_wt().await;

        let room = "wt-standup";

        let host_token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            TOKEN_TTL_SECS,
            "host@co.com",
            room,
            true,
            "Host",
        )
        .expect("generate host token");

        let attendee_token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            TOKEN_TTL_SECS,
            "attendee@co.com",
            room,
            false,
            "Attendee",
        )
        .expect("generate attendee token");

        let host = connect_client_with_token(&host_token)
            .await
            .expect("host token should connect");
        wait_for_session_ready(&host, "host")
            .await
            .expect("host should receive MEETING_STARTED");

        let attendee = connect_client_with_token(&attendee_token)
            .await
            .expect("attendee token should connect");
        wait_for_session_ready(&attendee, "attendee")
            .await
            .expect("attendee should receive MEETING_STARTED");

        host.close(0u32, b"done");
        attendee.close(0u32, b"done");
    }
}
