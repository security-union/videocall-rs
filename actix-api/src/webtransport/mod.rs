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
use crate::constants::VALID_ID_PATTERN;
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use crate::token_validator;
use actix::prelude::*;
use anyhow::{anyhow, Context, Result};
use bridge::WebTransportBridge;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::Read;
use std::{fs, io};
use std::{net::SocketAddr, path::PathBuf};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace_span, warn};

lazy_static::lazy_static! {
    static ref QUIC_MAX_IDLE_TIMEOUT_SECS: u64 = std::env::var("QUIC_MAX_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

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

    let mut server = web_transport_quinn::Server::new(endpoint);

    info!(
        "listening on {} with {}s idle timeout and {}s keep-alive",
        opt.listen, *QUIC_MAX_IDLE_TIMEOUT_SECS, *QUIC_KEEP_ALIVE_INTERVAL_SECS
    );

    // Accept new WebTransport connections
    // NOTE: We use actix_rt::spawn instead of tokio::spawn because the WtChatSession
    // actor requires the actix LocalSet context for spawn_local.
    while let Some(request) = server.accept().await {
        trace_span!("New connection being attempted");
        let chat_server = chat_server.clone();
        let nats_client = nats_client.clone();
        let tracker_sender = tracker_sender.clone();
        let session_manager = session_manager.clone();
        actix_rt::spawn(async move {
            if let Err(err) = run_webtransport_connection_from_request(
                request,
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

async fn run_webtransport_connection_from_request(
    request: web_transport_quinn::Request,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
) -> anyhow::Result<()> {
    warn!("received WebTransport request: {}", request.url());
    let uri = request.url();
    let path = urlencoding::decode(uri.path()).unwrap().into_owned();

    let parts = path.split('/').collect::<Vec<&str>>();
    // filter out the empty strings
    let parts: Vec<_> = parts.iter().filter(|s| !s.is_empty()).collect();
    info!("Parts {:?}", parts);

    // First part must be "lobby"
    if parts.is_empty() || parts[0] != &"lobby" {
        return Err(anyhow!("Invalid path: must start with /lobby"));
    }

    // Extract ?token= from query string
    let token = uri
        .query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, val)| val.into_owned());

    // Determine username and room from either the JWT or URL path params.
    let (username, lobby_id) = if let Some(ref tok) = token {
        // Token-based flow: identity and room come from the JWT claims.
        let jwt_secret = std::env::var("JWT_SECRET").unwrap_or_default();
        if jwt_secret.is_empty() {
            return Err(anyhow!("JWT_SECRET not set"));
        }
        let claims = token_validator::decode_room_token(&jwt_secret, tok).map_err(|e| {
            e.log("WT");
            anyhow!("token validation failed: {}", e.client_message())
        })?;
        info!(
            "WT token-based connection: email={}, room={}",
            claims.sub, claims.room
        );
        (claims.sub, claims.room)
    } else if !videocall_types::FeatureFlags::meeting_management_enabled() {
        // Deprecated path-based flow (FF=off only): /lobby/{username}/{room}
        if parts.len() != 3 {
            return Err(anyhow!(
                "Invalid path: expected /lobby/{{email}}/{{room}} (deprecated) or /lobby?token=<JWT>"
            ));
        }
        let username = parts[1].replace(' ', "_");
        let lobby_id = parts[2].replace(' ', "_");
        let re = regex::Regex::new(VALID_ID_PATTERN).unwrap();
        if !re.is_match(&username) || !re.is_match(&lobby_id) {
            return Err(anyhow!("Invalid path input chars"));
        }
        info!(
            "WT deprecated path-based connection: email={}, room={}",
            username, lobby_id
        );
        (username, lobby_id)
    } else {
        // FF=on but no token provided
        info!("WT connection rejected: no token provided and meeting management is enabled");
        return Err(anyhow!(
            "room access token is required. Use /lobby?token=<JWT>"
        ));
    };

    // Accept the session.
    let session = request.ok().await.context("failed to accept session")?;
    debug!("accepted session");

    // Run the session with actor
    if let Err(err) = handle_webtransport_session(
        session,
        &username,
        &lobby_id,
        chat_server,
        nats_client,
        tracker_sender,
        session_manager,
    )
    .await
    {
        info!("closing session: {}", err);
    }
    Ok(())
}

/// Handle a WebTransport session using the WtChatSession actor
#[tracing::instrument(
    level = "trace",
    skip(session, chat_server, nats_client, tracker_sender, session_manager)
)]
async fn handle_webtransport_session(
    session: Session,
    username: &str,
    lobby_id: &str,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
) -> anyhow::Result<()> {
    // Create channel for actor → WebTransport I/O
    let (outbound_tx, outbound_rx) = mpsc::channel::<WtOutbound>(256);

    // Start the WtChatSession actor
    let actor = WtChatSession::new(
        chat_server,
        lobby_id.to_string(),
        username.to_string(),
        outbound_tx,
        nats_client,
        tracker_sender,
        session_manager,
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

        // Connect to NATS
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        // Create SessionManager
        let session_manager = SessionManager::new();

        // Create connection tracker
        let (_, tracker_sender, tracker_receiver) =
            ServerDiagnostics::new_with_channel(nats_client.clone());

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
            match connect_client("test", "test").await {
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

    async fn connect_client(
        user: &str,
        meeting: &str,
    ) -> Result<web_transport_quinn::Session, Box<dyn std::error::Error>> {
        let base = std::env::var("WEBTRANSPORT_URL")
            .unwrap_or_else(|_| "https://127.0.0.1:4433".to_string());
        let url_str = format!("{}/lobby/{}/{}", base.trim_end_matches('/'), user, meeting);
        let url = url::Url::parse(&url_str)?;

        // Create WebTransport client using 0.7.3 API (same as bot)
        let client = unsafe {
            web_transport_quinn::ClientBuilder::new().with_no_certificate_verification()?
        };

        // Connect using modern API
        Ok(client.connect(url).await?)
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
                    if let Ok(mut stream) = session.accept_uni().await {
                        if let Ok(buf) = stream.read_to_end(usize::MAX).await {
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

        // FF=off: this test verifies packet relay without JWT.
        // JWT validation is tested separately in jwt_integration_tests.rs.
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        // Wrap entire test with 15 second timeout
        let test_result = tokio::time::timeout(Duration::from_secs(15), async {
            test_relay_packet_impl().await
        })
        .await;

        videocall_types::FeatureFlags::clear_meeting_management_override();

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
        let session_a = connect_client(user_a, meeting)
            .await
            .expect("connect client A");
        println!("Client A connected!");

        println!("Connecting client B: {user_b}");
        let session_b = connect_client(user_b, meeting)
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
            email: user_a.to_string(),
            ..Default::default()
        };
        let packet = VcPacketWrapper {
            packet_type: VcPacketType::MEDIA.into(),
            email: user_a.to_string(),
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
                    if let Ok(mut stream) = session.accept_uni().await {
                        if let Ok(buf) = stream.read_to_end(usize::MAX).await {
                            if !buf.is_empty() {
                                println!("Received packet on B (size: {} bytes)", buf.len());
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
        assert_eq!(sent_packet.email, received_packet.email, "email must match");
        assert_eq!(sent_packet.data, received_packet.data, "data must match");
        // Verify that server added session_id (it should not be zero)
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

        let session_a = connect_client(user_a, lobby_1)
            .await
            .expect("connect alice");
        println!("✓ Alice connected to lobby-secure");

        let session_b = connect_client(user_b, lobby_2).await.expect("connect bob");
        println!("✓ Bob connected to lobby-public");

        let session_c = connect_client(user_c, lobby_1)
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

        // Wait for Alice to receive Charlie's MEETING_STARTED broadcast
        // (Alice and Charlie are in the same room, so Alice gets notified when Charlie joins)
        wait_for_condition_bool(
            || async move { get_test_packet_counter_for_user(user_a) >= 2 },
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await
        .expect("Alice should receive Charlie's MEETING_STARTED");

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
            email: sender.to_string(),
            ..Default::default()
        };
        let packet = VcPacketWrapper {
            packet_type: VcPacketType::MEDIA.into(),
            email: sender.to_string(),
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

        // FF=off: this test verifies basic session lifecycle without JWT.
        // JWT validation is tested separately in jwt_integration_tests.rs.
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        let test_result = tokio::time::timeout(Duration::from_secs(30), async {
            test_meeting_lifecycle_impl().await
        })
        .await;

        videocall_types::FeatureFlags::clear_meeting_management_override();

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

        let session_alice = connect_client("alice", room_id)
            .await
            .expect("connect alice");
        wait_for_session_ready(&session_alice, "Alice")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Alice connected");

        // ========== STEP 2: Second user connects ==========
        println!("\n--- Step 2: Bob connects (second participant) ---");

        let session_bob = connect_client("bob", room_id).await.expect("connect bob");
        wait_for_session_ready(&session_bob, "Bob")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Bob connected");

        // ========== STEP 3: Third user connects ==========
        println!("\n--- Step 3: Charlie connects (third participant) ---");

        let session_charlie = connect_client("charlie", room_id)
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

    /// Set up env and start the real WT server for JWT tests.
    async fn setup_jwt_wt() {
        std::env::set_var("JWT_SECRET", JWT_SECRET);
        videocall_types::FeatureFlags::clear_meeting_management_override();
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

    #[actix_rt::test]
    async fn test_wt_deprecated_endpoint_works_ff_off() {
        setup_jwt_wt().await;
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        let session = connect_client("alice", "wt-jwt-room-6")
            .await
            .expect("deprecated endpoint with FF=off should work");
        wait_for_session_ready(&session, "alice")
            .await
            .expect("should receive MEETING_STARTED");
        session.close(0u32, b"done");

        videocall_types::FeatureFlags::clear_meeting_management_override();
    }

    #[actix_rt::test]
    async fn test_wt_deprecated_endpoint_rejected_ff_on() {
        setup_jwt_wt().await;
        videocall_types::FeatureFlags::set_meeting_management_override(true);

        let result = connect_client("alice", "wt-jwt-room-7").await;
        assert!(
            result.is_err(),
            "deprecated endpoint with FF=on should be rejected"
        );

        videocall_types::FeatureFlags::clear_meeting_management_override();
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
